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
    /// User-defined `def`s again, this time with the parameter NAMES and their
    /// [`ParamSig`]s plus the synthetic-capture count, which is everything
    /// [`Compiler::adapt_args`] needs to place named arguments, splice defaults,
    /// and collect a repeated parameter at a call site.
    func_sig: HashMap<String, (Vec<String>, Vec<ParamSig>, usize)>,
    /// Parameter names currently bound BY NAME (`x: => Int`). A read of one of
    /// these forces the thunk the caller passed, which is what makes a by-name
    /// argument re-evaluate at every use.
    by_name: HashSet<String>,
    /// Immutability of the bindings currently in scope: `name → is_val`.
    /// Reassigning a `val` (`true`) is a compile error (Scala rejects it too).
    /// Swapped out for a fresh map while a function body compiles so a `val`
    /// inside `main` cannot mask a `var` of the same name inside a `def`.
    vals: HashMap<String, bool>,
    /// What the width analysis knows about each binding in scope — see
    /// [`Width`]. Populated from a declared type (`val n: Long`, and a `def`
    /// parameter's mandatory annotation) or inferred from the initializer, and
    /// consulted by [`Compiler::num_ty`] to decide whether an arithmetic result
    /// wraps at 32 bits. Swapped out with [`Compiler::vals`] on entry to a
    /// function body, so one frame's widths never leak into another's.
    widths: HashMap<String, Width>,
    /// `Some` while compiling a function body: maps that function's local names
    /// (parameters, then `val`/`var`/`for` locals) to frame slot indices, so
    /// each call frame gets its own copies and recursion is correct. `None` in
    /// the top-level (`main`) scope, where every binding is a global addressed
    /// by `GetVar`/`SetVar`.
    scope: Option<Scope>,
    /// Every name bound at TOP LEVEL, i.e. every binding [`Compiler::declare_place`]
    /// gave a `Place::Global`. A nested body (a lambda, a `def`) sees a global by
    /// name — it is not a capture, so it has no frame slot, and `vals` was swapped
    /// out on entry. Without this set a call site inside such a body cannot tell
    /// `xs(i)` on a top-level `val` from a call to an undefined function, and
    /// rejects it. Reads never needed it: [`Compiler::resolve_place`] already
    /// falls back to `Place::Global`.
    global_binds: HashSet<String>,
    /// The widths a lambda literal's parameters take if one is lowered right
    /// now, positionally — set by [`Compiler::method`] from the receiver being
    /// traversed (`xs.map(x => …)` types `x` as the element of `xs`) and
    /// restored afterwards, so a nested traversal does not leak its element type
    /// into the enclosing one.
    lambda_param_widths: Vec<NumTy>,
    /// Every user `def`'s declared return width, by name. Scala infers a `def`'s
    /// result type from its body when the annotation is omitted; this frontend
    /// does not, so an unannotated `def` is absent here and its calls stay
    /// unnarrowed. Global, because [`crate::resolve`] has already hoisted every
    /// nested `def` into one flat namespace by the time this is built.
    def_widths: HashMap<String, NumTy>,
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
    /// The enclosing body's by-name parameters. A closure written inside a `def`
    /// can read one, and the read must still force the thunk, so the set travels
    /// with the queued body instead of being read from the compiler's cursor
    /// (which has long since moved on by the time the body is emitted).
    by_name: HashSet<String>,
    /// The enclosing scope's numeric widths, captured when the body was queued.
    /// A lambda body compiles with a fresh scope, so without this an enclosing
    /// `var n = 0` would lose its `Int` width the moment a closure touched it and
    /// `xs.foreach(_ => n += 1)` would stop wrapping.
    widths: HashMap<String, Width>,
    /// The widths of this lambda's own PARAMETERS, positionally, as inferred at
    /// the call site that took the lambda: `xs.map(x => …)` types `x` as the
    /// element of `xs`. A parameter with no inferred width shadows any enclosing
    /// binding of the same name with [`NumTy::Unknown`] rather than inheriting it.
    param_widths: Vec<NumTy>,
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
    /// The declared numeric width of every member — fields (constructor
    /// parameters and body `val`/`var`s) and methods (by return type), own and
    /// inherited. This is what types `c.n * 2` at a use site, and it is what
    /// keeps a user member named like a stdlib one (`size`, `count`, `length`)
    /// from being typed by the stdlib's rule. A member present here with
    /// [`NumTy::Unknown`] is a member whose type is declared but is not integer.
    member_widths: HashMap<String, NumTy>,
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
    let func_sig = prog
        .functions
        .iter()
        .map(|f| {
            (
                f.name.clone(),
                (f.params.clone(), f.sig.clone(), f.captured),
            )
        })
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
        // Member widths, base-most first so a subclass override wins. These are
        // read whenever the receiver's class is known — at a use site (`c.n * 2`)
        // and for the bare field references inside the class's own methods.
        let mut member_widths = HashMap::new();
        for anc in mro.iter().rev().filter_map(|a| by_name.get(a.as_str())) {
            for (p, ty) in anc.params.iter().zip(&anc.param_tys) {
                member_widths.insert(p.clone(), declared_width(ty.as_deref().unwrap_or("")));
            }
            for s in &anc.body {
                if let StmtKind::Local { name, ty, init, .. } = &s.kind {
                    // A field's own initializer types it when no annotation
                    // does, exactly as `val n = 0` types a local.
                    let w = match ty {
                        Some(t) => declared_width(t),
                        None => init.as_ref().map_or(NumTy::Unknown, literal_width),
                    };
                    member_widths.insert(name.clone(), w);
                }
            }
            for m in &anc.methods {
                member_widths.insert(
                    m.name.clone(),
                    declared_width(m.ret_ty.as_deref().unwrap_or("")),
                );
            }
        }
        class_meta.insert(
            cd.name.clone(),
            ClassMeta {
                field_names,
                arity: cd.params.len(),
                is_case: cd.is_case,
                is_trait: cd.is_trait,
                supers: mro[1..].to_vec(),
                responds,
                member_widths,
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
        func_sig,
        by_name: HashSet::new(),
        vals: HashMap::new(),
        widths: HashMap::new(),
        lambda_param_widths: Vec::new(),
        def_widths: prog
            .functions
            .iter()
            .filter_map(|f| {
                let t = f.ret_ty.as_deref()?;
                Some((f.name.clone(), declared_width(t)))
            })
            .collect(),
        scope: None,
        global_binds: HashSet::new(),
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
        param_tys: vec![None],
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
                self.emit_unit(0);
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
                name,
                init,
                is_val,
                ty,
            } => {
                let place = self.declare_place(name);
                self.vals.insert(name.clone(), *is_val);
                // The binding's numeric width: the declared type when there is
                // one (`val n: Long = 1`), otherwise inferred from the
                // initializer, which is how Scala itself types a bare `val`.
                let w = self.binding_width(ty.as_deref(), init.as_ref());
                self.widths.insert(name.clone(), w);
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
                    let target = self.widths.get(name).map_or(NumTy::Unknown, |w| w.num);
                    self.compound_tail(*op, value, target)?;
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
                            self.emit_unit(s.line);
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
                        self.emit_return_unit(s.line);
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
                Self::reject_char_range(start, end)?;
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
            // A `Long` literal loads exactly as an `Int` one does — the value
            // model is the same `i64`. Only the STATIC width differs, and that is
            // read off the AST node by `num_ty`, never off the emitted constant.
            Expr::Int(n) | Expr::Long(n) => {
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
            // `adapt_args` strips these off a `def` call; reaching the general
            // lowering means the callee has no named parameter list to match.
            Expr::NamedArg { name, .. } => {
                return Err(format!(
                    "scalars: named argument `{name} = …` is only supported on a `def` call"
                ))
            }
            Expr::Unary { op, rhs } => {
                let w = self.num_ty(rhs);
                self.expr(rhs)?;
                match op {
                    UnOp::Neg => {
                        self.b.emit(Op::Negate, 0);
                        // `-Int.MinValue` is `Int.MinValue` — negation is the
                        // one unary operator that can overflow.
                        self.narrow(w, 0);
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
                    self.emit_unit(0);
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
            // Only produced by the varargs collection in `adapt_args`.
            "ArraySeq" => crate::host::MAKE_ARRAYSEQ,
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
                    by_name: self.by_name.clone(),
                    widths: self.widths.clone(),
                    param_widths: self.lambda_param_widths.clone(),
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
            by_name: self.by_name.clone(),
            widths: self.widths.clone(),
            param_widths: self.lambda_param_widths.clone(),
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
        // The enclosing scope's widths travel into the body, so a captured
        // `var n = 0` is still known to be an `Int` here.
        let saved_widths = std::mem::replace(&mut self.widths, pc.widths);
        for (i, p) in pc.params.iter().enumerate() {
            self.vals.insert(p.clone(), true);
            // A parameter SHADOWS any enclosing binding of the same name. Its own
            // width is the one the call site inferred from the collection being
            // traversed (`xs.map(x => …)` over a `List[Int]` makes `x` an `Int`);
            // where that could not be proven it is unknown rather than inherited,
            // because inheriting would claim a width the parameter does not have.
            match pc.param_widths.get(i) {
                Some(&w) if w != NumTy::Unknown => {
                    self.widths.insert(p.clone(), Width::num(w));
                }
                _ => {
                    self.widths.remove(p);
                }
            }
        }
        let saved_class = std::mem::replace(&mut self.current_class, pc.current_class);
        let saved_object = std::mem::replace(&mut self.current_object, pc.current_object);
        // A parameter of this closure shadows an enclosing by-name parameter of
        // the same name, and is an ordinary value.
        let mut by_name = pc.by_name;
        for p in &pc.params {
            by_name.remove(p);
        }
        let saved_by_name = std::mem::replace(&mut self.by_name, by_name);

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
        self.widths = saved_widths;
        self.current_class = saved_class;
        self.current_object = saved_object;
        self.by_name = saved_by_name;
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
                self.emit_unit(0);
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
            self.emit_unit(0);
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
                self.emit_unit(0);
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
            // A by-name parameter's slot holds the caller's thunk, not a value.
            // Forcing it HERE — at the use, not at the call — is the whole point:
            // the argument runs once per read and not at all if never read.
            if self.by_name.contains(name) {
                // `APPLY`'s operand count excludes the callee itself, so a
                // zero-argument force is `APPLY, 0`.
                self.b.emit(Op::CallBuiltin(crate::host::APPLY, 0), 0);
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
    /// `target` is the width of the binding being updated; the result's width is
    /// that combined with the operand's, exactly as for the `x = x op e` form
    /// Scala expands this to.
    fn compound_tail(
        &mut self,
        op: AssignOp,
        value: &Expr,
        target: NumTy,
    ) -> Result<(), String> {
        let w = target.combine(self.num_ty(value));
        if !matches!(op, AssignOp::Add | AssignOp::Sub) {
            self.expr(value)?;
            match op {
                AssignOp::Div => self.b.emit(Op::CallBuiltin(crate::host::SDIV, 2), 0),
                _ => self.b.emit(compound_op(op), 0),
            };
            self.narrow(w, 0);
            return Ok(());
        }
        // No mutable collection in the program: `+=` can only be arithmetic,
        // so emit exactly the two ops it emitted before this feature existed.
        if !self.has_mutable {
            self.expr(value)?;
            self.b.emit(compound_op(op), 0);
            self.narrow(w, 0);
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
        // Only this branch is arithmetic — the growable one answers a collection,
        // which must not be narrowed.
        self.narrow(w, 0);
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
            self.compound_tail(op, value, NumTy::Unknown)?;
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
            self.compound_tail(op, value, NumTy::Unknown)?;
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

    /// Rewrite a written argument list into the exact positional list the
    /// callee's frame expects.
    ///
    /// Four Scala parameter-list features all resolve here, because all four are
    /// decided at the CALL site by the callee's signature:
    ///
    /// * a named argument (`f(b = 3, a = 4)`) moves to its parameter's position;
    /// * an omitted parameter with a default gets the default expression
    ///   spliced in — Scala evaluates it at the call site and only when the
    ///   argument is missing, which is exactly what splicing the unevaluated
    ///   expression here reproduces (Scala 3 forbids a default that reads
    ///   another parameter of the same list, so there is nothing else it could
    ///   depend on);
    /// * the trailing arguments of a repeated parameter (`xs: Int*`) collapse
    ///   into one `ArraySeq`, the class Scala hands a varargs method;
    /// * a by-name argument (`x: => Int`) is wrapped in a zero-argument thunk,
    ///   which [`Compiler::var_ref`] forces at each use inside the body.
    ///
    /// Trailing parameters synthesized by [`crate::resolve`]'s nested-`def`
    /// lifting are passed through untouched: the caller already appended those
    /// arguments, and they sit AFTER the written ones.
    fn adapt_args(&self, name: &str, args: &[Expr], line: u32) -> Result<Vec<Expr>, String> {
        let Some((params, sig, captured)) = self.func_sig.get(name) else {
            return Ok(args.to_vec());
        };
        let plain = sig
            .iter()
            .all(|p| p.default.is_none() && !p.vararg && !p.by_name);
        let named_any = args.iter().any(|a| matches!(a, Expr::NamedArg { .. }));
        if plain && !named_any {
            return Ok(args.to_vec());
        }
        // Split off the capture arguments the resolver appended.
        let ncap = (*captured).min(args.len());
        let (written, caps) = args.split_at(args.len() - ncap);
        let visible = params.len() - captured;
        let (params, sig) = (&params[..visible], &sig[..visible]);

        let vararg_at = sig.iter().position(|p| p.vararg);
        let mut slots: Vec<Option<Expr>> = vec![None; visible];
        let mut rest: Vec<Expr> = Vec::new();
        let mut pos = 0usize;
        for a in written {
            match a {
                Expr::NamedArg { name: pn, value } => {
                    let Some(i) = params.iter().position(|p| p == pn) else {
                        return Err(format!(
                            "scalars: {name} has no parameter named `{pn}` (line {line})"
                        ));
                    };
                    if slots[i].is_some() {
                        return Err(format!(
                            "scalars: parameter `{pn}` of {name} is given twice (line {line})"
                        ));
                    }
                    slots[i] = Some((**value).clone());
                }
                _ if vararg_at == Some(pos) => rest.push(a.clone()),
                _ => {
                    if pos >= visible {
                        return Err(format!(
                            "scalars: too many arguments for {name} (line {line})"
                        ));
                    }
                    slots[pos] = Some(a.clone());
                    pos += 1;
                }
            }
        }

        let mut out = Vec::with_capacity(params.len() + ncap);
        for (i, p) in sig.iter().enumerate() {
            let e = if p.vararg {
                Expr::Collection {
                    ctor: "ArraySeq".to_string(),
                    elems: std::mem::take(&mut rest),
                }
            } else {
                match slots[i].take().or_else(|| p.default.clone()) {
                    Some(e) => e,
                    None => {
                        return Err(format!(
                            "scalars: missing argument for parameter `{}` of {name} (line {line})",
                            params[i]
                        ))
                    }
                }
            };
            out.push(if p.by_name {
                Expr::Lambda {
                    params: Vec::new(),
                    body: Box::new(e),
                    partial: false,
                }
            } else {
                e
            });
        }
        out.extend_from_slice(caps);
        Ok(out)
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

    /// Lower a method call, then wrap the result back to 32 bits when it is an
    /// `Int` that could have left the range. Split from [`Self::method_inner`] so
    /// the narrowing covers EVERY lowering path — the math builtin, the operator
    /// spellings, and the generic `SMETHOD` fallback all funnel through here.
    fn method(&mut self, recv: &Expr, name: &str, args: &[Expr], line: u32) -> Result<(), String> {
        let w = self.method_width(recv, name, args);
        // A lambda argument's parameters are typed by the traversal, so the
        // widths are decided HERE — the lambda body itself is compiled much
        // later, out of a queue, with no memory of the call it came from. Saved
        // and restored so `xs.map(x => ys.filter(y => …))` gives each body its
        // own element type instead of the innermost winning both.
        let pw = self.traversal_param_widths(recv, name);
        let saved = std::mem::replace(&mut self.lambda_param_widths, pw);
        let r = self.method_inner(recv, name, args, line);
        self.lambda_param_widths = saved;
        r?;
        if NARROW_AFTER_METHODS.contains(&name) {
            self.narrow(w, line);
        }
        Ok(())
    }

    /// The widths a lambda passed to `recv.name(…)` binds its parameters to.
    ///
    /// Every traversal here hands the lambda ELEMENTS of the receiver, so one
    /// element width types every parameter: `map`/`filter`/`forall` take one,
    /// and `reduce` takes two of the same type. Deliberately absent are the
    /// folds (`foldLeft`'s accumulator is the width of the seed, not of an
    /// element) and `zip`ped traversals, whose parameter is a pair.
    fn traversal_param_widths(&self, recv: &Expr, name: &str) -> Vec<NumTy> {
        let arity = match name {
            _ if ELEMENT_TRAVERSALS.contains(&name) => 1,
            "reduce" | "reduceLeft" | "reduceRight" | "reduceOption" => 2,
            _ => return Vec::new(),
        };
        let elem = self.elem_ty(recv);
        if elem == NumTy::Unknown {
            return Vec::new();
        }
        vec![elem; arity]
    }

    /// Lower postfix `recv.name(args)`. Dispatch order:
    ///
    /// 1. **Static object member** — `Obj.method(...)` calls `Obj$method`;
    ///    `Obj.val` reads the `Obj.val` global.
    /// 2. **Instance method** — when some class declares `def name`, emit a
    ///    runtime class-tag dispatch chain (with a [`SMETHOD`] fallback).
    /// 3. **Fallback** — the universal `SMETHOD` builtin (String/Int/Double
    ///    stdlib and host-heap field/`toString`/`hashCode`/`equals` access).
    fn method_inner(
        &mut self,
        recv: &Expr,
        name: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<(), String> {
        // A shift is evaluated at the RECEIVER's width: `1 << 40` masks the
        // distance to five bits and answers 256, while `1L << 40` masks to six
        // and answers 1099511627776. The host cannot tell the two apart from the
        // value alone, so a `Long` receiver is dispatched under a distinct name.
        if args.len() == 1
            && matches!(name, "<<" | ">>" | ">>>")
            && self.num_ty(recv) == NumTy::Long
        {
            self.expr(recv)?;
            self.expr(&args[0])?;
            let nc = self.b.add_constant(Value::str(format!("{name}#long")));
            self.b.emit(Op::LoadConst(nc), line);
            self.b.emit(Op::CallBuiltin(crate::host::SMETHOD, 3), line);
            return Ok(());
        }
        // `Ordering.Int` / `Ordering.String` / … — the companion's members are
        // namespace, not receiver dispatch. Every one names the natural order;
        // the element type they differ by is a typing concern the runtime does
        // not have (see `host::MAKE_ORDERING`). `.reverse` is then an ordinary
        // method on the value this builds.
        if args.is_empty()
            && matches!(recv, Expr::Var(o) if o == "Ordering")
            && ORDERING_MEMBERS.contains(&name)
        {
            self.b.emit(Op::CallBuiltin(crate::host::MAKE_ORDERING, 0), line);
            return Ok(());
        }
        // `Int.MaxValue` and friends — the numeric companion objects' bounds.
        // These are constants, not receiver dispatch, so they fold to a literal.
        if args.is_empty() {
            if let Expr::Var(owner) = recv {
                if let Some(v) = companion_bound(owner, name) {
                    self.b.emit(Op::LoadInt(v), line);
                    return Ok(());
                }
            }
        }
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
        // `String.format(fmt, args…)` — the JDK static, which is a namespace
        // access rather than a receiver method. It is `fmt.format(args…)`, so it
        // lowers to exactly that and shares the one formatter implementation.
        if name == "format" && matches!(recv, Expr::Var(n) if n == "String") && !args.is_empty() {
            let (fmt, rest) = args.split_first().expect("non-empty");
            return self.method(fmt, "format", rest, line);
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
            Self::reject_char_range(recv, &args[0])?;
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
        // The class's own fields are in scope by bare name for the whole
        // constructor, so a later field's initializer (`val d = n * 2`) is typed
        // by the parameter it reads.
        let fw = self.class_field_widths(&cd.name);
        let saved_widths = std::mem::replace(&mut self.widths, fw);
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
        self.widths = saved_widths;
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
        // A bare field reference inside a method resolves to `this.field`, so the
        // field widths are the method body's starting widths — that is what makes
        // `def twice: Int = n * 2` wrap when `n: Int`.
        let fw = self.class_field_widths(&cd.name);
        let saved_widths = std::mem::replace(&mut self.widths, fw);
        self.vals.insert("this".to_string(), true);
        for (i, p) in m.params.iter().enumerate() {
            slots.insert(p.clone(), (i + 1) as u16);
            self.vals.insert(p.clone(), true);
            // A parameter shadows a field of the same name, so an unannotated one
            // must clear the field's width rather than inherit it.
            match m.sig.get(i).and_then(|s| s.ty.as_deref()) {
                Some(t) => {
                    let w = self.binding_width(Some(t), None);
                    self.widths.insert(p.clone(), w);
                }
                None => {
                    self.widths.remove(p);
                }
            }
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
        self.emit_return_unit(0);
        self.current_class = saved_class;

        self.scope = None;
        self.vals = saved_vals;
        self.widths = saved_widths;
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
        let saved_widths = std::mem::take(&mut self.widths);
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
        self.emit_return_unit(0);
        self.current_object = saved_obj;

        self.scope = None;
        self.vals = saved_vals;
        self.widths = saved_widths;
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
            let args = self.adapt_args(name, args, line)?;
            for a in &args {
                self.expr(a)?;
            }
            let nidx = self.b.add_name(name);
            self.b.emit(Op::Call(nidx, args.len() as u8), line);
            return Ok(());
        }
        // A call on a bound name that is not a `def` is an `apply`: the value is a
        // function (`f(x)`), a `List`/`Tuple` (`xs(i)` indexing), or a `Map`
        // (`m(k)` lookup). Load the value, push the args, and dispatch via APPLY.
        // `vals` covers the body being compiled and `scope.slots` its frame
        // (parameters, locals, and the captures a closure was given a slot for);
        // `global_binds` covers a TOP-LEVEL binding read from inside a nested
        // body, which is neither.
        let is_bound = self.vals.contains_key(name)
            || self
                .scope
                .as_ref()
                .is_some_and(|s| s.slots.contains_key(name))
            || self.global_binds.contains(name);
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
    /// A `Char`-endpoint range (`'a' to 'e'`) is Scala's `NumericRange[Char]`,
    /// which this frontend does not build: the endpoints would be read as
    /// integers and the range would come out silently wrong (`List(0)`, size 1)
    /// rather than as the characters. Reject it, per the rule that an
    /// unsupported construct is an error and never a mis-run.
    fn reject_char_range(start: &Expr, end: &Expr) -> Result<(), String> {
        if matches!(start, Expr::Char(_)) || matches!(end, Expr::Char(_)) {
            return Err(
                "scalars: a Char range (`'a' to 'z'`) is not modeled — its NumericRange[Char] \
                 has no representation here"
                    .to_string(),
            );
        }
        Ok(())
    }

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
        self.global_binds.insert(name.to_string());
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
        let saved_widths = std::mem::take(&mut self.widths);
        for (i, p) in f.params.iter().enumerate() {
            slots.insert(p.clone(), i as u16);
            // Scala method parameters are `val`s — reassigning one is an error.
            self.vals.insert(p.clone(), true);
            // Scala requires a type on every `def` parameter, so this is the one
            // place inside a function body where numeric widths are always known.
            if let Some(t) = f.sig.get(i).and_then(|s| s.ty.as_deref()) {
                self.widths
                    .insert(p.clone(), self.binding_width(Some(t), None));
            }
        }
        self.scope = Some(Scope {
            slots,
            next_slot: f.params.len() as u16,
            boxed: boxed_vars(&f.body),
        });
        // This body's by-name parameters hold thunks; every read forces one.
        let saved_by_name = std::mem::replace(
            &mut self.by_name,
            f.params
                .iter()
                .zip(&f.sig)
                .filter(|(_, s)| s.by_name)
                .map(|(p, _)| p.clone())
                .collect(),
        );

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
        self.emit_return_unit(0);

        self.scope = None;
        self.vals = saved_vals;
        self.widths = saved_widths;
        self.by_name = saved_by_name;
        Ok(())
    }

    /// Compile a statement list in tail position: every leading statement is a
    /// side effect, and the final one's value becomes the function result. An
    /// `if`/`else` in tail position returns from each branch (so `def f = if (c)
    /// a else b` works, including the canonical recursive `fact`).
    fn tail(&mut self, stmts: &[Stmt]) -> Result<(), String> {
        let Some((last, init)) = stmts.split_last() else {
            self.emit_return_unit(0);
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
                    self.emit_return_unit(0);
                } else {
                    self.tail(els)?;
                }
                Ok(())
            }
            // A loop / other statement in tail position has no value: run it for
            // its effect, then return `Unit`.
            _ => {
                self.stmt(s)?;
                self.emit_return_unit(0);
                Ok(())
            }
        }
    }

    /// Everything the width analysis can prove about a new binding, from its
    /// declared type when it has one and from its initializer otherwise — which
    /// is how Scala itself types a bare `val`. A declared type wins: `val n:
    /// Long = 1` is a `Long`, not the `Int` the initializer alone would suggest.
    fn binding_width(&self, ty: Option<&str>, init: Option<&Expr>) -> Width {
        let (num, elem) = match ty {
            Some(t) => (declared_width(t), declared_element_width(t)),
            None => (
                init.map_or(NumTy::Unknown, |e| self.num_ty(e)),
                init.map_or(NumTy::Unknown, |e| self.elem_ty(e)),
            ),
        };
        Width {
            num,
            elem,
            cls: init.and_then(|e| self.class_of(e)),
        }
    }

    /// The user-declared class of `e`, when it can be recovered. This is what
    /// lets a field or method access on a value be typed: `val b = new Box(…)`
    /// records `Box`, so `b.n` can read `Box`'s declared field width.
    fn class_of(&self, e: &Expr) -> Option<String> {
        match e {
            Expr::New { name, .. } => self.classes.contains_key(name).then(|| name.clone()),
            // A `case class` is also constructed through its companion `apply`,
            // written without `new`.
            Expr::Call { name, .. } => self
                .classes
                .get(name)
                .filter(|m| m.is_case)
                .map(|_| name.clone()),
            Expr::Var(n) => self.widths.get(n).and_then(|w| w.cls.clone()),
            _ => None,
        }
    }

    /// The width of the ELEMENTS of a collection-valued expression — the width
    /// Scala reads off `List[Int]` and this frontend has to recover from the
    /// syntax. It types a lambda parameter traversing the collection, and it is
    /// the result width of `sum`/`product`/`max`/`min`.
    fn elem_ty(&self, e: &Expr) -> NumTy {
        match e {
            Expr::Var(n) => self.widths.get(n).map_or(NumTy::Unknown, |w| w.elem),
            // A collection literal is typed by its elements: `List(1, 2)` is a
            // `List[Int]`, `List(1L, 2L)` a `List[Long]`, and a mixed or
            // non-integer one answers `Unknown` through `combine`. An EMPTY
            // literal has no element type here (`Nil`, `List()`), which is the
            // conservative answer.
            Expr::Collection { ctor, elems } if SEQ_CTORS.contains(&ctor.as_str()) => {
                combine_all(elems.iter().map(|x| self.num_ty(x)))
            }
            Expr::Method {
                recv, name, args, ..
            } => self.method_elem_ty(recv, name, args),
            _ => NumTy::Unknown,
        }
    }

    /// The element width of a collection-valued method result.
    fn method_elem_ty(&self, recv: &Expr, name: &str, args: &[Expr]) -> NumTy {
        // `a to b` / `a until b` — a `Range` of the endpoints' width. Scala's
        // `1 to 3` is a `Range` (an `Int` one); `1L to 3L` is a
        // `NumericRange[Long]`, which does not wrap.
        if args.len() == 1 && matches!(name, "to" | "until") {
            return self.num_ty(recv).combine(self.num_ty(&args[0]));
        }
        // The combinators that return a collection of the SAME element type, so
        // `xs.filter(…).sum` is still typed by `xs`. Not `map`: its element type
        // is the width of the lambda BODY, which would have to be analysed with
        // the parameter bound to a width the compiler is not holding yet (see
        // BUGS.md).
        if args.is_empty() && ELEM_PRESERVING_NILADIC.contains(&name)
            || args.len() == 1 && ELEM_PRESERVING_MONADIC.contains(&name)
        {
            return self.elem_ty(recv);
        }
        NumTy::Unknown
    }

    /// The statically known numeric width of `e` — see [`NumTy`]. Everything
    /// this cannot prove answers [`NumTy::Unknown`], which suppresses narrowing.
    fn num_ty(&self, e: &Expr) -> NumTy {
        match e {
            Expr::Int(_) => NumTy::Int,
            Expr::Long(_) => NumTy::Long,
            // A `Char` widens to `Int` the moment it enters arithmetic: `'a' + 1`
            // is the `Int` 98, and `Char` itself has no arithmetic of its own.
            Expr::Char(_) => NumTy::Int,
            Expr::Var(n) => self.widths.get(n).map_or(NumTy::Unknown, |w| w.num),
            // `-x` and `~x` keep their operand's width.
            Expr::Unary {
                op: UnOp::Neg | UnOp::Complement,
                rhs,
            } => self.num_ty(rhs),
            Expr::Binary {
                op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod,
                lhs,
                rhs,
            } => self.num_ty(lhs).combine(self.num_ty(rhs)),
            Expr::Method {
                recv, name, args, ..
            } => self.method_width(recv, name, args),
            // A call to a user `def` with a declared return type. This is the
            // only width a call site can know: the body is compiled separately
            // and its result is whatever the annotation promised.
            Expr::Call { name, .. } => {
                self.def_widths.get(name).copied().unwrap_or(NumTy::Unknown)
            }
            _ => NumTy::Unknown,
        }
    }

    /// The widths a class's own body sees by bare name: every field of the class,
    /// own and inherited. A constructor body and every method body start from
    /// these, because a bare field reference in either resolves to `this.field`.
    fn class_field_widths(&self, cls: &str) -> HashMap<String, Width> {
        let Some(meta) = self.classes.get(cls) else {
            return HashMap::new();
        };
        meta.field_names
            .iter()
            .filter_map(|f| {
                let w = *meta.member_widths.get(f)?;
                Some((f.clone(), Width::num(w)))
            })
            .collect()
    }

    /// The declared width of `cls`'s member `name`, or `None` when `cls` has no
    /// such member — in which case the call is a stdlib one on the instance and
    /// the name-based rules still apply.
    fn member_width(&self, cls: &str, name: &str) -> Option<NumTy> {
        self.classes.get(cls)?.member_widths.get(name).copied()
    }

    /// Whether `e` could evaluate to an instance of a user-declared class.
    ///
    /// Answers `false` only where the receiver is something else for certain — a
    /// literal of a built-in kind, or an expression whose numeric width is
    /// already proven. Everything else is `true`, because a wrong `false` here
    /// would let a stdlib rule outrank a user declaration, which is the bug
    /// `member_width` exists to prevent.
    fn could_be_user_instance(&self, e: &Expr) -> bool {
        match e {
            Expr::Int(_)
            | Expr::Long(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Char(_)
            | Expr::Bool(_)
            | Expr::Collection { .. }
            | Expr::Tuple(_)
            | Expr::Lambda { .. }
            | Expr::Format { .. } => false,
            Expr::Unary { rhs, .. } => self.could_be_user_instance(rhs),
            _ => self.num_ty(e) == NumTy::Unknown,
        }
    }

    /// Whether ANY user-declared class names `name` as a member. A receiver
    /// that could be an instance of that class cannot take a stdlib width.
    fn user_declares_member(&self, name: &str) -> bool {
        self.classes
            .values()
            .any(|m| m.member_widths.contains_key(name))
    }

    /// The width of a method call's result. Split three ways: names that are
    /// always `Int` (lengths, indices, comparisons), names that keep the
    /// receiver's width (`abs`, the shifts), and the operator spellings, which
    /// promote to the wider of the two operands.
    fn method_width(&self, recv: &Expr, name: &str, args: &[Expr]) -> NumTy {
        if args.is_empty() {
            if let Expr::Var(owner) = recv {
                if companion_bound(owner, name).is_some() {
                    return declared_width(owner);
                }
            }
        }
        // `math.abs(n)` / `math.max(a, b)` — the module is a namespace, not a
        // value, so the width comes from the ARGUMENTS. Scala keeps these at the
        // argument's own width (`math.abs(Int.MinValue)` is `Int.MinValue`).
        if math_module(recv).is_some() {
            return match (name, args.len()) {
                ("abs" | "signum", 1) => self.num_ty(&args[0]),
                ("max" | "min", 2) => self.num_ty(&args[0]).combine(self.num_ty(&args[1])),
                _ => NumTy::Unknown,
            };
        }
        if name == "toLong" {
            return NumTy::Long;
        }
        // A member of a USER-DECLARED class is typed by that class's own
        // declaration, and it takes priority over every name-based rule below.
        // Without this, `case class FileRec(name: String, size: Long)` had
        // `f.size` typed `Int` — because `size` is a stdlib `Int` method name —
        // and `f.size * 2` wrapped a `Long` field to 32 bits. The receiver's
        // width is not what was unproven there; the receiver was unproven and a
        // width was claimed anyway, which is the one thing `NumTy::Unknown`
        // exists to prevent.
        if let Some(w) = self
            .class_of(recv)
            .and_then(|cls| self.member_width(&cls, name))
        {
            return w;
        }
        // The same name declared by SOME user class, on a receiver that could BE
        // an instance of it: the call may land there, so no stdlib rule can be
        // claimed. A receiver already known to be a primitive or a collection is
        // exempt — otherwise one `class Thing { def abs: Int }` anywhere in the
        // program would stop `(-2147483648).abs` from wrapping.
        // The name test comes first on purpose: it is a hash lookup per class and
        // does not depend on the receiver, while the receiver test walks an
        // expression. Most programs declare no member named like a stdlib one, so
        // this short-circuits before anything is walked.
        if self.user_declares_member(name) && self.could_be_user_instance(recv) {
            return NumTy::Unknown;
        }
        // `xs.sum` / `xs.product` — the ELEMENT type, which is the width Scala
        // reads off `List[Int]` and the reason `(1 to 100000).sum` answers
        // 705082704 rather than 5000050000. `max`/`min` are the same rule; they
        // cannot overflow, so they type an enclosing expression without being
        // narrowed themselves.
        if args.is_empty() && ELEM_RESULT_METHODS.contains(&name) {
            return self.elem_ty(recv);
        }
        if INT_RESULT_METHODS.contains(&name) {
            return NumTy::Int;
        }
        if WIDTH_PRESERVING_METHODS.contains(&name) {
            return self.num_ty(recv);
        }
        if args.len() == 1 && WIDTH_COMBINING_METHODS.contains(&name) {
            return self.num_ty(recv).combine(self.num_ty(&args[0]));
        }
        NumTy::Unknown
    }

    /// Return `Unit` from the current frame.
    ///
    /// `Unit` and `null` are different values that print differently — `()` and
    /// `null` — so they cannot share fusevm's `Undef`, which is what a bare
    /// `Op::Return` leaves behind. That is why `println(f())` on a `Unit`-valued
    /// `f` used to render `null`. The unit LITERAL already lowers to an empty
    /// tuple, and it is structural (`() == ()` holds, `toString` is `()`), so
    /// returning the same value makes every `Unit` in the language one thing
    /// rather than adding a second representation.
    fn emit_return_unit(&mut self, line: u32) {
        self.emit_unit(line);
        self.b.emit(Op::ReturnValue, line);
    }

    /// Push the `Unit` value. Kept distinct from `Op::LoadUndef`, which stays
    /// the representation of `null` and of a genuinely absent value.
    fn emit_unit(&mut self, line: u32) {
        self.b
            .emit(Op::CallBuiltin(crate::host::MAKE_TUPLE, 0), line);
    }

    /// Wrap the integer on top of the stack back to 32 bits — Scala's `Int`
    /// overflow. `(n << 32) >> 32` sign-extends the low half, so 2147483648
    /// becomes -2147483648, which is what `2147483647 + 1` answers.
    ///
    /// Emitted as a shift pair rather than a host builtin on purpose: fusevm's
    /// JIT and AOT tiers compile `Shl`/`Shr` (to `ishl`/`sshr`) but REFUSE to
    /// trace through `CallBuiltin`, so a builtin here would silently stop every
    /// arithmetic loop in the program from ever being compiled. This is the same
    /// recipe the Java frontend uses for the identical rule.
    fn emit_wrap32(&mut self, line: u32) {
        self.b.emit(Op::LoadInt(32), line);
        self.b.emit(Op::Shl, line);
        self.b.emit(Op::LoadInt(32), line);
        self.b.emit(Op::Shr, line);
    }

    /// Wrap only when the result is provably a 32-bit `Int`. A `Long` result
    /// must keep its full width, and an unproven one might be a `Double` or a
    /// `String`, which the shift pair would destroy — so both are left alone.
    fn narrow(&mut self, w: NumTy, line: u32) {
        if w == NumTy::Int {
            self.emit_wrap32(line);
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
        // The result's width, decided before anything is emitted. `Int` operands
        // make an `Int` result, which wraps at 32 bits; a `Long` anywhere in the
        // expression promotes it and suppresses the wrap.
        // A comparison answers `Boolean` and `::` a `List`; neither narrows.
        let w = if matches!(
            op,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
        ) {
            self.num_ty(lhs).combine(self.num_ty(rhs))
        } else {
            NumTy::Unknown
        };
        self.expr(lhs)?;
        self.expr(rhs)?;
        // Scala `/` truncates for two `Int`s; fusevm's native `Op::Div` always
        // floats, so route division through the type-dispatching host builtin.
        if let BinOp::Div = op {
            self.b.emit(Op::CallBuiltin(crate::host::SDIV, 2), 0);
            // `Int.MinValue / -1` overflows to itself — the one division that
            // does not fit back into 32 bits.
            self.narrow(w, 0);
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
        self.narrow(w, 0);
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
        Expr::NamedArg { value, .. } => expr_any(value, pred),
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
        | Expr::Long(_)
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

/// The `scala.math.Ordering` companion members this frontend answers — the
/// per-type instances. All of them build the same natural ordering, because
/// `host::value_cmp` already orders each of these types the way Scala's instance
/// does; the member only picks the element TYPE, which is erased here.
const ORDERING_MEMBERS: &[&str] = &[
    "Int", "Long", "Short", "Byte", "Double", "Float", "String", "Char", "Boolean", "Unit",
    "BigInt", "BigDecimal",
];

/// The bound named by a numeric companion object — `Int.MaxValue`,
/// `Long.MinValue`, and the sub-`Int` widths Scala also exposes. Returns `None`
/// for anything else, which leaves the access on the ordinary dispatch path.
fn companion_bound(owner: &str, member: &str) -> Option<i64> {
    let max = match owner {
        "Int" => i64::from(i32::MAX),
        "Long" => i64::MAX,
        "Short" => i64::from(i16::MAX),
        "Byte" => i64::from(i8::MAX),
        _ => return None,
    };
    let min = match owner {
        "Int" => i64::from(i32::MIN),
        "Long" => i64::MIN,
        "Short" => i64::from(i16::MIN),
        "Byte" => i64::from(i8::MIN),
        _ => return None,
    };
    match member {
        "MaxValue" => Some(max),
        "MinValue" => Some(min),
        _ => None,
    }
}

/// The static WIDTH of a numeric expression.
///
/// Scala's `Int` is 32 bits and its `Long` is 64, and the two are the same
/// runtime representation here (an `i64`), so the only way to tell them apart is
/// statically. That is what this answers, and it is what decides whether an
/// arithmetic result gets wrapped back to 32 bits.
///
/// The default is deliberately [`NumTy::Unknown`], not [`NumTy::Int`]: wrapping
/// is emitted as a shift pair, and a shift applied to a `Double` or a `String`
/// would destroy it. So a width is claimed only where it is PROVEN, and an
/// unproven expression keeps the 64-bit behaviour this frontend had before.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum NumTy {
    /// Provably a 32-bit `Int`: arithmetic on it wraps.
    Int,
    /// Provably a 64-bit `Long`: arithmetic on it does not wrap.
    Long,
    /// A `Double`, a `String`, a collection, or a value whose type this frontend
    /// cannot recover. Never narrowed.
    #[default]
    Unknown,
}

/// Everything the width analysis knows about one binding.
///
/// Scala reads a width off the static type, and most of the positions this
/// frontend used to miss are ones where the width does not appear on the binding
/// at all: it appears on the binding's ELEMENT (`xs: List[Int]`, which is what
/// types `x` in `xs.map(x => …)` and what `xs.sum` answers) or on its CLASS
/// (`c: Box`, which is what types `c.n`). Carrying all three together means the
/// existing save/restore of [`Compiler::widths`] on entry to every function,
/// method, constructor and lambda body keeps working untouched.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Width {
    /// The binding's own numeric width.
    num: NumTy,
    /// For a collection, the width of its ELEMENTS; [`NumTy::Unknown`] otherwise.
    elem: NumTy,
    /// For an instance of a user-declared class, that class's name, which is how
    /// a field or method access on it recovers a width.
    cls: Option<String>,
}

impl Width {
    /// A binding whose only known property is its own width.
    fn num(n: NumTy) -> Width {
        Width {
            num: n,
            ..Width::default()
        }
    }
}

impl NumTy {
    /// The width of a binary numeric result, under Scala's promotion rule: two
    /// `Int`s stay `Int`, and a `Long` on either side promotes the whole
    /// expression to `Long` (so `2147483647 + 1L` is 2147483648, not a wrap).
    /// Anything unproven poisons the result, because a `Double` or `String`
    /// operand means this was never integer arithmetic at all.
    fn combine(self, other: NumTy) -> NumTy {
        match (self, other) {
            (NumTy::Int, NumTy::Int) => NumTy::Int,
            (NumTy::Long, NumTy::Int | NumTy::Long) | (NumTy::Int, NumTy::Long) => NumTy::Long,
            _ => NumTy::Unknown,
        }
    }
}

/// Fold a sequence of operand widths under [`NumTy::combine`]. An EMPTY
/// sequence answers [`NumTy::Unknown`]: `List()` and `Nil` name no element type,
/// and claiming one would narrow whatever was later put in them.
fn combine_all(mut widths: impl Iterator<Item = NumTy>) -> NumTy {
    match widths.next() {
        Some(first) => widths.fold(first, NumTy::combine),
        None => NumTy::Unknown,
    }
}

/// The collection literals whose elements are values (as opposed to `Map`'s
/// key/value pairs, which have two widths and no single element type).
const SEQ_CTORS: &[&str] = &[
    "List",
    "Seq",
    "Vector",
    "Array",
    "IndexedSeq",
    "Set",
    "HashSet",
    "ListBuffer",
    "ArrayBuffer",
    "Buffer",
    "Iterable",
];

/// Niladic methods whose result is one ELEMENT of the receiver, so they take the
/// receiver's element width. Only `sum` and `product` can leave the 32-bit range
/// (see [`NARROW_AFTER_METHODS`]); the rest are here so they can type an
/// enclosing expression.
const ELEM_RESULT_METHODS: &[&str] = &[
    "sum", "product", "max", "min", "head", "last", "headOption",
];

/// The width of an expression that carries its type on its face, with no
/// bindings consulted. Used where a width is needed before any scope exists —
/// class field initializers, indexed once at registration.
fn literal_width(e: &Expr) -> NumTy {
    match e {
        Expr::Int(_) | Expr::Char(_) => NumTy::Int,
        Expr::Long(_) => NumTy::Long,
        Expr::Unary {
            op: UnOp::Neg | UnOp::Complement,
            rhs,
        } => literal_width(rhs),
        Expr::Binary {
            op: BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod,
            lhs,
            rhs,
        } => literal_width(lhs).combine(literal_width(rhs)),
        _ => NumTy::Unknown,
    }
}

/// Methods that call a one-parameter lambda once per ELEMENT of the receiver,
/// so the parameter takes the receiver's element width.
const ELEMENT_TRAVERSALS: &[&str] = &[
    "map",
    "flatMap",
    "filter",
    "filterNot",
    "withFilter",
    "foreach",
    "exists",
    "forall",
    "count",
    "find",
    "takeWhile",
    "dropWhile",
    "sortBy",
    "maxBy",
    "minBy",
    "groupBy",
    "partition",
    "span",
    "indexWhere",
    "lastIndexWhere",
    "segmentLength",
];

/// Niladic combinators that answer a collection with the SAME element type.
const ELEM_PRESERVING_NILADIC: &[&str] = &[
    "reverse",
    "sorted",
    "distinct",
    "tail",
    "init",
    "toList",
    "toVector",
    "toSeq",
    "toArray",
    "toSet",
    "toBuffer",
    "toIndexedSeq",
    "flatten",
];

/// One-argument combinators that answer a collection with the SAME element type.
/// `map` is deliberately absent — see [`Compiler::method_elem_ty`].
const ELEM_PRESERVING_MONADIC: &[&str] = &[
    "filter",
    "filterNot",
    "withFilter",
    "take",
    "drop",
    "takeWhile",
    "dropWhile",
    "takeRight",
    "dropRight",
    "sortBy",
    "sortWith",
    "diff",
    "intersect",
];

/// The element width a declared collection type names — the `Int` of
/// `List[Int]`. Answers [`NumTy::Unknown`] for a type with no argument clause,
/// for a `Map` (two widths, no single element), and for a nested collection.
fn declared_element_width(ty: &str) -> NumTy {
    let Some(open) = ty.find('[') else {
        return NumTy::Unknown;
    };
    if !SEQ_CTORS.contains(&ty[..open].trim_start_matches("=>").trim()) {
        return NumTy::Unknown;
    }
    let inner = ty[open + 1..].trim_end_matches(']');
    // A comma means several type arguments and a `[` a nested collection;
    // neither names one element width.
    if inner.contains(',') || inner.contains('[') {
        return NumTy::Unknown;
    }
    declared_width(inner)
}

/// The width a declared type name denotes, for `val x: Long = …` and a `def`
/// parameter's mandatory annotation.
fn declared_width(ty: &str) -> NumTy {
    // A by-name parameter is spelled `=> Int`; the arrow is part of the string.
    match ty.trim_start_matches("=>").trim() {
        "Int" | "Short" | "Byte" | "Integer" => NumTy::Int,
        "Long" => NumTy::Long,
        _ => NumTy::Unknown,
    }
}

/// Method names whose result is an `Int` REGARDLESS of the receiver's width —
/// lengths, indices, counts and comparisons. Scala types every one of these as
/// `Int`, so an arithmetic expression built from them wraps at 32 bits.
const INT_RESULT_METHODS: &[&str] = &[
    "length",
    "size",
    "knownSize",
    "toInt",
    "indexOf",
    "lastIndexOf",
    "indexWhere",
    "lastIndexWhere",
    "segmentLength",
    "count",
    "hashCode",
    "compareTo",
    "compare",
    "compareToIgnoreCase",
    "asDigit",
    "productArity",
    "groupCount",
];

/// Method names that PRESERVE the receiver's width — `Int.abs` is an `Int`,
/// `Long.abs` is a `Long`. The bitwise and shift operators belong here too: they
/// are already evaluated at the receiver's width by the host.
const WIDTH_PRESERVING_METHODS: &[&str] = &["abs", "unary_~", "<<", ">>", ">>>"];

/// Method names whose result takes the WIDER of the receiver and the argument —
/// the numeric operators in their method spelling (`n.+(1)`, `a max b`).
const WIDTH_COMBINING_METHODS: &[&str] =
    &["+", "-", "*", "/", "%", "max", "min", "&", "|", "^"];

/// Method names whose result must be wrapped back to 32 bits when it is an
/// `Int`. Deliberately NOT every `Int`-typed method: a `length` or an `indexOf`
/// cannot leave the range, and wrapping it would cost four ops on every call for
/// no change in answer. These are the ones that genuinely overflow — the
/// arithmetic operators in method spelling, `abs` (at `Int.MinValue`), `toInt`,
/// which is Scala's explicit narrowing conversion, and the two folds over a
/// collection that accumulate — `sum` and `product` overflow on a long enough
/// `List[Int]` however small its elements are.
const NARROW_AFTER_METHODS: &[&str] = &[
    "toInt", "abs", "+", "-", "*", "/", "%", "sum", "product",
];

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
            Expr::NamedArg { value, .. } => self.expr(value, in_lambda),
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
            | Expr::Long(_)
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
        Expr::NamedArg { value, .. } => fv_expr(value, bound, out, seen),
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
        | Expr::Long(_)
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
