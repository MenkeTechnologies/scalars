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
use std::collections::HashMap;

/// The desugar target a `rust { ... }` block lowers to (see [`crate::rust_ffi`]).
const RUST_COMPILE: &str = "__rust_compile";

struct Compiler {
    b: ChunkBuilder,
    /// Distinguishes synthetic `for` upper-bound locals so nested loops do not
    /// alias one another.
    for_counter: u32,
    /// When true, a per-statement `DBG_LINE` marker is emitted before each
    /// statement (for `scala --dap`). Normal runs compile with `debug=false` and
    /// carry zero extra ops.
    debug: bool,
    /// True when the program contains a `rust { ... }` FFI block (a
    /// `__rust_compile` call). Only then does an unknown call name lower to a
    /// runtime FFI dispatch instead of a compile error — so non-FFI programs keep
    /// their exact "not found" compile-time diagnostic.
    has_ffi: bool,
    /// User-defined `def`s: `name → parameter count`. A call to one lowers to an
    /// `Op::Call` into the function's `sub_entry`; a bare reference to a
    /// zero-parameter one is a paren-less call (Scala allows `def x = …; x`).
    /// Anything else stays on the FFI/compile-error path.
    func_arity: HashMap<String, usize>,
    /// Immutability of the bindings currently in scope: `name → is_val`.
    /// Reassigning a `val` (`true`) is a compile error (Scala rejects it too).
    /// Swapped out for a fresh map while a function body compiles so a `val`
    /// inside `main` cannot mask a `var` of the same name inside a `def`.
    vals: HashMap<String, bool>,
    /// `Some` while compiling a function body: maps that function's local names
    /// (parameters, then `val`/`var`/`for` locals) to frame slot indices, so
    /// each call frame gets its own copies and recursion is correct. `None` in
    /// the top-level (`main`) scope, where every binding is a global addressed
    /// by `GetVar`/`SetVar`.
    scope: Option<Scope>,
}

/// A function body's local slot map (see [`Compiler::scope`]).
struct Scope {
    /// Local name → frame slot index.
    slots: HashMap<String, u16>,
    /// Next unused slot index (parameters take `0..arity`).
    next_slot: u16,
}

