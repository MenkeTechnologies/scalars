//! The Scala AST scalars parses and lowers to fusevm bytecode.
//!
//! Slice 1 is a single-object, single entry-point subset: the parser accepts an
//! `object Name { def main(args: Array[String]): Unit = { ... } }` shell (or the
//! `object Name extends App { ... }` form, whose body runs directly) and models
//! the statements and expressions inside the entry body. User-defined methods,
//! fields, classes, and traits are parsed no further than the entry point today
//! (see `BUGS.md`); the AST is shaped to grow into them.

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
}

/// A `class` / `case class` declaration. The instance is a host-heap record (an
/// ordered, type-tagged field list behind a `Value::Obj`); `is_case` selects
/// structural `equals`/`hashCode`/`toString` and enables companion
/// `apply`/`unapply`/`copy`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDecl {
    pub name: String,
    pub is_case: bool,
    /// Primary-constructor parameter names, in order. All are instance fields
    /// (the runtime is dynamically typed, so the `val`/`var`/private distinction
    /// is not enforced — a documented simplification).
    pub params: Vec<String>,
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
    /// `val`/`var` declarations (initialized once before `main`) and side effects.
    pub body: Vec<Stmt>,
    pub methods: Vec<Func>,
}

/// A user-defined method: `def name(p0: T0, p1: T1, …): R = body`. Parameter and
/// return types are parsed for diagnostics but do not gate the dynamically typed
/// runtime, so only the parameter *names* are retained (bound to frame slots).
#[derive(Debug, Clone, PartialEq)]
pub struct Func {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
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
    /// A local binding: `val x = expr`, `var y: Int = expr`. `is_val` records
    /// immutability for diagnostics; the declared type is retained for the same
    /// reason. The runtime is dynamically typed on the fusevm value model, so
    /// neither gates execution yet (val-reassignment is not rejected — `BUGS.md`).
    Local {
        is_val: bool,
        ty: Option<String>,
        name: String,
        init: Option<Expr>,
    },
    /// An assignment to an existing `var`: `x = expr`, `x += expr`.
    Assign {
        name: String,
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
    /// `name <- collExpr` — a collection generator (a `List`/`Map`/etc. source),
    /// desugared to `.map`/`.flatMap`/`.withFilter` (see [`crate::compiler`]).
    GenColl { name: String, coll: Expr },
    /// `if cond` — a filter that skips the remaining (inner) enumerators when the
    /// condition is false, desugaring to `withFilter`.
    Guard(Expr),
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
    Float(f64),
    Str(String),
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
    /// `new Class(args)` — construct a host-heap instance. Lowered to the class's
    /// `Class$new` constructor subroutine (see [`crate::compiler`]).
    New {
        name: String,
        args: Vec<Expr>,
        line: u32,
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
    /// A capitalized stable-identifier pattern (`case None =>`) — Scala treats an
    /// upper-case bare identifier in a pattern as a reference to a value/singleton
    /// (e.g. the `None` object), matched by `==`, not a binding.
    Stable(String),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
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
}
