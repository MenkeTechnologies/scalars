//! Lexical scoping for block-local `def`s: unique renaming + lambda lifting.
//!
//! The compiler consumes a *flat* function namespace ([`Program::functions`]) —
//! every `def` is a global subroutine reached by name. Scala, however, scopes a
//! `def` to the block that declares it, so two blocks may each declare `def f`
//! and an inner `f` must shadow an outer one. Hoisting them all into one table
//! keyed by the source name silently drops every definition but the first, which
//! is a wrong-answer bug rather than a diagnostic.
//!
//! This pass sits between the parser and the compiler and closes that gap in two
//! steps:
//!
//! 1. **Unique renaming.** The AST is walked with a scope stack. Each block
//!    pre-binds its own [`StmtKind::DefDecl`]s (so mutually recursive local
//!    `def`s and forward references inside one block resolve, as Scala's block
//!    scoping requires), and every `Var`/`Call` reference is rewritten to the
//!    globally unique name of whichever `def` it actually resolves to. A `val`
//!    or parameter shadows an outer `def` of the same name from its declaration
//!    onward. The first claimant of a name keeps it verbatim, so a program with
//!    no collisions compiles to exactly the bytecode it did before.
//!
//! 2. **Lambda lifting.** A hoisted `def` loses access to the enclosing call
//!    frame, so any enclosing local it reads becomes an extra trailing
//!    parameter and every call site passes it. Capture sets propagate through
//!    calls to a fixpoint (a local `def` that calls a sibling capturing `k`
//!    must itself thread `k`). Captured values are read at each call, which
//!    matches Scala for the `val`s and parameters that make up the common case;
//!    *assigning* to a captured binding is not expressible this way and is
//!    rejected with a diagnostic rather than silently lost.
//!
//! Class/object member `def`s are untouched: they already have their own
//! `Class$method` namespace and an explicit `this`.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

/// Resolve block-local `def`s in `prog`: rename to unique global names, lift
/// captures into parameters, and hoist into [`Program::functions`].
pub fn resolve(prog: &mut Program) -> Result<(), String> {
    let mut r = Resolver::new(prog);
    r.walk_program(prog)?;
    r.fixpoint()?;
    let (lifted, taken) = r.finish();
    if lifted.is_empty() {
        return Ok(());
    }
    // Final naming happens only now: a lifted `def` may keep its source name
    // (leaving a collision-free program's bytecode untouched) only once every
    // value binding in the program is known, because the compiler resolves a
    // bare name against one flat table of `def`s *and* globals.
    let mut taken = taken;
    let mut sigs: HashMap<String, Sig> = HashMap::new();
    let mut funcs = Vec::with_capacity(lifted.len());
    for l in lifted {
        let name = unique_name(&l.base, &mut taken);
        sigs.insert(
            l.tag.clone(),
            Sig {
                name: name.clone(),
                params: l.params.clone(),
                captures: l.captures.clone(),
            },
        );
        funcs.push(l.into_func(name));
    }
    prog.functions.extend(funcs);
    // Call sites learn their final callee name and extra capture arguments only
    // here, so the rewrite is a second walk over the whole tree.
    pass_calls(prog, &sigs);
    Ok(())
}

/// `base`, or `base$1`, `base$2`, … — the first spelling not already `taken`.
/// `$` is the Scala spec's reserved character for synthesized names.
fn unique_name(base: &str, taken: &mut HashSet<String>) -> String {
    if taken.insert(base.to_string()) {
        return base.to_string();
    }
    for n in 1u32.. {
        let cand = format!("{base}${n}");
        if taken.insert(cand.clone()) {
            return cand;
        }
    }
    unreachable!("u32 exhausted while uniquing a def name")
}

/// What a name resolves to in the scope stack.
enum Binding {
    /// A `val`/`var`/parameter/pattern/generator binding.
    Value,
    /// A `def`, under its globally unique name.
    Fun(String),
}

struct Scope {
    /// The call frame this scope belongs to. Frame 0 is the top level, where
    /// bindings are program globals and therefore never captured.
    frame: u32,
    names: HashMap<String, Binding>,
}