/// Where a name lives: a frame slot (function-local) or a global.
#[derive(Clone, Copy)]
enum Place {
    Slot(u16),
    Global(u16),
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
    // The FFI path fires on a `rust { ... }` block anywhere — main or a `def`.
    let has_ffi = body_has_ffi(&prog.main) || prog.functions.iter().any(|f| body_has_ffi(&f.body));
    let func_arity = prog
        .functions
        .iter()
        .map(|f| (f.name.clone(), f.params.len()))
        .collect();
    let mut c = Compiler {
        b: ChunkBuilder::new(),
        for_counter: 0,
        debug,
        has_ffi,
        func_arity,
        vals: HashMap::new(),
        scope: None,
    };
    // Main body runs first (the VM starts at ip 0 in frame 0), so the tracing
    // JIT's `ip == 0` anchor still fires on real work.
    for stmt in &prog.main {
        c.stmt(stmt)?;
    }
    // Function bodies live in the same chunk after main, jumped over on the
    // fall-through so main never walks into one; each is reached only through
    // its `Op::Call` sub_entry.
    if !prog.functions.is_empty() {
        let skip = c.b.emit(Op::Jump(0), 0);
        for func in &prog.functions {
            c.function_body(func)?;
        }
        let end = c.b.current_pos();
        c.b.patch_jump(skip, end);
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
            StmtKind::Local {
                name, init, is_val, ..
            } => {
                let place = self.declare_place(name);
                self.vals.insert(name.clone(), *is_val);
                if let Some(e) = init {
                    self.expr(e)?;
                    self.emit_store(place);
                }
                // An initializer-less binding is unbound until first assigned
                // (Scala requires an initializer for concrete `val`/`var`; slice
                // 1 does not enforce it).
                Ok(())
            }
            StmtKind::Assign { name, op, value } => {
                // Scala rejects reassignment to a `val` at compile time; so do we.
                if self.vals.get(name) == Some(&true) {
                    return Err(format!(
                        "scalars: reassignment to val `{name}` (line {})",
                        s.line
                    ));
                }
                let place = self.resolve_place(name);
                match op {
                    AssignOp::Assign => {
                        self.expr(value)?;
                    }
                    AssignOp::Div => {
                        // `x /= e` → x = x / e, through the type-dispatching
                        // division builtin (see `binary`).
                        self.emit_load(place);
                        self.expr(value)?;
                        self.b.emit(Op::CallBuiltin(crate::host::SDIV, 2), 0);
                    }
                    _ => {
                        // `x <op>= e` → x = x <op> e
                        self.emit_load(place);
                        self.expr(value)?;
                        self.b.emit(compound_op(*op), 0);
                    }
                }
                self.emit_store(place);
                Ok(())
            }
            StmtKind::Return(val) => {
                // `return e` / bare `return` — leave the enclosing `def` (frame
                // popped by `ReturnValue`/`Return`). At the top level (frame 0)
                // the VM treats it as program end.
                match val {
                    Some(e) => {
                        self.expr(e)?;
                        self.b.emit(Op::ReturnValue, s.line);
                    }
                    None => {
                        self.b.emit(Op::Return, s.line);
                    }
                }
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
        // name = start  (the loop variable is a fresh local — a frame slot
        // inside a `def`, a global at top level).
        self.expr(start)?;
        let vplace = self.declare_place(name);
        self.emit_store(vplace);
        // bound = end   (synthetic local; the leading space makes the name
        // impossible to collide with a lexed identifier)
        self.for_counter += 1;
        let bound = format!(" for_end_{}", self.for_counter);
        self.expr(end)?;
        let bplace = self.declare_place(&bound);
        self.emit_store(bplace);
        // top: while name < bound (or name <= bound for `to`)
        let top = self.b.current_pos();
        self.emit_load(vplace);
        self.emit_load(bplace);
        self.b
            .emit(if inclusive { Op::NumLe } else { Op::NumLt }, 0);
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        for s in body {
            self.stmt(s)?;
        }
        // step: name += 1
        self.emit_load(vplace);
        self.b.emit(Op::LoadInt(1), 0);
        self.b.emit(Op::Add, 0);
        self.emit_store(vplace);
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
                // A function-local slot wins. Otherwise a bare reference to a
                // zero-parameter `def` is a paren-less call; anything else is a
                // global read.
                let is_local = self
                    .scope
                    .as_ref()
                    .is_some_and(|s| s.slots.contains_key(name));
                if !is_local && self.func_arity.get(name) == Some(&0) {
                    let nidx = self.b.add_name(name);
                    self.b.emit(Op::Call(nidx, 0), 0);
                } else {
                    let place = self.resolve_place(name);
                    self.emit_load(place);
                }
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
            Expr::Call { name, args, line } => self.call(name, args, *line)?,
            Expr::Method {
                recv,
                name,
                args,
                line,
            } => self.method(recv, name, args, *line)?,
        }
        Ok(())
    }

