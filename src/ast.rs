//! The Scala AST scalars parses and lowers to fusevm bytecode.
//!
//! A compilation unit is one entry object — `object Name { def main(args:
//! Array[String]): Unit = { ... } }`, or the `object Name extends App { ... }`
//! form whose body runs directly — optionally alongside top-level `class`,
//! `case class`, `object` and `trait` declarations, all of which are modeled
//! here (see `BUGS.md` for what is not).

use std::collections::HashMap;

/// One implicit conversion: the type it converts FROM, the type it converts TO,
/// and the name that performs it — a plain function for `implicit def`, or a
/// `Conversion` instance whose `apply` is called.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversion {
    pub from: String,
    pub to: String,
    pub via: String,
    /// `true` when `via` names a `Conversion` VALUE, so applying it is
    /// `via.apply(x)` rather than `via(x)`.
    pub is_value: bool,
}

/// A parsed compilation unit: the entry object name, the body of its entry
/// point (`main`, or the `extends App` body), and the object's other `def`s.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// The name of the object that declares the entry point (informational; used
    /// by the `--dump-ast` output and error messages).
    pub object_name: String,
    /// The statements of the entry point.
    pub main: Vec<Stmt>,
    /// User-defined methods (every `def` other than `main`), hoisted to a flat
    /// object-level namespace. Each is callable by name from `main`, from
    /// another `def`, or recursively (see [`crate::compiler`]).
    pub functions: Vec<Func>,
    /// Top-level `class` / `case class` declarations (other than the entry
    /// object). Each compiles to a host-heap record constructor plus its methods
    /// (see [`crate::compiler`]).
    pub classes: Vec<ClassDecl>,
    /// Top-level `object` / `case object` declarations other than the entry
    /// object — singletons whose `def`s dispatch statically and whose `val`s are
    /// program-global.
    pub objects: Vec<ObjectDecl>,
    /// What an `import`'s named selectors bind: the LOCAL name a program may
    /// then write bare, mapped to the qualified path it stands for.
    /// `import scala.math.abs` records `abs → [scala, math, abs]`, and
    /// `import scala.math.{max, min => lo}` records `max` and `lo` the same way.
    ///
    /// Consulted by the compiler ONLY where a bare name resolved to nothing
    /// else, which is what gives Scala's rule that a definition shadows an
    /// import without the parser having to track scopes. A wildcard selector
    /// (`import scala.math._`) contributes nothing here — see `BUGS.md`.
    pub imports: HashMap<String, Vec<String>>,
    /// The package paths a WILDCARD `import` opened — `import scala.math._`
    /// records `[scala, math]`.
    ///
    /// A wildcard names no member, so unlike [`Self::imports`] it cannot be a
    /// map from a local name to a path. What it can do is say which packages a
    /// bare name may be looked for in, and the compiler asks only about the
    /// members it actually provides for that package — so an unknown name is
    /// still `not found` rather than a guess that fails further downstream.
    pub import_wildcards: Vec<Vec<String>>,
    /// `extension (x: T) def m(…)` declarations, as
    /// `(receiver type, method name, hoisted function name)`.
    ///
    /// An extension is dispatched on the RECEIVER's type: `3.name` and
    /// `"q".name` select different bodies, and a real MEMBER of the receiver
    /// wins over an extension of the same name. The compiler therefore consults
    /// this only after every member and stdlib rule has missed, and only with a
    /// receiver whose static type it can name.
    pub extensions: Vec<(String, String, String)>,
    /// Implicit CONVERSIONS, as `(from type, to type, how to apply it)`.
    ///
    /// Both spellings land here: `implicit def i2s(i: Int): String` records the
    /// function's name, and `given Conversion[Int, Boolean] = …` records the
    /// given's, applied through its `apply`. A conversion is inserted at the
    /// two points Scala inserts one — a value assigned to a declared type, and
    /// an argument passed to a parameter of one — and only when the value's
    /// static type is known and is not already the target.
    pub conversions: Vec<Conversion>,
    /// `implicit val NAME: TY` declarations, as `(name, declared type)`.
    ///
    /// This is the implicit SCOPE, and it is resolved by declared type name: an
    /// `implicit` / `using` parameter clause is not written at the call site, so
    /// the compiler supplies each of its parameters from the first binding here
    /// whose type matches. An annotation is required, because the type is the
    /// only thing that selects one.
    pub implicits: Vec<(String, String)>,
}