/// One hoisted block-local `def`.
struct Lifted {
    /// A placeholder that stands in for this `def` while the tree is walked;
    /// unique by construction, so references can be rewritten by name before the
    /// final (possibly unmangled) name is known.
    tag: String,
    /// The name written in the source.
    base: String,
    /// Declared parameters (captures are appended by [`Lifted::into_func`]).
    params: Vec<String>,
    /// The declared parameters' [`ParamSig`]s, carried through the lift so a
    /// nested `def` keeps its defaults / varargs / by-name parameters.
    sig: Vec<ParamSig>,
    /// The declared return type, carried through the lift so a hoisted nested
    /// `def` keeps the annotation its call sites' width analysis reads.
    ret_ty: Option<String>,
    body: Vec<Stmt>,
    /// Enclosing-frame locals the body reads, in discovery order.
    captures: Vec<String>,
    /// Index into [`Resolver::scopes`] of this `def`'s frame-root scope while it
    /// is being walked — the boundary that decides whether a name is a capture.
    scope_base: usize,
    /// Names the body assigns to. Checked against `captures` after the fixpoint:
    /// a by-value capture cannot carry a write back out.
    assigns: HashSet<String>,
}

impl Lifted {
    fn into_func(mut self, name: String) -> Func {
        // The captures become trailing parameters. They are ordinary by-value
        // parameters with no default, and `Func::captured` records how many so a
        // call site can tell them apart from the ones the user wrote.
        let captured = self.captures.len();
        self.params.extend(self.captures);
        self.sig.resize(self.params.len(), ParamSig::default());
        Func {
            name,
            params: self.params,
            sig: self.sig,
            ret_ty: self.ret_ty,
            captured,
            body: self.body,
            is_abstract: false,
        }
    }
}

/// A call recorded during the walk, replayed by the capture fixpoint.
struct Edge {
    /// The lifted `def` containing the call, or `None` for a call from `main` /
    /// a top-level `def` / a class member (frames that are not themselves
    /// lifted, so they can pass any capture directly).
    caller: Option<usize>,
    callee: usize,
    /// Names bound in the caller's own frame at the call site — the captures it
    /// can supply without capturing them itself.
    visible: HashSet<String>,
}

/// The final shape of a lifted `def`, for the call-site rewrite.
struct Sig {
    name: String,
    params: Vec<String>,
    captures: Vec<String>,
}

struct Resolver {
    scopes: Vec<Scope>,
    /// Scope index where each open frame begins; `frames[0]` is the top level.
    frames: Vec<usize>,
    /// Names a lifted `def` may not take: existing `def`s, class/object names,
    /// and (accumulated during the walk) every value binding, since the compiler
    /// resolves a bare name against one flat table covering all of them.
    taken: HashSet<String>,
    lifted: Vec<Lifted>,
    /// Placeholder tag → index into `lifted`.
    lifted_idx: HashMap<String, usize>,
    /// Lifted `def`s currently being walked, outermost first.
    lift_stack: Vec<usize>,
    edges: Vec<Edge>,
    next_frame: u32,
}

impl Resolver {
    fn new(prog: &Program) -> Self {
        let mut taken: HashSet<String> = prog.functions.iter().map(|f| f.name.clone()).collect();
        // A lifted `def` must not collide with a class/object name either: the
        // compiler resolves `Foo(...)` as a companion `apply` before a call.
        taken.extend(prog.classes.iter().map(|c| c.name.clone()));
        taken.extend(
            prog.classes
                .iter()
                .flat_map(|c| c.field_names.iter().cloned()),
        );
        taken.extend(prog.objects.iter().map(|o| o.name.clone()));
        let global = Scope {
            frame: 0,
            names: prog
                .functions
                .iter()
                .map(|f| (f.name.clone(), Binding::Fun(f.name.clone())))
                .collect(),
        };
        Resolver {
            scopes: vec![global],
            frames: vec![0],
            taken,
            lifted: Vec::new(),
            lifted_idx: HashMap::new(),
            lift_stack: Vec::new(),
            edges: Vec::new(),
            next_frame: 0,
        }
    }

    fn finish(self) -> (Vec<Lifted>, HashSet<String>) {
        (self.lifted, self.taken)
    }

    // ── scope plumbing ──────────────────────────────────────────────────────

