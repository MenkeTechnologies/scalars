//! Lower the Scala AST to a `fusevm::Chunk`.
//!
//! There is no bespoke VM or JVM here: statements and expressions emit fusevm
//! ops (`LoadInt`, `Add`, `GetVar`, `JumpIfFalse`, `CallBuiltin`, …) into a
//! `ChunkBuilder`, and fusevm runs the chunk on its three-tier Cranelift JIT.
//! Scala values ride the fusevm value model; the strict numeric hook in
//! `crate::host` supplies string `+` concatenation for the mixed operands the
//! VM's native arithmetic does not compute.
//!
//! Top-level bindings are addressed by name through `GetVar`/`SetVar`, while a
//! `def` body and a function literal each get a native `Op::Call` frame whose
//! parameters, captured upvalues and locals are frame slots, so this stays a
//! direct, readable lowering. Scala has no `break`/`continue`, so loops need no backpatch stack;
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
    /// Method name → `(runtime class tag, defining type)` pairs, for the runtime
    /// instance-method dispatch chain (`recv.m(...)`). The tag is the concrete
    /// class of the receiver; the defining type owns the `Owner$method`
    /// subroutine the call lands in, which differs whenever the method is
    /// inherited.
    method_index: HashMap<String, Vec<(String, String)>>,
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
    /// Whether the body currently being emitted is a LAMBDA body. A `return`
    /// there is non-local (it belongs to the enclosing `def`), so it lowers to
    /// `NLR_RAISE` + the unwind walk rather than a frame-local `ReturnValue`.
    in_lambda: bool,
    /// Whether the program builds a mutable collection anywhere — see
    /// [`body_has_mutable`].
    has_mutable: bool,
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
    /// Out of the enclosing `def`/method frame. This is the frame a non-local
    /// return lands in: the check takes the parked return value (`NLR_TAKE`) and
    /// returns it, or returns `Unit` and leaves a real exception in flight.
    Def,
    /// Out of the enclosing LAMBDA frame, returning `Unit`. A lambda is not a
    /// method, so a non-local return passes straight through it — that is the
    /// whole point of routing `return` through the unwind path.
    Lambda,
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
    /// The subset of `captures` that were BOXED in the enclosing frame, so the
    /// captured value is a cell handle and the body must read/write through it
    /// (see [`Scope::boxed`]).
    boxed: HashSet<String>,
    body: Expr,
    current_class: Option<(String, HashSet<String>)>,
    current_object: Option<String>,
}

/// Compile-time class shape.
struct ClassMeta {
    /// Instance fields in record order: this class's own constructor parameters
    /// first (so a `case class`'s derived members read the right prefix), then
    /// the fields inherited from its linearized supertypes, then its own body
    /// `val`/`var`s. Names appearing twice collapse to one field.
    field_names: Vec<String>,
    /// Primary-constructor arity (the `new`/`apply`/`unapply` argument count —
    /// the leading prefix of `field_names`).
    arity: usize,
    /// `case class` → structural semantics + companion `apply`/`unapply`/`copy`.
    is_case: bool,
    /// `trait` → no constructor is emitted and it cannot be instantiated.
    is_trait: bool,
    /// The linearization *excluding* this type: supertypes nearest first.
    supers: Vec<String>,
    /// Every method name an instance of this type responds to, own and
    /// inherited (abstract declarations included — they are dispatched, not
    /// called directly).
    responds: HashSet<String>,
    /// The method names this type *implements* itself — the `Owner$method`
    /// subroutines it owns, which is what `super.m` resolves against.
    own_methods: HashSet<String>,
}

/// Compile-time singleton-object shape.
struct ObjMeta {
    /// `val`/`var` member names (accessed as the `Name.val` global).
    vals: HashSet<String>,
    /// `def` member names, own and inherited (dispatched to the owning type's
    /// `Owner$method` subroutine).
    methods: HashSet<String>,
    /// `case object` → structural semantics (so two `None`s compare equal).
    is_case: bool,
    /// The linearization excluding the object itself.
    supers: Vec<String>,
}

/// Scala-style linearization of `name`: the type itself, then its supertypes
/// right-to-left, each recursively, keeping the first occurrence of a repeat.
/// This is the resolution order for `extends P with T1 with T2` (`T2`, `T1`,
/// `P`) — the full C3 rule differs only for diamond hierarchies, which this
/// frontend does not model.
fn linearize(name: &str, parents: &HashMap<&str, &[String]>) -> Vec<String> {
    fn go(name: &str, parents: &HashMap<&str, &[String]>, out: &mut Vec<String>, depth: u32) {
        // A cyclic `extends` would otherwise recurse forever; the depth cap
        // makes a malformed hierarchy terminate instead of blowing the stack.
        if depth > 64 || out.iter().any(|x| x == name) {
            return;
        }
        out.push(name.to_string());
        if let Some(ps) = parents.get(name) {
            for p in ps.iter().rev() {
                go(p, parents, out, depth + 1);
            }
        }
    }
    let mut out = Vec::new();
    go(name, parents, &mut out, 0);
    out
}

/// A function body's local slot map (see [`Compiler::scope`]).
struct Scope {
    /// Local name → frame slot index.
    slots: HashMap<String, u16>,
    /// Next unused slot index (parameters take `0..arity`).
    next_slot: u16,
    /// The locals whose slot holds a heap CELL rather than the value itself,
    /// because a closure in this frame ASSIGNS them (see [`boxed_vars`]). Every
    /// read of one goes through `CELL_GET` and every write through `CELL_SET`;
    /// a capture takes the raw slot, which is what makes the write shared.
    boxed: HashSet<String>,
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
    // `Either`'s two cases, so `Right(v)` / `case Left(e) =>` and the
    // `Option.toRight`/`toLeft` results all round-trip through one record shape.
    for (name, field) in [("Left", "value"), ("Right", "value")] {
        if !classes.iter().any(|c| c.name == name) {
            classes.push(builtin_case1(name, field));
        }
    }
    // Built-in `None` case object, unless redefined.
    let mut objects: Vec<ObjectDecl> = prog.objects.clone();
    if !objects.iter().any(|o| o.name == "None") {
        objects.push(builtin_none());
    }

    // Linearize the declared types, then index class shapes with their inherited
    // fields and the method → (runtime tag, defining type) dispatch table.
    let parents: HashMap<&str, &[String]> = classes
        .iter()
        .map(|c| (c.name.as_str(), c.parents.as_slice()))
        .chain(
            objects
                .iter()
                .map(|o| (o.name.as_str(), o.parents.as_slice())),
        )
        .collect();
    let lin: HashMap<String, Vec<String>> = parents
        .keys()
        .map(|n| (n.to_string(), linearize(n, &parents)))
        .collect();
    drop(parents);
    inherit_into_objects(&mut objects, &classes, &lin);
    let by_name: HashMap<&str, &ClassDecl> = classes.iter().map(|c| (c.name.as_str(), c)).collect();

    let mut class_meta = HashMap::new();
    for cd in &classes {
        let mro = &lin[&cd.name];
        let mut field_names = cd.params.clone();
        // Inherited fields, base-most first, then this class's body fields.
        for anc in mro.iter().skip(1).rev() {
            if let Some(p) = by_name.get(anc.as_str()) {
                for f in &p.field_names {
                    if !field_names.contains(f) {
                        field_names.push(f.clone());
                    }
                }
            }
        }
        for f in &cd.field_names {
            if !field_names.contains(f) {
                field_names.push(f.clone());
            }
        }
        let responds = mro
            .iter()
            .filter_map(|a| by_name.get(a.as_str()))
            .flat_map(|p| p.methods.iter().map(|m| m.name.clone()))
            .collect();
        class_meta.insert(
            cd.name.clone(),
            ClassMeta {
                field_names,
                arity: cd.params.len(),
                is_case: cd.is_case,
                is_trait: cd.is_trait,
                supers: mro[1..].to_vec(),
                responds,
                own_methods: cd
                    .methods
                    .iter()
                    .filter(|m| !m.is_abstract)
                    .map(|m| m.name.clone())
                    .collect(),
            },
        );
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
        // The singleton's `def`s, its own plus whatever `inherit_into_objects`
        // spliced in from its supertypes.
        let methods: HashSet<String> = od.methods.iter().map(|m| m.name.clone()).collect();
        obj_meta.insert(
            od.name.clone(),
            ObjMeta {
                vals,
                methods,
                is_case: od.is_case,
                supers: lin[&od.name][1..].to_vec(),
            },
        );
    }