/// A `class` / `case class` declaration. The instance is a host-heap record (an
/// ordered, type-tagged field list behind a `Value::Obj`); `is_case` selects
/// structural `equals`/`hashCode`/`toString` and enables companion
/// `apply`/`unapply`/`copy`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDecl {
    pub name: String,
    pub is_case: bool,
    /// `true` for a `trait`: no constructor is emitted and its fields are
    /// contributed by whichever class mixes it in.
    pub is_trait: bool,
    /// `extends P with T1 with T2` — the direct supertypes in source order.
    /// Method lookup walks them right-to-left after this class itself.
    pub parents: Vec<String>,
    /// The superclass constructor arguments of `extends P(a, b)`. Empty for a
    /// trait parent or a parameterless superclass.
    pub super_args: Vec<Expr>,
    /// Primary-constructor parameter names, in order. All are instance fields
    /// (the runtime is dynamically typed, so the `val`/`var`/private distinction
    /// is not enforced — a documented simplification).
    pub params: Vec<String>,
    /// The DEFAULT of each [`Self::params`] entry, same order and length;
    /// `None` where the parameter has none.
    ///
    /// Kept unevaluated, because Scala evaluates a default at the CALL site and
    /// only when the argument is omitted — splicing the expression in at the
    /// construction is what reproduces that exactly, and it is the same thing
    /// `Compiler::adapt_args` does for a `def`.
    pub param_defaults: Vec<Option<Expr>>,
    /// The declared type of each [`Self::params`] entry, same order and length.
    /// Scala requires an annotation on every constructor parameter, so this is
    /// the width of every field the primary constructor contributes — which is
    /// what types `c.n * 2` at a use site and a bare `n * 2` inside a method.
    pub param_tys: Vec<Option<String>>,
    /// The class body run as the constructor: `val`/`var` field declarations
    /// (their initializers may reference constructor params and earlier fields)
    /// and any side-effecting statements. `def`s are hoisted into `methods`.
    pub body: Vec<Stmt>,
    /// Every instance field in declared order: constructor `params` followed by
    /// the `val`/`var` names declared in `body`. This is the record's shape (used
    /// for `toString`, construction, and constructor-pattern binding).
    pub field_names: Vec<String>,
    /// Methods (`def`s) declared in the class body. Compiled as global
    /// subroutines named `Class$method` with an implicit leading `this` param.
    pub methods: Vec<Func>,
}

/// An `object` / `case object` singleton. Its `val`s become program globals
/// (`Name.val`) initialized before `main`; its `def`s become subroutines
/// (`Name$method`) dispatched statically off the object name.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectDecl {
    pub name: String,
    pub is_case: bool,
    /// `extends P with T …` — the singleton's supertypes, so `case o: Shape`
    /// matches it and its inherited methods dispatch.
    pub parents: Vec<String>,
    /// `val`/`var` declarations (initialized once before `main`) and side effects.
    pub body: Vec<Stmt>,
    pub methods: Vec<Func>,
}

