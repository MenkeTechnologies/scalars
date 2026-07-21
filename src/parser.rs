//! A recursive-descent parser with precedence-climbing for expressions.
//!
//! Grammar (slice 1): a compilation unit is `object Name { ... }`. scalars
//! locates the entry point two ways: an `object Name extends App { <body> }`
//! runs `<body>` directly, and an `object Name { ... def main(args: …) = { <body> }
//! ... }` runs its `main`. Other members are skipped so an object that also
//! declares helpers still finds its entry point. Statements cover `val`/`var`
//! bindings, assignments, `if`/`while`, the Scala range `for`, `println`/`print`,
//! and bare expressions. Statements are separated by inferred line breaks or
//! explicit `;` (see the lexer).

use crate::ast::*;
use crate::lexer::{Tok, Token};

/// Parse Scala `src` into a [`Program`].
pub fn parse(src: &str) -> Result<Program, String> {
    let tokens = crate::lexer::lex(src)?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        funcs: Vec::new(),
    };
    p.program()
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// User-defined `def`s collected while parsing (both object members and
    /// `def`s hoisted out of a body block). Flattened into `Program::functions`.
    funcs: Vec<Func>,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].kind
    }

    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }

    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].kind.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, want: &Tok) -> Result<(), String> {
        if std::mem::discriminant(self.peek()) == std::mem::discriminant(want) {
            self.advance();
            Ok(())
        } else {
            Err(format!(
                "scalars: expected {want} but found {} on line {}",
                self.peek(),
                self.line()
            ))
        }
    }

    fn is(&self, t: &Tok) -> bool {
        std::mem::discriminant(self.peek()) == std::mem::discriminant(t)
    }

    /// Consume any run of statement separators (`;` and inferred line breaks).
    fn skip_seps(&mut self) {
        while self.is(&Tok::Newline) || self.is(&Tok::Semi) {
            self.advance();
        }
    }

    /// `[package/import lines] object Name [extends …] { members… }` — find the
    /// entry object and its entry point.
    fn program(&mut self) -> Result<Program, String> {
        self.skip_seps();
        // Skip package/import prologue lines.
        loop {
            match self.peek() {
                Tok::Ident(w) if w == "package" || w == "import" => {
                    while !self.is(&Tok::Newline) && !self.is(&Tok::Semi) && !self.is(&Tok::Eof) {
                        self.advance();
                    }
                    self.skip_seps();
                }
                _ => break,
            }
        }
        // Object modifiers/annotations (`final`, `@main`, …) arrive as idents/
        // punctuation; skip to the `object` keyword.
        while !self.is(&Tok::Object) && !self.is(&Tok::Eof) {
            self.advance();
        }
        self.eat(&Tok::Object)?;
        let object_name = self.ident()?;

        // `extends Parent with Trait …` — note whether `App` is a parent (its
        // body then runs directly).
        let mut app_mode = false;
        if self.is(&Tok::Extends) {
            self.advance();
            while !self.is(&Tok::LBrace) && !self.is(&Tok::Eof) {
                if matches!(self.peek(), Tok::Ident(w) if w == "App") {
                    app_mode = true;
                }
                self.advance();
            }
        }
        self.eat(&Tok::LBrace)?;

        if app_mode {
            // The whole object body is the program; any `def` inside is hoisted
            // into `self.funcs` by `block`.
            let body = self.block()?;
            return Ok(Program {
                object_name,
                main: body,
                functions: std::mem::take(&mut self.funcs),
            });
        }

        // Otherwise scan members: `def main` is the entry, other `def`s are
        // functions, everything else (object fields, nested types) is skipped.
        let mut main = None;
        self.skip_seps();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            if self.is(&Tok::Def) {
                if let Some(body) = self.try_main()? {
                    main = Some(body);
                } else {
                    let f = self.parse_def()?;
                    self.funcs.push(f);
                }
            } else {
                self.skip_member()?;
            }
            self.skip_seps();
        }

        match main {
            Some(main) => Ok(Program {
                object_name,
                main,
                functions: std::mem::take(&mut self.funcs),
            }),
            None => Err(format!(
                "scalars: object `{object_name}` has no `def main(args: Array[String])` and does not `extend App`"
            )),
        }
    }

    /// Parse a non-`main` `def name[(...)]: T = body` into a [`Func`]. The cursor
    /// is on `def`. Type parameters and parameter/return types are consumed but
    /// only parameter *names* are kept (the runtime is dynamically typed).
    fn parse_def(&mut self) -> Result<Func, String> {
        self.eat(&Tok::Def)?;
        let name = self.ident()?;
        // Optional `[T, U]` type-parameter clause — skip the whole bracket group.
        if self.is(&Tok::LBracket) {
            let mut depth = 0;
            loop {
                match self.advance() {
                    Tok::LBracket => depth += 1,
                    Tok::RBracket => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    Tok::Eof => break,
                    _ => {}
                }
            }
        }
        // Parameter list. Scala allows a parameterless `def name = …`, so the
        // `(` is optional.
        let mut params = Vec::new();
        if self.is(&Tok::LParen) {
            self.advance();
            self.skip_seps();
            while !self.is(&Tok::RParen) && !self.is(&Tok::Eof) {
                let pname = self.ident()?;
                if self.is(&Tok::Colon) {
                    self.advance();
                    self.type_ref()?;
                }
                params.push(pname);
                if self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                } else {
                    break;
                }
            }
            self.eat(&Tok::RParen)?;
        }
        // Optional `: ReturnType`.
        if self.is(&Tok::Colon) {
            self.advance();
            self.type_ref()?;
        }
        self.eat(&Tok::Assign)?;
        self.skip_seps();
        let body = if self.is(&Tok::LBrace) {
            self.advance();
            self.block()?
        } else {
            vec![self.statement()?]
        };
        Ok(Func { name, params, body })
    }

    /// If the cursor is at `def main(…) [: Type] = <body>`, parse the body and
    /// return it. Otherwise leave the cursor untouched and return None.
    fn try_main(&mut self) -> Result<Option<Vec<Stmt>>, String> {
        if !self.is(&Tok::Def) {
            return Ok(None);
        }
        let save = self.pos;
        self.advance(); // def
        if !matches!(self.peek(), Tok::Ident(n) if n == "main") {
            self.pos = save;
            return Ok(None);
        }
        self.advance(); // main
        if !self.is(&Tok::LParen) {
            self.pos = save;
            return Ok(None);
        }
        self.eat(&Tok::LParen)?;
        // skip the parameter list — slice 1 ignores argv
        let mut depth = 1;
        while depth > 0 && !self.is(&Tok::Eof) {
            match self.advance() {
                Tok::LParen => depth += 1,
                Tok::RParen => depth -= 1,
                _ => {}
            }
        }
        // optional `: ReturnType`
        if self.is(&Tok::Colon) {
            self.advance();
            while !self.is(&Tok::Assign) && !self.is(&Tok::LBrace) && !self.is(&Tok::Eof) {
                self.advance();
            }
        }
        // `= <body>` — a method body is `= { block }` or `= singleExpr`.
        self.eat(&Tok::Assign)?;
        self.skip_seps();
        let body = if self.is(&Tok::LBrace) {
            self.advance();
            self.block()?
        } else {
            vec![self.statement()?]
        };
        Ok(Some(body))
    }

    /// Skip a non-`main` member: a brace-delimited body, or up to the next
    /// statement separator at bracket depth 0.
    fn skip_member(&mut self) -> Result<(), String> {
        let mut pdepth = 0i32;
        while !self.is(&Tok::Eof) {
            match self.peek() {
                Tok::LBrace if pdepth == 0 => {
                    let mut depth = 0;
                    loop {
                        match self.advance() {
                            Tok::LBrace => depth += 1,
                            Tok::RBrace => {
                                depth -= 1;
                                if depth == 0 {
                                    return Ok(());
                                }
                            }
                            Tok::Eof => return Ok(()),
                            _ => {}
                        }
                    }
                }
                Tok::Newline | Tok::Semi if pdepth == 0 => {
                    self.advance();
                    return Ok(());
                }
                Tok::RBrace if pdepth == 0 => return Ok(()),
                Tok::LParen | Tok::LBracket => {
                    pdepth += 1;
                    self.advance();
                }
                Tok::RParen | Tok::RBracket => {
                    pdepth -= 1;
                    self.advance();
                }
                _ => {
                    self.advance();
                }
            }
        }
        Ok(())
    }

    /// Parse a `{ ... }` body already past the opening brace; consumes the `}`.
    /// A nested `def` is hoisted into `self.funcs` (slice-1 has a flat function
    /// namespace) rather than becoming a statement.
    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        self.skip_seps();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            if self.is(&Tok::Def) {
                let f = self.parse_def()?;
                self.funcs.push(f);
            } else {
                out.push(self.statement()?);
            }
            self.skip_seps();
        }
        self.eat(&Tok::RBrace)?;
        Ok(out)
    }

    /// Parse a `{ ... }` or a single statement into a statement list. Leading
    /// separators (a line break after the header) are tolerated.
    fn braced_or_single(&mut self) -> Result<Vec<Stmt>, String> {
        self.skip_seps();
        if self.is(&Tok::LBrace) {
            self.advance();
            self.block()
        } else {
            Ok(vec![self.statement()?])
        }
    }

    /// Parse one statement, tagging it with the source line of its first token
    /// (the single choke point every statement — top-level or nested — flows
    /// through, so `--dap` markers land on real lines).
    fn statement(&mut self) -> Result<Stmt, String> {
        let line = self.line();
        let kind = self.statement_kind()?;
        Ok(Stmt { line, kind })
    }

    fn statement_kind(&mut self) -> Result<StmtKind, String> {
        match self.peek() {
            Tok::If => self.if_stmt(),
            Tok::While => self.while_stmt(),
            Tok::For => self.for_stmt(),
            Tok::Val | Tok::Var => self.local_decl(),
            Tok::Return => self.return_stmt(),
            Tok::LBrace => {
                self.advance();
                // A bare block: flatten into a single synthetic if-true. Slice 1
                // has no lexical scopes, so inlining is behavior-preserving.
                let body = self.block()?;
                Ok(StmtKind::If {
                    cond: Expr::Bool(true),
                    then: body,
                    els: vec![],
                })
            }
            _ => self.simple_statement(),
        }
    }

    /// `return [expr]` — an early exit from the enclosing `def`. A bare `return`
    /// (followed by a statement separator or `}`) yields `Unit`.
    fn return_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::Return)?;
        if self.is(&Tok::Newline)
            || self.is(&Tok::Semi)
            || self.is(&Tok::RBrace)
            || self.is(&Tok::Eof)
        {
            Ok(StmtKind::Return(None))
        } else {
            Ok(StmtKind::Return(Some(self.expression()?)))
        }
    }

    /// A `val`/`var` binding: `val x = e`, `var y: Int = e`.
    fn local_decl(&mut self) -> Result<StmtKind, String> {
        let is_val = matches!(self.peek(), Tok::Val);
        self.advance(); // val/var
        let name = self.ident()?;
        let ty = if self.is(&Tok::Colon) {
            self.advance();
            Some(self.type_ref()?)
        } else {
            None
        };
        let init = if self.is(&Tok::Assign) {
            self.advance();
            Some(self.expression()?)
        } else {
            None
        };
        Ok(StmtKind::Local {
            is_val,
            ty,
            name,
            init,
        })
    }

    /// Consume a type reference after `:` (`Int`, `Array[String]`, `a.b.C`) and
    /// return it as a string for diagnostics. Types do not gate execution.
    fn type_ref(&mut self) -> Result<String, String> {
        let mut s = String::new();
        loop {
            match self.peek().clone() {
                Tok::Ident(w) => {
                    s.push_str(&w);
                    self.advance();
                }
                Tok::Dot => {
                    s.push('.');
                    self.advance();
                }
                Tok::LBracket => {
                    let mut depth = 0;
                    loop {
                        match self.advance() {
                            Tok::LBracket => depth += 1,
                            Tok::RBracket => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            Tok::Eof => break,
                            _ => {}
                        }
                    }
                    s.push_str("[]");
                }
                _ => break,
            }
        }
        if s.is_empty() {
            return Err(format!(
                "scalars: expected a type after `:` on line {}",
                self.line()
            ));
        }
        Ok(s)
    }

    /// Assignment (`x = e`, `x += e`) or a bare expression statement.
    fn simple_statement(&mut self) -> Result<StmtKind, String> {
        if let Tok::Ident(name) = self.peek().clone() {
            let next = &self.toks[self.pos + 1].kind;
            if let Some(op) = assign_op(next) {
                self.advance(); // name
                self.advance(); // op
                let value = self.expression()?;
                return Ok(StmtKind::Assign { name, op, value });
            }
        }
        Ok(StmtKind::Expr(self.expression()?))
    }

    fn if_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::If)?;
        self.eat(&Tok::LParen)?;
        let cond = self.expression()?;
        self.eat(&Tok::RParen)?;
        let then = self.braced_or_single()?;
        // A line break may precede `else`; the lexer does not separate before
        // `else`, but tolerate an explicit one defensively.
        let save = self.pos;
        self.skip_seps();
        let els = if self.is(&Tok::Else) {
            self.advance();
            self.braced_or_single()?
        } else {
            self.pos = save;
            vec![]
        };
        Ok(StmtKind::If { cond, then, els })
    }

    fn while_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::While)?;
        self.eat(&Tok::LParen)?;
        let cond = self.expression()?;
        self.eat(&Tok::RParen)?;
        let body = self.braced_or_single()?;
        Ok(StmtKind::While { cond, body })
    }

    /// `for (name <- start until|to end) <body>` — a range comprehension in
    /// statement position.
    fn for_stmt(&mut self) -> Result<StmtKind, String> {
        self.eat(&Tok::For)?;
        self.eat(&Tok::LParen)?;
        let name = self.ident()?;
        self.eat(&Tok::LArrow)?;
        let start = self.expression()?;
        let inclusive = match self.peek().clone() {
            Tok::Ident(w) if w == "until" => {
                self.advance();
                false
            }
            Tok::Ident(w) if w == "to" => {
                self.advance();
                true
            }
            other => {
                return Err(format!(
                    "scalars: only `<- a until b` / `<- a to b` ranges are supported in `for` (found {other}) on line {}",
                    self.line()
                ))
            }
        };
        let end = self.expression()?;
        if matches!(self.peek(), Tok::Ident(w) if w == "by") {
            return Err(format!(
                "scalars: `by` step in `for` ranges is not supported yet (line {})",
                self.line()
            ));
        }
        self.eat(&Tok::RParen)?;
        if matches!(self.peek(), Tok::Ident(w) if w == "yield") {
            return Err(format!(
                "scalars: `for … yield` comprehensions are not supported yet (line {})",
                self.line()
            ));
        }
        let body = self.braced_or_single()?;
        Ok(StmtKind::For {
            name,
            start,
            end,
            inclusive,
            body,
        })
    }

    // ── expressions (precedence climbing) ─────────────────────────────────

    fn expression(&mut self) -> Result<Expr, String> {
        self.binary(0)
    }

    fn binary(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        while let Some((op, bp)) = binop(self.peek()) {
            if bp < min_bp {
                break;
            }
            self.advance();
            let rhs = self.binary(bp + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Tok::Minus => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    rhs: Box::new(self.unary()?),
                })
            }
            Tok::Not => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    rhs: Box::new(self.unary()?),
                })
            }
            _ => self.postfix(),
        }
    }

    /// Postfix method/field dispatch: `primary ( '.' member [ '(' args ')' ] )*`.
    /// A paren-less member (`s.length`, `n.toString`) is a zero-argument call;
    /// chains left-associatively (`s.trim.length`).
    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        while self.is(&Tok::Dot) {
            self.advance(); // `.`
            let line = self.line();
            let name = self.ident()?;
            let args = if self.is(&Tok::LParen) {
                self.advance();
                let mut args = Vec::new();
                if !self.is(&Tok::RParen) {
                    loop {
                        args.push(self.expression()?);
                        if self.is(&Tok::Comma) {
                            self.advance();
                            self.skip_seps();
                        } else {
                            break;
                        }
                    }
                }
                self.eat(&Tok::RParen)?;
                args
            } else {
                Vec::new()
            };
            e = Expr::Method {
                recv: Box::new(e),
                name,
                args,
                line,
            };
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.advance();
                Ok(Expr::Int(n))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Float(f))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::Null => {
                self.advance();
                Ok(Expr::Null)
            }
            Tok::LParen => {
                self.advance();
                let e = self.expression()?;
                self.eat(&Tok::RParen)?;
                Ok(e)
            }
            Tok::Ident(name) => {
                // `println(...)` / `print(...)`, a named call `name(args)`, or a
                // var read. Postfix `.member` dispatch is layered on in `postfix`.
                if (name == "println" || name == "print")
                    && matches!(self.toks[self.pos + 1].kind, Tok::LParen)
                {
                    return self.print_call(name == "println");
                }
                let line = self.line();
                self.advance();
                if self.is(&Tok::LParen) {
                    return self.call(name, line);
                }
                Ok(Expr::Var(name))
            }
            other => Err(format!(
                "scalars: unexpected token {other} in expression on line {}",
                self.line()
            )),
        }
    }

    /// Parse a named call `name(arg, arg, …)` — the cursor is on the `(` and
    /// `name`/`line` are already consumed. Resolved in the compiler: either the
    /// `__rust_compile` FFI-block builtin or a call to an FFI-exported bareword.
    fn call(&mut self, name: String, line: u32) -> Result<Expr, String> {
        self.eat(&Tok::LParen)?;
        let mut args = Vec::new();
        if !self.is(&Tok::RParen) {
            loop {
                args.push(self.expression()?);
                if self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                } else {
                    break;
                }
            }
        }
        self.eat(&Tok::RParen)?;
        Ok(Expr::Call { name, args, line })
    }

    /// Parse `println(arg)` / `print(arg)` (the cursor is on the ident).
    fn print_call(&mut self, newline: bool) -> Result<Expr, String> {
        self.advance(); // println/print
        self.eat(&Tok::LParen)?;
        let arg = if self.is(&Tok::RParen) {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        self.eat(&Tok::RParen)?;
        Ok(Expr::Println { newline, arg })
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.advance() {
            Tok::Ident(s) => Ok(s),
            other => Err(format!(
                "scalars: expected an identifier but found {other} on line {}",
                self.line()
            )),
        }
    }
}