    // Runtime method table: for every *instantiable* type, the subroutine that
    // owns each method it responds to. A method declared once and never
    // inherited keeps a single entry, so the emitted dispatch is what it was.
    let mut method_index: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for tag in lin.keys() {
        // Traits are never a runtime tag: only their concrete subtypes are.
        if by_name.get(tag.as_str()).is_some_and(|c| c.is_trait) {
            continue;
        }
        // A singleton dispatches only its own `def`s (see `obj_meta` above).
        if let Some(o) = objects.iter().find(|o| &o.name == tag) {
            for m in &o.methods {
                method_index
                    .entry(m.name.clone())
                    .or_default()
                    .push((tag.clone(), tag.clone()));
            }
            continue;
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for owner in &lin[tag] {
            let Some(od) = by_name.get(owner.as_str()) else {
                continue;
            };
            for m in &od.methods {
                // An abstract declaration still *reserves* the name, so a
                // subtype's override is not shadowed by a later supertype.
                if seen.insert(&m.name) && !m.is_abstract {
                    method_index
                        .entry(m.name.clone())
                        .or_default()
                        .push((tag.clone(), owner.clone()));
                }
            }
        }
    }
    for v in method_index.values_mut() {
        v.sort();
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
        // A `return` inside a lambda, and any `return` that must run a
        // `finally` on the way out, both travel the unwind path — so a program
        // with either needs the per-statement checks even without a `try`.
        has_try: program_any(prog, &classes, &objects, body_has_try)
            || program_any(prog, &classes, &objects, body_has_lambda_return),
        in_lambda: false,
        // Likewise for `+=`: only a program that builds a mutable collection
        // can need the run-time growable test (see `compound_tail`).
        has_mutable: program_any(prog, &classes, &objects, body_has_mutable),
        unwind: Vec::new(),
        try_counter: 0,
    };

    // Publish each declared type's supertypes + constructor arity to the runtime
    // before anything runs: `case s: Shape` and the derived `case class` members
    // both need shape the flat record cannot carry. Emitted only when the
    // program declares a supertype anywhere, so a hierarchy-free program keeps
    // the bytecode it had.
    let needs_types = classes
        .iter()
        .any(|cd| !cd.parents.is_empty() || cd.is_trait || cd.params.len() != cd.field_names.len())
        || objects.iter().any(|o| !o.parents.is_empty());
    if needs_types {
        for cd in &classes {
            let meta = &c.classes[&cd.name];
            let (supers, arity) = (meta.supers.join(","), meta.arity);
            c.emit_type_reg(&cd.name, &supers, arity);
        }
        for od in &objects {
            let supers = c.objects[&od.name].supers.join(",");
            c.emit_type_reg(&od.name, &supers, 0);
        }
    }

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
            // A trait has no constructor — only the concrete methods a mixing
            // class dispatches into.
            if !cd.is_trait {
                c.class_constructor(cd, &by_name)?;
            }
            for m in &cd.methods {
                if !m.is_abstract {
                    c.class_method(cd, m)?;
                }
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
    builtin_case1("Some", "value")
}

/// A built-in single-field `case class` (`Some`, `Left`, `Right`).
fn builtin_case1(name: &str, field: &str) -> ClassDecl {
    ClassDecl {
        name: name.to_string(),
        is_case: true,
        is_trait: false,
        parents: Vec::new(),
        super_args: Vec::new(),
        params: vec![field.to_string()],
        body: Vec::new(),
        field_names: vec![field.to_string()],
        methods: Vec::new(),
    }
}

/// The built-in `None` case object (`Option`'s empty case) — a zero-field
/// singleton so two `None`s compare structurally equal.
fn builtin_none() -> ObjectDecl {
    ObjectDecl {
        name: "None".to_string(),
        is_case: true,
        parents: Vec::new(),
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
            // A `def`/method body: the frame a non-local return targets. Take
            // the parked return value and answer it; a real exception makes
            // `NLR_TAKE` answer `Unit` and stays in flight, so the caller's own
            // check resumes the walk in its frame. `ReturnValue` (not the bare
            // `Return` the Unit tail paths use) so the call site still receives
            // exactly one value whatever position it is in.
            Some(UnwindKind::Def) => {
                self.b.emit(Op::CallBuiltin(crate::host::NLR_TAKE, 0), 0);
                self.b.emit(Op::ReturnValue, 0);
            }
            // A lambda body: pass everything through, non-local return included.
            Some(UnwindKind::Lambda) => {
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
                // A `var` a closure assigns lives in a heap cell, allocated here
                // even without an initializer so the closure has something to
                // write through (see [`boxed_vars`]).
                if self.is_boxed(name) {
                    match init {
                        Some(e) => self.expr(e)?,
                        None => {
                            self.b.emit(Op::LoadUndef, 0);
                        }
                    }
                    self.unwind_check_dropping(1);
                    self.b.emit(Op::CallBuiltin(crate::host::CELL_NEW, 1), 0);
                    self.emit_store(place);
                    return Ok(());
                }
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
            // `val (a, b) = pair` — a pattern definition. The initializer is
            // evaluated once into an anonymous place, the pattern binds against
            // it into the ENCLOSING scope, and a mismatch raises the same
            // `scala.MatchError` a failed `match` does.
            StmtKind::Destructure { pat, init } => {
                self.expr(init)?;
                self.unwind_check_dropping(1);
                self.obj_counter += 1;
                let src = self.declare_place(&format!(" destr_{}", self.obj_counter));
                self.emit_store(src);
                let mut fail_jumps = Vec::new();
                self.match_pattern(pat, src, &mut fail_jumps)?;
                let ok = self.b.emit(Op::Jump(0), 0);
                let at = self.b.current_pos();
                for j in fail_jumps {
                    self.b.patch_jump(j, at);
                }
                self.emit_load(src);
                self.b.emit(Op::CallBuiltin(crate::host::SMATCHERR, 1), 0);
                self.b.emit(Op::Pop, 0);
                let end = self.b.current_pos();
                self.b.patch_jump(ok, end);
                Ok(())
            }
            StmtKind::Assign { name, op, value } => {
                // Scala rejects reassignment to a `val` at compile time; so do
                // we — except for `+=`/`-=`, which on a `val` can only be the
                // growable-collection method (`val buf = ListBuffer(); buf += x`
                // is the idiom). It lowers as that call, so a `val Int` still
                // fails, with the "value += is not a member of Int" message.
                if self.vals.get(name) == Some(&true) {
                    if matches!(op, AssignOp::Add | AssignOp::Sub) {
                        let m = if *op == AssignOp::Add { "+=" } else { "-=" };
                        self.emit_smethod(
                            &Expr::Var(name.clone()),
                            m,
                            std::slice::from_ref(value),
                            s.line,
                        )?;
                        self.b.emit(Op::Pop, 0);
                        return Ok(());
                    }
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
                let boxed = self.is_boxed(name);
                // `x = e`, or `x <op>= e` → the current value then the operator.
                if *op == AssignOp::Assign {
                    self.expr(value)?;
                } else {
                    self.emit_load(place);
                    if boxed {
                        self.b.emit(Op::CallBuiltin(crate::host::CELL_GET, 1), 0);
                    }
                    self.compound_tail(*op, value)?;
                }
                self.unwind_check_dropping(1);
                // A boxed `var` is written through its cell, so the write is
                // seen by every closure that captured it.
                if boxed {
                    self.emit_load(place);
                    self.b.emit(Op::CallBuiltin(crate::host::CELL_SET, 2), 0);
                    self.b.emit(Op::Pop, 0);
                } else {
                    self.emit_store(place);
                }
                Ok(())
            }
            StmtKind::Return(val) => {
                // A `return` that has to leave a lambda frame, or run an
                // enclosing `finally` on its way out, is lowered the way Scala
                // lowers it: park the value as a non-local return and let the
                // unwind walk carry it (through every `finally`) to the method
                // frame, whose check answers it. Everything else is the direct,
                // free form — pop the frame with the value on the stack.
                if self.in_lambda || self.unwind.iter().any(|f| f.kind == UnwindKind::Try) {
                    match val {
                        Some(e) => self.expr(e)?,
                        None => {
                            self.b.emit(Op::LoadUndef, s.line);
                        }
                    };
                    self.b
                        .emit(Op::CallBuiltin(crate::host::NLR_RAISE, 1), s.line);
                    self.b.emit(Op::Pop, s.line);
                    return Ok(());
                }
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
            // `crate::resolve` hoists every block-local `def` into
            // `Program::functions` and deletes the statement, so one surviving
            // here means that pass was skipped.
            StmtKind::DefDecl(f) => Err(format!(
                "scalars: internal: unresolved local def `{}`",
                f.name
            )),
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
            // `y = e` inside a counted loop: re-evaluate `e` per iteration into
            // its own slot, in scope for the enumerators to the right and the
            // body. No closure is built, so the loop stays trace-eligible.
            ForEnum::Val { name, value } => {
                self.expr(value)?;
                let place = self.declare_place(name);
                self.emit_store(place);
                self.lower_for(enums, idx + 1, body, yield_into)?;
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
            // A `Char` is a host-interned value, so the literal loads its code
            // point and lets `CHAR_NEW` hand back the shared handle.
            Expr::Char(c) => {
                self.b.emit(Op::LoadInt(*c as u32 as i64), 0);
                self.b.emit(Op::CallBuiltin(crate::host::CHAR_NEW, 1), 0);
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
                    // `~x` is `x.unary_~`, dispatched through the host so it
                    // reports the Scala message on a non-integral receiver.
                    UnOp::Complement => {
                        let nc = self.b.add_constant(Value::str("unary_~".to_string()));
                        self.b.emit(Op::LoadConst(nc), 0);
                        self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), 0);
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
            Expr::Lambda {
                params,
                body,
                partial,
            } => self.lambda(params, body, *partial)?,
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

    /// Lower a `List`/`Seq`/`Vector`/`Set`/`Map`/`Array` literal to its host
    /// constructor builtin.
    fn collection(&mut self, ctor: &str, elems: &[Expr]) -> Result<(), String> {
        for el in elems {
            self.expr(el)?;
        }
        let id = match ctor {
            // Scala 3 `Seq` is `List`; `IndexedSeq` is `Vector`.
            "List" | "Seq" => crate::host::MAKE_LIST,
            "Vector" | "IndexedSeq" => crate::host::MAKE_VECTOR,
            "Set" => crate::host::MAKE_SET,
            "Map" => crate::host::MAKE_MAP,
            "Array" => crate::host::MAKE_ARRAY,
            "ListBuffer" => crate::host::MAKE_LISTBUFFER,
            "ArrayBuffer" => crate::host::MAKE_ARRAYBUFFER,
            "mutable.Set" => crate::host::MAKE_MUTSET,
            "mutable.Map" => crate::host::MAKE_MUTMAP,
            _ => return Err(format!("scalars: unknown collection constructor `{ctor}`")),
        };
        self.b.emit(Op::CallBuiltin(id, elems.len() as u8), 0);
        Ok(())
    }

    /// Lower a lambda literal: queue its body for emission as a subroutine region
    /// and, at the literal site, build the runtime closure handle. Free names that
    /// resolve to an enclosing frame slot are captured by value (upvalues); free
    /// names that are globals/`def`s stay unbound and resolve at call time.
    fn lambda(&mut self, params: &[String], body: &Expr, partial: bool) -> Result<(), String> {
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

        // Which captures are boxed cells in the enclosing frame — the closure
        // body must go through `CELL_GET`/`CELL_SET` for exactly those.
        let boxed: HashSet<String> = match self.scope.as_ref() {
            Some(scope) => captures
                .iter()
                .filter(|c| scope.boxed.contains(*c))
                .cloned()
                .collect(),
            None => HashSet::new(),
        };

        // A `{ case … }` literal is a `PartialFunction`: a second subroutine
        // answers `isDefinedAt`. It re-uses the same parameter and capture
        // layout, so the one capture list serves both bodies.
        let id = self.closures_seen;
        self.closures_seen += 1;
        let name_idx = self.b.add_name(&format!("$closure_{id}"));
        let defined_idx = match (partial, defined_at_body(body)) {
            (true, Some(test)) => {
                let idx = self.b.add_name(&format!("$defined_{id}"));
                self.pending_closures.push_back(PendingClosure {
                    name_idx: idx,
                    params: params.to_vec(),
                    captures: captures.clone(),
                    boxed: boxed.clone(),
                    body: test,
                    current_class: self.current_class.clone(),
                    current_object: self.current_object.clone(),
                });
                Some(idx)
            }
            _ => None,
        };

        // Push name index, param count, the pair-result flag, then each captured
        // value (read from the enclosing frame) so MAKE_CLOSURE stores them in
        // the handle. A partial function pushes its `isDefinedAt` name index too,
        // and MAKE_PARTIAL reads that one extra leading operand.
        self.b.emit(Op::LoadInt(name_idx as i64), 0);
        self.b.emit(Op::LoadInt(params.len() as i64), 0);
        self.b.emit(Op::LoadInt(body_flags(body)), 0);
        if let Some(idx) = defined_idx {
            self.b.emit(Op::LoadInt(idx as i64), 0);
        }
        for cap in &captures {
            let place = self.resolve_place(cap);
            self.emit_load(place);
        }
        let (builtin, extra) = match defined_idx {
            Some(_) => (crate::host::MAKE_PARTIAL, 4),
            None => (crate::host::MAKE_CLOSURE, 3),
        };
        self.b
            .emit(Op::CallBuiltin(builtin, captures.len() as u8 + extra), 0);
        self.pending_closures.push_back(PendingClosure {
            name_idx,
            params: params.to_vec(),
            captures,
            boxed,
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
        // A captured cell stays a cell inside the closure, and a `var` this body
        // declares gets boxed for exactly the same reason the enclosing frame's
        // would: a lambda nested one level deeper assigns it.
        let mut boxed = pc.boxed.clone();
        boxed.extend(boxed_vars_expr(&pc.body));
        let saved_scope = self.scope.replace(Scope {
            slots,
            next_slot: total as u16,
            boxed,
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
        // A closure body is its own unwind boundary (see `unwind_check`), but
        // NOT a method boundary: a `return` inside it belongs to the enclosing
        // `def`, so the frame is `Lambda` and lets a non-local return through.
        self.push_unwind(UnwindKind::Lambda);
        let saved_lambda = self.in_lambda;
        self.in_lambda = true;
        self.expr(&pc.body)?;
        self.in_lambda = saved_lambda;
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
                // `case Nil =>` — the empty `List`. Tested by runtime shape
                // rather than by `==` against a materialized object, because
                // `Nil` is a collection value, not a case singleton.
                if name == "Nil" && !self.objects.contains_key(name) {
                    self.emit_type_test(vplace, "Nil", fail_jumps);
                } else {
                    // `case None =>` / a stable-identifier pattern: `scrut == <value>`.
                    self.emit_load(vplace);
                    self.materialize_object(name)?;
                    self.b.emit(Op::NumEq, 0);
                    fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
                }
            }
            // `case n @ p =>` — bind the whole scrutinee, then match `p`. The
            // binding is emitted first; it is only ever *read* from the arm body,
            // which runs solely when `p` matched too, so the order is unobservable.
            Pattern::At { name, pat } => {
                let p = self.declare_place(name);
                self.emit_load(vplace);
                self.emit_store(p);
                self.match_pattern(pat, vplace, fail_jumps)?;
            }
            // `case a | b | c =>` — the first branch that matches wins. Each
            // non-final branch gets its own failure list, patched to the start of
            // the next branch; only the last branch's failures reach the arm's.
            Pattern::Alt(alts) => {
                let mut matched = Vec::new();
                for (i, alt) in alts.iter().enumerate() {
                    if i + 1 == alts.len() {
                        self.match_pattern(alt, vplace, fail_jumps)?;
                        break;
                    }
                    let mut local = Vec::new();
                    self.match_pattern(alt, vplace, &mut local)?;
                    matched.push(self.b.emit(Op::Jump(0), 0));
                    let next = self.b.current_pos();
                    for j in local {
                        self.b.patch_jump(j, next);
                    }
                }
                let end = self.b.current_pos();
                for j in matched {
                    self.b.patch_jump(j, end);
                }
            }
            // `case h :: t =>` — a non-empty `List`, destructured into head/tail.
            Pattern::Cons(head, tail) => {
                self.emit_type_test(vplace, "::", fail_jumps);
                let hp = self.bind_accessor(vplace, "head", "hd");
                self.match_pattern(head, hp, fail_jumps)?;
                let tp = self.bind_accessor(vplace, "tail", "tl");
                self.match_pattern(tail, tp, fail_jumps)?;
            }
            // A bare `_*` outside a sequence pattern is not Scala.
            Pattern::Rest(_) => {
                return Err(
                    "scalars: `_*` is only valid as the last element of a sequence pattern"
                        .to_string(),
                )
            }
            Pattern::Tuple(elems) => {
                // Arity test, then bind each element through the `_1`.. accessors.
                self.emit_load(vplace);
                let tn = self
                    .b
                    .add_constant(Value::str(format!("Tuple{}", elems.len())));
                self.b.emit(Op::LoadConst(tn), 0);
                self.b.emit(Op::CallBuiltin(crate::host::SISTYPE, 2), 0);
                fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
                for (i, elem) in elems.iter().enumerate() {
                    self.emit_load(vplace);
                    let acc = self.b.add_constant(Value::str(format!("_{}", i + 1)));
                    self.b.emit(Op::LoadConst(acc), 0);
                    self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), 0);
                    self.obj_counter += 1;
                    let ep = self.declare_place(&format!(" tup_{}", self.obj_counter));
                    self.emit_store(ep);
                    self.match_pattern(elem, ep, fail_jumps)?;
                }
            }
            Pattern::Constructor { name, elems } => {
                // Scala's derived `unapply` exposes the primary-constructor
                // parameters only — the leading prefix of the record.
                let fields = match self.classes.get(name) {
                    Some(meta) => meta.field_names[..meta.arity].to_vec(),
                    // `case List(a, b) =>` / `Seq(…)` / `Vector(…)` / `Array(…)`
                    // are sequence patterns, matched on shape and length rather
                    // than against a record's fields.
                    None if seq_pattern_ctor(name) => {
                        return self.match_seq_pattern(name, elems, vplace, fail_jumps)
                    }
                    // Not a type — so it is an extractor named by a *value*,
                    // Scala's stable-identifier pattern. The only one here is a
                    // `Regex` (`val p = "…".r; case p(a, b) => …`), and which
                    // value it is can only be known at run time, so the match
                    // becomes an `unapplySeq` call. A name that turns out not to
                    // be an extractor reports the same message from the host.
                    None => return self.match_extractor_pattern(name, elems, vplace, fail_jumps),
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

    /// A constructor pattern whose name is a value rather than a type — Scala's
    /// stable-identifier extractor, `case p(a, b) =>` where `p` is a `Regex`.
    ///
    /// Emits `UNAPPLY_SEQ(p, scrut, arity)`, which answers the bound values or
    /// `null` for no match, tests that, and binds each sub-pattern from the
    /// result. The extractor is loaded as an ordinary variable read, so it
    /// follows the same scoping as any other reference to `p`.
    fn match_extractor_pattern(
        &mut self,
        name: &str,
        elems: &[Pattern],
        vplace: Place,
        fail_jumps: &mut Vec<usize>,
    ) -> Result<(), String> {
        self.var_ref(name)?;
        self.emit_load(vplace);
        self.b.emit(Op::LoadInt(elems.len() as i64), 0);
        // The source name, so a non-extractor reports the identifier written.
        let nc = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(nc), 0);
        self.b.emit(Op::CallBuiltin(crate::host::UNAPPLY_SEQ, 4), 0);
        self.obj_counter += 1;
        let rp = self.declare_place(&format!(" unapp_{}", self.obj_counter));
        self.emit_store(rp);
        // No match is `null`; anything else is the tuple of bound values.
        self.emit_load(rp);
        self.b.emit(Op::LoadUndef, 0);
        self.b.emit(Op::NumNe, 0);
        fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
        // Bound positions come back as a tuple, read through the same `_1`..
        // accessors a tuple pattern uses.
        for (i, elem) in elems.iter().enumerate() {
            self.emit_load(rp);
            let acc = self.b.add_constant(Value::str(format!("_{}", i + 1)));
            self.b.emit(Op::LoadConst(acc), 0);
            self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), 0);
            self.obj_counter += 1;
            let ep = self.declare_place(&format!(" unappel_{}", self.obj_counter));
            self.emit_store(ep);
            self.match_pattern(elem, ep, fail_jumps)?;
        }
        Ok(())
    }

    /// Emit `SISTYPE(scrut, ty)` and push its failure branch onto `fail_jumps`.
    /// Every shape test in [`Self::match_pattern`] goes through here, so the
    /// branch is always taken BEFORE any method is dispatched on the scrutinee —
    /// that ordering is what keeps a wrong-shaped value from faulting the VM on
    /// an accessor it does not have.
    fn emit_type_test(&mut self, vplace: Place, ty: &str, fail_jumps: &mut Vec<usize>) {
        self.emit_load(vplace);
        let c = self.b.add_constant(Value::str(ty.to_string()));
        self.b.emit(Op::LoadConst(c), 0);
        self.b.emit(Op::CallBuiltin(crate::host::SISTYPE, 2), 0);
        fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
    }

    /// Call the zero-argument accessor `method` on the scrutinee and store the
    /// result in a fresh anonymous place, returning it for sub-pattern matching.
    fn bind_accessor(&mut self, vplace: Place, method: &str, tag: &str) -> Place {
        self.emit_load(vplace);
        let c = self.b.add_constant(Value::str(method.to_string()));
        self.b.emit(Op::LoadConst(c), 0);
        self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), 0);
        self.obj_counter += 1;
        let p = self.declare_place(&format!(" {tag}_{}", self.obj_counter));
        self.emit_store(p);
        p
    }

    /// Call the one-argument method `method(arg)` on the scrutinee and store the
    /// result in a fresh anonymous place.
    fn bind_accessor1(&mut self, vplace: Place, method: &str, arg: i64, tag: &str) -> Place {
        self.emit_load(vplace);
        let a = self.b.add_constant(Value::int(arg));
        self.b.emit(Op::LoadConst(a), 0);
        let c = self.b.add_constant(Value::str(method.to_string()));
        self.b.emit(Op::LoadConst(c), 0);
        self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 3), 0);
        self.obj_counter += 1;
        let p = self.declare_place(&format!(" {tag}_{}", self.obj_counter));
        self.emit_store(p);
        p
    }

    /// `case List(a, b) =>` / `case Seq(x, rest @ _*) =>` — a sequence pattern:
    /// a shape test on the receiver kind, then a length test (exact, or `>=` when
    /// a trailing `_*` absorbs the remainder), then positional binding through
    /// `apply(i)`. A named `_*` binds `drop(n)`.
    fn match_seq_pattern(
        &mut self,
        name: &str,
        elems: &[Pattern],
        vplace: Place,
        fail_jumps: &mut Vec<usize>,
    ) -> Result<(), String> {
        let (fixed, rest) = match elems.last() {
            Some(Pattern::Rest(r)) => (&elems[..elems.len() - 1], Some(r.clone())),
            _ => (elems, None),
        };
        if fixed.iter().any(|p| matches!(p, Pattern::Rest(_))) {
            return Err(
                "scalars: `_*` is only valid as the last element of a sequence pattern".to_string(),
            );
        }
        self.emit_type_test(vplace, name, fail_jumps);
        // Length test. `Op::NumGe` for the `_*` form, `Op::NumEq` otherwise.
        self.emit_load(vplace);
        let lc = self.b.add_constant(Value::str("length".to_string()));
        self.b.emit(Op::LoadConst(lc), 0);
        self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), 0);
        let n = self.b.add_constant(Value::int(fixed.len() as i64));
        self.b.emit(Op::LoadConst(n), 0);
        self.b
            .emit(if rest.is_some() { Op::NumGe } else { Op::NumEq }, 0);
        fail_jumps.push(self.b.emit(Op::JumpIfFalse(0), 0));
        for (i, elem) in fixed.iter().enumerate() {
            let p = self.bind_accessor1(vplace, "apply", i as i64, "elt");
            self.match_pattern(elem, p, fail_jumps)?;
        }
        if let Some(Some(rname)) = rest {
            let p = self.bind_accessor1(vplace, "drop", fixed.len() as i64, "rest");
            let dst = self.declare_place(&rname);
            self.emit_load(p);
            self.emit_store(dst);
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
            // A boxed `var` keeps its value in a heap cell (see [`boxed_vars`]).
            if self.is_boxed(name) {
                self.b.emit(Op::CallBuiltin(crate::host::CELL_GET, 1), 0);
            }
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
                // Virtual: a subtype may override this member, so the receiver's
                // runtime tag decides (the single-implementation case collapses
                // back to a direct call in `dispatch_instance_method`).
                let targets = self.method_index.get(name).cloned().unwrap_or_default();
                return self.dispatch_instance_method(
                    &Expr::Var("this".to_string()),
                    name,
                    &[],
                    &targets,
                    0,
                );
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
            return self.lambda(&params, &call, false);
        }
        let place = self.resolve_place(name);
        self.emit_load(place);
        Ok(())
    }

    /// Whether an instance of `cname` responds to `method` — declared by the
    /// class/trait itself or inherited from its linearization.
    fn class_defines_method(&self, cname: &str, method: &str) -> bool {
        self.classes
            .get(cname)
            .is_some_and(|m| m.responds.contains(method))
    }

    /// The type that owns `cname`'s implementation of `method` (the subroutine
    /// `Owner$method` a call on a `cname` instance lands in).
    fn method_owner(&self, cname: &str, method: &str) -> Option<String> {
        self.method_index.get(method).and_then(|v| {
            v.iter()
                .find(|(tag, _)| tag == cname)
                .map(|(_, owner)| owner.clone())
        })
    }

    /// The type owning a singleton's implementation of `method`. Falls back to
    /// the object itself so a plain `object`'s `def` keeps its `Name$m` name.
    fn object_method_owner(&self, obj: &str, method: &str) -> String {
        self.method_owner(obj, method)
            .unwrap_or_else(|| obj.to_string())
    }

    /// Lower `super.m(args)` inside a class/trait method: resolve `m` in the
    /// enclosing type's linearization *after* the type itself and call that
    /// implementation directly with the current `this`.
    fn super_call(&mut self, name: &str, args: &[Expr], line: u32) -> Result<(), String> {
        let Some((cname, _)) = self.current_class.clone() else {
            return Err(format!("scalars: `super` outside a class (line {line})"));
        };
        let supers = match self.classes.get(&cname) {
            Some(m) => m.supers.clone(),
            None => Vec::new(),
        };
        // The first supertype that owns an implementation of `m`.
        let owner = supers
            .iter()
            .find(|s| self.type_declares_method(s, name))
            .cloned()
            .ok_or_else(|| {
                format!("scalars: no supertype of {cname} defines `{name}` (line {line})")
            })?;
        let this = self.resolve_place("this");
        self.emit_load(this);
        for a in args {
            self.expr(a)?;
        }
        let nidx = self.b.add_name(&method_sub_name(&owner, name));
        self.b.emit(Op::Call(nidx, args.len() as u8 + 1), line);
        Ok(())
    }

    /// Whether type `ty` itself implements `method`. `super` resolves against
    /// the declarations, not the dispatch table: the table is keyed by concrete
    /// tag, and a trait whose method every subtype re-inherits never appears in
    /// it as an owner.
    fn type_declares_method(&self, ty: &str, method: &str) -> bool {
        self.classes
            .get(ty)
            .is_some_and(|m| m.own_methods.contains(method))
    }

    /// Emit a read of `this.field` (the receiver is the `this` slot).
    fn emit_field_get_this(&mut self, field: &str) {
        let this = self.resolve_place("this");
        self.emit_load(this);
        let c = self.b.add_constant(Value::str(field.to_string()));
        self.b.emit(Op::LoadConst(c), 0);
        self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), 0);
    }