    /// Lower postfix `recv.name(args)` dispatch. The receiver is pushed first
    /// (deepest), then the arguments, then the method-name string; the universal
    /// method builtin (`SMETHOD`) pops the name, `argc-2` arguments, and the
    /// receiver, and routes to the wired String/Int stdlib (see `crate::host`).
    fn method(&mut self, recv: &Expr, name: &str, args: &[Expr], line: u32) -> Result<(), String> {
        self.expr(recv)?;
        for a in args {
            self.expr(a)?;
        }
        let nc = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(nc), line);
        // argc = receiver (1) + arguments + method-name (1).
        self.b.emit(
            Op::CallBuiltin(crate::host::SMETHOD, args.len() as u8 + 2),
            line,
        );
        Ok(())
    }

    /// Lower a named call. Two shapes reach here (slice 1 has no user methods):
    ///
    /// * `__rust_compile("<b64>", line)` — the desugar target of a `rust { ... }`
    ///   block. Compile the base64 body string and hand it to the FFI-compile
    ///   builtin; the call evaluates to `Unit`.
    /// * an FFI-exported bareword (`add(2, 3)`) — only when the program carries a
    ///   `rust { ... }` block. Push the args (deepest first) and the name, then
    ///   dispatch by name through the FFI-call builtin. Without any FFI block, an
    ///   unknown call is a compile-time error, preserving the normal diagnostic.
    fn call(&mut self, name: &str, args: &[Expr], line: u32) -> Result<(), String> {
        if name == RUST_COMPILE {
            // Only the base64 body (first arg) is needed; the line arg is dropped.
            if let Some(body) = args.first() {
                self.expr(body)?;
                self.b
                    .emit(Op::CallBuiltin(crate::host::FFI_COMPILE, 1), line);
            } else {
                self.b.emit(Op::LoadUndef, line);
            }
            return Ok(());
        }
        // A call to a user-defined `def`: push args (deepest first) and jump into
        // the function's `sub_entry` frame. The callee prologue pops these args
        // into its slots (see `function_body`).
        if self.func_arity.contains_key(name) {
            for a in args {
                self.expr(a)?;
            }
            let nidx = self.b.add_name(name);
            self.b.emit(Op::Call(nidx, args.len() as u8), line);
            return Ok(());
        }
        if !self.has_ffi {
            return Err(format!("scalars: not found: {name} (line {line})"));
        }
        for a in args {
            self.expr(a)?;
        }
        let c = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(c), line);
        // argc is the arg count plus one (the name) — see `host::b_ffi_call`.
        self.b.emit(
            Op::CallBuiltin(crate::host::FFI_CALL, args.len() as u8 + 1),
            line,
        );
        Ok(())
    }

    // ── name resolution (frame slots inside a `def`, globals at top level) ──

    /// Resolve a name that already exists (a read, or an assignment target). A
    /// function-local slot wins; otherwise it is a global.
    fn resolve_place(&mut self, name: &str) -> Place {
        if let Some(scope) = &self.scope {
            if let Some(&slot) = scope.slots.get(name) {
                return Place::Slot(slot);
            }
        }
        Place::Global(self.b.add_name(name))
    }

    /// Introduce a fresh binding (`val`/`var`/`for` local). Inside a function it
    /// takes a new frame slot (reusing the name's slot if it was already bound,
    /// matching slice-1's flat, no-block-scope model); at top level it is a
    /// global.
    fn declare_place(&mut self, name: &str) -> Place {
        if let Some(scope) = &mut self.scope {
            if let Some(&slot) = scope.slots.get(name) {
                return Place::Slot(slot);
            }
            let slot = scope.next_slot;
            scope.next_slot += 1;
            scope.slots.insert(name.to_string(), slot);
            return Place::Slot(slot);
        }
        Place::Global(self.b.add_name(name))
    }

    fn emit_load(&mut self, p: Place) {
        match p {
            Place::Slot(s) => self.b.emit(Op::GetSlot(s), 0),
            Place::Global(i) => self.b.emit(Op::GetVar(i), 0),
        };
    }

    fn emit_store(&mut self, p: Place) {
        match p {
            Place::Slot(s) => self.b.emit(Op::SetSlot(s), 0),
            Place::Global(i) => self.b.emit(Op::SetVar(i), 0),
        };
    }

    // ── user-defined functions ──────────────────────────────────────────────

    /// Lower one `def` body: register its `sub_entry`, bind parameters to the
    /// first frame slots via a pop-into-slot prologue, compile the body in
    /// tail position (its last expression is the result), and emit a trailing
    /// `Return` so a body that falls off the end yields `Unit`.
    fn function_body(&mut self, f: &Func) -> Result<(), String> {
        let nidx = self.b.add_name(&f.name);
        let ip = self.b.current_pos();
        self.b.add_sub_entry(nidx, ip);

        // Enter a fresh function scope (params occupy slots `0..arity`). Save and
        // clear `vals` so val-immutability is tracked per function body.
        let mut slots = HashMap::new();
        let saved_vals = std::mem::take(&mut self.vals);
        for (i, p) in f.params.iter().enumerate() {
            slots.insert(p.clone(), i as u16);
            // Scala method parameters are `val`s — reassigning one is an error.
            self.vals.insert(p.clone(), true);
        }
        self.scope = Some(Scope {
            slots,
            next_slot: f.params.len() as u16,
        });

        // Prologue: args arrive on the stack (deepest = param 0). Pop them into
        // their slots in reverse so each parameter lands in its own slot.
        for i in (0..f.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), 0);
        }

        self.tail(&f.body)?;
        // A body that returned on every path never reaches here; one that fell
        // through (e.g. ends in a loop) returns `Unit`.
        self.b.emit(Op::Return, 0);

        self.scope = None;
        self.vals = saved_vals;
        Ok(())
    }

    /// Compile a statement list in tail position: every leading statement is a
    /// side effect, and the final one's value becomes the function result. An
    /// `if`/`else` in tail position returns from each branch (so `def f = if (c)
    /// a else b` works, including the canonical recursive `fact`).
    fn tail(&mut self, stmts: &[Stmt]) -> Result<(), String> {
        let Some((last, init)) = stmts.split_last() else {
            self.b.emit(Op::Return, 0);
            return Ok(());
        };
        for s in init {
            self.stmt(s)?;
        }
        self.tail_stmt(last)
    }

    fn tail_stmt(&mut self, s: &Stmt) -> Result<(), String> {
        match &s.kind {
            // The value of a tail expression is the function's result.
            StmtKind::Expr(e) => {
                self.expr(e)?;
                self.b.emit(Op::ReturnValue, s.line);
                Ok(())
            }
            // An explicit `return` already lowers to Return/ReturnValue.
            StmtKind::Return(_) => self.stmt(s),
            // `if`/`else` as a tail expression: each branch is itself a tail.
            StmtKind::If { cond, then, els } => {
                self.expr(cond)?;
                let jf = self.b.emit(Op::JumpIfFalse(0), 0);
                self.tail(then)?; // then-branch returns on every path
                let else_start = self.b.current_pos();
                self.b.patch_jump(jf, else_start);
                if els.is_empty() {
                    // No `else` → the false path yields `Unit`.
                    self.b.emit(Op::Return, 0);
                } else {
                    self.tail(els)?;
                }
                Ok(())
            }
            // A loop / other statement in tail position has no value: run it for
            // its effect, then return `Unit`.
            _ => {
                self.stmt(s)?;
                self.b.emit(Op::Return, 0);
                Ok(())
            }
        }
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

// ── FFI detection (does the program contain a `rust { ... }` block?) ────────

/// True if any statement in `body` (recursively) evaluates a `__rust_compile`
/// call — the desugar target of a `rust { ... }` block.
fn body_has_ffi(body: &[Stmt]) -> bool {
    body.iter().any(|s| match &s.kind {
        StmtKind::Local { init, .. } => init.as_ref().is_some_and(expr_has_ffi),
        StmtKind::Assign { value, .. } => expr_has_ffi(value),
        StmtKind::Expr(e) => expr_has_ffi(e),
        StmtKind::If { cond, then, els } => {
            expr_has_ffi(cond) || body_has_ffi(then) || body_has_ffi(els)
        }
        StmtKind::While { cond, body } => expr_has_ffi(cond) || body_has_ffi(body),
        StmtKind::For {
            start, end, body, ..
        } => expr_has_ffi(start) || expr_has_ffi(end) || body_has_ffi(body),
        StmtKind::Return(e) => e.as_ref().is_some_and(expr_has_ffi),
    })
}

fn expr_has_ffi(e: &Expr) -> bool {
    match e {
        Expr::Call { name, args, .. } => name == RUST_COMPILE || args.iter().any(expr_has_ffi),
        Expr::Method { recv, args, .. } => expr_has_ffi(recv) || args.iter().any(expr_has_ffi),
        Expr::Unary { rhs, .. } => expr_has_ffi(rhs),
        Expr::Binary { lhs, rhs, .. } => expr_has_ffi(lhs) || expr_has_ffi(rhs),
        Expr::Println { arg, .. } => arg.as_deref().is_some_and(expr_has_ffi),
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Var(_) => false,
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