/// Map a token to a compound-assignment operator, if it is one.
fn assign_op(t: &Tok) -> Option<AssignOp> {
    Some(match t {
        Tok::Assign => AssignOp::Assign,
        Tok::PlusAssign => AssignOp::Add,
        Tok::MinusAssign => AssignOp::Sub,
        Tok::StarAssign => AssignOp::Mul,
        Tok::SlashAssign => AssignOp::Div,
        Tok::PercentAssign => AssignOp::Mod,
        _ => return None,
    })
}

/// Binary operator + its binding power (higher binds tighter).
fn binop(t: &Tok) -> Option<(BinOp, u8)> {
    Some(match t {
        Tok::OrOr => (BinOp::Or, 1),
        Tok::AndAnd => (BinOp::And, 2),
        Tok::EqEq => (BinOp::Eq, 3),
        Tok::NotEq => (BinOp::Ne, 3),
        Tok::Lt => (BinOp::Lt, 4),
        Tok::Gt => (BinOp::Gt, 4),
        Tok::Le => (BinOp::Le, 4),
        Tok::Ge => (BinOp::Ge, 4),
        Tok::Plus => (BinOp::Add, 5),
        Tok::Minus => (BinOp::Sub, 5),
        Tok::Star => (BinOp::Mul, 6),
        Tok::Slash => (BinOp::Div, 6),
        Tok::Percent => (BinOp::Mod, 6),
        _ => return None,
    })
}
