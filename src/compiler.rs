//! Lower the Scala AST to a `fusevm::Chunk`.
//!
//! There is no bespoke VM or JVM here: statements and expressions emit fusevm
//! ops (`LoadInt`, `Add`, `GetVar`, `JumpIfFalse`, `CallBuiltin`, …) into a
//! `ChunkBuilder`, and fusevm runs the chunk on its three-tier Cranelift JIT.
//! Scala values ride the fusevm value model; the strict numeric hook in
//! `crate::host` supplies string `+` concatenation for the mixed operands the
//! VM's native arithmetic does not compute.
//!
//! Locals are addressed by name through `GetVar`/`SetVar` (slice 1 has a single
//! entry frame with no lexical scopes), so this stays a direct, readable
//! lowering. Scala has no `break`/`continue`, so loops need no backpatch stack;
//! a `for` range snapshots its upper bound into a synthetic local, matching
//! Scala's evaluate-the-range-once semantics.

use crate::ast::*;
use fusevm::{Chunk, ChunkBuilder, Op, Value};

struct Compiler {
    b: ChunkBuilder,
    /// Distinguishes synthetic `for` upper-bound locals so nested loops do not
    /// alias one another.
    for_counter: u32,
    /// When true, a per-statement `DBG_LINE` marker is emitted before each
    /// statement (for `scala --dap`). Normal runs compile with `debug=false` and
    /// carry zero extra ops.
    debug: bool,
}

/// Compile a parsed [`Program`]'s entry body to a runnable fusevm chunk.
pub fn compile(prog: &Program) -> Result<Chunk, String> {
    compile_inner(prog, false)
}

/// Compile with a per-statement `DBG_LINE` marker before each statement, on the
/// statement's source line, so the DAP adapter can stop on breakpoints and step.
/// The marker is `CallBuiltin(DBG_LINE, 0)` followed by a `Pop` to stay stack
/// balanced (the builtin returns `Unit`); it never runs under the tracing JIT
/// (see `crate::run_chunk_debug`), so hot loops keep their markers.
pub fn compile_debug(prog: &Program) -> Result<Chunk, String> {
    compile_inner(prog, true)
}

fn compile_inner(prog: &Program, debug: bool) -> Result<Chunk, String> {
    let mut c = Compiler {
        b: ChunkBuilder::new(),
        for_counter: 0,
        debug,
    };
    for stmt in &prog.main {
        c.stmt(stmt)?;
    }
    Ok(c.b.build())
}

impl Compiler {
    fn stmt(&mut self, s: &Stmt) -> Result<(), String> {
        if self.debug && s.line != 0 {
            self.b
                .emit(Op::CallBuiltin(crate::host::DBG_LINE, 0), s.line);
            self.b.emit(Op::Pop, s.line);
        }
        match &s.kind {
            StmtKind::Local { name, init, .. } => {
                if let Some(e) = init {
                    self.expr(e)?;
                    let idx = self.b.add_name(name);
                    self.b.emit(Op::SetVar(idx), 0);
                }
                // An initializer-less binding is unbound until first assigned
                // (Scala requires an initializer for concrete `val`/`var`; slice
                // 1 does not enforce it).
                Ok(())
            }
            StmtKind::Assign { name, op, value } => {
                let idx = self.b.add_name(name);
                match op {
                    AssignOp::Assign => {
                        self.expr(value)?;
                    }
                    AssignOp::Div => {
                        // `x /= e` → x = x / e, through the type-dispatching
                        // division builtin (see `binary`).
                        self.b.emit(Op::GetVar(idx), 0);
                        self.expr(value)?;
                        self.b.emit(Op::CallBuiltin(crate::host::SDIV, 2), 0);
                    }
                    _ => {
                        // `x <op>= e` → x = x <op> e
                        self.b.emit(Op::GetVar(idx), 0);
                        self.expr(value)?;
                        self.b.emit(compound_op(*op), 0);
                    }
                }
                self.b.emit(Op::SetVar(idx), 0);
                Ok(())
            }
            StmtKind::Expr(Expr::Println { newline, arg }) => {
                // The print builtin returns `Unit`; discard it in statement
                // position.
                self.println(*newline, arg.as_deref())?;
                self.b.emit(Op::Pop, 0);
                Ok(())
            }
            StmtKind::Expr(e) => {
                self.expr(e)?;
                self.b.emit(Op::Pop, 0);
                Ok(())
            }
            StmtKind::If { cond, then, els } => self.if_stmt(cond, then, els),
            StmtKind::While { cond, body } => self.while_stmt(cond, body),
            StmtKind::For {
                name,
                start,
                end,
                inclusive,
                body,
            } => self.for_stmt(name, start, end, *inclusive, body),
        }
    }

