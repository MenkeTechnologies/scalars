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

/// One enumerator of a `for` comprehension: a range generator or an `if` guard.
/// (Collection generators are not modeled — scalars has no `List`/`Vector`
/// literal and no `map`/`flatMap` yet; only integer ranges are iterable.)
#[derive(Debug, Clone, PartialEq)]
pub enum ForEnum {
    /// `name <- start until|to end` — an integer range generator (step 1).
    /// `inclusive` is `true` for `to`, `false` for `until`.
    Gen {
        name: String,
        start: Expr,
        end: Expr,
        inclusive: bool,
    },
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
/// are intentionally absent — they need an ordered record value the fusevm value
/// model does not yet expose. The parser rejects them rather than mis-lowering.
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
}
