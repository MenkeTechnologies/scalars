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
use std::collections::{HashMap, HashSet, VecDeque};

/// The desugar target a `rust { ... }` block lowers to (see [`crate::rust_ffi`]).
const RUST_COMPILE: &str = "__rust_compile";

/// The synthetic call name `desugar_for` emits for a range generator inside a
/// collection comprehension (materialized to a `List` via [`crate::host::RANGE_LIST`]).
const RANGE_LIST_CALL: &str = "$range_list";

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
    /// Lambda bodies discovered while lowering, awaiting emission as subroutine
    /// regions (drained after `main` + the class/object/function subs; draining a
    /// closure may enqueue further nested closures).
    pending_closures: VecDeque<PendingClosure>,
    /// Monotonic id for synthetic closure body names (`$closure_0`, …).
    closures_seen: u32,
    /// True when the program contains a `try` anywhere. Only then are the
    /// per-statement unwind checks emitted, so an exception-free program keeps
    /// byte-identical bytecode (and its speed).
    has_try: bool,
    /// Where a pending exception unwinds to from the statement currently being
    /// compiled — innermost last. Empty means top level (abort the run).
    unwind: Vec<UnwindFrame>,
    /// Distinguishes synthetic `try`-result locals so nested `try`s do not alias.
    try_counter: u32,
}

/// Where the unwind check emitted after a statement jumps when an exception is
/// in flight. Each variant corresponds to one enclosing construct, and they
/// compose: a raise inside a loop inside a `def` inside a `try` breaks the loop,
/// returns from the frame, then lands in the `catch` dispatch.
#[derive(Clone, Copy, PartialEq)]
enum UnwindKind {
    /// Into the enclosing `try`'s `catch` dispatch (or, for a handler body, past
    /// the handlers into its `finally`).
    Try,
    /// Out of the enclosing loop; the check after the loop statement continues
    /// the walk outward.
    Loop,
    /// Out of the enclosing `def`/method/closure frame, returning `Unit`.
    Def,
}

/// One entry of [`Compiler::unwind`]: the target kind plus the forward jumps
/// awaiting patching to it. `Def` needs no jump list (it returns inline), but
/// carrying one uniformly keeps the push/pop protocol simple.
struct UnwindFrame {
    kind: UnwindKind,
    jumps: Vec<usize>,
}