/// The parts of one parameter declaration that change how a CALL is lowered.
/// Everything else about the type is diagnostic-only and dropped.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParamSig {
    /// `b: Int = 10` — the default. Scala evaluates it AT THE CALL SITE and only
    /// when the argument is omitted, so it is stored as an unevaluated
    /// expression and spliced into the argument list there. Scala 3 forbids a
    /// default referring to another parameter of the same list, which is what
    /// makes call-site splicing exact rather than an approximation.
    pub default: Option<Expr>,
    /// `xs: Int*` — a repeated parameter. Trailing arguments are collected into
    /// an `ArraySeq` (the runtime class Scala hands a varargs method).
    pub vararg: bool,
    /// `x: => Int` — a by-name parameter. The argument is passed as a zero-arg
    /// thunk and forced at EVERY use inside the body, so a by-name argument with
    /// a side effect runs once per use (and not at all if never used).
    pub by_name: bool,
    /// `true` when this parameter belongs to an `implicit` / `using` clause.
    ///
    /// Such a clause is not written at the call site: the compiler supplies it
    /// from the implicit scope. Without the flag an omitted one looks exactly
    /// like a partial application, and `def f(x: Int)(implicit m: Int)` called
    /// `f(1)` eta-expanded to a function instead of answering a number.
    pub implicit_clause: bool,
    /// `true` when this parameter opens a SECOND or later parameter clause of a
    /// curried `def` — `def add(a: Int)(b: Int)` marks `b`.
    ///
    /// The clauses are flattened into one list, because a call supplying all of
    /// them is one call. The boundary still matters in exactly one place:
    /// Scala eta-expands a call that stops AT a clause boundary (`add(10)` is
    /// `b => add(10, b)`) and rejects one that stops inside a clause
    /// (`def two(a: Int, b: Int)` called `two(10)` is "missing argument for
    /// parameter b"). Without this flag the two are indistinguishable.
    pub clause_start: bool,
    /// The parameter's declared type, verbatim (`"Int"`, `"Long"`, `"Double"`).
    /// Scala REQUIRES a type on every `def` parameter, which makes this the one
    /// place a nested body's numeric widths are always knowable — and so the
    /// main source of `Int`-vs-`Long` information inside a `def`. Consulted only
    /// by the compiler's width analysis; the runtime stays dynamically typed.
    pub ty: Option<String>,
}

/// A user-defined method: `def name(p0: T0, p1: T1, …): R = body`. Parameter and
/// return types are parsed for diagnostics but do not gate the dynamically typed
/// runtime, so only the parameter *names* are retained (bound to frame slots) —
/// plus the [`ParamSig`] bits that a call site has to honor.
#[derive(Debug, Clone, PartialEq)]
pub struct Func {
    pub name: String,
    pub params: Vec<String>,
    /// One entry per [`Self::params`] entry, same order and length.
    pub sig: Vec<ParamSig>,
    /// The `def`'s type-parameter NAMES (`[A, B]` answers `["A", "B"]`).
    ///
    /// Kept because implicit resolution substitutes them: a `using` parameter
    /// of type `Sh[A]` names no given until `A` is known, and what makes it
    /// known is the argument passed for the value parameter declared `A`.
    pub type_params: Vec<String>,
    /// The declared return type, verbatim (`Some("Int")` for `def f(): Int`).
    /// Scala infers a return type when it is omitted, which this frontend cannot
    /// do, so an unannotated `def` answers `None` and its calls stay unnarrowed.
    /// Consulted only by the compiler's width analysis.
    pub ret_ty: Option<String>,
    /// How many TRAILING parameters are synthetic — the free variables that
    /// [`crate::resolve`] turned into parameters when it hoisted a nested `def`.
    /// A call site appends those arguments after the written ones, so argument
    /// adaptation (named arguments, defaults, varargs) has to match the written
    /// arguments against `params[..params.len() - captured]` and pass the rest
    /// through untouched.
    pub captured: usize,
    pub body: Vec<Stmt>,
    /// `true` for a declaration with no `= body` (`def area: Double` in a
    /// trait). No subroutine is emitted; a call dispatches to whichever subtype
    /// implements it.
    pub is_abstract: bool,
}

/// A Scala statement with its 1-based source line (used by `scala --dap` to emit
/// a per-statement debug marker so breakpoints and stepping land on real lines).
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub line: u32,
    pub kind: StmtKind,
}

