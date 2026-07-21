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
            // A `for` in statement position is the comprehension expression run
            // for effect (its `Vector`/`Unit` value is discarded).
            Tok::For => Ok(StmtKind::Expr(self.for_comprehension()?)),
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

    /// `for (enums) [yield] body` — a range comprehension. With `yield` it is an
    /// [`Expr::ForYield`] (collects a `Vector`); without, an [`Expr::ForEach`]
    /// (side-effecting, `Unit`). Enumerators are `;`-separated inside the parens
    /// (the lexer suppresses newlines there, so `;` is the separator).
    fn for_comprehension(&mut self) -> Result<Expr, String> {
        self.eat(&Tok::For)?;
        self.eat(&Tok::LParen)?;
        let enums = self.for_enums()?;
        self.eat(&Tok::RParen)?;
        if matches!(self.peek(), Tok::Ident(w) if w == "yield") {
            self.advance();
            // A `yield` body is a value expression (or block expression).
            let body = self.branch_expr()?;
            Ok(Expr::ForYield {
                enums,
                body: Box::new(body),
            })
        } else {
            // A side-effecting body is a statement block, so assignments
            // (`for (i <- …) s += i`) parse as statements, not expressions.
            self.skip_seps();
            let body = if self.is(&Tok::LBrace) {
                self.advance();
                Expr::Block(self.block()?)
            } else {
                Expr::Block(vec![self.statement()?])
            };
            Ok(Expr::ForEach {
                enums,
                body: Box::new(body),
            })
        }
    }

    /// Parse the enumerator list of a `for (...)`: one or more range generators
    /// (`name <- a until|to b`) with optional trailing `if` guards, separated by
    /// `;`.
    fn for_enums(&mut self) -> Result<Vec<ForEnum>, String> {
        let mut out = Vec::new();
        loop {
            self.skip_seps();
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
            out.push(ForEnum::Gen {
                name,
                start,
                end,
                inclusive,
            });
            // Trailing `if` guards bind to the generator just parsed.
            while self.is(&Tok::If) {
                self.advance();
                out.push(ForEnum::Guard(self.expression()?));
            }
            // Another enumerator follows only after an explicit `;`.
            if self.is(&Tok::Semi) {
                self.skip_seps();
                if self.is(&Tok::RParen) {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(out)
    }

    // ── expressions (precedence climbing) ─────────────────────────────────

    fn expression(&mut self) -> Result<Expr, String> {
        // `match` binds looser than every binary operator (`a + b match { … }`
        // matches on the sum), so it wraps the fully-parsed binary expression.
        let mut e = self.binary(0)?;
        while self.is(&Tok::Match) {
            e = self.match_expr(e)?;
        }
        Ok(e)
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
            // `if` in expression position: `val r = if (c) a else b`.
            Tok::If => self.if_expr(),
            // `for (...) yield …` / `for (...) …` in expression position.
            Tok::For => self.for_comprehension(),
            // An interpolated string `s"…"` / `f"…"` / `raw"…"` desugars to a
            // concatenation of its literal parts and (formatted) splices.
            Tok::InterpStr {
                raw,
                is_f,
                parts,
                exprs,
                fmts,
            } => {
                self.advance();
                self.build_interp(raw, is_f, &parts, &exprs, &fmts)
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

    /// `if (cond) then [else els]` as a value-producing expression. Each branch
    /// is a single expression or a `{ … }` block (an [`Expr::Block`]); a missing
    /// `else` yields `Unit`.
    fn if_expr(&mut self) -> Result<Expr, String> {
        self.eat(&Tok::If)?;
        self.eat(&Tok::LParen)?;
        let cond = self.expression()?;
        self.eat(&Tok::RParen)?;
        let then = self.branch_expr()?;
        // A line break may precede `else`; tolerate one defensively.
        let save = self.pos;
        self.skip_seps();
        let els = if self.is(&Tok::Else) {
            self.advance();
            Some(Box::new(self.branch_expr()?))
        } else {
            self.pos = save;
            None
        };
        Ok(Expr::If {
            cond: Box::new(cond),
            then: Box::new(then),
            els,
        })
    }

    /// An `if`/`match`-branch expression: a `{ … }` block (parsed to an
    /// [`Expr::Block`]) or a single expression.
    fn branch_expr(&mut self) -> Result<Expr, String> {
        self.skip_seps();
        if self.is(&Tok::LBrace) {
            self.advance();
            Ok(Expr::Block(self.block()?))
        } else {
            self.expression()
        }
    }

    /// `scrutinee match { case … }` — the cursor is on `match`, `scrut` already
    /// parsed.
    fn match_expr(&mut self, scrut: Expr) -> Result<Expr, String> {
        self.eat(&Tok::Match)?;
        self.skip_seps();
        self.eat(&Tok::LBrace)?;
        self.skip_seps();
        let mut arms = Vec::new();
        while self.is(&Tok::Case) {
            arms.push(self.match_arm()?);
            self.skip_seps();
        }
        self.eat(&Tok::RBrace)?;
        if arms.is_empty() {
            return Err(format!(
                "scalars: `match` requires at least one `case` (line {})",
                self.line()
            ));
        }
        Ok(Expr::Match {
            scrut: Box::new(scrut),
            arms,
        })
    }

    /// One `case pat [if guard] => body` arm. The body runs to the next `case`,
    /// the closing `}`, or EOF.
    fn match_arm(&mut self) -> Result<MatchArm, String> {
        self.eat(&Tok::Case)?;
        let pat = self.pattern()?;
        let guard = if self.is(&Tok::If) {
            self.advance();
            Some(self.expression()?)
        } else {
            None
        };
        self.eat(&Tok::FatArrow)?;
        self.skip_seps();
        let mut body = Vec::new();
        while !self.is(&Tok::Case) && !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            body.push(self.statement()?);
            self.skip_seps();
        }
        Ok(MatchArm { pat, guard, body })
    }

    /// A `case` pattern: literal, `_` wildcard, variable binding, or typed
    /// `name: Type` / `_: Type`. Constructor/extractor patterns (`Foo(x)`) are
    /// rejected — the fusevm value model has no ordered record value to bind.
    fn pattern(&mut self) -> Result<Pattern, String> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.advance();
                Ok(Pattern::Literal(Expr::Int(n)))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Pattern::Literal(Expr::Float(f)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Pattern::Literal(Expr::Str(s)))
            }
            Tok::True => {
                self.advance();
                Ok(Pattern::Literal(Expr::Bool(true)))
            }
            Tok::False => {
                self.advance();
                Ok(Pattern::Literal(Expr::Bool(false)))
            }
            Tok::Null => {
                self.advance();
                Ok(Pattern::Literal(Expr::Null))
            }
            // A negative numeric literal pattern (`case -1 =>`).
            Tok::Minus => {
                self.advance();
                match self.advance() {
                    Tok::Int(n) => Ok(Pattern::Literal(Expr::Int(-n))),
                    Tok::Float(f) => Ok(Pattern::Literal(Expr::Float(-f))),
                    other => Err(format!(
                        "scalars: expected a numeric literal after `-` in a pattern, found {other} (line {})",
                        self.line()
                    )),
                }
            }
            Tok::Ident(name) => {
                self.advance();
                // `Foo(...)` — a constructor/extractor pattern (fusevm-blocked).
                if self.is(&Tok::LParen) {
                    return Err(format!(
                        "scalars: constructor patterns (`case {name}(…)`) are not supported (line {})",
                        self.line()
                    ));
                }
                if self.is(&Tok::Colon) {
                    self.advance();
                    let ty = self.type_ref()?;
                    Ok(Pattern::Typed { name, ty })
                } else if name == "_" {
                    Ok(Pattern::Wildcard)
                } else {
                    Ok(Pattern::Bind(name))
                }
            }
            other => Err(format!(
                "scalars: unsupported pattern {other} on line {}",
                self.line()
            )),
        }
    }

    /// Build the desugared concatenation for an interpolated string. `s`/`raw`
    /// splices concatenate directly; `f` splices wrap each value in an
    /// [`Expr::Format`] with its spec (defaulting to `%s`). A leading empty
    /// `parts[0]` (`s"$x"`) still makes the whole chain a `String.+`, so a
    /// numeric first splice concatenates rather than adds.
    fn build_interp(
        &mut self,
        _raw: bool,
        is_f: bool,
        parts: &[String],
        exprs: &[String],
        fmts: &[Option<String>],
    ) -> Result<Expr, String> {
        let mut result = Expr::Str(parts[0].clone());
        for (idx, esrc) in exprs.iter().enumerate() {
            let mut val = parse_fragment(esrc)?;
            if is_f {
                let spec = fmts[idx].clone().unwrap_or_else(|| "%s".to_string());
                val = Expr::Format {
                    value: Box::new(val),
                    spec,
                    line: 0,
                };
            }
            result = Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(result),
                rhs: Box::new(val),
            };
            result = Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(result),
                rhs: Box::new(Expr::Str(parts[idx + 1].clone())),
            };
        }
        Ok(result)
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

/// Parse a single expression from a splice source fragment (an interpolation's
/// `$id` / `${expr}`). Runs a fresh sub-parser so the fragment can be any
/// expression the grammar accepts.
fn parse_fragment(src: &str) -> Result<Expr, String> {
    let tokens = crate::lexer::lex(src)?;
    let mut p = Parser {
        toks: tokens,
        pos: 0,
        funcs: Vec::new(),
    };
    p.skip_seps();
    p.expression()
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
