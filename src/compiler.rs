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
use std::collections::{HashMap, HashSet};

/// The desugar target a `rust { ... }` block lowers to (see [`crate::rust_ffi`]).
const RUST_COMPILE: &str = "__rust_compile";

struct Compiler {
    b: ChunkBuilder,
    /// Distinguishes synthetic `for` upper-bound locals so nested loops do not
    /// alias one another.
    for_counter: u32,
    /// Distinguishes synthetic `match`-scrutinee locals so nested matches do not
    /// alias one another.
    match_counter: u32,
    /// Distinguishes synthetic `for … yield` result-array/length locals so
    /// nested comprehensions do not alias one another.
    yield_counter: u32,
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
    /// Class metadata (`name → (ordered field names, is_case)`), for
    /// construction, `copy`, method dispatch, and constructor-pattern binding.
    classes: HashMap<String, ClassMeta>,
    /// Singleton `object` metadata (`name → members`), for static member
    /// dispatch (`Registry.greet(x)` / `Registry.name`).
    objects: HashMap<String, ObjMeta>,
    /// Method name → the classes that define a `def` of that name, for the
    /// runtime instance-method dispatch chain (`recv.m(...)`).
    method_index: HashMap<String, Vec<String>>,
    /// `Some((name, fields))` while compiling a class method: the enclosing
    /// class's name and field-name set, so a bare identifier naming a field
    /// resolves to `this.field` and a bare sibling-method call to `this.m(...)`.
    current_class: Option<(String, HashSet<String>)>,
    /// `Some(name)` while compiling an `object`'s method (or its `val` inits), so
    /// a bare identifier naming one of the object's `val`s resolves to the
    /// `Name.val` global and a bare method call to `Name$method`.
    current_object: Option<String>,
    /// Distinguishes synthetic method-dispatch / constructor-pattern temporaries.
    obj_counter: u32,
}

/// Compile-time class shape.
struct ClassMeta {
    /// Instance fields in declared order (constructor params then body fields).
    field_names: Vec<String>,
    /// Primary-constructor arity (the `new`/`apply` argument count — the leading
    /// prefix of `field_names`).
    arity: usize,
    /// `case class` → structural semantics + companion `apply`/`unapply`/`copy`.
    is_case: bool,
}

/// Compile-time singleton-object shape.
struct ObjMeta {
    /// `val`/`var` member names (accessed as the `Name.val` global).
    vals: HashSet<String>,
    /// `def` member names (dispatched to the `Name$method` subroutine).
    methods: HashSet<String>,
    /// `case object` → structural semantics (so two `None`s compare equal).
    is_case: bool,
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

    // Classes to emit constructors/methods for: the user's, plus the built-in
    // `Option` support (`Some(value)`), unless the user redefined them.
    let mut classes: Vec<ClassDecl> = prog.classes.clone();
    if !classes.iter().any(|c| c.name == "Some") {
        classes.push(builtin_some());
    }
    // Built-in `None` case object, unless redefined.
    let mut objects: Vec<ObjectDecl> = prog.objects.clone();
    if !objects.iter().any(|o| o.name == "None") {
        objects.push(builtin_none());
    }

