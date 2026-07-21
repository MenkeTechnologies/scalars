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
        classes: Vec::new(),
        objects: Vec::new(),
    };
    p.program()
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// User-defined `def`s collected while parsing (both object members and
    /// `def`s hoisted out of a body block). Flattened into `Program::functions`.
    funcs: Vec<Func>,
    /// Top-level `class`/`case class` declarations.
    classes: Vec<ClassDecl>,
    /// Top-level non-entry `object`/`case object` declarations.
    objects: Vec<ObjectDecl>,
}

/// The outcome of parsing a top-level `object`: the program entry point, or a
/// singleton object declaration.
enum TopObject {
    /// The entry object — `(object_name, entry-point body)`. Its helper `def`s
    /// were hoisted into `self.funcs`.
    Entry(String, Vec<Stmt>),
    /// A non-entry singleton `object`/`case object`.
    Singleton(ObjectDecl),
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

    /// A compilation unit: `[package/import] (class | object | case class | case
    /// object)*`. Exactly one `object` is the entry point (it `extends App` or
    /// declares `def main`); the rest are sibling `class`/`object` declarations.
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

        let mut entry: Option<(String, Vec<Stmt>)> = None;
        loop {
            self.skip_seps();
            if self.is(&Tok::Eof) {
                break;
            }
            // Leading modifiers/annotations (`final`, `sealed`, `abstract`, …)
            // arrive as bare idents; skip to the declaration keyword. `case`,
            // `object`, and the `class` ident all stop the skip.
            while !self.is(&Tok::Eof)
                && !self.is(&Tok::Object)
                && !self.is(&Tok::Case)
                && !matches!(self.peek(), Tok::Ident(w) if w == "class" || w == "trait")
            {
                self.advance();
            }
            if self.is(&Tok::Eof) {
                break;
            }
            // `case` prefix → `case class` / `case object`.
            let is_case = if self.is(&Tok::Case) {
                self.advance();
                true
            } else {
                false
            };
            if self.is(&Tok::Object) {
                match self.object_decl(is_case)? {
                    TopObject::Entry(name, body) => {
                        if entry.is_some() {
                            return Err(
                                "scalars: multiple entry objects (`extends App` / `def main`)"
                                    .to_string(),
                            );
                        }
                        entry = Some((name, body));
                    }
                    TopObject::Singleton(obj) => self.objects.push(obj),
                }
            } else if matches!(self.peek(), Tok::Ident(w) if w == "class") {
                let c = self.class_decl(is_case)?;
                self.classes.push(c);
            } else if matches!(self.peek(), Tok::Ident(w) if w == "trait") {
                // Traits are not modeled yet; skip the declaration body.
                self.advance();
                let _ = self.ident();
                self.skip_member()?;
            } else if !self.is(&Tok::Eof) {
                return Err(format!(
                    "scalars: expected a top-level `object`/`class` declaration, found {} on line {}",
                    self.peek(),
                    self.line()
                ));
            }
        }