/// The kind of a Scala statement (see [`Stmt`] for the line it carries).
#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// A local binding: `val x = expr`, `var y: Int = expr`. `is_val` is
    /// enforced — the compiler rejects a plain reassignment to a `val` — while
    /// the declared type is retained for diagnostics only, since the runtime is
    /// dynamically typed on the fusevm value model (see `BUGS.md`).
    Local {
        is_val: bool,
        /// `lazy val x = e` — the initializer is not evaluated at the
        /// declaration but at the FIRST read, and at most once. A `lazy val`
        /// never read is never evaluated at all, which is observable whenever
        /// the initializer prints, throws, or costs anything.
        is_lazy: bool,
        ty: Option<String>,
        name: String,
        init: Option<Expr>,
    },
    /// A pattern definition: `val (a, b) = pair`, `val Some(x) = opt`,
    /// `val h :: t = xs`. Every name the pattern binds enters the enclosing
    /// scope; a non-matching value raises `scala.MatchError`, as in Scala.
    Destructure { pat: Pattern, init: Expr },
    /// An assignment to an existing `var`: `x = expr`, `x += expr`.
    Assign {
        name: String,
        op: AssignOp,
        value: Expr,
    },
    /// A compound assignment whose target is not a simple name: `a(i) += 1`,
    /// `m(k) *= 2`, `obj.field += 1`.
    ///
    /// Scala resolves `l op= r` (SLS 6.12.4) by preferring an `op=` **member**
    /// on `l` and falling back to `l = l op r` — which for an *application*
    /// target is `l.update(args, l.apply(args) op r)`. A simple-name target is
    /// [`StmtKind::Assign`]; every other target shape lands here, so the
    /// receiver and the indices can be evaluated exactly once.
    PlaceAssign {
        place: Expr,
        op: AssignOp,
        value: Expr,
    },
    /// An expression evaluated for its side effects: `println(x)`.
    Expr(Expr),
    /// `if (cond) { .. } else { .. }`.
    If {
        cond: Expr,
        then: Vec<Stmt>,
        els: Vec<Stmt>,
    },
    /// `while (cond) { .. }`.
    While { cond: Expr, body: Vec<Stmt> },
    /// `return expr` / bare `return` — an early exit from the enclosing `def`.
    Return(Option<Expr>),
    /// A `def` declared inside a statement block (`def f(x: Int) = …` in a method
    /// body, an `if` branch, a bare `{ … }`, …). It is *not* executable: the
    /// [`crate::resolve`] pass gives it a unique global name, lambda-lifts its
    /// captured enclosing locals into extra parameters, hoists it into
    /// [`Program::functions`], and deletes the statement. The compiler therefore
    /// never sees this variant.
    DefDecl(Func),
}

/// One enumerator of a `for` comprehension: a range generator, a collection
/// generator, or an `if` guard.
#[derive(Debug, Clone, PartialEq)]
pub enum ForEnum {
    /// `name <- start until|to end [by step]` — an integer range generator.
    /// `inclusive` is `true` for `to`, `false` for `until`. `step` is `None` for
    /// the implicit step of 1; a `by` clause carries an arbitrary expression, so
    /// the loop's direction (whether the bound test is `<`/`<=` or `>`/`>=`) can
    /// only be decided at runtime unless the step is a literal.
    Gen {
        name: String,
        start: Expr,
        end: Expr,
        inclusive: bool,
        step: Option<Expr>,
    },
    /// `pat <- collExpr` — a collection generator (a `List`/`Map`/etc. source),
    /// desugared to `.map`/`.flatMap`/`.withFilter` (see [`crate::compiler`]).
    /// `pat` is a plain [`Pattern::Bind`] for the usual `x <- xs`, and a
    /// [`Pattern::Tuple`] for a destructuring generator (`(k, v) <- m`), which
    /// desugars to a pattern-matching anonymous function.
    /// `filtering` marks Scala 3's `for (case pat <- xs)` spelling: a REFUTABLE
    /// pattern, where an element that does not match is skipped instead of
    /// raising a `MatchError`. Scala 3 requires the `case` keyword for this —
    /// `for (Some(x) <- opts)` is a compile error, `for (case Some(x) <- opts)`
    /// filters — so it is a distinct form, not an inference.
    GenColl {
        pat: Pattern,
        coll: Expr,
        filtering: bool,
    },
    /// `if cond` — a filter that skips the remaining (inner) enumerators when the
    /// condition is false, desugaring to `withFilter`.
    Guard(Expr),
    /// `y = e` — a value definition. `e` is re-evaluated per binding of the
    /// generators to its left and `y` is in scope for every enumerator to its
    /// right and for the body. Over a *range* generator it lowers inline (a store
    /// into a loop-body slot, keeping the counted loop); over a *collection*
    /// generator it takes Scala's own translation, which pairs the value onto the
    /// generator (`for (x <- xs; y = f(x))` becomes `for ((x, y) <- xs.map(x =>
    /// (x, f(x))))`), so a later guard can see both names.
    Val { name: String, value: Expr },
}