    fn push_scope(&mut self) {
        let frame = self.scopes.last().map(|s| s.frame).unwrap_or(0);
        self.scopes.push(Scope {
            frame,
            names: HashMap::new(),
        });
    }

    /// Open a new call frame (a `def`/method/lambda body) whose root scope binds
    /// `names` as values.
    fn push_frame<I: IntoIterator<Item = String>>(&mut self, names: I) {
        self.next_frame += 1;
        let frame = self.next_frame;
        let names: HashMap<String, Binding> = names
            .into_iter()
            .map(|n| {
                self.taken.insert(n.clone());
                (n, Binding::Value)
            })
            .collect();
        self.frames.push(self.scopes.len());
        self.scopes.push(Scope { frame, names });
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn pop_frame(&mut self) {
        self.frames.pop();
        self.scopes.pop();
    }

    fn bind_value(&mut self, name: &str) {
        self.taken.insert(name.to_string());
        if let Some(s) = self.scopes.last_mut() {
            s.names.insert(name.to_string(), Binding::Value);
        }
    }

    /// Names bound as values in the innermost open frame — what a call site in
    /// that frame can hand to a callee without capturing anything itself.
    fn visible_in_frame(&self) -> HashSet<String> {
        let base = *self.frames.last().unwrap_or(&0);
        let mut out = HashSet::new();
        for s in &self.scopes[base..] {
            for (n, b) in &s.names {
                if matches!(b, Binding::Value) {
                    out.insert(n.clone());
                }
            }
        }
        out
    }

    /// Resolve `name`. A `def` hit yields its unique global name; a value hit
    /// records a capture on every lifted `def` whose frame the binding sits
    /// outside of.
    fn lookup(&mut self, name: &str) -> Option<String> {
        let mut found = None;
        for (i, s) in self.scopes.iter().enumerate().rev() {
            if let Some(b) = s.names.get(name) {
                found = Some((i, matches!(b, Binding::Fun(_))));
                break;
            }
        }
        let (idx, is_fun) = found?;
        if is_fun {
            return match self.scopes[idx].names.get(name) {
                Some(Binding::Fun(g)) => Some(g.clone()),
                _ => None,
            };
        }
        // A top-level (frame 0) binding is a program global — reachable from any
        // subroutine, so it is never threaded as a parameter.
        if self.scopes[idx].frame != 0 {
            for li in 0..self.lift_stack.len() {
                let l = self.lift_stack[li];
                if idx < self.lifted[l].scope_base
                    && !self.lifted[l].params.iter().any(|p| p == name)
                    && !self.lifted[l].captures.iter().any(|c| c == name)
                {
                    self.lifted[l].captures.push(name.to_string());
                }
            }
        }
        None
    }

    /// Record a call from the innermost lifted `def` (if any) to `callee`.
    fn note_call(&mut self, callee: &str) {
        if let Some(&ci) = self.lifted_idx.get(callee) {
            let caller = self.lift_stack.last().copied();
            let visible = self.visible_in_frame();
            self.edges.push(Edge {
                caller,
                callee: ci,
                visible,
            });
        }
    }

    /// A unique placeholder standing in for a block-local `def` until every
    /// value binding is known and a final name can be picked.
    fn tag(&self) -> String {
        format!("$lift{}", self.lifted.len())
    }

    // ── the walk ────────────────────────────────────────────────────────────

    fn walk_program(&mut self, prog: &mut Program) -> Result<(), String> {
        // `main` runs in the top-level frame, where every binding is a global.
        self.walk_block(&mut prog.main)?;
        for f in &mut prog.functions {
            let params = f.params.clone();
            self.push_frame(params);
            self.walk_block(&mut f.body)?;
            self.pop_frame();
        }
        for c in &mut prog.classes {
            // A class member sees `this` plus every instance field; the compiler
            // lowers a bare field name to `this.field`, so a lifted `def` that
            // captures one receives that read as its argument.
            self.push_frame(c.field_names.clone());
            self.walk_block(&mut c.body)?;
            self.pop_frame();
            for m in &mut c.methods {
                let names: Vec<String> = std::iter::once("this".to_string())
                    .chain(m.params.iter().cloned())
                    .chain(c.field_names.iter().cloned())
                    .collect();
                self.push_frame(names);
                self.walk_block(&mut m.body)?;
                self.pop_frame();
            }
        }
        for o in &mut prog.objects {
            self.walk_block(&mut o.body)?;
            for m in &mut o.methods {
                self.push_frame(m.params.clone());
                self.walk_block(&mut m.body)?;
                self.pop_frame();
            }
        }
        Ok(())
    }

    /// Walk a statement list as one lexical block: pre-bind its `def`s (so they
    /// see each other), walk each statement in order, then drop the `def`
    /// statements (they now live in the lifted table).
    fn walk_block(&mut self, stmts: &mut Vec<Stmt>) -> Result<(), String> {
        self.push_scope();
        let mut ids: HashMap<usize, usize> = HashMap::new();
        for (i, s) in stmts.iter().enumerate() {
            if let StmtKind::DefDecl(f) = &s.kind {
                let tag = self.tag();
                let li = self.lifted.len();
                self.lifted.push(Lifted {
                    tag: tag.clone(),
                    base: f.name.clone(),
                    params: f.params.clone(),
                    sig: f.sig.clone(),
                    ret_ty: f.ret_ty.clone(),
                    body: Vec::new(),
                    captures: Vec::new(),
                    scope_base: 0,
                    assigns: HashSet::new(),
                });
                self.lifted_idx.insert(tag.clone(), li);
                if let Some(sc) = self.scopes.last_mut() {
                    sc.names.insert(f.name.clone(), Binding::Fun(tag));
                }
                ids.insert(i, li);
            }
        }
        for (i, s) in stmts.iter_mut().enumerate() {
            match &mut s.kind {
                StmtKind::DefDecl(f) => {
                    let li = ids[&i];
                    let mut body = std::mem::take(&mut f.body);
                    let params = f.params.clone();
                    self.push_frame(params);
                    self.lifted[li].scope_base = self.scopes.len() - 1;
                    self.lift_stack.push(li);
                    let res = self.walk_block(&mut body);
                    self.lift_stack.pop();
                    self.pop_frame();
                    res?;
                    self.lifted[li].body = body;
                }
                _ => self.walk_stmt(s)?,
            }
        }
        stmts.retain(|s| !matches!(s.kind, StmtKind::DefDecl(_)));
        self.pop_scope();
        Ok(())
    }

    fn walk_stmt(&mut self, s: &mut Stmt) -> Result<(), String> {
        match &mut s.kind {
            StmtKind::DefDecl(_) => unreachable!("handled by walk_block"),
            StmtKind::Local { name, init, .. } => {
                if let Some(e) = init {
                    self.walk_expr(e)?;
                }
                // Bound only after its initializer, so `val x = x` reads the
                // outer `x` exactly as the compiler lowers it.
                let n = name.clone();
                self.bind_value(&n);
                Ok(())
            }
            // `val (a, b) = pair` — the initializer is walked first, then every
            // name the pattern binds enters the current scope.
            StmtKind::Destructure { pat, init } => {
                self.walk_expr(init)?;
                let mut names = HashSet::new();
                pattern_binds(pat, &mut names);
                for n in names {
                    self.bind_value(&n);
                }
                Ok(())
            }
            StmtKind::Assign { name, value, .. } => {
                self.walk_expr(value)?;
                let n = name.clone();
                self.lookup(&n);
                if let Some(&l) = self.lift_stack.last() {
                    self.lifted[l].assigns.insert(n);
                }
                Ok(())
            }
            // The target is mutated through a handle, not rebound, so unlike
            // `Assign` it records no `assigns` entry for the lifted `def`.
            StmtKind::PlaceAssign { place, value, .. } => {
                self.walk_expr(place)?;
                self.walk_expr(value)
            }
            StmtKind::Expr(e) => self.walk_expr(e),
            StmtKind::If { cond, then, els } => {
                self.walk_expr(cond)?;
                self.walk_block(then)?;
                self.walk_block(els)
            }
            StmtKind::While { cond, body } => {
                self.walk_expr(cond)?;
                self.walk_block(body)
            }
            StmtKind::Return(e) => match e {
                Some(e) => self.walk_expr(e),
                None => Ok(()),
            },
        }
    }

    fn walk_expr(&mut self, e: &mut Expr) -> Result<(), String> {
        match e {
            Expr::NamedArg { value, .. } => self.walk_expr(value),
            Expr::Var(name) => {
                if let Some(g) = self.lookup(name) {
                    self.note_call(&g);
                    *name = g;
                }
                Ok(())
            }
            Expr::Call { name, args, .. } => {
                for a in args.iter_mut() {
                    self.walk_expr(a)?;
                }
                if let Some(g) = self.lookup(name) {
                    self.note_call(&g);
                    *name = g;
                }
                Ok(())
            }
            Expr::Method { recv, args, .. } => {
                self.walk_expr(recv)?;
                for a in args.iter_mut() {
                    self.walk_expr(a)?;
                }
                Ok(())
            }
            Expr::New { args, .. } => {
                for a in args.iter_mut() {
                    self.walk_expr(a)?;
                }
                Ok(())
            }
            Expr::Copy { recv, updates, .. } => {
                self.walk_expr(recv)?;
                for (_, v) in updates.iter_mut() {
                    self.walk_expr(v)?;
                }
                Ok(())
            }
            Expr::Unary { rhs, .. } => self.walk_expr(rhs),
            Expr::Binary { lhs, rhs, .. } => {
                self.walk_expr(lhs)?;
                self.walk_expr(rhs)
            }
            Expr::Println { arg, .. } => match arg {
                Some(a) => self.walk_expr(a),
                None => Ok(()),
            },
            Expr::If { cond, then, els } => {
                self.walk_expr(cond)?;
                self.walk_expr(then)?;
                match els {
                    Some(x) => self.walk_expr(x),
                    None => Ok(()),
                }
            }
            Expr::Block(stmts) => self.walk_block(stmts),
            Expr::Match { scrut, arms } => {
                self.walk_expr(scrut)?;
                for arm in arms.iter_mut() {
                    self.push_scope();
                    let mut binds = HashSet::new();
                    pattern_binds(&arm.pat, &mut binds);
                    for b in binds {
                        self.bind_value(&b);
                    }
                    if let Some(g) = &mut arm.guard {
                        self.walk_expr(g)?;
                    }
                    self.walk_block(&mut arm.body)?;
                    self.pop_scope();
                }
                Ok(())
            }
            Expr::Try {
                body,
                catches,
                finalizer,
            } => {
                self.walk_block(body)?;
                for arm in catches.iter_mut() {
                    self.push_scope();
                    let mut binds = HashSet::new();
                    pattern_binds(&arm.pat, &mut binds);
                    for b in binds {
                        self.bind_value(&b);
                    }
                    if let Some(g) = &mut arm.guard {
                        self.walk_expr(g)?;
                    }
                    self.walk_block(&mut arm.body)?;
                    self.pop_scope();
                }
                if let Some(f) = finalizer {
                    self.walk_block(f)?;
                }
                Ok(())
            }
            Expr::Throw { value, .. } => self.walk_expr(value),
            Expr::Format { value, .. } => self.walk_expr(value),
            Expr::ForYield { enums, body } | Expr::ForEach { enums, body } => {
                self.push_scope();
                for en in enums.iter_mut() {
                    match en {
                        ForEnum::Gen {
                            name,
                            start,
                            end,
                            step,
                            ..
                        } => {
                            self.walk_expr(start)?;
                            self.walk_expr(end)?;
                            if let Some(s) = step {
                                self.walk_expr(s)?;
                            }
                            let n = name.clone();
                            self.bind_value(&n);
                        }
                        ForEnum::GenColl { pat, coll, .. } => {
                            self.walk_expr(coll)?;
                            let mut names = HashSet::new();
                            pattern_binds(pat, &mut names);
                            for n in names {
                                self.bind_value(&n);
                            }
                        }
                        ForEnum::Guard(g) => self.walk_expr(g)?,
                        ForEnum::Val { name, value } => {
                            self.walk_expr(value)?;
                            let n = name.clone();
                            self.bind_value(&n);
                        }
                    }
                }
                self.walk_expr(body)?;
                self.pop_scope();
                Ok(())
            }
            Expr::Lambda { params, body, .. } => {
                // A lambda body is its own frame: the compiler captures its free
                // enclosing locals as upvalues, so a `def` nested inside it that
                // reads one gets the value threaded through the lambda.
                self.push_frame(params.clone());
                self.walk_expr(body)?;
                self.pop_frame();
                Ok(())
            }
            Expr::Tuple(elems) | Expr::Collection { elems, .. } => {
                for el in elems.iter_mut() {
                    self.walk_expr(el)?;
                }
                Ok(())
            }
            Expr::Int(_)
            | Expr::Long(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Char(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Placeholder => Ok(()),
        }
    }

    // ── capture propagation ─────────────────────────────────────────────────

    /// Propagate capture sets across calls until stable: a local `def` that
    /// calls another must supply that callee's captures, and any it cannot see
    /// in its own frame it has to capture itself.
    fn fixpoint(&mut self) -> Result<(), String> {
        loop {
            let mut changed = false;
            for i in 0..self.edges.len() {
                let (caller, callee) = (self.edges[i].caller, self.edges[i].callee);
                let needed = self.lifted[callee].captures.clone();
                for c in needed {
                    if self.edges[i].visible.contains(&c) {
                        continue; // the caller's own frame supplies it
                    }
                    let Some(l) = caller else { continue };
                    if l == callee {
                        continue; // direct recursion: already a parameter
                    }
                    if self.lifted[l].params.contains(&c) || self.lifted[l].captures.contains(&c) {
                        continue;
                    }
                    self.lifted[l].captures.push(c);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        // A capture travels by value, one direction only. Writing to one inside
        // the lifted body would be lost, so reject it instead of losing it.
        for l in &self.lifted {
            for c in &l.captures {
                if l.assigns.contains(c) {
                    return Err(format!(
                        "scalars: local `def {}` assigns to `{c}` from the enclosing method \
                         (a captured binding is read-only here)",
                        l.base
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Add every name a pattern binds to `out`.
fn pattern_binds(p: &Pattern, out: &mut HashSet<String>) {
    match p {
        Pattern::Bind(n) => {
            out.insert(n.clone());
        }
        Pattern::Typed { name, .. } if name != "_" => {
            out.insert(name.clone());
        }
        Pattern::Constructor { elems, .. } | Pattern::Tuple(elems) | Pattern::Alt(elems) => {
            for e in elems {
                pattern_binds(e, out);
            }
        }
        Pattern::At { name, pat } => {
            out.insert(name.clone());
            pattern_binds(pat, out);
        }
        Pattern::Cons(h, t) => {
            pattern_binds(h, out);
            pattern_binds(t, out);
        }
        Pattern::Rest(Some(n)) => {
            out.insert(n.clone());
        }
        _ => {}
    }
}

// ── call-site rewrite ───────────────────────────────────────────────────────

/// Append each lifted `def`'s captures to every call of it, and turn a bare
/// reference into the equivalent call / eta-expansion.
fn pass_calls(prog: &mut Program, sigs: &HashMap<String, Sig>) {
    cs_block(&mut prog.main, sigs);
    for f in &mut prog.functions {
        cs_block(&mut f.body, sigs);
    }
    for c in &mut prog.classes {
        cs_block(&mut c.body, sigs);
        for m in &mut c.methods {
            cs_block(&mut m.body, sigs);
        }
    }
    for o in &mut prog.objects {
        cs_block(&mut o.body, sigs);
        for m in &mut o.methods {
            cs_block(&mut m.body, sigs);
        }
    }
}

fn cs_block(stmts: &mut [Stmt], sigs: &HashMap<String, Sig>) {
    for s in stmts {
        match &mut s.kind {
            StmtKind::Local { init, .. } => {
                if let Some(e) = init {
                    cs_expr(e, sigs);
                }
            }
            StmtKind::Destructure { init, .. } => cs_expr(init, sigs),
            StmtKind::Assign { value, .. } => cs_expr(value, sigs),
            StmtKind::PlaceAssign { place, value, .. } => {
                cs_expr(place, sigs);
                cs_expr(value, sigs);
            }
            StmtKind::Expr(e) => cs_expr(e, sigs),
            StmtKind::If { cond, then, els } => {
                cs_expr(cond, sigs);
                cs_block(then, sigs);
                cs_block(els, sigs);
            }
            StmtKind::While { cond, body } => {
                cs_expr(cond, sigs);
                cs_block(body, sigs);
            }
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    cs_expr(e, sigs);
                }
            }
            StmtKind::DefDecl(_) => unreachable!("lifted away by `resolve`"),
        }
    }
}

fn cs_expr(e: &mut Expr, sigs: &HashMap<String, Sig>) {
    match e {
        Expr::NamedArg { value, .. } => cs_expr(value, sigs),
        Expr::Call { name, args, .. } => {
            for a in args.iter_mut() {
                cs_expr(a, sigs);
            }
            if let Some(sig) = sigs.get(name) {
                *name = sig.name.clone();
                args.extend(sig.captures.iter().map(|c| Expr::Var(c.clone())));
            }
        }
        Expr::Var(name) => {
            if let Some(sig) = sigs.get(name) {
                if sig.captures.is_empty() {
                    // No threading needed: the compiler's own paren-less-call and
                    // eta-expansion handling of a bare `def` name still applies.
                    *name = sig.name.clone();
                    return;
                }
                let call = Expr::Call {
                    name: sig.name.clone(),
                    args: sig
                        .params
                        .iter()
                        .chain(sig.captures.iter())
                        .map(|p| Expr::Var(p.clone()))
                        .collect(),
                    line: 0,
                };
                // Parameterless: a bare reference *is* the call. Otherwise it is
                // a function value, so eta-expand over the declared parameters.
                *e = if sig.params.is_empty() {
                    call
                } else {
                    Expr::Lambda {
                        params: sig.params.clone(),
                        body: Box::new(call),
                        partial: false,
                    }
                };
            }
        }
        Expr::Method { recv, args, .. } => {
            cs_expr(recv, sigs);
            for a in args.iter_mut() {
                cs_expr(a, sigs);
            }
        }
        Expr::New { args, .. } => {
            for a in args.iter_mut() {
                cs_expr(a, sigs);
            }
        }
        Expr::Copy { recv, updates, .. } => {
            cs_expr(recv, sigs);
            for (_, v) in updates.iter_mut() {
                cs_expr(v, sigs);
            }
        }
        Expr::Unary { rhs, .. } => cs_expr(rhs, sigs),
        Expr::Binary { lhs, rhs, .. } => {
            cs_expr(lhs, sigs);
            cs_expr(rhs, sigs);
        }
        Expr::Println { arg, .. } => {
            if let Some(a) = arg {
                cs_expr(a, sigs);
            }
        }
        Expr::If { cond, then, els } => {
            cs_expr(cond, sigs);
            cs_expr(then, sigs);
            if let Some(x) = els {
                cs_expr(x, sigs);
            }
        }
        Expr::Block(stmts) => cs_block(stmts, sigs),
        Expr::Match { scrut, arms } => {
            cs_expr(scrut, sigs);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    cs_expr(g, sigs);
                }
                cs_block(&mut arm.body, sigs);
            }
        }
        Expr::Try {
            body,
            catches,
            finalizer,
        } => {
            cs_block(body, sigs);
            for arm in catches {
                if let Some(g) = &mut arm.guard {
                    cs_expr(g, sigs);
                }
                cs_block(&mut arm.body, sigs);
            }
            if let Some(f) = finalizer {
                cs_block(f, sigs);
            }
        }
        Expr::Throw { value, .. } => cs_expr(value, sigs),
        Expr::Format { value, .. } => cs_expr(value, sigs),
        Expr::ForYield { enums, body } | Expr::ForEach { enums, body } => {
            for en in enums.iter_mut() {
                match en {
                    ForEnum::Gen {
                        start, end, step, ..
                    } => {
                        cs_expr(start, sigs);
                        cs_expr(end, sigs);
                        if let Some(s) = step {
                            cs_expr(s, sigs);
                        }
                    }
                    ForEnum::GenColl { coll, .. } => cs_expr(coll, sigs),
                    ForEnum::Guard(g) => cs_expr(g, sigs),
                    ForEnum::Val { value, .. } => cs_expr(value, sigs),
                }
            }
            cs_expr(body, sigs);
        }
        Expr::Lambda { body, .. } => cs_expr(body, sigs),
        Expr::Tuple(elems) | Expr::Collection { elems, .. } => {
            for el in elems.iter_mut() {
                cs_expr(el, sigs);
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