        match entry {
            Some((object_name, main)) => Ok(Program {
                object_name,
                main,
                functions: std::mem::take(&mut self.funcs),
                classes: std::mem::take(&mut self.classes),
                objects: std::mem::take(&mut self.objects),
            }),
            None => Err(
                "scalars: no entry object (`extends App` or `def main(args: Array[String])`)"
                    .to_string(),
            ),
        }
    }

    /// Parse an `object`/`case object` declaration (cursor on `object`). Returns
    /// the program entry if it `extends App` or declares `def main`, otherwise a
    /// singleton declaration.
    fn object_decl(&mut self, is_case: bool) -> Result<TopObject, String> {
        self.eat(&Tok::Object)?;
        let name = self.ident()?;
        // `extends Parent with Trait …` — note whether `App` is a parent.
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
            return Ok(TopObject::Entry(name, body));
        }

        // Scan members into defs / body statements / an optional `main`.
        let mut defs = Vec::new();
        let mut body = Vec::new();
        let mut main = None;
        self.skip_seps();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            if self.is(&Tok::Def) {
                if let Some(m) = self.try_main()? {
                    main = Some(m);
                } else {
                    defs.push(self.parse_def()?);
                }
            } else if self.is(&Tok::Val) || self.is(&Tok::Var) {
                body.push(self.statement()?);
            } else {
                self.skip_member()?;
            }
            self.skip_seps();
        }
        self.eat(&Tok::RBrace)?;

        if let Some(main) = main {
            // Entry object via `def main`: its helper `def`s join the flat
            // function namespace; object-level `val`s are ignored (as before).
            self.funcs.extend(defs);
            Ok(TopObject::Entry(name, main))
        } else {
            Ok(TopObject::Singleton(ObjectDecl {
                name,
                is_case,
                body,
                methods: defs,
            }))
        }
    }

    /// Parse a `class`/`case class` declaration (cursor on the `class` ident).
    /// The primary constructor's parameters become instance fields; the class
    /// body's `val`/`var` declarations become further fields (initialized by the
    /// constructor) and its `def`s become methods.
    fn class_decl(&mut self, is_case: bool) -> Result<ClassDecl, String> {
        self.advance(); // `class`
        let name = self.ident()?;
        // Optional `[T, …]` type-parameter clause.
        if self.is(&Tok::LBracket) {
            self.skip_bracket_group();
        }
        // Primary-constructor parameters (all become fields).
        let mut params = Vec::new();
        if self.is(&Tok::LParen) {
            self.advance();
            self.skip_seps();
            while !self.is(&Tok::RParen) && !self.is(&Tok::Eof) {
                // Optional `val`/`var`/modifier before the parameter name.
                while self.is(&Tok::Val) || self.is(&Tok::Var) {
                    self.advance();
                }
                let pname = self.ident()?;
                if self.is(&Tok::Colon) {
                    self.advance();
                    self.type_ref()?;
                }
                // A default value (`x: Int = 0`) is parsed and ignored.
                if self.is(&Tok::Assign) {
                    self.advance();
                    self.expression()?;
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
        // Optional `extends …` clause — consumed and ignored (no inheritance).
        if self.is(&Tok::Extends) {
            self.advance();
            while !self.is(&Tok::LBrace)
                && !self.is(&Tok::Newline)
                && !self.is(&Tok::Semi)
                && !self.is(&Tok::Eof)
            {
                self.advance();
            }
        }
        // Class body (optional). `def`s → methods; `val`/`var` → fields + ctor
        // statements; anything else → a constructor side-effect statement.
        let mut body = Vec::new();
        let mut methods = Vec::new();
        let mut field_names = params.clone();
        if self.is(&Tok::LBrace) {
            self.advance();
            self.skip_seps();
            while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
                if self.is(&Tok::Def) {
                    methods.push(self.parse_def()?);
                } else {
                    let st = self.statement()?;
                    if let StmtKind::Local { name, .. } = &st.kind {
                        field_names.push(name.clone());
                    }
                    body.push(st);
                }
                self.skip_seps();
            }
            self.eat(&Tok::RBrace)?;
        }
        Ok(ClassDecl {
            name,
            is_case,
            params,
            body,
            field_names,
            methods,
        })
    }

    /// Consume a `[ … ]` group, balancing nested brackets. The cursor is on `[`.
    fn skip_bracket_group(&mut self) {
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
    /// A type reference that may include a function-type arrow (`Int => Int`).
    /// Used everywhere a `=>` after the type is unambiguously part of the type
    /// (`val`/`def`/parameter/lambda-parameter annotations).
    fn type_ref(&mut self) -> Result<String, String> {
        self.type_ref_inner(true)
    }

    /// A type reference in a context where a following `=>` is NOT part of the
    /// type (a `case name: Type =>` pattern, where `=>` is the arm separator).
    fn type_ref_no_arrow(&mut self) -> Result<String, String> {
        self.type_ref_inner(false)
    }

    fn type_ref_inner(&mut self, allow_arrow: bool) -> Result<String, String> {
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
                // A function type: `Int => Int`, `(Int, Int) => Int`. The `=>`
                // and its result type are consumed so the following `= <init>`
                // parses (the type is diagnostic-only). Suppressed in a pattern,
                // where `=>` is the arm separator.
                Tok::FatArrow if allow_arrow => {
                    s.push_str("=>");
                    self.advance();
                }
                // A parenthesized parameter-type tuple in a function type. Balance
                // the group (it is not a lambda — this is a type position).
                Tok::LParen => {
                    let mut depth = 0;
                    loop {
                        match self.advance() {
                            Tok::LParen => depth += 1,
                            Tok::RParen => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            Tok::Eof => break,
                            _ => {}
                        }
                    }
                    s.push_str("()");
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

    /// Whether the cursor is at an assignment target (`ident <assign-op>`), used
    /// to route a lambda body like `x => acc += x` through statement parsing.
    fn assignment_ahead(&self) -> bool {
        matches!(self.peek(), Tok::Ident(_))
            && self
                .toks
                .get(self.pos + 1)
                .is_some_and(|t| assign_op(&t.kind).is_some())
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
            // `a until b` / `a to b` is a range generator; anything else is a
            // collection generator (`x <- List(…)`), desugared to `.map`/`.flatMap`.
            let inclusive = match self.peek().clone() {
                Tok::Ident(w) if w == "until" => {
                    self.advance();
                    false
                }
                Tok::Ident(w) if w == "to" => {
                    self.advance();
                    true
                }
                _ => {
                    out.push(ForEnum::GenColl { name, coll: start });
                    // Trailing `if` guards bind to the generator just parsed.
                    while self.is(&Tok::If) {
                        self.advance();
                        out.push(ForEnum::Guard(self.expression()?));
                    }
                    if self.is(&Tok::Semi) {
                        self.skip_seps();
                        if self.is(&Tok::RParen) {
                            break;
                        }
                        continue;
                    }
                    break;
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
        // A function literal (`x => e`, `(a, b) => e`) is a complete expression.
        if self.at_lambda_start() {
            return self.lambda();
        }
        // `match` binds looser than every binary operator (`a + b match { … }`
        // matches on the sum), so it wraps the fully-parsed binary expression.
        let mut e = self.binary(0)?;
        // `a -> b` — the tuple-pair sugar (binds looser than every binary op).
        if self.is(&Tok::RArrow) {
            self.advance();
            let rhs = if self.at_lambda_start() {
                self.lambda()?
            } else {
                self.binary(0)?
            };
            e = Expr::Tuple(vec![e, rhs]);
        }
        while self.is(&Tok::Match) {
            e = self.match_expr(e)?;
        }
        Ok(e)
    }

    /// Whether the cursor begins a lambda: `ident =>` or a `( … ) =>` parameter
    /// list. Scans a balanced paren group to find the token after `)`.
    fn at_lambda_start(&self) -> bool {
        match self.peek() {
            Tok::Ident(_) => matches!(
                self.toks.get(self.pos + 1).map(|t| &t.kind),
                Some(Tok::FatArrow)
            ),
            Tok::LParen => {
                let mut depth = 0;
                let mut i = self.pos;
                while i < self.toks.len() {
                    match &self.toks[i].kind {
                        Tok::LParen => depth += 1,
                        Tok::RParen => {
                            depth -= 1;
                            if depth == 0 {
                                return matches!(
                                    self.toks.get(i + 1).map(|t| &t.kind),
                                    Some(Tok::FatArrow)
                                );
                            }
                        }
                        Tok::Eof => return false,
                        _ => {}
                    }
                    i += 1;
                }
                false
            }
            _ => false,
        }
    }

    /// Parse a function literal `params => body`. `params` is a single bare
    /// identifier or a parenthesized (optionally typed) list; `body` is a single
    /// expression or a `{ … }` block.
    fn lambda(&mut self) -> Result<Expr, String> {
        let mut params = Vec::new();
        if self.is(&Tok::LParen) {
            self.advance();
            self.skip_seps();
            while !self.is(&Tok::RParen) && !self.is(&Tok::Eof) {
                let name = self.ident()?;
                if self.is(&Tok::Colon) {
                    self.advance();
                    self.type_ref()?;
                }
                params.push(name);
                if self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                } else {
                    break;
                }
            }
            self.eat(&Tok::RParen)?;
        } else {
            params.push(self.ident()?);
        }
        self.eat(&Tok::FatArrow)?;
        self.skip_seps();
        let body = if self.is(&Tok::LBrace) {
            self.advance();
            Expr::Block(self.block()?)
        } else if self.assignment_ahead() {
            // A lambda whose body is an assignment (`x => acc += x`) is a `Unit`
            // statement, not an expression — wrap it in a single-statement block.
            Expr::Block(vec![self.statement()?])
        } else {
            self.expression()?
        };
        Ok(Expr::Lambda {
            params,
            body: Box::new(body),
        })
    }

    fn binary(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        while let Some((op, bp)) = binop(self.peek()) {
            if bp < min_bp {
                break;
            }
            self.advance();
            // `::` (cons) is right-associative: `1 :: 2 :: Nil` == `1 :: (2 :: Nil)`.
            let rhs = if op == BinOp::Cons {
                self.binary(bp)?
            } else {
                self.binary(bp + 1)?
            };
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
            // `.copy(field = e, …)` — a `case class` copy with named/positional
            // updates. Named args (`field =`) are not general-purpose method args
            // in this frontend, so `copy` is parsed specially.
            if name == "copy" && self.is(&Tok::LParen) {
                let updates = self.copy_updates()?;
                e = Expr::Copy {
                    recv: Box::new(e),
                    updates,
                    line,
                };
                continue;
            }
            // Optional `[T]` type arguments on a method (`xs.map[Int](f)`).
            if self.is(&Tok::LBracket) {
                self.skip_bracket_group();
            }
            // A method may be curried (`foldLeft(z)(op)`); flatten every trailing
            // argument list into the one call.
            let mut args = Vec::new();
            while self.is(&Tok::LParen) {
                args.extend(self.arg_list()?);
            }
            e = Expr::Method {
                recv: Box::new(e),
                name,
                args,
                line,
            };
        }
        // A trailing application on a value expression (`f(x)`, `xs(i)`) when the
        // receiver is not a `.method` chain — e.g. `getFn()(arg)`. `apply`.
        while self.is(&Tok::LParen) {
            let line = self.line();
            let args = self.arg_list()?;
            e = Expr::Method {
                recv: Box::new(e),
                name: "apply".to_string(),
                args,
                line,
            };
        }
        Ok(e)
    }

    /// Parse a `.copy(...)` update list (cursor on `(`): comma-separated
    /// `field = expr` (named) or bare `expr` (positional) updates.
    fn copy_updates(&mut self) -> Result<Vec<(Option<String>, Expr)>, String> {
        self.eat(&Tok::LParen)?;
        let mut updates = Vec::new();
        if !self.is(&Tok::RParen) {
            loop {
                // A named update is `ident =` (a single `=`, not `==`).
                let named = if let Tok::Ident(fname) = self.peek().clone() {
                    if matches!(self.toks[self.pos + 1].kind, Tok::Assign) {
                        self.advance(); // field
                        self.advance(); // =
                        Some(fname)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let value = self.expression()?;
                updates.push((named, value));
                if self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                } else {
                    break;
                }
            }
        }
        self.eat(&Tok::RParen)?;
        Ok(updates)
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
            // `new Class(args)` — construct a host-heap instance.
            Tok::New => self.new_expr(),
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
            // `( e )` grouping, or a tuple literal `(a, b, …)`. A `( … ) =>`
            // lambda is caught earlier in `expression`.
            Tok::LParen => {
                self.advance();
                self.skip_seps();
                let mut elems = vec![self.expression()?];
                while self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                    elems.push(self.expression()?);
                }
                self.eat(&Tok::RParen)?;
                if elems.len() == 1 {
                    Ok(elems.pop().unwrap())
                } else {
                    Ok(Expr::Tuple(elems))
                }
            }
            Tok::Ident(name) => {
                let next = self.toks.get(self.pos + 1).map(|t| &t.kind);
                // A bare `_` is an argument placeholder (rewritten to a lambda by
                // `wrap_placeholders` at the enclosing argument boundary).
                if name == "_" && !matches!(next, Some(Tok::LParen)) {
                    self.advance();
                    return Ok(Expr::Placeholder);
                }
                // `println(...)` / `print(...)`, or bare `println` as a function
                // value (eta-expansion to `x => println(x)`, for `xs.foreach(println)`).
                if name == "println" || name == "print" {
                    let newline = name == "println";
                    if matches!(next, Some(Tok::LParen)) {
                        return self.print_call(newline);
                    }
                    self.advance();
                    return Ok(Expr::Lambda {
                        params: vec!["$eta".to_string()],
                        body: Box::new(Expr::Println {
                            newline,
                            arg: Some(Box::new(Expr::Var("$eta".to_string()))),
                        }),
                    });
                }
                let line = self.line();
                self.advance();
                // Optional generic type arguments (`List[Int](…)`, `foo[T](…)`).
                if self.is(&Tok::LBracket) {
                    self.skip_bracket_group();
                }
                if self.is(&Tok::LParen) {
                    // `List(...)` / `Map(...)` collection literals.
                    if name == "List" || name == "Map" {
                        let elems = self.arg_list()?;
                        return Ok(Expr::Collection { ctor: name, elems });
                    }
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
                args.push(wrap_placeholders(self.expression()?));
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

    /// `new Class[Types](args)` — construct a host-heap instance (the cursor is
    /// on `new`). Type arguments are consumed and ignored; the argument list is
    /// optional (`new Empty`).
    fn new_expr(&mut self) -> Result<Expr, String> {
        self.eat(&Tok::New)?;
        let line = self.line();
        let name = self.ident()?;
        if self.is(&Tok::LBracket) {
            self.skip_bracket_group();
        }
        let args = if self.is(&Tok::LParen) {
            self.arg_list()?
        } else {
            Vec::new()
        };
        Ok(Expr::New { name, args, line })
    }

    /// Parse a parenthesized, comma-separated positional argument list (cursor on
    /// `(`); consumes the closing `)`.
    fn arg_list(&mut self) -> Result<Vec<Expr>, String> {
        self.eat(&Tok::LParen)?;
        let mut args = Vec::new();
        if !self.is(&Tok::RParen) {
            loop {
                let a = wrap_placeholders(self.expression()?);
                args.push(a);
                if self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                } else {
                    break;
                }
            }
        }
        self.eat(&Tok::RParen)?;
        Ok(args)
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
                // `Foo(sub, …)` — a constructor/extractor pattern.
                if self.is(&Tok::LParen) {
                    self.advance();
                    self.skip_seps();
                    let mut elems = Vec::new();
                    if !self.is(&Tok::RParen) {
                        loop {
                            elems.push(self.pattern()?);
                            if self.is(&Tok::Comma) {
                                self.advance();
                                self.skip_seps();
                            } else {
                                break;
                            }
                        }
                    }
                    self.eat(&Tok::RParen)?;
                    return Ok(Pattern::Constructor { name, elems });
                }
                if self.is(&Tok::Colon) {
                    self.advance();
                    let ty = self.type_ref_no_arrow()?;
                    Ok(Pattern::Typed { name, ty })
                } else if name == "_" {
                    Ok(Pattern::Wildcard)
                } else if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    // A capitalized bare identifier is a stable-identifier
                    // pattern (`case None =>`), matched by `==`, not a binding.
                    Ok(Pattern::Stable(name))
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
        classes: Vec::new(),
        objects: Vec::new(),
    };
    p.skip_seps();
    p.expression()
}

/// If `e` contains underscore placeholders, rewrite it into a [`Expr::Lambda`]
/// whose synthetic parameters (`$ph0`, `$ph1`, …) replace the placeholders
/// left-to-right — Scala's `_ + 1` ⇒ `x => x + 1`, `_ + _` ⇒ `(a, b) => a + b`.
/// A placeholder inside an already-wrapped nested lambda is left untouched (the
/// walk does not descend into a `Lambda`).
fn wrap_placeholders(e: Expr) -> Expr {
    let mut n = 0usize;
    let mut body = e;
    replace_placeholders(&mut body, &mut n);
    if n == 0 {
        return body;
    }
    let params = (0..n).map(|i| format!("$ph{i}")).collect();
    Expr::Lambda {
        params,
        body: Box::new(body),
    }
}

/// Replace each [`Expr::Placeholder`] with `Var($ph{n})`, bumping `n`. Does not
/// recurse into a nested `Lambda` (its placeholders already belong to it).
fn replace_placeholders(e: &mut Expr, n: &mut usize) {
    match e {
        Expr::Placeholder => {
            *e = Expr::Var(format!("$ph{n}"));
            *n += 1;
        }
        Expr::Lambda { .. } => {}
        Expr::Unary { rhs, .. } => replace_placeholders(rhs, n),
        Expr::Binary { lhs, rhs, .. } => {
            replace_placeholders(lhs, n);
            replace_placeholders(rhs, n);
        }
        Expr::Method { recv, args, .. } => {
            replace_placeholders(recv, n);
            for a in args {
                replace_placeholders(a, n);
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                replace_placeholders(a, n);
            }
        }
        Expr::New { args, .. } | Expr::Tuple(args) | Expr::Collection { elems: args, .. } => {
            for a in args {
                replace_placeholders(a, n);
            }
        }
        Expr::Println { arg: Some(a), .. } => replace_placeholders(a, n),
        Expr::Format { value, .. } => replace_placeholders(value, n),
        Expr::If { cond, then, els } => {
            replace_placeholders(cond, n);
            replace_placeholders(then, n);
            if let Some(e) = els {
                replace_placeholders(e, n);
            }
        }
        // Placeholders do not reach across a block/match/comprehension boundary
        // (Scala's placeholder scope is the enclosing expression), and the other
        // arms carry no sub-expressions.
        _ => {}
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
        // `::` (cons) — Scala's `:`-family precedence sits just below `+`/`-`
        // and is right-associative (handled in `binary`).
        Tok::ColonColon => (BinOp::Cons, 5),
        Tok::Plus => (BinOp::Add, 6),
        Tok::Minus => (BinOp::Sub, 6),
        Tok::Star => (BinOp::Mul, 7),
        Tok::Slash => (BinOp::Div, 7),
        Tok::Percent => (BinOp::Mod, 7),
        _ => return None,
    })
}