    /// Finish a compound assignment whose *current value* is already on the
    /// stack, leaving the new value there.
    ///
    /// Scala resolves `x += e` by preferring the `+=` **method** when the
    /// receiver has one and falling back to `x = x + e` otherwise, which is a
    /// static choice this frontend cannot make: a `ListBuffer` mutates in place
    /// and answers itself, an `Int` adds. So the choice is made at run time —
    /// [`crate::host::IS_GROWABLE`] asks the receiver, and only the taken branch
    /// runs. A program with no mutable collection in it still emits exactly the
    /// arithmetic it did before, because the test is only emitted for `+=`/`-=`.
    fn compound_tail(&mut self, op: AssignOp, value: &Expr) -> Result<(), String> {
        if !matches!(op, AssignOp::Add | AssignOp::Sub) {
            self.expr(value)?;
            match op {
                AssignOp::Div => self.b.emit(Op::CallBuiltin(crate::host::SDIV, 2), 0),
                _ => self.b.emit(compound_op(op), 0),
            };
            return Ok(());
        }
        // No mutable collection in the program: `+=` can only be arithmetic,
        // so emit exactly the two ops it emitted before this feature existed.
        if !self.has_mutable {
            self.expr(value)?;
            self.b.emit(compound_op(op), 0);
            return Ok(());
        }
        let method = if op == AssignOp::Add { "+=" } else { "-=" };
        self.b.emit(Op::Dup, 0);
        self.b.emit(Op::CallBuiltin(crate::host::IS_GROWABLE, 1), 0);
        let to_arith = self.b.emit(Op::JumpIfFalse(0), 0);
        // Growable: `recv.+=(value)` mutates and answers the receiver.
        self.expr(value)?;
        let nc = self.b.add_constant(Value::str(method.to_string()));
        self.b.emit(Op::LoadConst(nc), 0);
        self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 3), 0);
        let to_end = self.b.emit(Op::Jump(0), 0);
        let arith = self.b.current_pos();
        self.b.patch_jump(to_arith, arith);
        self.expr(value)?;
        self.b.emit(compound_op(op), 0);
        let end = self.b.current_pos();
        self.b.patch_jump(to_end, end);
        Ok(())
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
        if op == AssignOp::Assign {
            self.expr(value)?;
        } else {
            self.emit_field_get_this(field);
            self.compound_tail(op, value)?;
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
        if op == AssignOp::Assign {
            self.expr(value)?;
        } else {
            self.b.emit(Op::GetVar(g), 0);
            self.compound_tail(op, value)?;
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
            // `new Regex("…")` is the constructor spelling of `"…".r`; both make
            // the same `Regex` value, so it lowers to the same `r` method. The
            // second parameter list (group names) is not modeled.
            if name == "Regex" && args.len() == 1 {
                self.expr(&args[0])?;
                let m = self.b.add_constant(Value::str("r".to_string()));
                self.b.emit(Op::LoadConst(m), line);
                self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 2), line);
                return Ok(());
            }
        }
        let arity = match self.classes.get(name) {
            Some(meta) if meta.is_trait => {
                return Err(format!(
                    "scalars: trait {name} is abstract; it cannot be instantiated (line {line})"
                ))
            }
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

    /// Emit one [`crate::host::TYPE_REG`] call: publish a declared type's
    /// supertype list and primary-constructor arity to the runtime. Leaves
    /// nothing on the stack.
    fn emit_type_reg(&mut self, name: &str, supers_csv: &str, arity: usize) {
        let n = self.b.add_constant(Value::str(name.to_string()));
        self.b.emit(Op::LoadConst(n), 0);
        let s = self.b.add_constant(Value::str(supers_csv.to_string()));
        self.b.emit(Op::LoadConst(s), 0);
        self.b.emit(Op::LoadInt(arity as i64), 0);
        self.b.emit(Op::CallBuiltin(crate::host::TYPE_REG, 3), 0);
        self.b.emit(Op::Pop, 0);
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
        // `super.m(args)` — skip this class in the linearization and call the
        // nearest supertype that defines `m` (a static call, as the JVM's
        // `invokespecial` is).
        if matches!(recv, Expr::Var(v) if v == "super") {
            return self.super_call(name, args, line);
        }
        // `x.isInstanceOf[T]` is the runtime type test the parser captured the
        // type name for; `x.asInstanceOf[T]` is a static assertion with no
        // runtime effect on a dynamically typed value model (see `BUGS.md`).
        match (name, args) {
            ("isInstanceOf", [ty @ Expr::Str(_)]) => {
                self.expr(recv)?;
                self.expr(ty)?;
                self.b.emit(Op::CallBuiltin(crate::host::SISTYPE, 2), line);
                return Ok(());
            }
            ("asInstanceOf", [Expr::Str(_)]) => return self.expr(recv),
            _ => {}
        }
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
                    let owner = self.object_method_owner(obj, name);
                    let nidx = self.b.add_name(&method_sub_name(&owner, name));
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
        // `mutable.ListBuffer(…)` / `scala.collection.mutable.Set(…)` — a
        // package member, not a receiver method, so it lowers to the collection
        // literal its name selects.
        if mutable_module(recv) {
            if let Some(ctor) = mutable_ctor(name) {
                return self.collection(ctor, args);
            }
        }
        // `scala.math.<member>` / `math.<member>` / `Math.<member>` — the JDK
        // math module, which is a value namespace rather than a receiver.
        if let Some(java) = math_module(recv) {
            for a in args {
                self.expr(a)?;
            }
            // `java.lang.Math` and `scala.math` are not the same namespace:
            // `Math.signum(5)` widens to `1.0` (the JDK has no `int` overload)
            // where `math.signum(5)` is `1`. The spelling travels with the name.
            let member = if java {
                format!("java.{name}")
            } else {
                name.to_string()
            };
            let nc = self.b.add_constant(Value::str(member));
            self.b.emit(Op::LoadConst(nc), line);
            self.b.emit(
                Op::CallBuiltin(crate::host::SMATH, args.len() as u8 + 1),
                line,
            );
            return Ok(());
        }
        // `a to b` / `a until b` — build a first-class `Range`. A `by` step then
        // rebuilds it through the host (see `crate::host`), so `(1 to 9 by 2)`
        // is one range value rather than a chain of collections.
        if (name == "to" || name == "until") && args.len() == 1 {
            self.expr(recv)?;
            self.expr(&args[0])?;
            self.b.emit(
                if name == "to" {
                    Op::LoadTrue
                } else {
                    Op::LoadFalse
                },
                line,
            );
            self.b.emit(Op::LoadInt(1), line);
            self.b
                .emit(Op::CallBuiltin(crate::host::MAKE_RANGE, 4), line);
            return Ok(());
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
        classes: &[(String, String)],
        line: u32,
    ) -> Result<(), String> {
        // A single implementation and a receiver already known to be that class
        // needs no tag test: call it directly.
        if let [(tag, owner)] = classes {
            if matches!(recv, Expr::Var(v) if v == "this")
                && self.current_class.as_ref().is_some_and(|(c, _)| c == tag)
            {
                self.expr(recv)?;
                for a in args {
                    self.expr(a)?;
                }
                let nidx = self.b.add_name(&method_sub_name(owner, name));
                self.b.emit(Op::Call(nidx, args.len() as u8 + 1), line);
                return Ok(());
            }
        }
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
        for (tag, owner) in classes {
            self.emit_load(cls);
            let cc = self.b.add_constant(Value::str(tag.clone()));
            self.b.emit(Op::LoadConst(cc), line);
            self.b.emit(Op::NumEq, line);
            let jf = self.b.emit(Op::JumpIfFalse(0), line);
            self.emit_load(t);
            for a in args {
                self.expr(a)?;
            }
            let nidx = self.b.add_name(&method_sub_name(owner, name));
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
    fn class_constructor(
        &mut self,
        cd: &ClassDecl,
        by_name: &HashMap<&str, &ClassDecl>,
    ) -> Result<(), String> {
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
            boxed: boxed_vars(&cd.body),
        });
        for i in (0..cd.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), 0);
        }
        // Supertype initialization, in Scala's order: every `extends P(args)`
        // argument list is evaluated first, walking *up* the superclass chain
        // (each level's arguments are written in terms of the level below it),
        // and only then do the constructor bodies run base-most first, so a
        // class body can read the fields its supertypes just initialized.
        let mut level = cd;
        while let Some(parent) = level.parents.first().and_then(|p| by_name.get(p.as_str())) {
            for (name, arg) in parent.params.iter().zip(&level.super_args) {
                self.expr(arg)?;
                let place = self.declare_place(name);
                self.emit_store(place);
                self.vals.insert(name.clone(), true);
            }
            level = parent;
        }
        let supers: Vec<String> = self.classes[&cd.name].supers.clone();
        for anc in supers.iter().rev() {
            let Some(p) = by_name.get(anc.as_str()) else {
                continue;
            };
            for s in &p.body {
                self.stmt(s)?;
            }
        }
        for s in &cd.body {
            self.stmt(s)?;
        }
        // Assemble the record: field values (in record order), then the class
        // name, the field-name CSV, and the `is_case` flag.
        let field_names = self.classes[&cd.name].field_names.clone();
        for f in &field_names {
            let place = self.resolve_place(f);
            self.emit_load(place);
        }
        let cn = self.b.add_constant(Value::str(cd.name.clone()));
        self.b.emit(Op::LoadConst(cn), 0);
        let csv = self.b.add_constant(Value::str(field_names.join(",")));
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
            Op::CallBuiltin(crate::host::OBJ_NEW, field_names.len() as u8 + 4),
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
            boxed: boxed_vars(&m.body),
        });
        // Prologue: args arrive as `[this, p0, …]` (deepest = this); pop reverse.
        for i in (0..=m.params.len()).rev() {
            self.b.emit(Op::SetSlot(i as u16), 0);
        }
        let saved_class = self.current_class.take();
        // The field set is the *flattened* one, so a method inherited into a
        // subclass and a trait method alike see every field the instance holds.
        let fields = self
            .classes
            .get(&cd.name)
            .map(|m| m.field_names.iter().cloned().collect())
            .unwrap_or_default();
        self.current_class = Some((cd.name.clone(), fields));
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
            boxed: boxed_vars(&m.body),
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

    /// Lower a named call that is not a user `def` (those are resolved to a
    /// direct `Op::Call` earlier). Two shapes reach here:
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
        // `Option(x)` — the factory whose `null` case is `None`. Only when the
        // program has not bound `Option` itself.
        if name == "Option"
            && args.len() == 1
            && !self.func_arity.contains_key(name)
            && !self.classes.contains_key(name)
        {
            self.expr(&args[0])?;
            self.b
                .emit(Op::CallBuiltin(crate::host::MAKE_OPTION, 1), line);
            return Ok(());
        }
        // `new Array[T](n)` — the length and the element-type name.
        if name == crate::parser::NEW_ARRAY {
            for a in args {
                self.expr(a)?;
            }
            self.b
                .emit(Op::CallBuiltin(crate::host::ARRAY_FILL, 2), line);
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
                let targets = self.method_index.get(name).cloned().unwrap_or_default();
                return self.dispatch_instance_method(
                    &Expr::Var("this".to_string()),
                    name,
                    args,
                    &targets,
                    line,
                );
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

    /// Whether `name` is a boxed local of the current frame (see [`Scope::boxed`]).
    fn is_boxed(&self, name: &str) -> bool {
        self.scope.as_ref().is_some_and(|s| s.boxed.contains(name))
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
            boxed: boxed_vars(&f.body),
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
            // The value of a tail expression is the function's result — unless
            // it left something in flight (a raise, or a non-local return out of
            // a `try`/lambda inside it), in which case the check drops the
            // garbage value and dispatches instead.
            StmtKind::Expr(e) => {
                self.expr(e)?;
                self.unwind_check_dropping(1);
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

// ── whole-program feature scans ────────────────────────────────────────────
//
// Three compile-time decisions are made once per program from the same walk of
// the AST, so a program that uses none of the three features emits exactly the
// bytecode it did before the feature existed:
//
//   * a `rust { … }` FFI block anywhere arms the `__rust_compile` prologue,
//   * a `try` anywhere arms the per-statement unwind checks, and
//   * a mutable-collection literal anywhere arms the `+=` growable test.

/// Whether any expression in `body` (recursively) satisfies `pred`.
fn body_any(body: &[Stmt], pred: &impl Fn(&Expr) -> bool) -> bool {
    body.iter().any(|s| match &s.kind {
        StmtKind::Local { init, .. } => init.as_ref().is_some_and(|e| expr_any(e, pred)),
        StmtKind::Destructure { init, .. } => expr_any(init, pred),
        StmtKind::Assign { value, .. } => expr_any(value, pred),
        StmtKind::Expr(e) => expr_any(e, pred),
        StmtKind::If { cond, then, els } => {
            expr_any(cond, pred) || body_any(then, pred) || body_any(els, pred)
        }
        StmtKind::While { cond, body } => expr_any(cond, pred) || body_any(body, pred),
        StmtKind::Return(e) => e.as_ref().is_some_and(|e| expr_any(e, pred)),
        // Lifted into `Program::functions` before compiling, and scanned there.
        StmtKind::DefDecl(_) => false,
    })
}

/// Whether `e` or any expression under it satisfies `pred`.
fn expr_any(e: &Expr, pred: &impl Fn(&Expr) -> bool) -> bool {
    if pred(e) {
        return true;
    }
    match e {
        Expr::Try {
            body,
            catches,
            finalizer,
        } => {
            body_any(body, pred)
                || catches.iter().any(|a| body_any(&a.body, pred))
                || finalizer.as_deref().is_some_and(|f| body_any(f, pred))
        }
        Expr::Throw { value, .. } | Expr::Format { value, .. } => expr_any(value, pred),
        Expr::Unary { rhs, .. } => expr_any(rhs, pred),
        Expr::Binary { lhs, rhs, .. } => expr_any(lhs, pred) || expr_any(rhs, pred),
        Expr::Println { arg, .. } => arg.as_deref().is_some_and(|a| expr_any(a, pred)),
        Expr::Call { args, .. } | Expr::New { args, .. } => args.iter().any(|a| expr_any(a, pred)),
        Expr::Method { recv, args, .. } => {
            expr_any(recv, pred) || args.iter().any(|a| expr_any(a, pred))
        }
        Expr::Copy { recv, updates, .. } => {
            expr_any(recv, pred) || updates.iter().any(|(_, v)| expr_any(v, pred))
        }
        Expr::If { cond, then, els } => {
            expr_any(cond, pred)
                || expr_any(then, pred)
                || els.as_deref().is_some_and(|x| expr_any(x, pred))
        }
        Expr::Block(stmts) => body_any(stmts, pred),
        Expr::Match { scrut, arms } => {
            expr_any(scrut, pred)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(|g| expr_any(g, pred)) || body_any(&a.body, pred)
                })
        }
        Expr::ForYield { enums, body } | Expr::ForEach { enums, body } => {
            enums.iter().any(|en| enum_any(en, pred)) || expr_any(body, pred)
        }
        Expr::Lambda { body, .. } => expr_any(body, pred),
        Expr::Tuple(elems) | Expr::Collection { elems, .. } => {
            elems.iter().any(|el| expr_any(el, pred))
        }
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::Char(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Placeholder
        | Expr::Var(_) => false,
    }
}

/// Whether a `for` enumerator's bounds / step / collection / guard satisfy `pred`.
fn enum_any(e: &ForEnum, pred: &impl Fn(&Expr) -> bool) -> bool {
    match e {
        ForEnum::Gen {
            start, end, step, ..
        } => {
            expr_any(start, pred)
                || expr_any(end, pred)
                || step.as_ref().is_some_and(|s| expr_any(s, pred))
        }
        ForEnum::GenColl { coll, .. } => expr_any(coll, pred),
        ForEnum::Guard(c) => expr_any(c, pred),
        ForEnum::Val { value, .. } => expr_any(value, pred),
    }
}

/// True if any statement in `body` (recursively) evaluates a `__rust_compile`
/// call — the desugar target of a `rust { ... }` block.
fn body_has_ffi(body: &[Stmt]) -> bool {
    body_any(
        body,
        &|e| matches!(e, Expr::Call { name, .. } if name == RUST_COMPILE),
    )
}

/// True if a statement list contains a `try` anywhere (including inside nested
/// expressions), which is what arms the per-statement unwind checks.
fn body_has_try(stmts: &[Stmt]) -> bool {
    body_any(stmts, &|e| matches!(e, Expr::Try { .. }))
}

/// True if a statement list contains a `return` inside something that becomes a
/// LAMBDA body — an explicit lambda, or a `for`/`for … yield` comprehension,
/// which desugars to `foreach`/`map` closures after this scan runs. Such a
/// `return` is non-local: it travels the unwind path, so the per-statement
/// checks must be armed even in a program with no `try`.
fn body_has_lambda_return(stmts: &[Stmt]) -> bool {
    body_any(stmts, &|e| match e {
        Expr::Lambda { body, .. } | Expr::ForEach { body, .. } | Expr::ForYield { body, .. } => {
            expr_has_return(body)
        }
        _ => false,
    })
}

/// Whether a lambda/comprehension body contains a `return` statement anywhere.
///
/// A statement-level walk, because `body_any` only tests EXPRESSIONS and a
/// `return` is a statement — and the block that holds it is often a bare
/// `Vec<Stmt>` (an `if` branch, a `while` body), never an `Expr::Block`.
fn expr_has_return(e: &Expr) -> bool {
    // Any nested block reached through a sub-expression counts too, which is
    // what the recursive `expr_any` walk covers.
    expr_any(e, &|inner| match inner {
        Expr::Block(stmts) => stmts_have_return(stmts),
        Expr::Try {
            body,
            catches,
            finalizer,
        } => {
            stmts_have_return(body)
                || catches.iter().any(|a| stmts_have_return(&a.body))
                || finalizer.as_deref().is_some_and(stmts_have_return)
        }
        _ => false,
    })
}

/// Whether a statement list holds a `return` at any nesting depth.
fn stmts_have_return(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| match &s.kind {
        StmtKind::Return(_) => true,
        StmtKind::If { then, els, cond } => {
            stmts_have_return(then) || stmts_have_return(els) || expr_has_return(cond)
        }
        StmtKind::While { body, cond } => stmts_have_return(body) || expr_has_return(cond),
        StmtKind::Expr(e) => expr_has_return(e),
        StmtKind::Local { init, .. } => init.as_ref().is_some_and(expr_has_return),
        StmtKind::Destructure { init, .. } => expr_has_return(init),
        StmtKind::Assign { value, .. } => expr_has_return(value),
        // Lifted into `Program::functions` before compiling; its own `return`s
        // are local to it, not to the enclosing body.
        StmtKind::DefDecl(_) => false,
    })
}

/// True if a statement list builds a mutable collection anywhere — a
/// `ListBuffer`/`ArrayBuffer` literal (which the parser already recognizes) or a
/// `scala.collection.mutable` factory call. Only such a program pays for the
/// run-time `+=` dispatch test in [`Compiler::compound_tail`]; every other one
/// keeps emitting a bare `Add`/`Sub`, so a counted loop stays trace-eligible.
fn body_has_mutable(stmts: &[Stmt]) -> bool {
    body_any(stmts, &|e| match e {
        Expr::Collection { ctor, .. } => mutable_buffer_literal(ctor),
        Expr::Method { recv, name, .. } => mutable_module(recv) && mutable_ctor(name).is_some(),
        _ => false,
    })
}

/// Whether `scan` answers true for any statement list in the whole program —
/// the entry body, every hoisted `def`, and every class/object body and method.
fn program_any(
    prog: &Program,
    classes: &[ClassDecl],
    objects: &[ObjectDecl],
    scan: fn(&[Stmt]) -> bool,
) -> bool {
    scan(&prog.main)
        || prog.functions.iter().any(|f| scan(&f.body))
        || classes
            .iter()
            .any(|c| scan(&c.body) || c.methods.iter().any(|m| scan(&m.body)))
        || objects
            .iter()
            .any(|o| scan(&o.body) || o.methods.iter().any(|m| scan(&m.body)))
}

/// Whether a collection-literal constructor names a mutable collection.
fn mutable_buffer_literal(ctor: &str) -> bool {
    matches!(ctor, "ListBuffer" | "ArrayBuffer")
}

/// Whether a constructor pattern names a *sequence* extractor (`case List(a, b)`)
/// rather than a case class. Only consulted when no user class shadows the name.
fn seq_pattern_ctor(name: &str) -> bool {
    matches!(name, "List" | "Seq" | "Vector" | "IndexedSeq" | "Array")
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

/// Whether `e` names a math module — `math`/`scala.math` or `Math`/
/// `java.lang.Math` — and which of the two. These are namespaces, not values, so
/// a member access on one lowers to the math builtin rather than to receiver
/// dispatch. `Some(true)` is the `java.lang.Math` spelling.
fn math_module(e: &Expr) -> Option<bool> {
    match e {
        Expr::Var(n) if n == "math" => Some(false),
        Expr::Var(n) if n == "Math" => Some(true),
        Expr::Method {
            recv, name, args, ..
        } if args.is_empty() => {
            if name == "math" && matches!(&**recv, Expr::Var(p) if p == "scala") {
                Some(false)
            } else if name == "Math" && is_java_lang(recv) {
                Some(true)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether `e` names the `scala.collection.mutable` package — spelled
/// `mutable`, `collection.mutable` or `scala.collection.mutable`. A member
/// access on one is a mutable-collection factory, not a receiver method, so
/// `mutable.Set(1, 2)` builds a `mutable.HashSet` where a bare `Set(1, 2)`
/// builds the immutable one.
fn mutable_module(e: &Expr) -> bool {
    match e {
        Expr::Var(n) => n == "mutable",
        Expr::Method {
            recv, name, args, ..
        } if args.is_empty() && name == "mutable" => match &**recv {
            Expr::Var(p) => p == "collection",
            Expr::Method {
                recv, name, args, ..
            } => {
                args.is_empty()
                    && name == "collection"
                    && matches!(&**recv, Expr::Var(p) if p == "scala")
            }
            _ => false,
        },
        _ => false,
    }
}

/// The collection constructor a `scala.collection.mutable` member names, if it
/// is one this frontend builds.
fn mutable_ctor(name: &str) -> Option<&'static str> {
    // `LinkedHashSet`/`LinkedHashMap` are deliberately absent: they keep
    // insertion order, so mapping them onto the hash-table ones would mis-run.
    Some(match name {
        "ListBuffer" => "ListBuffer",
        "ArrayBuffer" | "Buffer" => "ArrayBuffer",
        "Set" | "HashSet" => "mutable.Set",
        "Map" | "HashMap" => "mutable.Map",
        _ => return None,
    })
}

/// Whether `e` is the `java.lang` package prefix.
fn is_java_lang(e: &Expr) -> bool {
    matches!(e, Expr::Method { recv, name, args, .. }
        if name == "lang" && args.is_empty() && matches!(&**recv, Expr::Var(p) if p == "java"))
}

/// Splice every concrete member a singleton `object` inherits into the object
/// itself.
///
/// A `class` instance is a host-heap record, so a supertype's method body is
/// compiled once and dispatched with the instance as `this`. An `object` has no
/// record: its `val`s are program globals and its `def`s are statically
/// dispatched subroutines. So `object X extends T` gets T's concrete members by
/// *copying* them into X, exactly as if they had been written in X's body — the
/// members then compile in the object's own name scope, where a `val` is a
/// global and a sibling `def` is `X$name`.
///
/// A member X declares itself wins (an `override`), and among supertypes the
/// linearization order decides. Inherited `val`s are prepended base-most first,
/// so a supertype's initializer runs before the body that may read it.
fn inherit_into_objects(
    objects: &mut [ObjectDecl],
    classes: &[ClassDecl],
    lin: &HashMap<String, Vec<String>>,
) {
    let by_name: HashMap<&str, &ClassDecl> = classes.iter().map(|c| (c.name.as_str(), c)).collect();
    for od in objects.iter_mut() {
        let Some(mro) = lin.get(&od.name) else {
            continue;
        };
        let mut have_methods: HashSet<&str> = od.methods.iter().map(|m| m.name.as_str()).collect();
        let mut have_vals: HashSet<String> = od.body.iter().filter_map(local_name).collect();
        let mut inherited_methods = Vec::new();
        let mut inherited_vals: Vec<Vec<Stmt>> = Vec::new();
        for anc in mro.iter().skip(1) {
            let Some(parent) = by_name.get(anc.as_str()) else {
                continue;
            };
            for m in &parent.methods {
                if !m.is_abstract && have_methods.insert(&m.name) {
                    inherited_methods.push(m.clone());
                }
            }
            let vals: Vec<Stmt> = parent
                .body
                .iter()
                .filter(|s| local_name(s).is_some_and(|n| have_vals.insert(n)))
                .cloned()
                .collect();
            inherited_vals.push(vals);
        }
        // `have_methods` borrows `od.methods`; release it before extending.
        drop(have_methods);
        od.methods.extend(inherited_methods);
        let mut body: Vec<Stmt> = inherited_vals.into_iter().rev().flatten().collect();
        body.append(&mut od.body);
        od.body = body;
    }
}

/// The name a `val`/`var` declaration statement binds, if `s` is one.
fn local_name(s: &Stmt) -> Option<String> {
    match &s.kind {
        StmtKind::Local { name, .. } => Some(name.clone()),
        _ => None,
    }
}

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
/// * `for (case p <- e) …`             → `e.withFilter(<p matches>)` then continue
///
/// A range generator (`i <- a to b`) is materialized to a `List` first.
fn desugar_for(enums: &[ForEnum], body: &Expr, is_yield: bool) -> Expr {
    let (pat, mut src) = gen_source(&enums[0]);
    // A `case` generator's pattern is refutable, so Scala filters the source by
    // it before binding — that is what makes a non-matching element skipped
    // rather than a `MatchError`.
    if matches!(
        enums[0],
        ForEnum::GenColl {
            filtering: true,
            ..
        }
    ) {
        src = method(src, "withFilter", vec![pattern_test(&pat)]);
    }
    // Guards immediately after the generator become `withFilter` on its source.
    let mut i = 1;
    while let Some(ForEnum::Guard(g)) = enums.get(i) {
        src = method(src, "withFilter", vec![lambda1(&pat, g.clone())]);
        i += 1;
    }
    // `y = e` — Scala's own translation pairs the value onto the generator, so
    // every enumerator to the right (and the body) sees BOTH names:
    //   `for (x <- xs; y = e; rest)` → `for ((x, y) <- xs.map(x => (x, e)); rest)`
    // Recursing on the rewritten list handles a run of definitions (each nests
    // one more tuple) and a guard that reads a defined name.
    if let Some(ForEnum::Val { name, value }) = enums.get(i) {
        // The pair carries the generator's element THROUGH unchanged (read from
        // the mapping lambda's own parameter), so the rewritten generator can
        // re-destructure it with the very same pattern — whatever shape it has.
        let paired = Expr::Tuple(vec![gen_elem_expr(&pat), value.clone()]);
        let mut rewritten = vec![ForEnum::GenColl {
            pat: Pattern::Tuple(vec![pat.clone(), Pattern::Bind(name.clone())]),
            coll: method(src, "map", vec![lambda1(&pat, paired)]),
            // The rewrite re-destructures the pair with the SAME pattern, so a
            // refutable one has already been filtered out by the `withFilter`
            // above and cannot fail here.
            filtering: false,
        }];
        rewritten.extend_from_slice(&enums[i + 1..]);
        return desugar_for(&rewritten, body, is_yield);
    }
    let rest = &enums[i..];
    if rest.is_empty() {
        let m = if is_yield { "map" } else { "foreach" };
        method(src, m, vec![lambda1(&pat, body.clone())])
    } else {
        let inner = desugar_for(rest, body, is_yield);
        // Nested yield collects via `flatMap`; nested foreach nests `foreach`.
        let m = if is_yield { "flatMap" } else { "foreach" };
        method(src, m, vec![lambda1(&pat, inner)])
    }
}

/// A one-argument predicate answering whether its argument matches `pat` —
/// `x => x match { case pat => true; case _ => false }`.
///
/// This is the `withFilter` a `for (case pat <- xs)` generator runs before
/// binding, and it is why the pattern may fail without raising: the wildcard arm
/// answers `false` for exactly the elements a non-`case` generator would have
/// hit a `MatchError` on.
fn pattern_test(pat: &Pattern) -> Expr {
    let name = "$forpat".to_string();
    let arm = |p: Pattern, v: bool| MatchArm {
        pat: p,
        guard: None,
        body: vec![Stmt {
            line: 0,
            kind: StmtKind::Expr(Expr::Bool(v)),
        }],
    };
    Expr::Lambda {
        params: vec![name.clone()],
        body: Box::new(Expr::Match {
            scrut: Box::new(Expr::Var(name)),
            arms: vec![arm(pat.clone(), true), arm(Pattern::Wildcard, false)],
        }),
        partial: false,
    }
}

/// The element a [`lambda1`] over `pat` received, as an expression readable from
/// inside that lambda's body: its parameter. [`lambda1`] names that parameter
/// after a plain binder and `$forpat` for every other shape, so this mirrors the
/// same choice — the two must agree.
fn gen_elem_expr(pat: &Pattern) -> Expr {
    Expr::Var(match pat {
        Pattern::Bind(n) => n.clone(),
        _ => "$forpat".to_string(),
    })
}

/// The `(bound name, source-collection expr)` of a generator enumerator. A range
/// generator is wrapped in the `$range_list` materialization call.
fn gen_source(e: &ForEnum) -> (Pattern, Expr) {
    match e {
        ForEnum::GenColl { pat, coll, .. } => (pat.clone(), coll.clone()),
        ForEnum::Gen {
            name,
            start,
            end,
            inclusive,
            step,
        } => (
            Pattern::Bind(name.clone()),
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
        // Scala requires the first enumerator to be a generator, so neither of
        // these can open a comprehension.
        ForEnum::Guard(_) => unreachable!("a comprehension does not begin with a guard"),
        ForEnum::Val { .. } => {
            unreachable!("a comprehension does not begin with a value definition")
        }
    }
}

/// Whether every value this function body can answer is a *pair literal* —
/// `(a, b)` or the `a -> b` sugar.
///
/// Scala picks a `map`/`collect` result's builder from the function's **static**
/// result type: over a `Map`, a pair result rebuilds a `Map` and anything else
/// falls back to `immutable.Iterable`'s builder, which is `List`. The runtime
/// can read that off the results it produced — except when there are none, where
/// `Map[K,V]().map(f)` and a `collect` that matched nothing are indistinguishable
/// at run time. This flag travels with the closure to decide exactly that case;
/// with at least one result the host reads the results themselves.
fn yields_pairs(body: &Expr) -> bool {
    match body {
        Expr::Tuple(elems) => elems.len() == 2,
        Expr::Block(stmts) => match stmts.last().map(|s| &s.kind) {
            Some(StmtKind::Expr(e)) => yields_pairs(e),
            _ => false,
        },
        Expr::Match { arms, .. } => arms.iter().all(|a| match a.body.last().map(|s| &s.kind) {
            Some(StmtKind::Expr(e)) => yields_pairs(e),
            _ => false,
        }),
        Expr::If { then, els, .. } => {
            yields_pairs(then) && els.as_ref().is_some_and(|e| yields_pairs(e))
        }
        _ => false,
    }
}

/// The compile-time facts about a lambda body, packed into the single operand
/// `MAKE_CLOSURE`/`MAKE_PARTIAL` carries: bit 0 is [`yields_pairs`], bit 1 is
/// [`yields_chars`], bit 2 is [`yields_strings`]. One operand means a new fact
/// costs no change to the closure's stack layout.
fn body_flags(body: &Expr) -> i64 {
    i64::from(yields_pairs(body))
        | (i64::from(yields_chars(body)) << 1)
        | (i64::from(yields_strings(body)) << 2)
}

/// Whether this function body answers a `Char`.
///
/// `Char` is its own runtime type, so `String.map`/`collect` normally read
/// Scala's `Char => Char` vs `Char => B` overload split straight off the
/// results. The one case with no results to read is an EMPTY receiver, where
/// Scala still answers by static type — `"".map(_.toUpper)` is `""` but
/// `"".map(_.toString)` is `ArraySeq()`. Only the syntax can settle that, so
/// these are the shapes whose result type is `Char`: the element itself, a
/// `Char` literal, and the `Char`-returning methods.
fn yields_chars(body: &Expr) -> bool {
    match body {
        // The element flows straight through (`c => c`, `_`).
        Expr::Var(_) | Expr::Placeholder => true,
        Expr::Char(_) => true,
        Expr::Method { name, .. } => matches!(name.as_str(), "toUpper" | "toLower" | "toChar"),
        Expr::Block(stmts) => match stmts.last().map(|s| &s.kind) {
            Some(StmtKind::Expr(e)) => yields_chars(e),
            _ => false,
        },
        Expr::Match { arms, .. } => arms.iter().all(|a| match a.body.last().map(|s| &s.kind) {
            Some(StmtKind::Expr(e)) => yields_chars(e),
            _ => false,
        }),
        Expr::If { then, els, .. } => {
            yields_chars(then) && els.as_ref().is_some_and(|e| yields_chars(e))
        }
        _ => false,
    }
}

/// Whether every value this function body can answer is statically a `String`.
///
/// `String.flatMap` has the same empty-receiver problem as [`yields_chars`], but
/// splits on `Char => String` (concatenate) vs `Char => IterableOnce[B]` (build
/// a `Vector`), so it needs the `String` side rather than the `Char` side.
fn yields_strings(body: &Expr) -> bool {
    match body {
        Expr::Str(_) => true,
        Expr::Method { name, .. } => name == "toString" || name == "mkString",
        // A concatenation is a `String` as soon as either side is one.
        Expr::Binary {
            op: BinOp::Add,
            lhs,
            rhs,
        } => yields_strings(lhs) || yields_strings(rhs),
        Expr::Block(stmts) => match stmts.last().map(|s| &s.kind) {
            Some(StmtKind::Expr(e)) => yields_strings(e),
            _ => false,
        },
        Expr::Match { arms, .. } => arms.iter().all(|a| match a.body.last().map(|s| &s.kind) {
            Some(StmtKind::Expr(e)) => yields_strings(e),
            _ => false,
        }),
        Expr::If { then, els, .. } => {
            yields_strings(then) && els.as_ref().is_some_and(|e| yields_strings(e))
        }
        _ => false,
    }
}

/// The `isDefinedAt` body of a `{ case … }` literal, derived from its `apply`
/// body: the same scrutinee, patterns and guards, with every arm body replaced
/// by `true` and a trailing `case _ => false` catching the rest.
///
/// Deriving it rather than re-parsing means the guards have already been through
/// [`crate::resolve`], so a guard that calls a block-local `def` or reads a
/// captured binding resolves identically in both bodies. Only the arm *bodies*
/// are dropped, which is exactly Scala's `applyOrElse`/`isDefinedAt` split: the
/// result expression never runs for an element the function is not defined at.
fn defined_at_body(body: &Expr) -> Option<Expr> {
    let Expr::Match { scrut, arms } = body else {
        return None;
    };
    let lit = |b: bool| MatchArm {
        pat: Pattern::Wildcard,
        guard: None,
        body: vec![Stmt {
            line: 0,
            kind: StmtKind::Expr(Expr::Bool(b)),
        }],
    };
    let mut tests: Vec<MatchArm> = arms
        .iter()
        .map(|a| MatchArm {
            pat: a.pat.clone(),
            guard: a.guard.clone(),
            ..lit(true)
        })
        .collect();
    tests.push(lit(false));
    Some(Expr::Match {
        scrut: scrut.clone(),
        arms: tests,
    })
}

/// Build the single-parameter function a desugared generator maps with. A plain
/// binder is `name => body`; a destructuring one becomes the pattern-matching
/// anonymous function `{ case pat => body }`, so `for ((k, v) <- m)` binds both.
fn lambda1(pat: &Pattern, body: Expr) -> Expr {
    let name = match pat {
        Pattern::Bind(n) => n.clone(),
        _ => "$forpat".to_string(),
    };
    let body = match pat {
        Pattern::Bind(_) => body,
        _ => Expr::Match {
            scrut: Box::new(Expr::Var(name.clone())),
            arms: vec![MatchArm {
                pat: pat.clone(),
                guard: None,
                body: vec![Stmt {
                    line: 0,
                    kind: StmtKind::Expr(body),
                }],
            }],
        },
    };
    Expr::Lambda {
        params: vec![name],
        body: Box::new(body),
        partial: false,
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

// ── boxed-`var` analysis (closures that mutate an enclosing local) ──────────
//
// Captures are threaded BY VALUE, so a closure writing to a captured slot would
// write to its own copy and the enclosing frame would never see it. Scala solves
// this by boxing such a local (`scala.runtime.IntRef` and friends); so does this
// compiler, with one heap cell per boxed `var`. The analysis below finds exactly
// which locals need it, so every other local keeps a plain frame slot and the
// bytecode for a program with no mutating closure is unchanged.

/// The locals of one frame that a closure inside it ASSIGNS: declared as a `var`
/// directly in this frame, and written from inside a nested lambda.
fn boxed_vars(body: &[Stmt]) -> HashSet<String> {
    let mut scan = BoxScan::default();
    scan.block(body, false);
    scan.finish()
}

/// [`boxed_vars`] for a frame whose body is a single expression (a lambda body).
fn boxed_vars_expr(body: &Expr) -> HashSet<String> {
    let mut scan = BoxScan::default();
    scan.expr(body, false);
    scan.finish()
}

/// One walk collecting both halves of [`boxed_vars`]: the `var`s declared in
/// this frame and the names assigned from inside a nested lambda.
#[derive(Default)]
struct BoxScan {
    declared: HashSet<String>,
    assigned_in_lambda: HashSet<String>,
}

impl BoxScan {
    fn finish(self) -> HashSet<String> {
        self.declared
            .intersection(&self.assigned_in_lambda)
            .cloned()
            .collect()
    }

    /// `in_lambda` is true once the walk has entered a nested function body —
    /// a lambda literal, or a comprehension over a COLLECTION, which
    /// [`desugar_for`] turns into `map`/`foreach` closures. A comprehension over
    /// a range stays an inline counted loop, so its body is still this frame.
    fn block(&mut self, body: &[Stmt], in_lambda: bool) {
        for s in body {
            match &s.kind {
                StmtKind::Local {
                    name, init, is_val, ..
                } if !*is_val => {
                    if !in_lambda {
                        self.declared.insert(name.clone());
                    }
                    if let Some(e) = init {
                        self.expr(e, in_lambda);
                    }
                }
                StmtKind::Local { init, .. } => {
                    if let Some(e) = init {
                        self.expr(e, in_lambda);
                    }
                }
                StmtKind::Destructure { init, .. } => self.expr(init, in_lambda),
                StmtKind::Assign { name, value, .. } => {
                    if in_lambda {
                        self.assigned_in_lambda.insert(name.clone());
                    }
                    self.expr(value, in_lambda);
                }
                StmtKind::Expr(e) => self.expr(e, in_lambda),
                StmtKind::If { cond, then, els } => {
                    self.expr(cond, in_lambda);
                    self.block(then, in_lambda);
                    self.block(els, in_lambda);
                }
                StmtKind::While { cond, body } => {
                    self.expr(cond, in_lambda);
                    self.block(body, in_lambda);
                }
                StmtKind::Return(e) => {
                    if let Some(e) = e {
                        self.expr(e, in_lambda);
                    }
                }
                // Lifted into `Program::functions` by `resolve` before compiling,
                // and scanned there as a frame of its own.
                StmtKind::DefDecl(_) => {}
            }
        }
    }

    fn expr(&mut self, e: &Expr, in_lambda: bool) {
        match e {
            Expr::Lambda { body, .. } => self.expr(body, true),
            Expr::ForYield { enums, body } | Expr::ForEach { enums, body } => {
                let desugars = enums.iter().any(is_coll_gen);
                for en in enums {
                    match en {
                        ForEnum::Gen {
                            start, end, step, ..
                        } => {
                            self.expr(start, in_lambda);
                            self.expr(end, in_lambda);
                            if let Some(s) = step {
                                self.expr(s, in_lambda);
                            }
                        }
                        ForEnum::GenColl { coll, .. } => self.expr(coll, in_lambda),
                        ForEnum::Guard(g) => self.expr(g, in_lambda || desugars),
                        ForEnum::Val { value, .. } => self.expr(value, in_lambda || desugars),
                    }
                }
                self.expr(body, in_lambda || desugars);
            }
            Expr::Block(stmts) => self.block(stmts, in_lambda),
            Expr::Try {
                body,
                catches,
                finalizer,
            } => {
                self.block(body, in_lambda);
                for a in catches {
                    if let Some(g) = &a.guard {
                        self.expr(g, in_lambda);
                    }
                    self.block(&a.body, in_lambda);
                }
                if let Some(f) = finalizer {
                    self.block(f, in_lambda);
                }
            }
            Expr::Match { scrut, arms } => {
                self.expr(scrut, in_lambda);
                for a in arms {
                    if let Some(g) = &a.guard {
                        self.expr(g, in_lambda);
                    }
                    self.block(&a.body, in_lambda);
                }
            }
            Expr::Throw { value, .. }
            | Expr::Format { value, .. }
            | Expr::Unary { rhs: value, .. } => self.expr(value, in_lambda),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs, in_lambda);
                self.expr(rhs, in_lambda);
            }
            Expr::Println { arg, .. } => {
                if let Some(a) = arg {
                    self.expr(a, in_lambda);
                }
            }
            Expr::Call { args, .. } | Expr::New { args, .. } => {
                for a in args {
                    self.expr(a, in_lambda);
                }
            }
            Expr::Method { recv, args, .. } => {
                self.expr(recv, in_lambda);
                for a in args {
                    self.expr(a, in_lambda);
                }
            }
            Expr::Copy { recv, updates, .. } => {
                self.expr(recv, in_lambda);
                for (_, v) in updates {
                    self.expr(v, in_lambda);
                }
            }
            Expr::If { cond, then, els } => {
                self.expr(cond, in_lambda);
                self.expr(then, in_lambda);
                if let Some(x) = els {
                    self.expr(x, in_lambda);
                }
            }
            Expr::Tuple(elems) | Expr::Collection { elems, .. } => {
                for el in elems {
                    self.expr(el, in_lambda);
                }
            }
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Char(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Placeholder
            | Expr::Var(_) => {}
        }
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
            StmtKind::Destructure { pat, init } => {
                fv_expr(init, &b, out, seen);
                pattern_binds(pat, &mut b);
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
            // Lifted into `Program::functions` before compiling.
            StmtKind::DefDecl(_) => {}
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
                    ForEnum::GenColl { pat, coll, .. } => {
                        fv_expr(coll, &b, out, seen);
                        pattern_binds(pat, &mut b);
                    }
                    ForEnum::Guard(g) => fv_expr(g, &b, out, seen),
                    // The definition's expression is evaluated with everything
                    // to its LEFT in scope; the name it binds joins for the rest.
                    ForEnum::Val { name, value } => {
                        fv_expr(value, &b, out, seen);
                        b.insert(name.clone());
                    }
                }
            }
            fv_expr(body, &b, out, seen);
        }
        Expr::Lambda { params, body, .. } => {
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
        | Expr::Char(_)
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
        Pattern::Constructor { elems, .. } | Pattern::Tuple(elems) | Pattern::Alt(elems) => {
            for e in elems {
                pattern_binds(e, bound);
            }
        }
        Pattern::At { name, pat } => {
            bound.insert(name.clone());
            pattern_binds(pat, bound);
        }
        Pattern::Cons(h, t) => {
            pattern_binds(h, bound);
            pattern_binds(t, bound);
        }
        Pattern::Rest(Some(n)) => {
            bound.insert(n.clone());
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