/// Compound-assignment operator. `Assign` is a plain `=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    Assign,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// A Scala expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64),
    /// An `L`-suffixed integer literal (`1L`, `5000000000L`). Its *value* is an
    /// integer exactly like [`Expr::Int`], and it compiles to the same constant;
    /// the variant exists so the compiler can tell a `Long` from an `Int`
    /// STATICALLY. That distinction is what keeps 32-bit overflow wrapping off
    /// `Long` arithmetic — `2147483647L + 1` is `2147483648`, while the same
    /// expression written without the suffix wraps to `-2147483648`.
    Long(i64),
    Float(f64),
    /// An `f`-suffixed floating literal (`1.5f`, `0.1F`) — a `Float`.
    ///
    /// The value it carries is ALREADY rounded to 32-bit precision (the lexer
    /// does it), so it compiles to the same `LoadFloat` constant an
    /// [`Expr::Float`] does. The variant exists so the compiler can tell a
    /// `Float` from a `Double` STATICALLY, which is what routes its arithmetic
    /// through the 32-bit operation and its rendering through
    /// `Float.toString` — the same reason [`Expr::Long`] is separate from
    /// [`Expr::Int`].
    Float32(f64),
    Str(String),
    /// A `Char` literal. Distinct from [`Expr::Str`]: a `Char` is its own type,
    /// so it answers `Char`'s methods (`toInt` is the code point, not a parse)
    /// and enters arithmetic as a number.
    Char(char),
    Bool(bool),
    /// The `null` literal.
    Null,
    /// A bare identifier — a local binding read.
    Var(String),
    /// A unary operator applied to one operand (`-x`, `!b`).
    Unary {
        op: UnOp,
        rhs: Box<Expr>,
    },
    /// A binary operator applied to two operands.
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `println(arg)` / `print(arg)`. Modeled directly (rather than as a general
    /// method call) until user methods land.
    Println {
        newline: bool,
        arg: Option<Box<Expr>>,
    },
    /// One command-line argument, converted to a `@main` parameter's declared
    /// type. Scala 3 compiles `@main def go(n: Int)` into a generated `main` that
    /// calls `scala.util.CommandLineParser.parseArgument[Int](args, 0)` per
    /// parameter, left to right, so this carries the same two things: the
    /// argument's position and the type to read it as. A missing or unreadable
    /// argument ends the program the way that parser does — see
    /// [`crate::host::MAIN_ARG`].
    MainArg {
        index: usize,
        ty: String,
    },
    /// The `args: Array[String]` parameter of a `def main` entry point: the
    /// program arguments as an `Array` of `String`.
    MainArgv,
    /// A named call `name(arg, …)`: a user-defined `def` (lowered to `Op::Call`),
    /// the `__rust_compile("<b64>", line)` FFI-block desugar target, or a call to
    /// a function exported by such a block (`add(2, 3)`). Resolved in
    /// [`crate::compiler`]. `line` is the source line of the callee.
    Call {
        name: String,
        args: Vec<Expr>,
        line: u32,
    },
    /// Postfix method/field dispatch on a receiver: `s.length`, `n.toString`,
    /// `s.substring(1, 3)`. A paren-less member is 0 arguments. Routed through
    /// the host's universal method dispatcher (see [`crate::host`]).
    Method {
        recv: Box<Expr>,
        name: String,
        args: Vec<Expr>,
        line: u32,
    },
    /// A compound assignment read for its VALUE — `println(buf += 1)`,
    /// `val r = (n -= 2)`, `(buf += 1) += 2`.
    ///
    /// The statement forms ([`StmtKind::Assign`], [`StmtKind::PlaceAssign`])
    /// discard that value; this variant keeps it. It is deliberately NOT one of
    /// them wrapped in a statement, because the two have different results and
    /// only the run time knows which: Scala prefers the `op=` MEMBER when the
    /// receiver has one, whose value is the receiver (`buf += 1` is `buf`), and
    /// falls back to the `x = x op e` expansion, whose value is `()` (SLS
    /// 6.12.4). `target` is any shape the statement forms accept — a name, an
    /// application (`a(i)`), or a selection (`p.n`).
    CompoundAssign {
        target: Box<Expr>,
        op: AssignOp,
        value: Box<Expr>,
        line: u32,
    },
    /// `new Class(args)` — construct a host-heap instance. Lowered to the class's
    /// `Class$new` constructor subroutine (see [`crate::compiler`]).
    New {
        name: String,
        args: Vec<Expr>,
        line: u32,
    },
    /// `name = value` in an argument list — a named argument. Legal ONLY in
    /// argument position: the compiler's call lowering matches it to the
    /// callee's parameter of that name and never sees it anywhere else.
    NamedArg {
        name: String,
        value: Box<Expr>,
    },
    /// `recv.copy(field = e, …)` on a `case class` — a new instance with the
    /// named (or positional) fields overwritten and the rest copied from `recv`.
    /// `updates` pairs an optional field name (`None` = positional) with its
    /// value expression.
    Copy {
        recv: Box<Expr>,
        updates: Vec<(Option<String>, Expr)>,
        line: u32,
    },
    /// `if (cond) then [else els]` in *expression* position — a value-producing
    /// conditional (`val r = if (c) a else b`). Distinct from [`StmtKind::If`]
    /// (statement position, run for effect). When `els` is `None` the false path
    /// yields `Unit` (Scala's `if` without `else` has type `Unit`).
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        els: Option<Box<Expr>>,
    },
    /// A brace-delimited block used as an expression: `{ s1; s2; last }`. Its
    /// value is the last statement's value (Scala's block expression); a block
    /// whose last statement is not an expression, or an empty block, yields
    /// `Unit`. Used as an `if`/`match` branch body.
    Block(Vec<Stmt>),
    /// `scrutinee match { case pat [if guard] => body … }` — a pattern match in
    /// expression position. Arms are tried top-to-bottom; the first whose pattern
    /// (and guard) matches produces the value. No arm matching is a
    /// `scala.MatchError` at runtime. Constructor/case-class patterns are not
    /// modeled (fusevm-blocked); see [`Pattern`].
    Match {
        scrut: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// `try { body } [catch { case … }] [finally { fin }]` in expression
    /// position. Scala's `try` is value-producing (`val r = try f() catch { case
    /// _ => 0 }`), so it lives here rather than in [`StmtKind`]; a `try` used as
    /// a statement is a `StmtKind::Expr` wrapping this.
    ///
    /// `catches` reuses the [`MatchArm`] shape because a Scala catch block *is*
    /// a pattern match on the thrown value — the same `case p if g => body`
    /// grammar, matched against the exception instead of a scrutinee expression.
    Try {
        body: Vec<Stmt>,
        catches: Vec<MatchArm>,
        finalizer: Option<Vec<Stmt>>,
    },
    /// `throw e` — raise `e` as an exception. Scala types `throw` as `Nothing`,
    /// which conforms to every type, so it is an expression and may appear in
    /// operand position (`val x = if (bad) throw new Exception("…") else v`).
    Throw {
        value: Box<Expr>,
        line: u32,
    },
    /// An `f"…"`-interpolator formatted splice: format `value` with the Java
    /// `Formatter` spec `spec` (`%d`, `%.2f`, `%-8s`, …). Produced only by the
    /// interpolated-string desugar in [`crate::parser`].
    Format {
        value: Box<Expr>,
        spec: String,
        line: u32,
    },
    /// `for (enums) yield body` — a comprehension that collects `body` for each
    /// binding into a `Vector` (Scala's `IndexedSeq` result for a range
    /// generator). Multiple generators nest (`flatMap`); `if` guards filter.
    ForYield {
        enums: Vec<ForEnum>,
        body: Box<Expr>,
    },
    /// `for (enums) body` — the side-effecting comprehension (`foreach`); its
    /// value is `Unit`. Multiple generators nest; `if` guards filter.
    ForEach {
        enums: Vec<ForEnum>,
        body: Box<Expr>,
    },
    /// A first-class function value: `x => e`, `(a, b) => e`, `x => { … }`. The
    /// parameter names bind in `body`; free names are captured from the enclosing
    /// frame (see [`crate::compiler`]). Lowered to a host-heap closure.
    Lambda {
        params: Vec<String>,
        body: Box<Expr>,
        /// `true` for a `{ case … }` literal, which is Scala's `PartialFunction`
        /// literal: besides `apply` it answers `isDefinedAt`, so `collect` can
        /// skip an element no arm matches instead of raising `MatchError`. The
        /// compiler emits a **second** subroutine for such a lambda — the same
        /// patterns and guards with every arm body replaced by `true` and a
        /// trailing `case _ => false` — and stores its name index alongside the
        /// `apply` body in the closure handle.
        partial: bool,
    },
    /// An underscore placeholder (`_`) in an argument expression. The enclosing
    /// argument is rewritten into a [`Expr::Lambda`] whose synthetic parameters
    /// replace the placeholders left-to-right (see [`crate::parser`]); a
    /// `Placeholder` never survives into the compiler.
    Placeholder,
    /// A tuple literal: `(a, b)`, or the `a -> b` pair sugar. Lowered to a
    /// host-heap tuple; a 2-tuple is `Tuple2` (as `Map` pairs use).
    Tuple(Vec<Expr>),
    /// `List(...)` / `Nil` / `Map(...)` collection literal. `ctor` is the
    /// constructor name (`List`, `Map`, `Nil`); `elems` the element/pair exprs.
    Collection {
        ctor: String,
        elems: Vec<Expr>,
    },
}