/// A lambda body queued for emission as a subroutine region. `captures` are the
/// enclosing-frame locals it reads as upvalues (bound to slots after the
/// parameters); the enclosing class/object context is carried so a lambda that
/// reads a field or calls a sibling method lowers it the same way the method body
/// would.
struct PendingClosure {
    name_idx: u16,
    params: Vec<String>,
    captures: Vec<String>,
    body: Expr,
    current_class: Option<(String, HashSet<String>)>,
    current_object: Option<String>,
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
        pending_closures: VecDeque::new(),
        closures_seen: 0,
        // Scan once up front: the unwind checks cost two ops per statement, so
        // they are emitted only for programs that can actually catch.
        has_try: body_has_try(&prog.main)
            || prog.functions.iter().any(|f| body_has_try(&f.body))
            || classes
                .iter()
                .any(|c| body_has_try(&c.body) || c.methods.iter().any(|m| body_has_try(&m.body)))
            || objects
                .iter()
                .any(|o| body_has_try(&o.body) || o.methods.iter().any(|m| body_has_try(&m.body))),
        unwind: Vec::new(),
        try_counter: 0,
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
    // methods, and lambda bodies) lives after `main`, jumped over on the
    // fall-through; each is reached only through its `Op::Call`/`find_sub` entry.
    // Lambdas discovered while lowering `main` mean the subs region is needed even
    // with no `def`/`class`.
    let has_subs = !prog.functions.is_empty()
        || !classes.is_empty()
        || objects.iter().any(|o| !o.methods.is_empty())
        || !c.pending_closures.is_empty();
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
        // Drain lambda bodies last; emitting one may enqueue further nested ones.
        while let Some(pc) = c.pending_closures.pop_front() {
            c.emit_closure(pc)?;
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
    /// Compile one statement, then — in a program that contains a `try` — the
    /// unwind check that carries an in-flight exception outward.
    ///
    /// The check lives at the statement boundary, which is the only point where
    /// the operand stack is guaranteed balanced, so jumping away from it cannot
    /// strand a partial value.
    fn stmt(&mut self, s: &Stmt) -> Result<(), String> {
        self.stmt_inner(s)?;
        self.unwind_check();
        Ok(())
    }

    /// Emit the post-statement `EXC_PENDING` test and the jump the innermost
    /// enclosing construct wants for it. A no-op unless the program has a `try`.
    fn unwind_check(&mut self) {
        self.unwind_check_dropping(0);
    }

    /// [`unwind_check`](Self::unwind_check) at a point where `drop` values are
    /// sitting on the operand stack: they are popped on the unwind path so the
    /// jump leaves the stack balanced.
    ///
    /// The `drop == 1` form guards a *binding store*. Without it, a raise while
    /// computing an initializer would still commit the resulting garbage to the
    /// `val`/`var` before control reached the handler, and a handler that reads
    /// that binding would see `null` instead of its previous value — visible in
    /// `try { acc += 10 / 0 } catch { case _ => acc += 100 }`.
    fn unwind_check_dropping(&mut self, drop: usize) {
        if !self.has_try {
            return;
        }
        self.b.emit(Op::CallBuiltin(crate::host::EXC_PENDING, 0), 0);
        let j_ok = self.b.emit(Op::JumpIfFalse(0), 0);
        for _ in 0..drop {
            self.b.emit(Op::Pop, 0);
        }
        match self.unwind.last().map(|f| f.kind) {
            // A `try` body or a loop body: jump forward to the construct's
            // exception exit, patched by whoever pushed the frame.
            Some(UnwindKind::Try) | Some(UnwindKind::Loop) => {
                let j = self.b.emit(Op::Jump(0), 0);
                self.unwind
                    .last_mut()
                    .expect("just matched a frame")
                    .jumps
                    .push(j);
            }
            // A `def`/method/closure body: return `Unit` immediately; the
            // caller's own check resumes the walk in its frame. `ReturnValue`
            // (not the bare `Return` the Unit tail paths use) so the call site
            // still receives exactly one value whatever position it is in.
            Some(UnwindKind::Def) => {
                self.b.emit(Op::LoadUndef, 0);
                self.b.emit(Op::ReturnValue, 0);
            }
            // Top level: nothing left to unwind into, so the exception is
            // uncaught and stops the run.
            None => {
                self.b.emit(Op::CallBuiltin(crate::host::EXC_ABORT, 0), 0);
                self.b.emit(Op::Pop, 0);
            }
        }
        let at = self.b.current_pos();
        self.b.patch_jump(j_ok, at);
    }

    /// Push an unwind frame for a construct that catches the walk (`try` body,
    /// loop body, function body).
    fn push_unwind(&mut self, kind: UnwindKind) {
        self.unwind.push(UnwindFrame {
            kind,
            jumps: Vec::new(),
        });
    }

    /// Pop the innermost unwind frame and patch its collected jumps to `target`.
    fn pop_unwind_to(&mut self, target: usize) {
        if let Some(f) = self.unwind.pop() {
            for j in f.jumps {
                self.b.patch_jump(j, target);
            }
        }
    }

    fn stmt_inner(&mut self, s: &Stmt) -> Result<(), String> {
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
                    self.unwind_check_dropping(1);
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
                self.unwind_check_dropping(1);
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
        // A raise inside the body must leave the loop instead of spinning on
        // garbage; the check after the whole `while` statement continues outward.
        self.push_unwind(UnwindKind::Loop);
        for s in body {
            self.stmt(s)?;
        }
        self.b.emit(Op::Jump(top), 0);
        let end = self.b.current_pos();
        self.pop_unwind_to(end);
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
            // Collection generators are desugared to `.map`/`.flatMap` before
            // `lower_for` (the counted-loop path handles integer ranges only).
            ForEnum::GenColl { .. } => {
                unreachable!("collection generators are desugared before lower_for")
            }
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
                step,
            } => {
                self.expr(start)?;
                let vplace = self.declare_place(name);
                self.emit_store(vplace);
                self.for_counter += 1;
                let bound = format!(" for_end_{}", self.for_counter);
                self.expr(end)?;
                let bplace = self.declare_place(&bound);
                self.emit_store(bplace);
                // The step is snapshotted alongside the bound: Scala evaluates
                // the whole `a until b by s` range object once, before iterating.
                // `None` (no `by`) keeps the historical `LoadInt(1)` increment so
                // step-less loops emit byte-identical code to before.
                let splace = match step {
                    Some(s) => {
                        self.for_counter += 1;
                        let sname = format!(" for_step_{}", self.for_counter);
                        self.expr(s)?;
                        let p = self.declare_place(&sname);
                        self.emit_store(p);
                        Some(p)
                    }
                    None => None,
                };
                // A zero step has no direction and would spin forever, so guard
                // it exactly as Scala's `Range` does. Skipped when the step is a
                // literal known non-zero (the usual case), so `by 2` costs
                // nothing.
                let j_zero = match splace {
                    Some(sp) if const_step(step.as_ref()).is_none() => {
                        self.emit_load(sp);
                        self.b.emit(Op::LoadInt(0), 0);
                        self.b.emit(Op::NumEq, 0);
                        let j_ok = self.b.emit(Op::JumpIfFalse(0), 0);
                        self.emit_throwable(
                            "java.lang.IllegalArgumentException",
                            "step cannot be 0.",
                        );
                        let j = self.b.emit(Op::Jump(0), 0);
                        let ok = self.b.current_pos();
                        self.b.patch_jump(j_ok, ok);
                        Some(j)
                    }
                    _ => None,
                };
                let top = self.b.current_pos();
                match (splace, const_step(step.as_ref())) {
                    // No `by`, or a literal `by` whose sign is known at compile
                    // time: one static bound test, exactly as before.
                    (None, _) | (Some(_), Some(_)) => {
                        let descending = const_step(step.as_ref()).is_some_and(|n| n < 0);
                        self.emit_load(vplace);
                        self.emit_load(bplace);
                        self.b.emit(range_test(*inclusive, descending), 0);
                    }
                    // A non-literal `by`: the direction is only known at runtime,
                    // so branch on `step > 0` and pick the matching bound test.
                    (Some(sp), None) => {
                        self.emit_load(sp);
                        self.b.emit(Op::LoadInt(0), 0);
                        self.b.emit(Op::NumGt, 0);
                        let j_desc = self.b.emit(Op::JumpIfFalse(0), 0);
                        self.emit_load(vplace);
                        self.emit_load(bplace);
                        self.b.emit(range_test(*inclusive, false), 0);
                        let j_done = self.b.emit(Op::Jump(0), 0);
                        let desc_at = self.b.current_pos();
                        self.b.patch_jump(j_desc, desc_at);
                        self.emit_load(vplace);
                        self.emit_load(bplace);
                        self.b.emit(range_test(*inclusive, true), 0);
                        let done_at = self.b.current_pos();
                        self.b.patch_jump(j_done, done_at);
                    }
                }
                let jf = self.b.emit(Op::JumpIfFalse(0), 0);
                // As in `while_stmt`: a raise in the body exits the loop rather
                // than iterating on garbage.
                self.push_unwind(UnwindKind::Loop);
                self.lower_for(enums, idx + 1, body, yield_into)?;
                // The innermost `lower_for` ends in an *expression* (the body
                // value), not a statement, so nothing above emitted a check for
                // it; without this one a raise in a single-expression body would
                // spin the loop forever.
                self.unwind_check();
                self.emit_load(vplace);
                match splace {
                    Some(sp) => self.emit_load(sp),
                    None => {
                        self.b.emit(Op::LoadInt(1), 0);
                    }
                }
                self.b.emit(Op::Add, 0);
                self.emit_store(vplace);
                self.b.emit(Op::Jump(top), 0);
                let end_pos = self.b.current_pos();
                self.pop_unwind_to(end_pos);
                self.b.patch_jump(jf, end_pos);
                if let Some(j) = j_zero {
                    self.b.patch_jump(j, end_pos);
                }
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
            Expr::Try {
                body,
                catches,
                finalizer,
            } => self.try_expr(body, catches, finalizer.as_deref())?,
            Expr::Throw { value, line } => {
                self.expr(value)?;
                self.b
                    .emit(Op::CallBuiltin(crate::host::EXC_THROW, 1), *line);
            }
            Expr::Format { value, spec, line } => {
                self.expr(value)?;
                let c = self.b.add_constant(Value::str(spec.clone()));
                self.b.emit(Op::LoadConst(c), *line);
                self.b.emit(Op::CallBuiltin(crate::host::SFORMAT, 2), *line);
            }
            Expr::ForYield { enums, body } => {
                // A collection generator (`x <- List(…)`) desugars to a
                // `.map`/`.flatMap`/`.withFilter` chain; a pure integer-range
                // comprehension keeps the counted-loop lowering (JIT-friendly).
                if enums.iter().any(is_coll_gen) {
                    let d = desugar_for(enums, body, true);
                    self.expr(&d)?;
                } else {
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
            }
            Expr::ForEach { enums, body } => {
                if enums.iter().any(is_coll_gen) {
                    let d = desugar_for(enums, body, false);
                    self.expr(&d)?;
                } else {
                    self.lower_for(enums, 0, body, None)?;
                    // `foreach` yields `Unit`.
                    self.b.emit(Op::LoadUndef, 0);
                }
            }
            Expr::Lambda { params, body } => self.lambda(params, body)?,
            Expr::Placeholder => {
                return Err("scalars: `_` placeholder outside an argument".to_string())
            }
            Expr::Tuple(elems) => {
                for el in elems {
                    self.expr(el)?;
                }
                self.b.emit(
                    Op::CallBuiltin(crate::host::MAKE_TUPLE, elems.len() as u8),
                    0,
                );
            }
            Expr::Collection { ctor, elems } => self.collection(ctor, elems)?,
        }
        Ok(())
    }

    /// Lower a `List(...)` / `Map(...)` literal to its host constructor builtin.
    fn collection(&mut self, ctor: &str, elems: &[Expr]) -> Result<(), String> {
        for el in elems {
            self.expr(el)?;
        }
        let id = match ctor {
            "List" => crate::host::MAKE_LIST,
            "Map" => crate::host::MAKE_MAP,
            _ => return Err(format!("scalars: unknown collection constructor `{ctor}`")),
        };
        self.b.emit(Op::CallBuiltin(id, elems.len() as u8), 0);
        Ok(())
    }

    /// Lower a lambda literal: queue its body for emission as a subroutine region
    /// and, at the literal site, build the runtime closure handle. Free names that
    /// resolve to an enclosing frame slot are captured by value (upvalues); free
    /// names that are globals/`def`s stay unbound and resolve at call time.
    fn lambda(&mut self, params: &[String], body: &Expr) -> Result<(), String> {
        // Upvalues: free names bound to an enclosing frame slot. At top level
        // (`scope` is `None`) a lambda captures nothing — its free names are the
        // program-global bindings, read live when the closure runs.
        let mut captures: Vec<String> = match self.scope.as_ref() {
            Some(scope) => free_vars(params, body)
                .into_iter()
                .filter(|n| scope.slots.contains_key(n))
                .collect(),
            None => Vec::new(),
        };
        // A lambda inside a class method that reads a field or calls a sibling
        // method needs the enclosing `this` (slot 0) as an upvalue.
        if let Some((cname, fields)) = self.current_class.clone() {
            let uses_this = free_vars(params, body)
                .iter()
                .any(|n| fields.contains(n) || self.class_defines_method(&cname, n));
            if uses_this
                && self
                    .scope
                    .as_ref()
                    .is_some_and(|s| s.slots.contains_key("this"))
                && !captures.iter().any(|c| c == "this")
            {
                captures.push("this".to_string());
            }
        }

        // Push name index, param count, then each captured value (read from the
        // enclosing frame) so MAKE_CLOSURE stores them in the handle.
        let id = self.closures_seen;
        self.closures_seen += 1;
        let name_idx = self.b.add_name(&format!("$closure_{id}"));
        self.b.emit(Op::LoadInt(name_idx as i64), 0);
        self.b.emit(Op::LoadInt(params.len() as i64), 0);
        for cap in &captures {
            let place = self.resolve_place(cap);
            self.emit_load(place);
        }
        self.b.emit(
            Op::CallBuiltin(crate::host::MAKE_CLOSURE, captures.len() as u8 + 2),
            0,
        );
        self.pending_closures.push_back(PendingClosure {
            name_idx,
            params: params.to_vec(),
            captures,
            body: body.clone(),
            current_class: self.current_class.clone(),
            current_object: self.current_object.clone(),
        });
        Ok(())
    }

    /// Emit a queued lambda body as a subroutine region: bind parameters then
    /// captured upvalues into frame slots, lower the body as the (single-value)
    /// result, and end with a `ReturnValue`.
    fn emit_closure(&mut self, pc: PendingClosure) -> Result<(), String> {
        let ip = self.b.current_pos();
        self.b.add_sub_entry(pc.name_idx, ip);

        let mut slots = HashMap::new();
        for (i, p) in pc.params.iter().enumerate() {
            slots.insert(p.clone(), i as u16);
        }
        for (j, cap) in pc.captures.iter().enumerate() {
            slots.insert(cap.clone(), (pc.params.len() + j) as u16);
        }
        let total = pc.params.len() + pc.captures.len();
        let saved_scope = self.scope.replace(Scope {
            slots,
            next_slot: total as u16,
        });
        let saved_vals = std::mem::take(&mut self.vals);
        for p in &pc.params {
            self.vals.insert(p.clone(), true);
        }
        let saved_class = std::mem::replace(&mut self.current_class, pc.current_class);
        let saved_object = std::mem::replace(&mut self.current_object, pc.current_object);

        // Prologue: pop the pushed params + captures (top-down) into their slots.
        for i in (0..total).rev() {
            self.b.emit(Op::SetSlot(i as u16), 0);
        }
        // A closure body is its own unwind boundary (see `unwind_check`).
        self.push_unwind(UnwindKind::Def);
        self.expr(&pc.body)?;
        self.pop_unwind_to(self.b.current_pos());
        self.b.emit(Op::ReturnValue, 0);

        self.scope = saved_scope;
        self.vals = saved_vals;
        self.current_class = saved_class;
        self.current_object = saved_object;
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

    /// Lower `try body [catch { case … }] [finally fin]` as a value.
    ///
    /// The shape, in order:
    ///
    /// ```text
    ///   EXC_ENTER                 ; raises now park instead of halting
    ///   <body>            -> res  ; unwind checks inside jump to `dispatch`
    ///   EXC_EXIT                  ; normal exit
    ///   Jump fin
    /// dispatch:                   ; exceptional exit — `res` is Unit
    ///   EXC_EXIT                  ; a raise in a handler is *not* caught here
    ///   <catch arms>      -> res  ; each arm: EXC_MATCH, then EXC_TAKE to bind
    /// fin:
    ///   EXC_STASH                 ; park any still-in-flight exception …
    ///   <finalizer>               ; … so the finalizer runs to completion …
    ///   EXC_UNSTASH               ; … then resume unwinding it
    ///   load res
    /// ```
    ///
    /// The result travels in a synthetic local rather than on the operand stack
    /// because the exceptional path enters at `dispatch` from an arbitrary
    /// statement boundary inside the body, where nothing has been pushed.
    ///
    /// The handler arms run under their own `Try` frame targeting `fin`, so an
    /// exception thrown *by* a handler still runs the `finally` before
    /// propagating — the JVM's ordering.
    fn try_expr(
        &mut self,
        body: &[Stmt],
        catches: &[MatchArm],
        finalizer: Option<&[Stmt]>,
    ) -> Result<(), String> {
        self.try_counter += 1;
        let res = self.declare_place(&format!(" try_{}", self.try_counter));

        self.b.emit(Op::CallBuiltin(crate::host::EXC_ENTER, 0), 0);
        self.b.emit(Op::Pop, 0);

        self.push_unwind(UnwindKind::Try);
        self.block_expr(body)?;
        self.emit_store(res);
        // The body's own trailing expression has no statement after it, so emit
        // the check here — a raise in the last expression must still dispatch.
        self.unwind_check();
        self.b.emit(Op::CallBuiltin(crate::host::EXC_EXIT, 0), 0);
        self.b.emit(Op::Pop, 0);
        let j_normal = self.b.emit(Op::Jump(0), 0);

        let dispatch = self.b.current_pos();
        self.pop_unwind_to(dispatch);
        self.b.emit(Op::LoadUndef, 0);
        self.emit_store(res);
        self.b.emit(Op::CallBuiltin(crate::host::EXC_EXIT, 0), 0);
        self.b.emit(Op::Pop, 0);

        self.push_unwind(UnwindKind::Try);
        let mut handled_jumps = Vec::new();
        for arm in catches {
            let ty = catch_type_name(&arm.pat)?;
            let c = self.b.add_constant(Value::str(ty.to_string()));
            self.b.emit(Op::LoadConst(c), 0);
            self.b.emit(Op::CallBuiltin(crate::host::EXC_MATCH, 1), 0);
            let j_type = self.b.emit(Op::JumpIfFalse(0), 0);
            // The type matched. Consume the exception *before* the guard runs:
            // while one is in flight every side-effecting builtin is suppressed,
            // so a guard like `if e.getMessage.length > 1` would read `null`
            // instead of dispatching. Keep a copy in a temporary so a guard that
            // rejects the arm can put it back.
            self.obj_counter += 1;
            let held = self.declare_place(&format!(" exc_{}", self.obj_counter));
            self.b.emit(Op::CallBuiltin(crate::host::EXC_TAKE, 0), 0);
            self.emit_store(held);
            if let Some(name) = catch_binding(&arm.pat) {
                let p = self.declare_place(name);
                self.emit_load(held);
                self.emit_store(p);
            }
            let j_guard = match &arm.guard {
                Some(g) => {
                    self.expr(g)?;
                    Some(self.b.emit(Op::JumpIfFalse(0), 0))
                }
                None => None,
            };
            self.block_expr(&arm.body)?;
            self.emit_store(res);
            self.unwind_check();
            handled_jumps.push(self.b.emit(Op::Jump(0), 0));
            // Guard rejected the arm: re-arm the exception and fall through to
            // the next one, which must still see it.
            if let Some(jg) = j_guard {
                let at = self.b.current_pos();
                self.b.patch_jump(jg, at);
                self.emit_load(held);
                self.b.emit(Op::CallBuiltin(crate::host::EXC_RESTORE, 1), 0);
                self.b.emit(Op::Pop, 0);
            }
            let next = self.b.current_pos();
            self.b.patch_jump(j_type, next);
        }
        // Falling off the last arm leaves the exception in flight: it is
        // unhandled here and keeps unwinding after the `finally` runs.

        let fin = self.b.current_pos();
        self.pop_unwind_to(fin);
        for j in handled_jumps {
            self.b.patch_jump(j, fin);
        }
        self.b.patch_jump(j_normal, fin);

        if let Some(f) = finalizer {
            self.b.emit(Op::CallBuiltin(crate::host::EXC_STASH, 0), 0);
            self.b.emit(Op::Pop, 0);
            // A raise inside the finalizer jumps straight to `EXC_UNSTASH`,
            // which keeps the *new* exception and discards the parked one —
            // exactly the JVM rule, and it stops the stash from leaking.
            self.push_unwind(UnwindKind::Try);
            for s in f {
                self.stmt(s)?;
            }
            let unstash = self.b.current_pos();
            self.pop_unwind_to(unstash);
            self.b.emit(Op::CallBuiltin(crate::host::EXC_UNSTASH, 0), 0);
            self.b.emit(Op::Pop, 0);
        }
        self.emit_load(res);
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
        // `Nil` — the empty `List`.
        if name == "Nil" {
            self.b.emit(Op::CallBuiltin(crate::host::MAKE_LIST, 0), 0);
            return Ok(());
        }
        // A bare reference to a singleton object (e.g. `None`) materializes it.
        if self.objects.contains_key(name) {
            return self.materialize_object(name);
        }
        // A bare reference to a `def`. A zero-parameter `def` is a paren-less
        // call; a `def` with parameters used as a value is eta-expanded to a
        // closure (`fib` ⇒ `x => fib(x)`), so `List(…).map(fib)` works.
        if let Some(&arity) = self.func_arity.get(name) {
            if arity == 0 {
                let nidx = self.b.add_name(name);
                self.b.emit(Op::Call(nidx, 0), 0);
                return Ok(());
            }
            let params: Vec<String> = (0..arity).map(|i| format!("$eta{i}")).collect();
            let call = Expr::Call {
                name: name.to_string(),
                args: params.iter().map(|p| Expr::Var(p.clone())).collect(),
                line: 0,
            };
            return self.lambda(&params, &call);
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
        self.unwind_check_dropping(1);
        self.b.emit(Op::SetVar(g), 0);
        Ok(())
    }

    /// Lower `new Class(args)` / a `case class` companion `apply` — both invoke
    /// the class's `Class$new` constructor subroutine, which builds the record.
    fn construct(&mut self, name: &str, args: &[Expr], line: u32) -> Result<(), String> {
        // `new RuntimeException("…")` and friends: built-in throwables have no
        // user `class` declaration, so they construct through the host rather
        // than a `Class$new` subroutine. A user class of the same name wins —
        // shadowing a JDK name is legal Scala.
        if !self.classes.contains_key(name) {
            if let Some(fqn) = crate::host::throwable_fqn(name) {
                return self.construct_throwable(name, fqn, args, line);
            }
        }
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

    /// Lower `new <BuiltinThrowable>([message])` to the [`EXC_NEW`] builtin.
    /// The JVM's `Throwable` constructors this models are the no-arg one (whose
    /// `getMessage` is `null`) and the single-`String` one.
    ///
    /// [`EXC_NEW`]: crate::host::EXC_NEW
    fn construct_throwable(
        &mut self,
        name: &str,
        fqn: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<(), String> {
        if args.len() > 1 {
            return Err(format!(
                "scalars: {name} takes 0 or 1 constructor argument(s), found {} (line {line})",
                args.len()
            ));
        }
        let c = self.b.add_constant(Value::str(fqn.to_string()));
        self.b.emit(Op::LoadConst(c), line);
        match args.first() {
            Some(a) => self.expr(a)?,
            None => {
                self.b.emit(Op::LoadUndef, line);
            }
        }
        self.b.emit(Op::CallBuiltin(crate::host::EXC_NEW, 2), line);
        Ok(())
    }

    /// Emit "construct a built-in throwable and raise it", leaving nothing on
    /// the stack. Used for runtime checks the compiler plants itself (the range
    /// step-zero guard) rather than for a user's `throw`.
    fn emit_throwable(&mut self, fqn: &str, msg: &str) {
        let c = self.b.add_constant(Value::str(fqn.to_string()));
        self.b.emit(Op::LoadConst(c), 0);
        let m = self.b.add_constant(Value::str(msg.to_string()));
        self.b.emit(Op::LoadConst(m), 0);
        self.b.emit(Op::CallBuiltin(crate::host::EXC_NEW, 2), 0);
        self.b.emit(Op::CallBuiltin(crate::host::EXC_THROW, 1), 0);
        self.b.emit(Op::Pop, 0);
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
        self.push_unwind(UnwindKind::Def);
        self.tail(&m.body)?;
        self.pop_unwind_to(self.b.current_pos());
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
        self.push_unwind(UnwindKind::Def);
        self.tail(&m.body)?;
        self.pop_unwind_to(self.b.current_pos());
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
        // A synthetic range-materialization call emitted by `desugar_for` for a
        // range generator appearing in a collection comprehension.
        if name == RANGE_LIST_CALL {
            for a in args {
                self.expr(a)?;
            }
            self.b
                .emit(Op::CallBuiltin(crate::host::RANGE_LIST, 3), line);
            return Ok(());
        }
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
        // A call on a bound name that is not a `def` is an `apply`: the value is a
        // function (`f(x)`), a `List`/`Tuple` (`xs(i)` indexing), or a `Map`
        // (`m(k)` lookup). Load the value, push the args, and dispatch via APPLY.
        let is_bound = self.vals.contains_key(name)
            || self
                .scope
                .as_ref()
                .is_some_and(|s| s.slots.contains_key(name));
        if is_bound {
            let place = self.resolve_place(name);
            self.emit_load(place);
            for a in args {
                self.expr(a)?;
            }
            self.b
                .emit(Op::CallBuiltin(crate::host::APPLY, args.len() as u8), line);
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

        self.push_unwind(UnwindKind::Def);
        self.tail(&f.body)?;
        self.pop_unwind_to(self.b.current_pos());
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
        // `::` (cons) prepends the left operand to the right `List` via the host
        // constructor builtin.
        if let BinOp::Cons = op {
            self.b.emit(Op::CallBuiltin(crate::host::LIST_CONS, 2), 0);
            return Ok(());
        }
        let vop = match op {
            BinOp::Add => Op::Add,
            BinOp::Sub => Op::Sub,
            BinOp::Mul => Op::Mul,
            BinOp::Div => unreachable!("division routed through the SDIV builtin above"),
            BinOp::Cons => unreachable!("cons routed through the LIST_CONS builtin above"),
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
        Expr::Try {
            body,
            catches,
            finalizer,
        } => {
            body_has_ffi(body)
                || catches.iter().any(|a| body_has_ffi(&a.body))
                || finalizer.as_deref().is_some_and(body_has_ffi)
        }
        Expr::Throw { value, .. } => expr_has_ffi(value),
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
        Expr::Lambda { body, .. } => expr_has_ffi(body),
        Expr::Tuple(elems) | Expr::Collection { elems, .. } => elems.iter().any(expr_has_ffi),
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Placeholder
        | Expr::Var(_) => false,
    }
}

/// True if a `for` enumerator (generator bounds / step / guard) evaluates an
/// FFI call.
fn enum_has_ffi(e: &ForEnum) -> bool {
    match e {
        ForEnum::Gen {
            start, end, step, ..
        } => expr_has_ffi(start) || expr_has_ffi(end) || step.as_ref().is_some_and(expr_has_ffi),
        ForEnum::GenColl { coll, .. } => expr_has_ffi(coll),
        ForEnum::Guard(c) => expr_has_ffi(c),
    }
}

/// The caught type of a `catch` arm's pattern. A bare `case e =>` / `case _ =>`
/// catches everything, which is `Throwable` — Scala infers exactly that.
fn catch_type_name(p: &Pattern) -> Result<&str, String> {
    match p {
        Pattern::Typed { ty, .. } => Ok(ty),
        Pattern::Bind(_) | Pattern::Wildcard => Ok("Throwable"),
        _ => Err(
            "scalars: only `case e: Type`, `case _: Type`, `case e` and `case _` are supported in `catch`"
                .to_string(),
        ),
    }
}

/// The name a `catch` arm binds the caught exception to, if any.
fn catch_binding(p: &Pattern) -> Option<&str> {
    match p {
        Pattern::Typed { name, .. } if name != "_" => Some(name),
        Pattern::Bind(name) => Some(name),
        _ => None,
    }
}

/// True if a statement list contains a `try` anywhere (including inside nested
/// expressions), which is what arms the per-statement unwind checks.
fn body_has_try(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match &s.kind {
        StmtKind::Local { init, .. } => init.as_ref().is_some_and(expr_has_try),
        StmtKind::Assign { value, .. } => expr_has_try(value),
        StmtKind::Expr(e) => expr_has_try(e),
        StmtKind::If { cond, then, els } => {
            expr_has_try(cond) || body_has_try(then) || body_has_try(els)
        }
        StmtKind::While { cond, body } => expr_has_try(cond) || body_has_try(body),
        StmtKind::Return(e) => e.as_ref().is_some_and(expr_has_try),
    })
}

/// True if an expression tree contains a `try`.
fn expr_has_try(e: &Expr) -> bool {
    match e {
        Expr::Try { .. } => true,
        Expr::Throw { value, .. } => expr_has_try(value),
        Expr::Unary { rhs, .. } => expr_has_try(rhs),
        Expr::Binary { lhs, rhs, .. } => expr_has_try(lhs) || expr_has_try(rhs),
        Expr::Println { arg, .. } => arg.as_deref().is_some_and(expr_has_try),
        Expr::Call { args, .. } | Expr::New { args, .. } => args.iter().any(expr_has_try),
        Expr::Method { recv, args, .. } => expr_has_try(recv) || args.iter().any(expr_has_try),
        Expr::Copy { recv, updates, .. } => {
            expr_has_try(recv) || updates.iter().any(|(_, v)| expr_has_try(v))
        }
        Expr::If { cond, then, els } => {
            expr_has_try(cond) || expr_has_try(then) || els.as_deref().is_some_and(expr_has_try)
        }
        Expr::Block(stmts) => body_has_try(stmts),
        Expr::Match { scrut, arms } => {
            expr_has_try(scrut)
                || arms
                    .iter()
                    .any(|a| a.guard.as_ref().is_some_and(expr_has_try) || body_has_try(&a.body))
        }
        Expr::Format { value, .. } => expr_has_try(value),
        Expr::ForYield { enums, body } | Expr::ForEach { enums, body } => {
            enums.iter().any(enum_has_try) || expr_has_try(body)
        }
        Expr::Lambda { body, .. } => expr_has_try(body),
        Expr::Tuple(elems) | Expr::Collection { elems, .. } => elems.iter().any(expr_has_try),
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Placeholder
        | Expr::Var(_) => false,
    }
}

/// True if a `for` enumerator's bounds / step / guard contain a `try`.
fn enum_has_try(e: &ForEnum) -> bool {
    match e {
        ForEnum::Gen {
            start, end, step, ..
        } => expr_has_try(start) || expr_has_try(end) || step.as_ref().is_some_and(expr_has_try),
        ForEnum::GenColl { coll, .. } => expr_has_try(coll),
        ForEnum::Guard(c) => expr_has_try(c),
    }
}

/// The compile-time value of a `by` step, when it is a (possibly negated)
/// integer literal. A known-sign step lets [`Compiler::lower_for`] emit a single
/// static bound test instead of branching on the sign every iteration — the
/// overwhelmingly common case (`by 2`, `by -1`). A literal `0` deliberately
/// returns `None`: it has no valid direction and is rejected at runtime by the
/// emitted step-zero guard, matching Scala's
/// `IllegalArgumentException: step cannot be 0.`
fn const_step(step: Option<&Expr>) -> Option<i64> {
    match step? {
        Expr::Int(n) if *n != 0 => Some(*n),
        Expr::Unary { op: UnOp::Neg, rhs } => match &**rhs {
            Expr::Int(n) if *n != 0 => Some(-*n),
            _ => None,
        },
        _ => None,
    }
}

/// The bound test of a range loop: `i < end` / `i <= end` ascending, `i > end` /
/// `i >= end` descending. Scala's `Range` flips the comparison with the step's
/// sign, so `10 to 1 by -3` counts down instead of yielding nothing.
fn range_test(inclusive: bool, descending: bool) -> Op {
    match (inclusive, descending) {
        (false, false) => Op::NumLt,
        (true, false) => Op::NumLe,
        (false, true) => Op::NumGt,
        (true, true) => Op::NumGe,
    }
}

// ── for-comprehension desugaring (collection generators) ────────────────────

/// Whether an enumerator is a collection generator (`x <- List(…)`).
fn is_coll_gen(e: &ForEnum) -> bool {
    matches!(e, ForEnum::GenColl { .. })
}

/// Desugar a `for` comprehension containing a collection generator into a
/// `.map`/`.flatMap`/`.withFilter`/`.foreach` chain (Scala's translation):
///
/// * `for (x <- e) yield b`            → `e.map(x => b)`
/// * `for (x <- e) b`      (foreach)   → `e.foreach(x => b)`
/// * `for (x <- e if g; …) …`          → `e.withFilter(x => g)` then continue
/// * `for (x <- e; rest…) yield b`     → `e.flatMap(x => <for rest yield b>)`
///
/// A range generator (`i <- a to b`) is materialized to a `List` first.
fn desugar_for(enums: &[ForEnum], body: &Expr, is_yield: bool) -> Expr {
    let (name, mut src) = gen_source(&enums[0]);
    // Guards immediately after the generator become `withFilter` on its source.
    let mut i = 1;
    while let Some(ForEnum::Guard(g)) = enums.get(i) {
        src = method(src, "withFilter", vec![lambda1(&name, g.clone())]);
        i += 1;
    }
    let rest = &enums[i..];
    if rest.is_empty() {
        let m = if is_yield { "map" } else { "foreach" };
        method(src, m, vec![lambda1(&name, body.clone())])
    } else {
        let inner = desugar_for(rest, body, is_yield);
        // Nested yield collects via `flatMap`; nested foreach nests `foreach`.
        let m = if is_yield { "flatMap" } else { "foreach" };
        method(src, m, vec![lambda1(&name, inner)])
    }
}

/// The `(bound name, source-collection expr)` of a generator enumerator. A range
/// generator is wrapped in the `$range_list` materialization call.
fn gen_source(e: &ForEnum) -> (String, Expr) {
    match e {
        ForEnum::GenColl { name, coll } => (name.clone(), coll.clone()),
        ForEnum::Gen {
            name,
            start,
            end,
            inclusive,
            step,
        } => (
            name.clone(),
            Expr::Call {
                name: RANGE_LIST_CALL.to_string(),
                // An absent `by` materializes with the implicit step of 1, so
                // the host builtin has a single 4-argument shape.
                args: vec![
                    start.clone(),
                    end.clone(),
                    Expr::Bool(*inclusive),
                    step.clone().unwrap_or(Expr::Int(1)),
                ],
                line: 0,
            },
        ),
        ForEnum::Guard(_) => unreachable!("a comprehension does not begin with a guard"),
    }
}

/// Build a single-parameter lambda `name => body`.
fn lambda1(name: &str, body: Expr) -> Expr {
    Expr::Lambda {
        params: vec![name.to_string()],
        body: Box::new(body),
    }
}

/// Build a method call `recv.name(args)`.
fn method(recv: Expr, name: &str, args: Vec<Expr>) -> Expr {
    Expr::Method {
        recv: Box::new(recv),
        name: name.to_string(),
        args,
        line: 0,
    }
}

// ── free-variable analysis (lambda upvalue capture) ─────────────────────────

/// The names referenced free in a lambda `body` given its `params` — the
/// candidates for upvalue capture (the compiler keeps only those bound to an
/// enclosing frame slot). Over-reporting is harmless: a reported name that is not
/// an enclosing slot is filtered out at the capture site.
fn free_vars(params: &[String], body: &Expr) -> Vec<String> {
    let bound: HashSet<String> = params.iter().cloned().collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fv_expr(body, &bound, &mut out, &mut seen);
    out
}

/// Record `name` as free if it is neither bound in this scope nor already seen.
fn fv_note(name: &str, bound: &HashSet<String>, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    if !bound.contains(name) && seen.insert(name.to_string()) {
        out.push(name.to_string());
    }
}

/// Free-variable scan of a statement block (a fresh nested scope: `val`/`var`
/// declarations bind for the remainder of the block).
fn fv_block(
    stmts: &[Stmt],
    bound: &HashSet<String>,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let mut b = bound.clone();
    for s in stmts {
        match &s.kind {
            StmtKind::Local { name, init, .. } => {
                if let Some(e) = init {
                    fv_expr(e, &b, out, seen);
                }
                b.insert(name.clone());
            }
            StmtKind::Assign { name, value, .. } => {
                fv_note(name, &b, out, seen);
                fv_expr(value, &b, out, seen);
            }
            StmtKind::Expr(e) => fv_expr(e, &b, out, seen),
            StmtKind::If { cond, then, els } => {
                fv_expr(cond, &b, out, seen);
                fv_block(then, &b, out, seen);
                fv_block(els, &b, out, seen);
            }
            StmtKind::While { cond, body } => {
                fv_expr(cond, &b, out, seen);
                fv_block(body, &b, out, seen);
            }
            StmtKind::Return(opt) => {
                if let Some(e) = opt {
                    fv_expr(e, &b, out, seen);
                }
            }
        }
    }
}

/// Free-variable scan of an expression. Nested lambdas / match arms /
/// comprehension generators introduce their own bound names.
fn fv_expr(e: &Expr, bound: &HashSet<String>, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    match e {
        Expr::Var(name) => fv_note(name, bound, out, seen),
        Expr::Try {
            body,
            catches,
            finalizer,
        } => {
            fv_block(body, bound, out, seen);
            for a in catches {
                // The caught exception's binding is local to its arm.
                let mut b = bound.clone();
                pattern_binds(&a.pat, &mut b);
                if let Some(g) = &a.guard {
                    fv_expr(g, &b, out, seen);
                }
                fv_block(&a.body, &b, out, seen);
            }
            if let Some(f) = finalizer {
                fv_block(f, bound, out, seen);
            }
        }
        Expr::Throw { value, .. } => fv_expr(value, bound, out, seen),
        Expr::Unary { rhs, .. } => fv_expr(rhs, bound, out, seen),
        Expr::Binary { lhs, rhs, .. } => {
            fv_expr(lhs, bound, out, seen);
            fv_expr(rhs, bound, out, seen);
        }
        Expr::Println { arg, .. } => {
            if let Some(a) = arg {
                fv_expr(a, bound, out, seen);
            }
        }
        Expr::Call { name, args, .. } => {
            // The callee may be a captured function value (`f(x)` where `f` is an
            // enclosing binding). Noting a real `def`/builtin name too is harmless
            // — it is filtered out unless it is an enclosing frame slot.
            fv_note(name, bound, out, seen);
            for a in args {
                fv_expr(a, bound, out, seen);
            }
        }
        Expr::Method { recv, args, .. } => {
            fv_expr(recv, bound, out, seen);
            for a in args {
                fv_expr(a, bound, out, seen);
            }
        }
        Expr::New { args, .. } => {
            for a in args {
                fv_expr(a, bound, out, seen);
            }
        }
        Expr::Copy { recv, updates, .. } => {
            fv_expr(recv, bound, out, seen);
            for (_, val) in updates {
                fv_expr(val, bound, out, seen);
            }
        }
        Expr::If { cond, then, els } => {
            fv_expr(cond, bound, out, seen);
            fv_expr(then, bound, out, seen);
            if let Some(e) = els {
                fv_expr(e, bound, out, seen);
            }
        }
        Expr::Block(stmts) => fv_block(stmts, bound, out, seen),
        Expr::Match { scrut, arms } => {
            fv_expr(scrut, bound, out, seen);
            for arm in arms {
                let mut b = bound.clone();
                pattern_binds(&arm.pat, &mut b);
                if let Some(g) = &arm.guard {
                    fv_expr(g, &b, out, seen);
                }
                fv_block(&arm.body, &b, out, seen);
            }
        }
        Expr::Format { value, .. } => fv_expr(value, bound, out, seen),
        Expr::ForYield { enums, body } | Expr::ForEach { enums, body } => {
            let mut b = bound.clone();
            for en in enums {
                match en {
                    ForEnum::Gen {
                        name,
                        start,
                        end,
                        step,
                        ..
                    } => {
                        fv_expr(start, &b, out, seen);
                        fv_expr(end, &b, out, seen);
                        // The `by` step is evaluated in the *enclosing* scope,
                        // before the loop variable is bound.
                        if let Some(s) = step {
                            fv_expr(s, &b, out, seen);
                        }
                        b.insert(name.clone());
                    }
                    ForEnum::GenColl { name, coll } => {
                        fv_expr(coll, &b, out, seen);
                        b.insert(name.clone());
                    }
                    ForEnum::Guard(g) => fv_expr(g, &b, out, seen),
                }
            }
            fv_expr(body, &b, out, seen);
        }
        Expr::Lambda { params, body } => {
            let mut b = bound.clone();
            for p in params {
                b.insert(p.clone());
            }
            fv_expr(body, &b, out, seen);
        }
        Expr::Tuple(elems) | Expr::Collection { elems, .. } => {
            for el in elems {
                fv_expr(el, bound, out, seen);
            }
        }
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Placeholder => {}
    }
}

/// Add the names a pattern binds to `bound` (recursing into constructor
/// sub-patterns) so a lambda inside a `match` arm does not capture them.
fn pattern_binds(p: &Pattern, bound: &mut HashSet<String>) {
    match p {
        Pattern::Bind(n) => {
            bound.insert(n.clone());
        }
        Pattern::Typed { name, .. } if name != "_" => {
            bound.insert(name.clone());
        }
        Pattern::Constructor { elems, .. } => {
            for e in elems {
                pattern_binds(e, bound);
            }
        }
        _ => {}
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