    fn if_stmt(&mut self, cond: &Expr, then: &[Stmt], els: &[Stmt]) -> Result<(), String> {
        self.expr(cond)?;
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        for s in then {
            self.stmt(s)?;
        }
        if els.is_empty() {
            let end = self.b.current_pos();
            self.b.patch_jump(jf, end);
        } else {
            let jend = self.b.emit(Op::Jump(0), 0);
            let else_start = self.b.current_pos();
            self.b.patch_jump(jf, else_start);
            for s in els {
                self.stmt(s)?;
            }
            let end = self.b.current_pos();
            self.b.patch_jump(jend, end);
        }
        Ok(())
    }

    fn while_stmt(&mut self, cond: &Expr, body: &[Stmt]) -> Result<(), String> {
        let top = self.b.current_pos();
        self.expr(cond)?;
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        for s in body {
            self.stmt(s)?;
        }
        self.b.emit(Op::Jump(top), 0);
        let end = self.b.current_pos();
        self.b.patch_jump(jf, end);
        Ok(())
    }

    /// Lower `for (name <- start until|to end) body` to a counted loop. The
    /// upper bound is evaluated once into a synthetic local so a body that
    /// mutates its inputs cannot change the range (Scala snapshots the range).
    fn for_stmt(
        &mut self,
        name: &str,
        start: &Expr,
        end: &Expr,
        inclusive: bool,
        body: &[Stmt],
    ) -> Result<(), String> {
        // name = start
        self.expr(start)?;
        let vidx = self.b.add_name(name);
        self.b.emit(Op::SetVar(vidx), 0);
        // bound = end   (synthetic local; the leading space makes the name
        // impossible to collide with a lexed identifier)
        self.for_counter += 1;
        let bound = format!(" for_end_{}", self.for_counter);
        self.expr(end)?;
        let bidx = self.b.add_name(&bound);
        self.b.emit(Op::SetVar(bidx), 0);
        // top: while name < bound (or name <= bound for `to`)
        let top = self.b.current_pos();
        self.b.emit(Op::GetVar(vidx), 0);
        self.b.emit(Op::GetVar(bidx), 0);
        self.b
            .emit(if inclusive { Op::NumLe } else { Op::NumLt }, 0);
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        for s in body {
            self.stmt(s)?;
        }
        // step: name += 1
        self.b.emit(Op::GetVar(vidx), 0);
        self.b.emit(Op::LoadInt(1), 0);
        self.b.emit(Op::Add, 0);
        self.b.emit(Op::SetVar(vidx), 0);
        self.b.emit(Op::Jump(top), 0);
        let end_pos = self.b.current_pos();
        self.b.patch_jump(jf, end_pos);
        Ok(())
    }

    /// Lower `println(arg)` / `print(arg)` to the Scala-formatting print builtin.
    /// Leaves the builtin's `Unit` return value on the stack.
    fn println(&mut self, newline: bool, arg: Option<&Expr>) -> Result<(), String> {
        let n = match arg {
            Some(e) => {
                self.expr(e)?;
                1
            }
            None => 0,
        };
        let id = if newline {
            crate::host::SPRINTLN
        } else {
            crate::host::SPRINT
        };
        self.b.emit(Op::CallBuiltin(id, n), 0);
        Ok(())
    }