/// One arm of a [`Expr::Match`]: `case pat [if guard] => body`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pat: Pattern,
    /// An optional `if` guard evaluated only after the pattern matches.
    pub guard: Option<Expr>,
    /// The arm body as a statement block; its last statement's value is the arm
    /// result (see [`Expr::Block`]).
    pub body: Vec<Stmt>,
}

/// A `match` pattern. Constructor/case-class/extractor patterns (`case Foo(x)`)
/// are lowered against the host-heap record model (see [`crate::compiler`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// `case _ =>` — matches anything, binds nothing.
    Wildcard,
    /// `case 1 =>`, `case "x" =>`, `case true =>`, `case null =>` — matches when
    /// the scrutinee equals this literal.
    Literal(Expr),
    /// `case x =>` — matches anything and binds the scrutinee to `x`.
    Bind(String),
    /// `case x: String =>` / `case _: Int =>` — matches when the scrutinee has the
    /// named runtime type; binds it to `name` (unless `name` is `_`).
    Typed { name: String, ty: String },
    /// `case Point(x, y) =>` / `case Some(v) =>` — a constructor (extractor)
    /// pattern: matches when the scrutinee is an instance of `name` (a `case
    /// class`/`case object`) whose field arity matches, then binds each field
    /// position against the corresponding sub-pattern (nesting and guards work).
    Constructor { name: String, elems: Vec<Pattern> },
    /// `case (a, b) =>` — a tuple pattern: matches a tuple of that arity and
    /// binds each element position against the corresponding sub-pattern. This is
    /// what a `Map` comprehension (`for ((k, v) <- m)`) and a pair-taking
    /// anonymous function (`xs.map { case (k, v) => … }`) destructure through.
    Tuple(Vec<Pattern>),
    /// A capitalized stable-identifier pattern (`case None =>`) — Scala treats an
    /// upper-case bare identifier in a pattern as a reference to a value/singleton
    /// (e.g. the `None` object), matched by `==`, not a binding.
    Stable(String),
    /// `case n @ Some(v) =>` — a binder: matches `pat`, and additionally binds
    /// the *whole* scrutinee to `name`. Scala's `Pattern2` production.
    At { name: String, pat: Box<Pattern> },
    /// `case 1 | 2 | 3 =>` — an alternation: matches when ANY branch matches.
    /// Scala forbids variable bindings inside alternatives, so no branch may
    /// contribute bindings to the arm body.
    Alt(Vec<Pattern>),
    /// `case h :: t =>` — the cons pattern: matches a non-empty `List`, binding
    /// its head and tail. Right-associative, so `a :: b :: rest` nests right.
    Cons(Box<Pattern>, Box<Pattern>),
    /// `case List(a, rest @ _*) =>` — the trailing sequence wildcard. Only legal
    /// as the last element of a sequence pattern; binds the remaining elements
    /// (as a `List`) when `name` is `Some`.
    Rest(Option<String>),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    /// `~x` — the bitwise complement (`Int.unary_~`).
    Complement,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    /// `::` — list cons (right-associative; prepends the left operand).
    Cons,
    /// `#::` — `LazyList`'s cons. Its right operand is BY-NAME: the compiler
    /// wraps it in a thunk, which is what lets a `LazyList` be defined in terms
    /// of itself (`val fibs = 0 #:: 1 #:: fibs.zip(fibs.tail)…`).
    LazyCons,
}
