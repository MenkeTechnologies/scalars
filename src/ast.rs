//! The Scala AST scalars parses and lowers to fusevm bytecode.
//!
//! Slice 1 is a single-object, single entry-point subset: the parser accepts an
//! `object Name { def main(args: Array[String]): Unit = { ... } }` shell (or the
//! `object Name extends App { ... }` form, whose body runs directly) and models
//! the statements and expressions inside the entry body. User-defined methods,
//! fields, classes, and traits are parsed no further than the entry point today
//! (see `BUGS.md`); the AST is shaped to grow into them.

/// A parsed compilation unit: the entry object name and the body of its entry
/// point (`main`, or the `extends App` body).
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// The name of the object that declares the entry point (informational; used
    /// by the `--dump-ast` output and error messages).
    pub object_name: String,
    /// The statements of the entry point.
    pub main: Vec<Stmt>,
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
    /// `for (name <- start until end) { .. }` — a Scala range for-comprehension
    /// in statement (side-effecting) position. `inclusive` is `true` for `to`,
    /// `false` for `until`. Step is 1 in slice 1 (`by` is a later wave).
    For {
        name: String,
        start: Expr,
        end: Expr,
        inclusive: bool,
        body: Vec<Stmt>,
    },
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