    // Index class shapes and the method → defining-classes map.
    let mut class_meta = HashMap::new();
    let mut method_index: HashMap<String, Vec<String>> = HashMap::new();
    for cd in &classes {
        class_meta.insert(
            cd.name.clone(),
            ClassMeta {
                field_names: cd.field_names.clone(),
                arity: cd.params.len(),
                is_case: cd.is_case,
            },
        );
        for m in &cd.methods {
            method_index
                .entry(m.name.clone())
                .or_default()
                .push(cd.name.clone());
        }
    }
    // Index singleton objects.
    let mut obj_meta = HashMap::new();
    for od in &objects {
        let vals = od
            .body
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Local { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let methods = od.methods.iter().map(|m| m.name.clone()).collect();
        obj_meta.insert(
            od.name.clone(),
            ObjMeta {
                vals,
                methods,
                is_case: od.is_case,
            },
        );
    }

    let mut c = Compiler {
        b: ChunkBuilder::new(),
        for_counter: 0,
        match_counter: 0,
        yield_counter: 0,
        debug,
        has_ffi,
        func_arity,
        vals: HashMap::new(),
        scope: None,
        classes: class_meta,
        objects: obj_meta,
        method_index,
        current_class: None,
        current_object: None,
        obj_counter: 0,
    };

    // Singleton-object `val`s initialize once before `main` (into `Name.val`
    // globals). Scala inits objects lazily; eager pre-init is a documented
    // simplification that is observably identical for pure val bodies.
    for od in &objects {
        c.object_inits(od)?;
    }

    // Main body runs first after the object inits (the VM starts at ip 0), so the
    // tracing JIT's early anchor still fires on real work.
    for stmt in &prog.main {
        c.stmt(stmt)?;
    }

    // Every subroutine (free `def`s, class constructors + methods, object
    // methods) lives after `main`, jumped over on the fall-through; each is
    // reached only through its `Op::Call` sub_entry.
    let has_subs = !prog.functions.is_empty()
        || !classes.is_empty()
        || objects.iter().any(|o| !o.methods.is_empty());
    if has_subs {
        let skip = c.b.emit(Op::Jump(0), 0);
        for func in &prog.functions {
            c.function_body(func)?;
        }
        for cd in &classes {
            c.class_constructor(cd)?;
            for m in &cd.methods {
                c.class_method(cd, m)?;
            }
        }
        for od in &objects {
            for m in &od.methods {
                c.object_method(od, m)?;
            }
        }
        let end = c.b.current_pos();
        c.b.patch_jump(skip, end);
    }
    Ok(c.b.build())
}

/// The built-in `Some(value)` case class (`Option`'s non-empty case).
fn builtin_some() -> ClassDecl {
    ClassDecl {
        name: "Some".to_string(),
        is_case: true,
        params: vec!["value".to_string()],
        body: Vec::new(),
        field_names: vec!["value".to_string()],
        methods: Vec::new(),
    }
}

/// The built-in `None` case object (`Option`'s empty case) — a zero-field
/// singleton so two `None`s compare structurally equal.
fn builtin_none() -> ObjectDecl {
    ObjectDecl {
        name: "None".to_string(),
        is_case: true,
        body: Vec::new(),
        methods: Vec::new(),
    }
}

/// The synthetic name of a class constructor subroutine.
fn ctor_name(class: &str) -> String {
    format!("{class}$new")
}

/// The synthetic name of a `class`/`object` method subroutine.
fn method_sub_name(owner: &str, method: &str) -> String {
    format!("{owner}${method}")
}

/// The global-variable name backing an `object`'s `val` member.
fn object_field_global(obj: &str, field: &str) -> String {
    format!("{obj}.{field}")
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
                let is_local = self
                    .scope
                    .as_ref()
                    .is_some_and(|s| s.slots.contains_key(name));
                // A `var` field assignment inside a method mutates the heap record.
                if !is_local {
                    if let Some((_, fields)) = &self.current_class {
                        if fields.contains(name) {
                            return self.field_assign(name, *op, value, s.line);
                        }
                    }
                    // A `var` reassignment inside an object method updates its
                    // `Name.val` global.
                    if let Some(obj) = self.current_object.clone() {
                        if self
                            .objects
                            .get(&obj)
                            .is_some_and(|m| m.vals.contains(name))
                        {
                            return self.object_val_assign(&obj, name, *op, value);
                        }
                    }
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

    /// Lower a `for` comprehension. `yield_into` carries the `(result-array,
    /// length-counter)` places for a `yield` (append each body value); it is
    /// `None` for a side-effecting `foreach`. Enumerators are lowered
    /// left-to-right by `idx`: a generator emits a counted loop whose body
    /// recurses into the next enumerator; a guard emits a conditional around the
    /// recursion (`withFilter`). The range's upper bound is snapshotted into a
    /// synthetic local (Scala evaluates the range once).
    fn lower_for(
        &mut self,
        enums: &[ForEnum],
        idx: usize,
        body: &Expr,
        yield_into: Option<(Place, Place)>,
    ) -> Result<(), String> {
        if idx == enums.len() {
            // Innermost: evaluate the body per binding.
            match yield_into {
                Some((arr, len)) => {
                    // result(len) = body; len += 1
                    self.expr(body)?;
                    self.emit_load(len);
                    self.emit_array_set(arr);
                    self.emit_load(len);
                    self.b.emit(Op::LoadInt(1), 0);
                    self.b.emit(Op::Add, 0);
                    self.emit_store(len);
                }
                None => {
                    // foreach: run for effect, discard the value.
                    self.expr(body)?;
                    self.b.emit(Op::Pop, 0);
                }
            }
            return Ok(());
        }
        match &enums[idx] {
            ForEnum::Guard(cond) => {
                self.expr(cond)?;
                let jf = self.b.emit(Op::JumpIfFalse(0), 0);
                self.lower_for(enums, idx + 1, body, yield_into)?;
                let end = self.b.current_pos();
                self.b.patch_jump(jf, end);
            }
            ForEnum::Gen {
                name,
                start,
                end,
                inclusive,
            } => {
                self.expr(start)?;
                let vplace = self.declare_place(name);
                self.emit_store(vplace);
                self.for_counter += 1;
                let bound = format!(" for_end_{}", self.for_counter);
                self.expr(end)?;
                let bplace = self.declare_place(&bound);
                self.emit_store(bplace);
                let top = self.b.current_pos();
                self.emit_load(vplace);
                self.emit_load(bplace);
                self.b
                    .emit(if *inclusive { Op::NumLe } else { Op::NumLt }, 0);
                let jf = self.b.emit(Op::JumpIfFalse(0), 0);
                self.lower_for(enums, idx + 1, body, yield_into)?;
                self.emit_load(vplace);
                self.b.emit(Op::LoadInt(1), 0);
                self.b.emit(Op::Add, 0);
                self.emit_store(vplace);
                self.b.emit(Op::Jump(top), 0);
                let end_pos = self.b.current_pos();
                self.b.patch_jump(jf, end_pos);
            }
        }
        Ok(())
    }

    /// Emit an array element-set for `place` — `[value, index]` on the stack,
    /// growing the array to fit (frame slot inside a `def`, global at top level).
    fn emit_array_set(&mut self, p: Place) {
        match p {
            Place::Slot(s) => self.b.emit(Op::SlotArraySet(s), 0),
            Place::Global(i) => self.b.emit(Op::ArraySet(i), 0),
        };
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
            Expr::Var(name) => self.var_ref(name)?,
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
            Expr::New { name, args, line } => self.construct(name, args, *line)?,
            Expr::Copy {
                recv,
                updates,
                line,
            } => self.copy_expr(recv, updates, *line)?,
            Expr::If { cond, then, els } => self.if_expr(cond, then, els.as_deref())?,
            Expr::Block(stmts) => self.block_expr(stmts)?,
            Expr::Match { scrut, arms } => self.match_expr(scrut, arms)?,
            Expr::Format { value, spec, line } => {
                self.expr(value)?;
                let c = self.b.add_constant(Value::str(spec.clone()));
                self.b.emit(Op::LoadConst(c), *line);
                self.b.emit(Op::CallBuiltin(crate::host::SFORMAT, 2), *line);
            }
            Expr::ForYield { enums, body } => {
                // result = empty Vector; len = 0
                self.b.emit(Op::MakeArray(0), 0);
                self.yield_counter += 1;
                let arr = self.declare_place(&format!(" yield_{}", self.yield_counter));
                self.emit_store(arr);
                let len = self.declare_place(&format!(" yield_len_{}", self.yield_counter));
                self.b.emit(Op::LoadInt(0), 0);
                self.emit_store(len);
                self.lower_for(enums, 0, body, Some((arr, len)))?;
                // The comprehension's value is the accumulated Vector.
                self.emit_load(arr);
            }
            Expr::ForEach { enums, body } => {
                self.lower_for(enums, 0, body, None)?;
                // `foreach` yields `Unit`.
                self.b.emit(Op::LoadUndef, 0);
            }
        }
        Ok(())
    }

    /// Lower `if (cond) then [else els]` in value position: both branches leave
    /// exactly one value on the stack; a missing `else` leaves `Unit`.
    fn if_expr(&mut self, cond: &Expr, then: &Expr, els: Option<&Expr>) -> Result<(), String> {
        self.expr(cond)?;
        let jf = self.b.emit(Op::JumpIfFalse(0), 0);
        self.expr(then)?;
        let jend = self.b.emit(Op::Jump(0), 0);
        let else_start = self.b.current_pos();
        self.b.patch_jump(jf, else_start);
        match els {
            Some(e) => self.expr(e)?,
            None => {
                self.b.emit(Op::LoadUndef, 0);
            }
        }
        let end = self.b.current_pos();
        self.b.patch_jump(jend, end);
        Ok(())
    }

    /// Lower a block used as an expression: run every statement but the last for
    /// effect, and leave the last statement's value on the stack (its expression
    /// value, or `Unit` for a non-expression last statement or an empty block).
    fn block_expr(&mut self, stmts: &[Stmt]) -> Result<(), String> {
        let Some((last, init)) = stmts.split_last() else {
            self.b.emit(Op::LoadUndef, 0);
            return Ok(());
        };
        for s in init {
            self.stmt(s)?;
        }
        match &last.kind {
            // A trailing expression (including `println`, which leaves `Unit`) is
            // the block's value.
            StmtKind::Expr(e) => self.expr(e)?,
            // A non-expression last statement runs for effect; the block is `Unit`.
            _ => {
                self.stmt(last)?;
                self.b.emit(Op::LoadUndef, 0);
            }
        }
        Ok(())
    }

    /// Lower `scrutinee match { arms }`. The scrutinee is evaluated once into a
    /// synthetic local; arms are tried top-to-bottom. Each arm tests its pattern
    /// (jumping to the next arm on mismatch), binds any variable, checks its
    /// guard, then leaves its body's value and jumps to the end. Falling off the
    /// last arm raises `scala.MatchError` (via [`crate::host::SMATCHERR`]).
    fn match_expr(&mut self, scrut: &Expr, arms: &[MatchArm]) -> Result<(), String> {
        self.expr(scrut)?;
        self.match_counter += 1;
        let sname = format!(" match_{}", self.match_counter);
        let splace = self.declare_place(&sname);
        self.emit_store(splace);

        let mut end_jumps = Vec::new();
        for arm in arms {
            let mut fail_jumps = Vec::new();
            self.match_pattern(&arm.pat, splace, &mut fail_jumps)?;
            if let Some(g) = &arm.guard {
                self.expr(g)?;
                fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
            }
            self.block_expr(&arm.body)?;
            end_jumps.push(self.b.emit(Op::Jump(0), 0));
            let next = self.b.current_pos();
            for jf in fail_jumps {
                self.b.patch_jump(jf, next);
            }
        }
        // No arm matched: raise MatchError with the scrutinee. The builtin faults
        // (halting the VM) but still returns a value to keep the stack balanced.
        self.emit_load(splace);
        self.b.emit(Op::CallBuiltin(crate::host::SMATCHERR, 1), 0);
        let end = self.b.current_pos();
        for je in end_jumps {
            self.b.patch_jump(je, end);
        }
        Ok(())
    }

    /// Match one pattern against the value in `vplace`, pushing a
    /// `JumpIfFalse` onto `fail_jumps` at each point that must branch to the next
    /// arm on mismatch. Recurses for constructor sub-patterns.
    fn match_pattern(
        &mut self,
        pat: &Pattern,
        vplace: Place,
        fail_jumps: &mut Vec<usize>,
    ) -> Result<(), String> {
        match pat {
            Pattern::Wildcard => {}
            Pattern::Bind(name) => {
                let p = self.declare_place(name);
                self.emit_load(vplace);
                self.emit_store(p);
            }
            Pattern::Literal(lit) => {
                self.emit_load(vplace);
                self.expr(lit)?;
                self.b.emit(Op::NumEq, 0);
                fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
            }
            Pattern::Typed { name, ty } => {
                self.emit_load(vplace);
                let c = self.b.add_constant(Value::str(ty.clone()));
                self.b.emit(Op::LoadConst(c), 0);
                self.b.emit(Op::CallBuiltin(crate::host::SISTYPE, 2), 0);
                fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
                if name != "_" {
                    let p = self.declare_place(name);
                    self.emit_load(vplace);
                    self.emit_store(p);
                }
            }
            Pattern::Stable(name) => {
                // `case None =>` / a stable-identifier pattern: `scrut == <value>`.
                self.emit_load(vplace);
                self.materialize_object(name)?;
                self.b.emit(Op::NumEq, 0);
                fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
            }
            Pattern::Constructor { name, elems } => {
                let fields = match self.classes.get(name) {
                    Some(meta) => meta.field_names.clone(),
                    None => {
                        return Err(format!("scalars: not found: constructor pattern `{name}`"))
                    }
                };
                if elems.len() != fields.len() {
                    return Err(format!(
                        "scalars: wrong number of arguments for constructor pattern `{name}` (expected {}, found {})",
                        fields.len(),
                        elems.len()
                    ));
                }
                // Class-tag test: `OBJ_CLASS(scrut) == name`.
                self.emit_load(vplace);
                self.b.emit(Op::CallBuiltin(crate::host::OBJ_CLASS, 1), 0);
                let c = self.b.add_constant(Value::str(name.clone()));
                self.b.emit(Op::LoadConst(c), 0);
                self.b.emit(Op::NumEq, 0);
                fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
                // Bind each field position against its sub-pattern.
                for (elem, fname) in elems.iter().zip(&fields) {
                    self.emit_load(vplace);
                    let fc = self.b.add_constant(Value::str(fname.clone()));
                    self.b.emit(Op::LoadConst(fc), 0);
                    self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), 0);
                    self.obj_counter += 1;
                    let fp = self.declare_place(&format!(" fld_{}", self.obj_counter));
                    self.emit_store(fp);
                    self.match_pattern(elem, fp, fail_jumps)?;
                }
            }
        }
        Ok(())
    }

    /// Push a singleton `object`/`case object` value (e.g. `None`) — a zero-field
    /// host-heap record tagged with the object's name.
    fn materialize_object(&mut self, name: &str) -> Result<(), String> {
        let is_case = match self.objects.get(name) {
            Some(meta) => meta.is_case,
            None => return Err(format!("scalars: not found: value {name}")),
        };
        let cn = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(cn), 0);
        let csv = self.b.add_constant(Value::str(String::new()));
        self.b.emit(Op::LoadConst(csv), 0);
        self.b
            .emit(if is_case { Op::LoadTrue } else { Op::LoadFalse }, 0);
        // A singleton object — render as its bare name.
        self.b.emit(Op::LoadTrue, 0);
        self.b.emit(Op::CallBuiltin(crate::host::OBJ_NEW, 4), 0);
        Ok(())
    }

    /// Read a bare identifier. Resolution order: function-local slot, enclosing
    /// class field (`this.field`) or sibling method, enclosing object `val`/method,
    /// singleton object value, zero-arg `def` (paren-less call), then global.
    fn var_ref(&mut self, name: &str) -> Result<(), String> {
        let is_local = self
            .scope
            .as_ref()
            .is_some_and(|s| s.slots.contains_key(name));
        if is_local {
            let place = self.resolve_place(name);
            self.emit_load(place);
            return Ok(());
        }
        // Inside a class method: a bare field is `this.field`; a bare sibling
        // (zero-arg) method is `this.m`.
        if let Some((cname, fields)) = self.current_class.clone() {
            if fields.contains(name) {
                self.emit_field_get_this(name);
                return Ok(());
            }
            if self.class_defines_method(&cname, name) {
                let this = self.resolve_place("this");
                self.emit_load(this);
                let nidx = self.b.add_name(&method_sub_name(&cname, name));
                self.b.emit(Op::Call(nidx, 1), 0);
                return Ok(());
            }
        }
        // Inside an object method / val-init: a bare `val` is the `Name.val`
        // global; a bare (zero-arg) method is `Name$method`.
        if let Some(obj) = self.current_object.clone() {
            if let Some(meta) = self.objects.get(&obj) {
                if meta.vals.contains(name) {
                    let g = self.b.add_name(&object_field_global(&obj, name));
                    self.b.emit(Op::GetVar(g), 0);
                    return Ok(());
                }
                if meta.methods.contains(name) {
                    let nidx = self.b.add_name(&method_sub_name(&obj, name));
                    self.b.emit(Op::Call(nidx, 0), 0);
                    return Ok(());
                }
            }
        }
        // A bare reference to a singleton object (e.g. `None`) materializes it.
        if self.objects.contains_key(name) {
            return self.materialize_object(name);
        }
        // A bare reference to a zero-parameter `def` is a paren-less call.
        if self.func_arity.get(name) == Some(&0) {
            let nidx = self.b.add_name(name);
            self.b.emit(Op::Call(nidx, 0), 0);
            return Ok(());
        }
        let place = self.resolve_place(name);
        self.emit_load(place);
        Ok(())
    }

    /// Whether class `cname` declares a `def` named `method`.
    fn class_defines_method(&self, cname: &str, method: &str) -> bool {
        self.method_index
            .get(method)
            .is_some_and(|cs| cs.iter().any(|c| c == cname))
    }

    /// Emit a read of `this.field` (the receiver is the `this` slot).
    fn emit_field_get_this(&mut self, field: &str) {
        let this = self.resolve_place("this");
        self.emit_load(this);
        let c = self.b.add_constant(Value::str(field.to_string()));
        self.b.emit(Op::LoadConst(c), 0);
        self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), 0);
    }

    /// Lower a `var` field assignment inside a method (`field <op>= e`) to an
    /// in-place [`OBJ_SET`] on `this` (a compound op reads the field first).
    fn field_assign(
        &mut self,
        field: &str,
        op: AssignOp,
        value: &Expr,
        line: u32,
    ) -> Result<(), String> {
        // OBJ_SET pops `[this, name, value]`.
        let this = self.resolve_place("this");
        self.emit_load(this);
        let c = self.b.add_constant(Value::str(field.to_string()));
        self.b.emit(Op::LoadConst(c), line);
        match op {
            AssignOp::Assign => {
                self.expr(value)?;
            }
            AssignOp::Div => {
                self.emit_field_get_this(field);
                self.expr(value)?;
                self.b.emit(Op::CallBuiltin(crate::host::SDIV, 2), 0);
            }
            _ => {
                self.emit_field_get_this(field);
                self.expr(value)?;
                self.b.emit(compound_op(op), 0);
            }
        }
        self.b.emit(Op::CallBuiltin(crate::host::OBJ_SET, 3), line);
        self.b.emit(Op::Pop, 0); // discard the `Unit` result
        Ok(())
    }

    /// Lower a `var` reassignment inside an object method to a store into the
    /// object's `Name.val` global.
    fn object_val_assign(
        &mut self,
        obj: &str,
        name: &str,
        op: AssignOp,
        value: &Expr,
    ) -> Result<(), String> {
        let g = self.b.add_name(&object_field_global(obj, name));
        match op {
            AssignOp::Assign => {
                self.expr(value)?;
            }
            AssignOp::Div => {
                self.b.emit(Op::GetVar(g), 0);
                self.expr(value)?;
                self.b.emit(Op::CallBuiltin(crate::host::SDIV, 2), 0);
            }
            _ => {
                self.b.emit(Op::GetVar(g), 0);
                self.expr(value)?;
                self.b.emit(compound_op(op), 0);
            }
        }
        self.b.emit(Op::SetVar(g), 0);
        Ok(())
    }

    /// Lower `new Class(args)` / a `case class` companion `apply` — both invoke
    /// the class's `Class$new` constructor subroutine, which builds the record.
    fn construct(&mut self, name: &str, args: &[Expr], line: u32) -> Result<(), String> {
        let arity = match self.classes.get(name) {
            Some(meta) => meta.arity,
            None => return Err(format!("scalars: not found: type {name} (line {line})")),
        };
        if args.len() != arity {
            return Err(format!(
                "scalars: {name} takes {arity} constructor argument(s), found {} (line {line})",
                args.len()
            ));
        }
        for a in args {
            self.expr(a)?;
        }
        let nidx = self.b.add_name(&ctor_name(name));
        self.b.emit(Op::Call(nidx, args.len() as u8), line);
        Ok(())
    }

    /// Lower `recv.copy(updates)` — clone `recv`'s record with the named
    /// (`field = e`) or positional updates applied, via the [`OBJ_COPY`] builtin.
    fn copy_expr(
        &mut self,
        recv: &Expr,
        updates: &[(Option<String>, Expr)],
        line: u32,
    ) -> Result<(), String> {
        self.expr(recv)?;
        // Spec CSV: a field name for a named update, `#index` for a positional one.
        let spec = updates
            .iter()
            .enumerate()
            .map(|(i, (named, _))| named.clone().unwrap_or_else(|| format!("#{i}")))
            .collect::<Vec<_>>()
            .join(",");
        let sc = self.b.add_constant(Value::str(spec));
        self.b.emit(Op::LoadConst(sc), line);
        for (_, val) in updates {
            self.expr(val)?;
        }
        self.b.emit(
            Op::CallBuiltin(crate::host::OBJ_COPY, updates.len() as u8 + 2),
            line,
        );
        Ok(())
    }

    /// Lower postfix `recv.name(args)`. Dispatch order:
    ///
    /// 1. **Static object member** — `Obj.method(...)` calls `Obj$method`;
    ///    `Obj.val` reads the `Obj.val` global.
    /// 2. **Instance method** — when some class declares `def name`, emit a
    ///    runtime class-tag dispatch chain (with a [`SMETHOD`] fallback).
    /// 3. **Fallback** — the universal `SMETHOD` builtin (String/Int/Double
    ///    stdlib and host-heap field/`toString`/`hashCode`/`equals` access).
    fn method(&mut self, recv: &Expr, name: &str, args: &[Expr], line: u32) -> Result<(), String> {
        if let Expr::Var(obj) = recv {
            let member = self
                .objects
                .get(obj)
                .map(|m| (m.methods.contains(name), m.vals.contains(name)));
            if let Some((is_method, is_val)) = member {
                if is_method {
                    for a in args {
                        self.expr(a)?;
                    }
                    let nidx = self.b.add_name(&method_sub_name(obj, name));
                    self.b.emit(Op::Call(nidx, args.len() as u8), line);
                    return Ok(());
                }
                if is_val && args.is_empty() {
                    let g = self.b.add_name(&object_field_global(obj, name));
                    self.b.emit(Op::GetVar(g), line);
                    return Ok(());
                }
            }
        }
        if let Some(classes) = self.method_index.get(name).cloned() {
            return self.dispatch_instance_method(recv, name, args, &classes, line);
        }
        self.emit_smethod(recv, name, args, line)
    }

    /// Emit the universal [`SMETHOD`] dispatch: receiver (deepest), args, then the
    /// method-name string.
    fn emit_smethod(
        &mut self,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<(), String> {
        self.expr(recv)?;
        for a in args {
            self.expr(a)?;
        }
        let nc = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(nc), line);
        self.b.emit(
            Op::CallBuiltin(crate::host::SMETHOD, args.len() as u8 + 2),
            line,
        );
        Ok(())
    }

    /// Emit a runtime class-tag dispatch chain for `recv.name(args)`: evaluate the
    /// receiver once, read its class, and for each class defining `name` compare
    /// the tag and call `Class$name` with `this` + args. A receiver whose class
    /// matches none falls back to [`SMETHOD`] (non-object receivers, field reads).
    fn dispatch_instance_method(
        &mut self,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        classes: &[String],
        line: u32,
    ) -> Result<(), String> {
        self.expr(recv)?;
        self.obj_counter += 1;
        let n = self.obj_counter;
        let t = self.declare_place(&format!(" recv_{n}"));
        self.emit_store(t);
        self.emit_load(t);
        self.b
            .emit(Op::CallBuiltin(crate::host::OBJ_CLASS, 1), line);
        let cls = self.declare_place(&format!(" cls_{n}"));
        self.emit_store(cls);

        let mut end_jumps = Vec::new();
        for class in classes {
            self.emit_load(cls);
            let cc = self.b.add_constant(Value::str(class.clone()));
            self.b.emit(Op::LoadConst(cc), line);
            self.b.emit(Op::NumEq, line);
            let jf = self.b.emit(Op::JumpIfFalse(0), line);
            self.emit_load(t);
            for a in args {
                self.expr(a)?;
            }
            let nidx = self.b.add_name(&method_sub_name(class, name));
            self.b.emit(Op::Call(nidx, args.len() as u8 + 1), line);
            end_jumps.push(self.b.emit(Op::Jump(0), line));
            let next = self.b.current_pos();
            self.b.patch_jump(jf, next);
        }
        // Fallback: universal dispatcher on the stored receiver.
        self.emit_load(t);
        for a in args {
            self.expr(a)?;
        }
        let nc = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(nc), line);
        self.b.emit(
            Op::CallBuiltin(crate::host::SMETHOD, args.len() as u8 + 2),
            line,
        );
        let end = self.b.current_pos();
        for je in end_jumps {
            self.b.patch_jump(je, end);
        }
        Ok(())
    }

    // ── class / object subroutine emission ──────────────────────────────────

    /// Emit `object`-`val` initialization (before `main`) into the `Name.val`
    /// globals; run any side-effecting body statement for effect.
    fn object_inits(&mut self, od: &ObjectDecl) -> Result<(), String> {
        let saved = self.current_object.take();
        self.current_object = Some(od.name.clone());
        for s in &od.body {
            match &s.kind {
                StmtKind::Local { name, init, .. } => {
                    if let Some(e) = init {
                        self.expr(e)?;
                        let g = self.b.add_name(&object_field_global(&od.name, name));
                        self.b.emit(Op::SetVar(g), 0);
                    }
                }
                _ => self.stmt(s)?,
            }
        }
        self.current_object = saved;
        Ok(())
    }

    /// Emit a class's `Class$new` constructor subroutine: bind the constructor
    /// params to slots, run the body (evaluating `val`/`var` field initializers
    /// into their slots), then assemble the ordered record via [`OBJ_NEW`].
    fn class_constructor(&mut self, cd: &ClassDecl) -> Result<(), String> {
        let nidx = self.b.add_name(&ctor_name(&cd.name));
        let ip = self.b.current_pos();
        self.b.add_sub_entry(nidx, ip);

        let mut slots = HashMap::new();
        let saved_vals = std::mem::take(&mut self.vals);
        for (i, p) in cd.params.iter().enumerate() {
            slots.insert(p.clone(), i as u16);
            self.vals.insert(p.clone(), true);
        }
        self.scope = Some(Scope {
            slots,
            next_slot: cd.params.len() as u16,
        });
        for i in (0..cd.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), 0);
        }
        for s in &cd.body {
            self.stmt(s)?;
        }
        // Assemble the record: field values (in declared order), then the class
        // name, the field-name CSV, and the `is_case` flag.
        for f in &cd.field_names {
            let place = self.resolve_place(f);
            self.emit_load(place);
        }
        let cn = self.b.add_constant(Value::str(cd.name.clone()));
        self.b.emit(Op::LoadConst(cn), 0);
        let csv = self.b.add_constant(Value::str(cd.field_names.join(",")));
        self.b.emit(Op::LoadConst(csv), 0);
        self.b.emit(
            if cd.is_case {
                Op::LoadTrue
            } else {
                Op::LoadFalse
            },
            0,
        );
        // A `class`/`case class` is not a singleton object.
        self.b.emit(Op::LoadFalse, 0);
        self.b.emit(
            Op::CallBuiltin(crate::host::OBJ_NEW, cd.field_names.len() as u8 + 4),
            0,
        );
        self.b.emit(Op::ReturnValue, 0);

        self.scope = None;
        self.vals = saved_vals;
        Ok(())
    }

    /// Emit a class method as the `Class$method` subroutine: an implicit leading
    /// `this` slot, then the declared params; the body compiles with the class's
    /// field set in scope (bare fields resolve to `this.field`).
    fn class_method(&mut self, cd: &ClassDecl, m: &Func) -> Result<(), String> {
        let nidx = self.b.add_name(&method_sub_name(&cd.name, &m.name));
        let ip = self.b.current_pos();
        self.b.add_sub_entry(nidx, ip);

        let mut slots = HashMap::new();
        slots.insert("this".to_string(), 0u16);
        let saved_vals = std::mem::take(&mut self.vals);
        self.vals.insert("this".to_string(), true);
        for (i, p) in m.params.iter().enumerate() {
            slots.insert(p.clone(), (i + 1) as u16);
            self.vals.insert(p.clone(), true);
        }
        self.scope = Some(Scope {
            slots,
            next_slot: (m.params.len() + 1) as u16,
        });
        // Prologue: args arrive as `[this, p0, …]` (deepest = this); pop reverse.
        for i in (0..=m.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), 0);
        }
        let saved_class = self.current_class.take();
        self.current_class = Some((cd.name.clone(), cd.field_names.iter().cloned().collect()));
        self.tail(&m.body)?;
        self.b.emit(Op::Return, 0);
        self.current_class = saved_class;

        self.scope = None;
        self.vals = saved_vals;
        Ok(())
    }

    /// Emit an object method as the `Name$method` subroutine (no `this`); the
    /// body compiles with the object's `val`s reachable as `Name.val` globals.
    fn object_method(&mut self, od: &ObjectDecl, m: &Func) -> Result<(), String> {
        let nidx = self.b.add_name(&method_sub_name(&od.name, &m.name));
        let ip = self.b.current_pos();
        self.b.add_sub_entry(nidx, ip);

        let mut slots = HashMap::new();
        let saved_vals = std::mem::take(&mut self.vals);
        for (i, p) in m.params.iter().enumerate() {
            slots.insert(p.clone(), i as u16);
            self.vals.insert(p.clone(), true);
        }
        self.scope = Some(Scope {
            slots,
            next_slot: m.params.len() as u16,
        });
        for i in (0..m.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), 0);
        }
        let saved_obj = self.current_object.take();
        self.current_object = Some(od.name.clone());
        self.tail(&m.body)?;
        self.b.emit(Op::Return, 0);
        self.current_object = saved_obj;

        self.scope = None;
        self.vals = saved_vals;
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
        // `Class(args)` — a `case class` companion `apply` (construct without
        // `new`) / built-in `Some(v)`. A plain class has no companion `apply`, so
        // it must be built with `new` (bare `PlainClass(args)` is not a call).
        if self.classes.get(name).is_some_and(|m| m.is_case) {
            return self.construct(name, args, line);
        }
        // An unqualified method call inside a class method (`m(x)` == `this.m(x)`).
        if let Some((cname, _)) = self.current_class.clone() {
            if self.class_defines_method(&cname, name) {
                let this = self.resolve_place("this");
                self.emit_load(this);
                for a in args {
                    self.expr(a)?;
                }
                let nidx = self.b.add_name(&method_sub_name(&cname, name));
                self.b.emit(Op::Call(nidx, args.len() as u8 + 1), line);
                return Ok(());
            }
        }
        // An unqualified method call inside an object method (`m(x)` == `Obj.m(x)`).
        if let Some(obj) = self.current_object.clone() {
            if self
                .objects
                .get(&obj)
                .is_some_and(|meta| meta.methods.contains(name))
            {
                for a in args {
                    self.expr(a)?;
                }
                let nidx = self.b.add_name(&method_sub_name(&obj, name));
                self.b.emit(Op::Call(nidx, args.len() as u8), line);
                return Ok(());
            }
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
        StmtKind::Return(e) => e.as_ref().is_some_and(expr_has_ffi),
    })
}