    fn expr(&mut self, e: &Expr) -> Result<(), String> {
        match e {
            Expr::Int(n) => {
                self.b.emit(Op::LoadInt(*n), 0);
            }
            Expr::Float(f) => {
                let c = self.b.add_constant(Value::float(*f));
                self.b.emit(Op::LoadConst(c), 0);
            }
            Expr::Str(s) => {
                let c = self.b.add_constant(Value::str(s.clone()));
                self.b.emit(Op::LoadConst(c), 0);
            }
            Expr::Bool(b) => {
                self.b
                    .emit(if *b { Op::LoadTrue } else { Op::LoadFalse }, 0);
            }
            Expr::Null => {
                let c = self.b.add_constant(Value::Undef);
                self.b.emit(Op::LoadConst(c), 0);
            }
            Expr::Var(name) => {
                let idx = self.b.add_name(name);
                self.b.emit(Op::GetVar(idx), 0);
            }
            Expr::Unary { op, rhs } => {
                self.expr(rhs)?;
                match op {
                    UnOp::Neg => {
                        self.b.emit(Op::Negate, 0);
                    }
                    UnOp::Not => {
                        self.b.emit(Op::LogNot, 0);
                    }
                }
            }
            Expr::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs)?,
            // Println in value position: the print builtin already leaves its
            // `Unit` return value on the stack.
            Expr::Println { newline, arg } => {
                self.println(*newline, arg.as_deref())?;
            }
        }
        Ok(())
    }

    fn binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<(), String> {
        // `&&` / `||` short-circuit: keep the deciding operand as the result.
        match op {
            BinOp::And => {
                self.expr(lhs)?;
                let jf = self.b.emit(Op::JumpIfFalseKeep(0), 0);
                self.b.emit(Op::Pop, 0);
                self.expr(rhs)?;
                let end = self.b.current_pos();
                self.b.patch_jump(jf, end);
                return Ok(());
            }
            BinOp::Or => {
                self.expr(lhs)?;
                let jt = self.b.emit(Op::JumpIfTrueKeep(0), 0);
                self.b.emit(Op::Pop, 0);
                self.expr(rhs)?;
                let end = self.b.current_pos();
                self.b.patch_jump(jt, end);
                return Ok(());
            }
            _ => {}
        }
        self.expr(lhs)?;
        self.expr(rhs)?;
        // Scala `/` truncates for two `Int`s; fusevm's native `Op::Div` always
        // floats, so route division through the type-dispatching host builtin.
        if let BinOp::Div = op {
            self.b.emit(Op::CallBuiltin(crate::host::SDIV, 2), 0);
            return Ok(());
        }
        let vop = match op {
            BinOp::Add => Op::Add,
            BinOp::Sub => Op::Sub,
            BinOp::Mul => Op::Mul,
            BinOp::Div => unreachable!("division routed through the SDIV builtin above"),
            BinOp::Mod => Op::Mod,
            BinOp::Eq => Op::NumEq,
            BinOp::Ne => Op::NumNe,
            BinOp::Lt => Op::NumLt,
            BinOp::Gt => Op::NumGt,
            BinOp::Le => Op::NumLe,
            BinOp::Ge => Op::NumGe,
            BinOp::And | BinOp::Or => unreachable!("handled above"),
        };
        self.b.emit(vop, 0);
        Ok(())
    }
}

fn compound_op(op: AssignOp) -> Op {
    match op {
        AssignOp::Add => Op::Add,
        AssignOp::Sub => Op::Sub,
        AssignOp::Mul => Op::Mul,
        AssignOp::Div => unreachable!("`/=` routed through the SDIV builtin"),
        AssignOp::Mod => Op::Mod,
        AssignOp::Assign => unreachable!("plain assign never lowers through compound_op"),
    }
}