fn expr_has_ffi(e: &Expr) -> bool {
    match e {
        Expr::Call { name, args, .. } => name == RUST_COMPILE || args.iter().any(expr_has_ffi),
        Expr::Method { recv, args, .. } => expr_has_ffi(recv) || args.iter().any(expr_has_ffi),
        Expr::New { args, .. } => args.iter().any(expr_has_ffi),
        Expr::Copy { recv, updates, .. } => {
            expr_has_ffi(recv) || updates.iter().any(|(_, e)| expr_has_ffi(e))
        }
        Expr::Unary { rhs, .. } => expr_has_ffi(rhs),
        Expr::Binary { lhs, rhs, .. } => expr_has_ffi(lhs) || expr_has_ffi(rhs),
        Expr::Println { arg, .. } => arg.as_deref().is_some_and(expr_has_ffi),
        Expr::If { cond, then, els } => {
            expr_has_ffi(cond) || expr_has_ffi(then) || els.as_deref().is_some_and(expr_has_ffi)
        }
        Expr::Block(stmts) => body_has_ffi(stmts),
        Expr::Match { scrut, arms } => {
            expr_has_ffi(scrut)
                || arms
                    .iter()
                    .any(|a| a.guard.as_ref().is_some_and(expr_has_ffi) || body_has_ffi(&a.body))
        }
        Expr::Format { value, .. } => expr_has_ffi(value),
        Expr::ForYield { enums, body } | Expr::ForEach { enums, body } => {
            enums.iter().any(enum_has_ffi) || expr_has_ffi(body)
        }
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Var(_) => false,
    }
}

/// True if a `for` enumerator (generator bounds / guard) evaluates an FFI call.
fn enum_has_ffi(e: &ForEnum) -> bool {
    match e {
        ForEnum::Gen { start, end, .. } => expr_has_ffi(start) || expr_has_ffi(end),
        ForEnum::Guard(c) => expr_has_ffi(c),
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
