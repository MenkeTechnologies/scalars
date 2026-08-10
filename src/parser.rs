//! A recursive-descent parser with precedence-climbing for expressions.
//!
//! Grammar: a compilation unit is an entry `object Name { ... }`, optionally
//! alongside top-level `class`/`case class`/`object`/`trait` declarations. scalars
//! locates the entry point two ways: an `object Name extends App { <body> }`
//! runs `<body>` directly, and an `object Name { ... def main(args: …) = { <body> }
//! ... }` runs its `main`. Other members are skipped so an object that also
//! declares helpers still finds its entry point. Statements cover `val`/`var`
//! bindings, assignments, `if`/`while`, the Scala range `for`, `println`/`print`,
//! and bare expressions. Statements are separated by inferred line breaks or
//! explicit `;` (see the lexer).

use crate::ast::*;
use crate::lexer::{Tok, Token};

/// The synthetic call `new Array[T](n)` desugars to; lowered by
/// [`crate::compiler`] to the zero-filling array builtin.
pub const NEW_ARRAY: &str = "$new_array";

/// The reserved collection constructor that marks a varargs SPREAD argument
/// (`f(xs: _*)`). It holds the one spread operand. `Compiler::adapt_args`
/// strips it at a call site whose parameter is repeated; anywhere else it
/// reaches `Compiler::collection`, which rejects it by name.
pub const SPREAD: &str = "$spread";

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
    let mut prog = p.program()?;
    // Block-local `def`s are still statements at this point; scope, uniquely
    // rename, and lambda-lift them into the flat namespace the compiler wants.
    crate::resolve::resolve(&mut prog)?;
    Ok(prog)
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

    /// The token `n` positions ahead of the cursor (`Eof` past the end).
    fn peek_at(&self, n: usize) -> &Tok {
        match self.toks.get(self.pos + n) {
            Some(t) => &t.kind,
            None => &Tok::Eof,
        }
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
                    self.advance();
                    // Consume to the end of the line — or to the first token
                    // that can only start a declaration. Newline inference does
                    // not emit a separator before `case`, so an
                    // `import …` line followed by a `case class` would
                    // otherwise swallow the declaration whole.
                    while !self.is(&Tok::Newline)
                        && !self.is(&Tok::Semi)
                        && !self.is(&Tok::Eof)
                        && !self.at_declaration_start()
                    {
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
            // arrive as bare idents; skip to the declaration keyword.
            while !self.is(&Tok::Eof) && !self.at_declaration_start() {
                self.advance();
            }
            if self.is(&Tok::Eof) {
                break;
            }
            // `case` prefix → `case class` / `case object`.
            let line = self.line();
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
                    TopObject::Singleton(obj) => self.declare_object(obj, line)?,
                }
            } else if matches!(self.peek(), Tok::Ident(w) if w == "class" || w == "trait") {
                let is_trait = matches!(self.peek(), Tok::Ident(w) if w == "trait");
                let c = self.class_decl(is_case, is_trait)?;
                self.declare_class(c, line)?;
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
        // `extends Parent with Trait …`. `App` selects the run-the-body entry
        // form; any other supertype list is a mixin on the singleton.
        let (parents, _) = self.parents_clause()?;
        let app_mode = parents.iter().any(|p| p == "App");
        // A bodyless singleton (`case object Red extends Color`) is the ADT
        // idiom and has no braces at all.
        if !self.is(&Tok::LBrace) {
            return Ok(TopObject::Singleton(ObjectDecl {
                name,
                is_case,
                parents,
                body: Vec::new(),
                methods: Vec::new(),
            }));
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
            self.skip_member_modifiers();
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
                parents,
                body,
                methods: defs,
            }))
        }
    }

    /// Parse a `class`/`case class` declaration (cursor on the `class` ident).
    /// The primary constructor's parameters become instance fields; the class
    /// body's `val`/`var` declarations become further fields (initialized by the
    /// constructor) and its `def`s become methods.
    fn class_decl(&mut self, is_case: bool, is_trait: bool) -> Result<ClassDecl, String> {
        self.advance(); // `class` / `trait`
        let name = self.ident()?;
        // Optional `[T, …]` type-parameter clause.
        if self.is(&Tok::LBracket) {
            self.skip_bracket_group();
        }
        // Primary-constructor parameters (all become fields).
        let mut params = Vec::new();
        let mut param_tys = Vec::new();
        if self.is(&Tok::LParen) {
            self.advance();
            self.skip_seps();
            while !self.is(&Tok::RParen) && !self.is(&Tok::Eof) {
                // Optional `val`/`var`/modifier before the parameter name.
                while self.is(&Tok::Val) || self.is(&Tok::Var) {
                    self.advance();
                }
                let pname = self.ident()?;
                let mut pty = None;
                if self.is(&Tok::Colon) {
                    self.advance();
                    pty = Some(self.type_ref()?);
                }
                param_tys.push(pty);
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
        // `extends Parent[(args)] [with T]*`.
        let (parents, super_args) = self.parents_clause()?;
        // Class body (optional). `def`s → methods; `val`/`var` → fields + ctor
        // statements; anything else → a constructor side-effect statement.
        let mut body = Vec::new();
        let mut methods = Vec::new();
        let mut field_names = params.clone();
        if self.is(&Tok::LBrace) {
            self.advance();
            self.skip_seps();
            while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
                self.skip_member_modifiers();
                if self.is(&Tok::Def) {
                    methods.push(self.parse_def()?);
                } else {
                    let st = self.statement()?;
                    match &st.kind {
                        // An initializer-less `val` in a trait is an abstract
                        // member: it names a field the mixing class must supply,
                        // so it joins `field_names` but contributes no
                        // constructor statement.
                        StmtKind::Local {
                            name, init: None, ..
                        } if is_trait => field_names.push(name.clone()),
                        StmtKind::Local { name, .. } => {
                            field_names.push(name.clone());
                            body.push(st);
                        }
                        _ => body.push(st),
                    }
                }
                self.skip_seps();
            }
            self.eat(&Tok::RBrace)?;
        }
        Ok(ClassDecl {
            name,
            is_case,
            is_trait,
            parents,
            super_args,
            params,
            param_tys,
            body,
            field_names,
            methods,
        })
    }

    /// An `extends P[(args)] [with T]*` clause: the supertype names in source
    /// order plus the superclass constructor arguments (empty when absent).
    fn parents_clause(&mut self) -> Result<(Vec<String>, Vec<Expr>), String> {
        let mut parents = Vec::new();
        let mut super_args = Vec::new();
        if !self.is(&Tok::Extends) {
            return Ok((parents, super_args));
        }
        self.advance();
        loop {
            self.skip_seps();
            let p = self.ident()?;
            if self.is(&Tok::LBracket) {
                self.skip_bracket_group();
            }
            // Only the first parent (the superclass) may take arguments.
            if self.is(&Tok::LParen) {
                let args = self.arg_list()?;
                if parents.is_empty() {
                    super_args = args;
                }
            }
            parents.push(p);
            if matches!(self.peek(), Tok::Ident(w) if w == "with") {
                self.advance();
            } else {
                break;
            }
        }
        Ok((parents, super_args))
    }

    /// Skip the member modifiers that may precede a `def`/`val`/`var` inside a
    /// `class`/`trait`/`object` body. They carry no runtime meaning here (the
    /// runtime is dynamically typed and every member is reachable).
    fn skip_member_modifiers(&mut self) {
        while matches!(self.peek(), Tok::Ident(w)
            if w == "override"
                || w == "private"
                || w == "protected"
                || w == "final"
                || w == "abstract"
                || w == "implicit"
                || w == "lazy")
        {
            self.advance();
        }
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
        let (params, sig) = self.param_list()?;
        // Optional `: ReturnType`.
        let mut ret_ty = None;
        if self.is(&Tok::Colon) {
            self.advance();
            ret_ty = Some(self.type_ref()?);
        }
        // No `= body` — an abstract declaration (`def area: Double` in a trait).
        if !self.is(&Tok::Assign) {
            return Ok(Func {
                name,
                params,
                sig,
                ret_ty,
                captured: 0,
                body: Vec::new(),
                is_abstract: true,
            });
        }
        self.eat(&Tok::Assign)?;
        self.skip_seps();
        let body = if self.is(&Tok::LBrace) {
            self.advance();
            self.block()?
        } else {
            vec![self.statement()?]
        };
        Ok(Func {
            name,
            params,
            sig,
            ret_ty,
            captured: 0,
            body,
            is_abstract: false,
        })
    }

    /// Parse a `def`'s parameter list, returning the names and their
    /// [`ParamSig`]s. The `(` is optional — Scala allows a parameterless
    /// `def name = …`. A second parameter list (currying) is parsed and its
    /// parameters appended, which is exact for a call written with both lists
    /// supplied.
    fn param_list(&mut self) -> Result<(Vec<String>, Vec<ParamSig>), String> {
        let mut params = Vec::new();
        let mut sig = Vec::new();
        while self.is(&Tok::LParen) {
            self.advance();
            self.skip_seps();
            while !self.is(&Tok::RParen) && !self.is(&Tok::Eof) {
                // `implicit`/`using` lead a parameter list, not a parameter.
                if matches!(self.peek(), Tok::Ident(w) if w == "implicit" || w == "using")
                    && !matches!(
                        self.toks.get(self.pos + 1).map(|t| &t.kind),
                        Some(Tok::Colon)
                    )
                {
                    self.advance();
                    self.skip_seps();
                    continue;
                }
                // `val`/`var` prefixes are legal on a class parameter.
                if self.is(&Tok::Val) || self.is(&Tok::Var) {
                    self.advance();
                }
                let pname = self.ident()?;
                let mut ps = ParamSig::default();
                if self.is(&Tok::Colon) {
                    self.advance();
                    // `x: => Int` — a by-name parameter. `type_ref` folds the
                    // arrow into the type string, so the leading `=>` is the
                    // marker.
                    ps.by_name = self.is(&Tok::FatArrow);
                    ps.ty = Some(self.type_ref()?);
                    // `xs: Int*` — a repeated parameter.
                    if self.is(&Tok::Star) {
                        self.advance();
                        ps.vararg = true;
                    }
                }
                // `b: Int = 10` — a default argument.
                if self.is(&Tok::Assign) {
                    self.advance();
                    self.skip_seps();
                    ps.default = Some(self.expression()?);
                }
                params.push(pname);
                sig.push(ps);
                if self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                } else {
                    break;
                }
            }
            self.eat(&Tok::RParen)?;
        }
        Ok((params, sig))
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
        // skip the parameter list — `args` is parsed and ignored
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
    /// A nested `def` stays *in place* as a [`StmtKind::DefDecl`] so the
    /// [`crate::resolve`] pass can see which block declared it; that pass hoists
    /// it (uniquely renamed, with its captures lambda-lifted into parameters)
    /// into the flat function namespace the compiler consumes.
    ///
    /// A nested TYPE is not a statement and does not stay in place: Scala lets a
    /// `class`/`trait`/`object` be declared wherever a statement can appear, and
    /// the entry object's body — the whole program, under `extends App` — is the
    /// commonest such place. It is filed straight into the declaration lists
    /// [`Parser::program`] fills, so `object T extends App { class C(…); … }`
    /// declares exactly what `class C(…)` beside the object would.
    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        self.skip_seps();
        while !self.is(&Tok::RBrace) && !self.is(&Tok::Eof) {
            if self.at_nested_declaration() {
                self.nested_declaration()?;
            } else {
                out.push(self.statement()?);
            }
            self.skip_seps();
        }
        self.eat(&Tok::RBrace)?;
        Ok(out)
    }

    /// Whether the cursor is on a type declaration in STATEMENT position.
    ///
    /// Stricter than [`Parser::at_declaration_start`], which also fires on a
    /// bare `case`: inside a block a bare `case` is a `match` arm, so `case` is
    /// only a declaration when `class`/`object` follows it. Neither of those two
    /// words can be an identifier — both are reserved — so no expression
    /// statement can be mistaken for a declaration here.
    fn at_nested_declaration(&self) -> bool {
        if self.is(&Tok::Object) {
            return true;
        }
        if matches!(self.peek(), Tok::Ident(w) if w == "class" || w == "trait") {
            return true;
        }
        if !self.is(&Tok::Case) {
            return false;
        }
        match self.toks.get(self.pos + 1).map(|t| &t.kind) {
            Some(Tok::Object) => true,
            Some(Tok::Ident(w)) => w == "class",
            _ => false,
        }
    }

    /// Consume a type declaration met in statement position and file it in the
    /// flat namespace the compiler consumes.
    ///
    /// A nested `object` that would itself be an entry point (it `extends App`
    /// or declares `def main`) is REFUSED rather than silently demoted: its body
    /// would otherwise be dropped, which is a wrong answer where this is a
    /// diagnosable one.
    fn nested_declaration(&mut self) -> Result<(), String> {
        let line = self.line();
        let is_case = if self.is(&Tok::Case) {
            self.advance();
            true
        } else {
            false
        };
        if self.is(&Tok::Object) {
            match self.object_decl(is_case)? {
                TopObject::Singleton(o) => self.declare_object(o, line)?,
                TopObject::Entry(name, _) => {
                    return Err(format!(
                        "scalars: nested entry object `{name}` (`extends App` / `def main`) on line {line}"
                    ))
                }
            }
        } else {
            let is_trait = matches!(self.peek(), Tok::Ident(w) if w == "trait");
            let c = self.class_decl(is_case, is_trait)?;
            self.declare_class(c, line)?;
        }
        Ok(())
    }

    /// File a `class`/`trait` declaration, refusing a name already declared.
    ///
    /// Scala distinguishes same-named types by SCOPE: `def a() = { case class
    /// Q(v: Int); … }` and `def b() = { case class Q(v: Int, w: Int); … }`
    /// declare two different `Q`s, and a member `class Q` shadows a top-level
    /// one. Every declaration here lands in ONE flat namespace, so a second `Q`
    /// would silently replace the first and every mention of `Q` in the program
    /// would resolve to whichever won — a wrong answer, and a quiet one. Refuse
    /// instead: a program that cannot be modelled must not be run.
    ///
    /// A `class Q` beside an `object Q` is the COMPANION idiom, which Scala
    /// allows and this frontend supports, so the two kinds are counted apart.
    fn declare_class(&mut self, c: ClassDecl, line: u32) -> Result<(), String> {
        if self.classes.iter().any(|d| d.name == c.name) {
            return Err(format!(
                "scalars: type `{}` is already declared; shadowing type declarations are not modelled (line {line})",
                c.name
            ));
        }
        self.classes.push(c);
        Ok(())
    }

    /// File a singleton `object` declaration. The companion-aware counterpart of
    /// [`Parser::declare_class`], and refusing a redeclaration for the same
    /// reason: one flat namespace cannot hold two.
    fn declare_object(&mut self, o: ObjectDecl, line: u32) -> Result<(), String> {
        if self.objects.iter().any(|d| d.name == o.name) {
            return Err(format!(
                "scalars: object `{}` is already declared; shadowing object declarations are not modelled (line {line})",
                o.name
            ));
        }
        self.objects.push(o);
        Ok(())
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
            // A block-local `def`. Kept in place (rather than hoisted here) so
            // `crate::resolve` can scope it to the block that declared it.
            Tok::Def => Ok(StmtKind::DefDecl(self.parse_def()?)),
            Tok::If => self.if_stmt(),
            Tok::While => self.while_stmt(),
            // A `for` in statement position is the comprehension expression run
            // for effect (its `Vector`/`Unit` value is discarded).
            Tok::For => Ok(StmtKind::Expr(self.for_comprehension()?)),
            Tok::Val | Tok::Var => self.local_decl(),
            Tok::Return => self.return_stmt(),
            Tok::LBrace => {
                self.advance();
                // A bare block is Scala's block *expression*: its value is its
                // last statement's, which matters when it is the trailing
                // statement of a `match` arm or a method body.
                Ok(StmtKind::Expr(Expr::Block(self.block()?)))
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
                        // A pattern definition — `val (a, b) = pair`, `val Some(x) = opt`.
                        // Recognized by a non-identifier start, or by an identifier that a
                        // pattern would treat as a constructor/stable id rather than a binder.
        if self.starts_pattern_decl() {
            let pat = self.pattern()?;
            self.eat(&Tok::Assign)?;
            let init = self.expression()?;
            return Ok(StmtKind::Destructure { pat, init });
        }
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

    /// Whether the token after `val`/`var` starts a PATTERN definition rather
    /// than a plain binder. `(` opens a tuple pattern; a capitalized identifier
    /// is a constructor (`Some(x)`) or stable-id pattern; a lower-case one
    /// followed by `::` is a cons pattern. A bare lower-case identifier is the
    /// ordinary `val x = …`, which stays on the simple path.
    fn starts_pattern_decl(&self) -> bool {
        match self.peek() {
            Tok::LParen => true,
            Tok::Ident(n) => {
                n.chars().next().is_some_and(char::is_uppercase)
                    || matches!(self.peek_at(1), Tok::ColonColon)
            }
            _ => false,
        }
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
                // A type-argument clause. Its CONTENTS are kept, not just the
                // brackets: `List[Int]` is the only place the element type of a
                // collection is written down, and that element type is what
                // types a lambda parameter traversing it and what `.sum`
                // answers. Nested arguments (`List[List[Int]]`) nest verbatim.
                Tok::LBracket => {
                    let mut depth = 0;
                    loop {
                        let t = self.advance();
                        match &t {
                            Tok::LBracket => depth += 1,
                            Tok::RBracket => depth -= 1,
                            Tok::Eof => break,
                            _ => {}
                        }
                        s.push_str(&type_tok_text(&t));
                        if depth == 0 {
                            break;
                        }
                    }
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
            let next = self.toks[self.pos + 1].kind.clone();
            // `xs ++= ys` / `xs --= ys` have no arithmetic reading at all: they
            // are the growable-collection methods, so they lower as calls.
            if let Tok::OpAssign(op) = next {
                let line = self.line();
                self.advance(); // name
                self.advance(); // op
                let value = self.expression()?;
                return Ok(StmtKind::Expr(Expr::Method {
                    recv: Box::new(Expr::Var(name)),
                    name: op,
                    args: vec![value],
                    line,
                }));
            }
            if let Some(op) = assign_op(&next) {
                self.advance(); // name
                self.advance(); // op
                let value = self.expression()?;
                return Ok(StmtKind::Assign { name, op, value });
            }
        }
        let e = self.expression()?;
        // A compound assignment whose target is not a plain name — `a(i) += 1`,
        // `m(k) *= 2`, `obj.field += 1`. `expression` reads those as the
        // [`Expr::CompoundAssign`] they are in Scala; in STATEMENT position
        // nothing reads the value, so unwrap back to the statement forms rather
        // than lowering a result slot that would only be discarded. The target
        // is kept whole so the compiler still evaluates its receiver and indices
        // exactly once.
        if let Expr::CompoundAssign {
            target,
            op,
            value,
            line: _,
        } = e
        {
            return Ok(match *target {
                // `(n) += 1` and the like: the bare-name form is normally caught
                // above, before `expression` ever runs.
                Expr::Var(name) => StmtKind::Assign {
                    name,
                    op,
                    value: *value,
                },
                place => StmtKind::PlaceAssign {
                    place,
                    op,
                    value: *value,
                },
            });
        }
        // `a(i) = v` — Scala's element-assignment sugar for `a.update(i, v)`.
        // The indexing form reaches here either as a bare call (`a(i)`, from
        // `primary`) or as an `apply` on a computed receiver (`xs.head(i)`).
        if self.is(&Tok::Assign) {
            let (recv, mut args, line) = match e {
                Expr::Call { name, args, line } => (Expr::Var(name), args, line),
                Expr::Method {
                    recv,
                    ref name,
                    ref args,
                    line,
                } if name == "apply" => (*recv.clone(), args.clone(), line),
                other => return Ok(StmtKind::Expr(other)),
            };
            self.advance();
            args.push(self.expression()?);
            return Ok(StmtKind::Expr(Expr::Method {
                recv: Box::new(recv),
                name: "update".to_string(),
                args,
                line,
            }));
        }
        Ok(StmtKind::Expr(e))
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
        // Scala accepts either bracketing for the enumerator group. Inside `( )`
        // the lexer suppresses line breaks, so `;` separates; inside `{ }` a line
        // break separates and `;` is optional.
        let braced = self.is(&Tok::LBrace);
        self.eat(if braced { &Tok::LBrace } else { &Tok::LParen })?;
        let enums = self.for_enums(braced)?;
        self.skip_seps();
        self.eat(if braced { &Tok::RBrace } else { &Tok::RParen })?;
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

    /// Parse the enumerator list of a `for`: generators (`pat <- source`, either
    /// a range or a collection) and `if` guards. In the parenthesized form an
    /// explicit `;` separates them; in the brace form a line break does too, and
    /// a guard may stand alone on its own line.
    fn for_enums(&mut self, braced: bool) -> Result<Vec<ForEnum>, String> {
        let closer = if braced { Tok::RBrace } else { Tok::RParen };
        let mut out = Vec::new();
        loop {
            self.skip_seps();
            if self.is(&closer) {
                break;
            }
            // A guard on its own (`for { x <- xs; if x > 1 }`) filters the
            // generator before it.
            if self.is(&Tok::If) {
                self.advance();
                out.push(ForEnum::Guard(self.expression()?));
            } else if self.at_for_value_def() {
                // `y = e` (Scala also accepts the `val y = e` spelling) — a value
                // definition, in scope for every later enumerator and the body.
                if self.is(&Tok::Val) {
                    self.advance();
                }
                let name = self.ident()?;
                // A declared type is parsed and dropped, as everywhere else.
                if self.is(&Tok::Colon) {
                    self.advance();
                    self.type_ref()?;
                }
                self.eat(&Tok::Assign)?;
                out.push(ForEnum::Val {
                    name,
                    value: self.expression()?,
                });
                // Trailing `if` guards bind to the value just defined.
                while self.is(&Tok::If) {
                    self.advance();
                    out.push(ForEnum::Guard(self.expression()?));
                }
            } else {
                out.push(self.for_generator()?);
                // Trailing `if` guards bind to the generator just parsed.
                while self.is(&Tok::If) {
                    self.advance();
                    out.push(ForEnum::Guard(self.expression()?));
                }
            }
            // In the parenthesized form only an explicit `;` continues the list;
            // in the brace form the inferred line break does too.
            if self.is(&Tok::Semi) || (braced && self.is(&Tok::Newline)) {
                self.skip_seps();
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// Whether the enumerator at the cursor is a value definition (`y = e`,
    /// optionally spelled `val y = e` or typed `y: Int = e`) rather than a
    /// generator. Decided by scanning to the first `=` / `<-`: a `<-` first means
    /// a generator, and only a *bare* `=` (never `==`, which lexes as its own
    /// token) opens a definition.
    fn at_for_value_def(&self) -> bool {
        if self.is(&Tok::Val) {
            return true;
        }
        if !matches!(self.peek(), Tok::Ident(_)) {
            return false;
        }
        match self.peek_at(1) {
            Tok::Assign => true,
            // `y: T = e` — walk the type annotation, which cannot itself contain
            // an `=` or a `<-`, to the assignment.
            Tok::Colon => {
                let mut i = self.pos + 2;
                while let Some(t) = self.toks.get(i) {
                    match t.kind {
                        Tok::Assign => return true,
                        Tok::LArrow | Tok::Semi | Tok::Newline | Tok::Eof => return false,
                        _ => i += 1,
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// One `pat <- source` generator. A parenthesized binder is a destructuring
    /// (tuple) pattern; a bare identifier is the usual binding.
    fn for_generator(&mut self) -> Result<ForEnum, String> {
        // Scala 3's `for (case pat <- xs)`: the pattern may fail, and a failing
        // element is filtered out rather than raising.
        let filtering = self.is(&Tok::Case);
        if filtering {
            self.advance();
        }
        let pat = if filtering || self.is(&Tok::LParen) {
            self.pattern()?
        } else {
            Pattern::Bind(self.ident()?)
        };
        self.eat(&Tok::LArrow)?;
        // `expression` already parses `a to b [by s]` as an infix range, so a
        // generator source is recognized by *shape*: a literal range over a plain
        // binder keeps the counted-loop lowering, anything else is a collection
        // generator desugared to `.map`/`.flatMap`.
        let src = self.expression()?;
        match (&pat, as_range(&src)) {
            (Pattern::Bind(name), Some((start, end, inclusive, step))) if !filtering => {
                Ok(ForEnum::Gen {
                    name: name.clone(),
                    start,
                    end,
                    inclusive,
                    step,
                })
            }
            _ => Ok(ForEnum::GenColl {
                pat,
                coll: src,
                filtering,
            }),
        }
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
        // Alphanumeric infix method syntax: Scala reads `a m b` as `a.m(b)` for
        // any single-argument method — `xs contains 2`, `1 to n`, `s startsWith
        // "a"`. An alphanumeric operator binds looser than every symbolic one
        // (so `1 to n - 1` is `1 to (n - 1)`) and is left-associative
        // (`xs map f filter g` is `(xs.map(f)).filter(g)`).
        while let Tok::Ident(name) = self.peek().clone() {
            if !self.at_infix_operator(&name) {
                break;
            }
            let line = self.line();
            self.advance();
            let rhs = if self.at_lambda_start() {
                self.lambda()?
            } else if self.is(&Tok::LBrace) {
                self.brace_arg()?
            } else {
                self.binary(0)?
            };
            e = Expr::Method {
                recv: Box::new(e),
                name,
                args: vec![rhs],
                line,
            };
        }
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
        // A compound assignment, which in Scala is an EXPRESSION at the lowest
        // precedence of all — `println(buf += 1)`, `val r = (n -= 2)`. `=`
        // itself is not read here: a bare `=` after an expression is either the
        // `a(i) = v` update sugar or a named argument, both of which are decided
        // by their own callers.
        //
        // `++=`/`--=` have no arithmetic reading at all, so they need no
        // dedicated node — they are the growable method, and a method call is
        // already an expression with the receiver as its value.
        if let Tok::OpAssign(name) = self.peek().clone() {
            let line = self.line();
            self.advance();
            let value = self.expression()?;
            return Ok(Expr::Method {
                recv: Box::new(e),
                name,
                args: vec![value],
                line,
            });
        }
        if let Some(op) = assign_op(self.peek()) {
            if op != AssignOp::Assign {
                let line = self.line();
                self.advance();
                let value = self.expression()?;
                return Ok(Expr::CompoundAssign {
                    target: Box::new(e),
                    op,
                    value: Box::new(value),
                    line,
                });
            }
        }
        Ok(e)
    }

    /// Whether the identifier at the cursor is an infix method name rather than
    /// the start of the next construct.
    ///
    /// Two conditions decide it. First, `name` must not be one of Scala's *soft*
    /// keywords — the words the lexer hands back as identifiers (`yield`, `with`,
    /// `class`, `private`, …) and which legitimately follow a complete
    /// expression. Second, the token after it must be able to open an operand:
    /// a line break between the receiver and the name already rules the infix
    /// reading out, because the lexer only infers a [`Tok::Newline`] where one
    /// statement can end and another begin.
    fn at_infix_operator(&self, name: &str) -> bool {
        const SOFT_KEYWORDS: &[&str] = &[
            "yield",
            "then",
            "with",
            "do",
            "forSome",
            "given",
            "using",
            "end",
            "derives",
            "class",
            "trait",
            "type",
            "enum",
            "import",
            "export",
            "package",
            "extension",
            "implicit",
            "inline",
            "opaque",
            "override",
            "private",
            "protected",
            "final",
            "sealed",
            "abstract",
            "lazy",
        ];
        if SOFT_KEYWORDS.contains(&name) || name == "_" {
            return false;
        }
        matches!(
            self.toks.get(self.pos + 1).map(|t| &t.kind),
            Some(
                Tok::Int(_)
                    | Tok::Long(_)
                    | Tok::Float(_)
                    | Tok::Str(_)
                    | Tok::Char(_)
                    | Tok::InterpStr { .. }
                    | Tok::Ident(_)
                    | Tok::True
                    | Tok::False
                    | Tok::Null
                    | Tok::LParen
                    | Tok::LBrace
                    | Tok::Minus
                    | Tok::Not
                    | Tok::New
            )
        )
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
        let params = self.lambda_params()?;
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
            partial: false,
        })
    }

    /// A function literal's parameter list, through the `=>`: a single bare
    /// identifier or a parenthesized (optionally typed) list.
    fn lambda_params(&mut self) -> Result<Vec<String>, String> {
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
        Ok(params)
    }

    fn binary(&mut self, min_bp: u8) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        loop {
            if let Some((op, bp)) = binop(self.peek()) {
                if bp < min_bp {
                    break;
                }
                self.advance();
                // `::` (cons) is right-associative: `1 :: 2 :: Nil` is
                // `1 :: (2 :: Nil)`.
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
                continue;
            }
            // A symbolic method operator (`++`, `:+`, `+:`, `--`) is an infix
            // method call. Scala takes its precedence from the first character
            // and makes it right-associative when the name ends in `:`, in which
            // case the *right* operand is the receiver (`0 +: xs` is `xs.+:(0)`).
            if let Tok::Op(sym) = self.peek().clone() {
                let (bp, right_assoc) = symbolic_op(&sym);
                if bp < min_bp {
                    break;
                }
                let line = self.line();
                self.advance();
                let rhs = if right_assoc {
                    self.binary(bp)?
                } else {
                    self.binary(bp + 1)?
                };
                let (recv, arg) = if right_assoc { (rhs, lhs) } else { (lhs, rhs) };
                lhs = Expr::Method {
                    recv: Box::new(recv),
                    name: sym,
                    args: vec![arg],
                    line,
                };
                continue;
            }
            break;
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Tok::Minus => {
                self.advance();
                // Scala folds a prefix `-` into a numeric *literal*, so the
                // sign is part of the value a postfix chain then applies to:
                // `-3.abs` is `(-3).abs`, which is `3`, where `-x.abs` is
                // `-(x.abs)`. Nothing else about `-` changes.
                let negated = match self.peek().clone() {
                    Tok::Int(n) => {
                        self.advance();
                        Some(Expr::Int(n.wrapping_neg()))
                    }
                    Tok::Long(n) => {
                        self.advance();
                        Some(Expr::Long(n.wrapping_neg()))
                    }
                    Tok::Float(f) => {
                        self.advance();
                        Some(Expr::Float(-f))
                    }
                    _ => None,
                };
                match negated {
                    Some(lit) => self.postfix_from(lit),
                    None => Ok(Expr::Unary {
                        op: UnOp::Neg,
                        rhs: Box::new(self.unary()?),
                    }),
                }
            }
            Tok::Not => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    rhs: Box::new(self.unary()?),
                })
            }
            // `~x` — the bitwise complement, Scala's `Int.unary_~`.
            Tok::Tilde => {
                self.advance();
                Ok(Expr::Unary {
                    op: UnOp::Complement,
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
        let e = self.primary()?;
        self.postfix_from(e)
    }

    /// The postfix chain of [`Self::postfix`], applied to an already-parsed
    /// receiver (a negated numeric literal comes in this way).
    fn postfix_from(&mut self, e: Expr) -> Result<Expr, String> {
        let mut e = e;
        // `.member` and a trailing application are ONE chain, alternating freely:
        // `"abc"(1).toUpper` applies then selects, `getFn()(arg)` applies twice.
        // Two sequential loops would stop at the first switch of kind.
        loop {
            if !self.is(&Tok::Dot) {
                // A trailing application on a value expression (`f(x)`, `xs(i)`,
                // `getFn()(arg)`, `"abc"(1)`) — Scala's `apply`.
                if self.is(&Tok::LParen) {
                    let line = self.line();
                    let args = self.arg_list()?;
                    e = Expr::Method {
                        recv: Box::new(e),
                        name: "apply".to_string(),
                        args,
                        line,
                    };
                    continue;
                }
                break;
            }
            self.advance(); // `.`
            let line = self.line();
            // A method name is normally an identifier, but Scala's symbolic
            // operators are ordinary method names too and may be written in the
            // dotted form (`"a".*(3)`, `n.+(1)`) — the lexer hands those back as
            // operator tokens, so they are spelled out here.
            let name = match self.peek() {
                Tok::Ident(_) => self.ident()?,
                _ => match dotted_operator(self.peek()) {
                    Some(op) => {
                        self.advance();
                        op.to_string()
                    }
                    None => self.ident()?,
                },
            };
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
            // `x.isInstanceOf[T]` / `x.asInstanceOf[T]` — the *only* methods
            // whose type argument is load-bearing, so it is captured (as a
            // string argument) instead of skipped.
            if (name == "isInstanceOf" || name == "asInstanceOf") && self.is(&Tok::LBracket) {
                self.advance();
                let ty = self.type_ref()?;
                self.eat(&Tok::RBracket)?;
                e = Expr::Method {
                    recv: Box::new(e),
                    name,
                    args: vec![Expr::Str(ty)],
                    line,
                };
                continue;
            }
            // Optional `[T]` type arguments on a method (`xs.map[Int](f)`).
            if self.is(&Tok::LBracket) {
                self.skip_bracket_group();
            }
            // A method may be curried (`foldLeft(z)(op)`); flatten every trailing
            // argument list into the one call. A brace group takes the place of a
            // parenthesized single argument (`xs.map { x => … }`), including the
            // trailing one of a curried call (`xs.foldLeft(0) { (a, b) => … }`).
            //
            // A `scala.collection.mutable` FACTORY is the exception: it is not
            // curried, so a second group is an `apply` on the collection it just
            // built (`mutable.ArrayBuffer(1,2,3)(1)` is the element `2`). Without
            // this the flattening would silently append the index as a fourth
            // element.
            let factory = is_mutable_pkg(&e) && mutable_factory_name(&name);
            let mut args = Vec::new();
            while self.is(&Tok::LParen) {
                args.extend(self.arg_list()?);
                if factory {
                    break;
                }
            }
            if self.is(&Tok::LBrace) {
                args.push(self.brace_arg()?);
            }
            e = Expr::Method {
                recv: Box::new(e),
                name,
                args,
                line,
            };
        }
        Ok(e)
    }

    /// Whether the cursor is on a token that can only begin a top-level
    /// declaration: `case` (of `case class`/`case object`), `object`, or the
    /// `class`/`trait` soft keywords.
    fn at_declaration_start(&self) -> bool {
        self.is(&Tok::Object)
            || self.is(&Tok::Case)
            || matches!(self.peek(), Tok::Ident(w) if w == "class" || w == "trait")
    }

    /// A `{ … }` group standing in for a method's single argument: either the
    /// pattern-matching anonymous function `{ case … }` or a block whose value is
    /// the argument — most often a function literal (`{ x => x + 1 }`).
    fn brace_arg(&mut self) -> Result<Expr, String> {
        if self.at_case_block() {
            return self.case_lambda();
        }
        self.eat(&Tok::LBrace)?;
        self.skip_seps();
        if self.at_lambda_start() {
            // `{ x => s1; s2 }` — the arrow's body is the rest of the brace group,
            // which may be several statements, so it is read as a block.
            let params = self.lambda_params()?;
            return Ok(Expr::Lambda {
                params,
                body: Box::new(Expr::Block(self.block()?)),
                partial: false,
            });
        }
        Ok(Expr::Block(self.block()?))
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
            Tok::Long(n) => {
                self.advance();
                Ok(Expr::Long(n))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Float(f))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::Char(c) => {
                self.advance();
                Ok(Expr::Char(c))
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
            // A brace block in value position: `val q = { s1; s2; last }`. Its
            // value is the last statement's (Scala's block expression). A block
            // made only of `case` arms is instead a pattern-matching anonymous
            // function (`xs.map { case (k, v) => k + v }`).
            Tok::LBrace => {
                if self.at_case_block() {
                    return self.case_lambda();
                }
                self.advance();
                Ok(Expr::Block(self.block()?))
            }
            // `new Class(args)` — construct a host-heap instance.
            Tok::New => self.new_expr(),
            // `if` in expression position: `val r = if (c) a else b`.
            Tok::If => self.if_expr(),
            // `for (...) yield …` / `for (...) …` in expression position.
            Tok::For => self.for_comprehension(),
            // `try`/`throw` are expressions in Scala (a `try` has a value; a
            // `throw` has type `Nothing`), so they are primaries.
            Tok::Try => self.try_expr(),
            Tok::Throw => {
                let line = self.line();
                self.advance();
                Ok(Expr::Throw {
                    value: Box::new(self.expression()?),
                    line,
                })
            }
            // `return` is an EXPRESSION in Scala (of type `Nothing`), so it is
            // legal wherever a value is — most often a brace-less lambda body,
            // `xs.foreach(x => if (p(x)) return x)`. A one-statement block is the
            // exact lowering: the statement runs, and the block's `Unit` stands
            // in for the value `return` never actually produces.
            Tok::Return => {
                let line = self.line();
                let kind = self.return_stmt()?;
                Ok(Expr::Block(vec![Stmt { line, kind }]))
            }
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
            // `( e )` grouping, `( e: T )` ascription, the unit literal `()`, or a
            // tuple literal `(a, b, …)`. A `( … ) =>` lambda is caught earlier in
            // `expression`.
            Tok::LParen => {
                self.advance();
                self.skip_seps();
                // `()` — the unit literal. Scala's `Unit` value prints `()`, which
                // is exactly what an empty tuple renders as, so it lowers to one
                // (`HeapVal::Tuple(vec![])`) rather than to a bespoke value: the
                // representation is structural, so `() == ()` and `().toString`
                // both answer what Scala answers.
                if self.is(&Tok::RParen) {
                    self.advance();
                    return Ok(Expr::Tuple(Vec::new()));
                }
                // Parentheses delimit an `_` placeholder.s scope, so `(_ * 2)` is
                // the function `x => x * 2` wherever it appears — including as the
                // right operand of an infix call (`xs map (_ * 2)`).
                let mut elems = vec![self.ascribed()?];
                while self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                    elems.push(self.ascribed()?);
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
                // `wrap_placeholders` at the enclosing argument boundary). A `(`
                // after it is an APPLICATION of the placeholder — `xs.map(_(0))`
                // is `xs.map(x => x(0))` — which `postfix` builds as an `apply`
                // on this node; it is never a call to a function named `_`.
                if name == "_" {
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
                        partial: false,
                    });
                }
                // `scala.util.control.Breaks`. Scala ships `break`/`breakable` as
                // ordinary methods, but they are the language's only loop-exit
                // idiom, so they are recognized here rather than left to the
                // generic call path: `breakable { … }` is a block-argument
                // application, a shape this frontend does not otherwise parse.
                //
                // The desugar is exactly the library's own implementation —
                // `break()` raises a `BreakControl` and `breakable` catches it —
                // so `finally` blocks between the two still run, a `break` from
                // inside a nested `def` still unwinds to the enclosing
                // `breakable`, and an intervening `catch { case e: Exception }`
                // still lets it through.
                if name == "breakable" && matches!(next, Some(Tok::LBrace)) {
                    self.advance(); // breakable
                    self.advance(); // {
                    let body = self.block()?;
                    return Ok(Expr::Try {
                        body,
                        catches: vec![MatchArm {
                            pat: Pattern::Typed {
                                name: "_".to_string(),
                                ty: "BreakControl".to_string(),
                            },
                            guard: None,
                            body: Vec::new(),
                        }],
                        finalizer: None,
                    });
                }
                // `break` and `break()` are the same expression; Scala types it
                // `Nothing`, so it is legal in operand position.
                if name == "break" && !matches!(next, Some(Tok::Dot)) {
                    let line = self.line();
                    self.advance();
                    if self.is(&Tok::LParen) {
                        self.advance();
                        self.eat(&Tok::RParen)?;
                    }
                    return Ok(Expr::Throw {
                        value: Box::new(Expr::New {
                            name: "BreakControl".to_string(),
                            args: Vec::new(),
                            line,
                        }),
                        line,
                    });
                }
                let line = self.line();
                self.advance();
                // Optional generic type arguments (`List[Int](…)`, `foo[T](…)`).
                if self.is(&Tok::LBracket) {
                    self.skip_bracket_group();
                }
                if self.is(&Tok::LParen) {
                    // `List(...)` / `Map(...)` / `Array(...)` collection literals.
                    // `ListBuffer`/`ArrayBuffer`/`Buffer` are the mutable names
                    // that can only mean the mutable collection, so they work
                    // unqualified (imports are skipped, not tracked). `Set` and
                    // `Map` stay immutable — see `BUGS.md`.
                    if matches!(
                        name.as_str(),
                        "List" | "Map" | "Array" | "Seq" | "Vector" | "Set" | "IndexedSeq"
                    ) {
                        let elems = self.arg_list()?;
                        return Ok(Expr::Collection { ctor: name, elems });
                    }
                    if let Some(ctor) = mutable_buffer_ctor(&name) {
                        let elems = self.arg_list()?;
                        return Ok(Expr::Collection {
                            ctor: ctor.to_string(),
                            elems,
                        });
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

    /// One parenthesized element: an expression, optionally with a *type
    /// ascription* (`e: T`). Scala's ascription tells the typer which type to
    /// use; the runtime here is dynamically typed, so the annotation is dropped —
    /// except for the one ascription that is OBSERVABLE without a type checker,
    /// a numeric widening (`(3: Double)` prints `3.0`, not `3`), which lowers to
    /// the same `toDouble` conversion Scala's implicit widening performs.
    fn ascribed(&mut self) -> Result<Expr, String> {
        let line = self.line();
        let e = wrap_placeholders(self.expression()?);
        if !self.is(&Tok::Colon) {
            return Ok(e);
        }
        self.advance();
        let ty = self.type_ref()?;
        Ok(match ty.as_str() {
            "Double" | "Float" => Expr::Method {
                recv: Box::new(e),
                name: "toDouble".to_string(),
                args: Vec::new(),
                line,
            },
            _ => e,
        })
    }

    /// Parse a named call `name(arg, arg, …)` — the cursor is on the `(` and
    /// `name`/`line` are already consumed. Resolved in the compiler: either the
    /// `__rust_compile` FFI-block builtin or a call to an FFI-exported bareword.
    fn call(&mut self, name: String, line: u32) -> Result<Expr, String> {
        let args = self.arg_list()?;
        Ok(Expr::Call { name, args, line })
    }

    /// `new Class[Types](args)` — construct a host-heap instance (the cursor is
    /// on `new`). Type arguments are consumed and ignored; the argument list is
    /// optional (`new Empty`).
    fn new_expr(&mut self) -> Result<Expr, String> {
        self.eat(&Tok::New)?;
        let line = self.line();
        let mut name = self.ident()?;
        // A package-qualified `new scala.collection.mutable.ListBuffer[Int]`:
        // consume the prefix and keep the type's own name. Only a prefix this
        // frontend recognizes is accepted, so an unknown one still fails on the
        // type rather than silently dropping the qualification.
        let mut mutable_qualified = false;
        while self.is(&Tok::Dot) && MUTABLE_PREFIXES.contains(&name.as_str()) {
            mutable_qualified = true;
            self.advance();
            name = self.ident()?;
        }
        // `new Array[T](n)` is the one `new` whose type argument is
        // load-bearing: it picks the zero value the array is filled with.
        let mut elem_ty = None;
        if self.is(&Tok::LBracket) {
            if name == "Array" {
                self.advance();
                elem_ty = Some(self.type_ref()?);
                self.eat(&Tok::RBracket)?;
            } else {
                self.skip_bracket_group();
            }
        }
        let mut args = if self.is(&Tok::LParen) {
            self.arg_list()?
        } else {
            Vec::new()
        };
        if let Some(ty) = elem_ty {
            args.push(Expr::Str(ty));
            return Ok(Expr::Call {
                name: NEW_ARRAY.to_string(),
                args,
                line,
            });
        }
        // `new ListBuffer[Int]()` builds the same empty buffer the factory does.
        // `new mutable.HashSet[Int]` likewise, but only when qualified — a bare
        // `new Set` is not a Scala constructor and stays an error.
        let ctor = match name.as_str() {
            "Set" | "HashSet" if mutable_qualified => Some("mutable.Set"),
            "Map" | "HashMap" if mutable_qualified => Some("mutable.Map"),
            other => mutable_buffer_ctor(other),
        };
        if let Some(ctor) = ctor {
            return Ok(Expr::Collection {
                ctor: ctor.to_string(),
                elems: args,
            });
        }
        Ok(Expr::New { name, args, line })
    }

    /// Parse a parenthesized, comma-separated argument list (cursor on `(`);
    /// consumes the closing `)`. An argument written `name = value` is a NAMED
    /// argument, kept as [`Expr::NamedArg`] for the call lowering to place. The
    /// `name` must be a bare identifier followed by a single `=` — `a == b` and
    /// `a += b` are ordinary expressions, and the lexer already gives those
    /// their own tokens.
    /// `e: _*` — a varargs SPREAD, which is legal in an argument position only.
    /// It hands `e` itself to a repeated parameter instead of making it one
    /// element of one. Marked with the reserved [`SPREAD`] collection ctor so
    /// every generic expression walker still recurses into the operand, and
    /// stripped by `Compiler::adapt_args` at the call site that consumes it.
    fn spread_or(&mut self, e: Expr) -> Result<Expr, String> {
        let spread = self.is(&Tok::Colon)
            && matches!(self.peek_at(1), Tok::Ident(w) if w == "_")
            && matches!(self.peek_at(2), Tok::Star);
        if !spread {
            return Ok(e);
        }
        self.advance(); // `:`
        self.advance(); // `_`
        self.advance(); // `*`
        Ok(Expr::Collection {
            ctor: SPREAD.to_string(),
            elems: vec![e],
        })
    }

    fn arg_list(&mut self) -> Result<Vec<Expr>, String> {
        self.eat(&Tok::LParen)?;
        let mut args = Vec::new();
        if !self.is(&Tok::RParen) {
            loop {
                let named = match (self.peek(), self.toks.get(self.pos + 1).map(|t| &t.kind)) {
                    (Tok::Ident(w), Some(Tok::Assign)) if w != "_" => Some(w.clone()),
                    _ => None,
                };
                let a = match named {
                    Some(pname) => {
                        self.advance(); // name
                        self.advance(); // =
                        self.skip_seps();
                        Expr::NamedArg {
                            name: pname,
                            value: Box::new(wrap_arg_placeholders(self.expression()?)),
                        }
                    }
                    None => {
                        let e = wrap_arg_placeholders(self.expression()?);
                        self.spread_or(e)?
                    }
                };
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

    /// `try body [catch { case … }] [finally body]` as a value-producing
    /// expression.
    ///
    /// The body and finalizer are parsed as *statement lists* (not
    /// [`branch_expr`](Self::branch_expr)) because a `try` block is usually
    /// several statements; a brace-less one-expression form (`try f() catch …`)
    /// is wrapped into a one-statement list so both shapes lower identically.
    /// Scala requires at least one of `catch`/`finally`, and so does this.
    fn try_expr(&mut self) -> Result<Expr, String> {
        let line = self.line();
        self.eat(&Tok::Try)?;
        let body = self.braced_or_single()?;
        // A line break may precede `catch`/`finally` (the lexer suppresses the
        // separator, but tolerate an explicit one as `if_expr` does for `else`).
        let save = self.pos;
        self.skip_seps();
        let catches = if self.is(&Tok::Catch) {
            self.advance();
            self.skip_seps();
            self.catch_arms()?
        } else {
            self.pos = save;
            Vec::new()
        };
        let save = self.pos;
        self.skip_seps();
        let finalizer = if self.is(&Tok::Finally) {
            self.advance();
            Some(self.braced_or_single()?)
        } else {
            self.pos = save;
            None
        };
        if catches.is_empty() && finalizer.is_none() {
            return Err(format!(
                "scalars: `try` requires a `catch` or a `finally` (line {line})"
            ));
        }
        Ok(Expr::Try {
            body,
            catches,
            finalizer,
        })
    }

    /// The `{ case … }` arms of a `catch`. Scala also allows a brace-less
    /// single-expression handler function, which is not modeled; the `{ case … }`
    /// form is what real code writes.
    fn catch_arms(&mut self) -> Result<Vec<MatchArm>, String> {
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
                "scalars: `catch` requires at least one `case` (line {})",
                self.line()
            ));
        }
        Ok(arms)
    }

    /// An `if`/`match`-branch expression: a `{ … }` block (parsed to an
    /// [`Expr::Block`]) or a single expression.
    fn branch_expr(&mut self) -> Result<Expr, String> {
        self.skip_seps();
        if self.is(&Tok::LBrace) {
            self.advance();
            return Ok(Expr::Block(self.block()?));
        }
        // A brace-less branch that is an ASSIGNMENT (`if (p) c += x`) is a `Unit`
        // statement, not an expression — the same one-statement block a
        // brace-less lambda body takes.
        if self.assignment_ahead() {
            let line = self.line();
            let kind = self.simple_statement()?;
            return Ok(Expr::Block(vec![Stmt { line, kind }]));
        }
        self.expression()
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

    /// Whether the cursor is on a `{` that opens a pattern-matching anonymous
    /// function — a brace whose first token (past statement separators) is `case`.
    fn at_case_block(&self) -> bool {
        if !self.is(&Tok::LBrace) {
            return false;
        }
        let mut i = self.pos + 1;
        while matches!(
            self.toks.get(i).map(|t| &t.kind),
            Some(Tok::Newline | Tok::Semi)
        ) {
            i += 1;
        }
        matches!(self.toks.get(i).map(|t| &t.kind), Some(Tok::Case))
    }

    /// `{ case p1 => e1; case p2 => e2 }` — Scala's pattern-matching anonymous
    /// function, which is a one-parameter lambda matching on its argument.
    fn case_lambda(&mut self) -> Result<Expr, String> {
        self.eat(&Tok::LBrace)?;
        self.skip_seps();
        let mut arms = Vec::new();
        while self.is(&Tok::Case) {
            arms.push(self.match_arm()?);
            self.skip_seps();
        }
        self.eat(&Tok::RBrace)?;
        let param = "$case".to_string();
        Ok(Expr::Lambda {
            params: vec![param.clone()],
            body: Box::new(Expr::Match {
                scrut: Box::new(Expr::Var(param)),
                arms,
            }),
            // Scala reads a `{ case … }` literal as a `PartialFunction` when one
            // is expected and as a total `FunctionN` otherwise. There are no
            // static types here, so it is always built as a partial function —
            // which behaves as the total one everywhere else, because applying it
            // where no arm matches still raises `MatchError`.
            partial: true,
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

    /// A `case` pattern, at Scala's outermost `Pattern` precedence: one or more
    /// [`Self::pattern_cons`] branches separated by `|`.
    fn pattern(&mut self) -> Result<Pattern, String> {
        let first = self.pattern_cons()?;
        if !self.is_bar() {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.is_bar() {
            self.advance();
            self.skip_seps();
            alts.push(self.pattern_cons()?);
        }
        Ok(Pattern::Alt(alts))
    }

    /// Whether the next token is the alternation `|` (lexed as a symbolic op).
    fn is_bar(&self) -> bool {
        matches!(self.peek(), Tok::Op(o) if o == "|")
    }

    /// Scala's `Pattern3` — the infix `::` pattern, right-associative so
    /// `a :: b :: rest` nests as `a :: (b :: rest)`.
    fn pattern_cons(&mut self) -> Result<Pattern, String> {
        let head = self.pattern_primary()?;
        if self.is(&Tok::ColonColon) {
            self.advance();
            self.skip_seps();
            let tail = self.pattern_cons()?;
            return Ok(Pattern::Cons(Box::new(head), Box::new(tail)));
        }
        Ok(head)
    }

    /// A simple `case` pattern: literal, `_` wildcard, `_*` sequence wildcard,
    /// variable binding, `name @ pat` binder, typed `name: Type` / `_: Type`,
    /// constructor/extractor `Foo(x, …)`, or a tuple `(a, b)`.
    fn pattern_primary(&mut self) -> Result<Pattern, String> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.advance();
                Ok(Pattern::Literal(Expr::Int(n)))
            }
            // A literal in PATTERN position is only ever compared for equality,
            // never used in arithmetic, so its width cannot be observed — a
            // `Long` pattern keeps the `Int` node and needs no separate lowering.
            Tok::Long(n) => {
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
            Tok::Char(c) => {
                self.advance();
                Ok(Pattern::Literal(Expr::Char(c)))
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
                    Tok::Int(n) | Tok::Long(n) => Ok(Pattern::Literal(Expr::Int(n.wrapping_neg()))),
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
                } else if self.is(&Tok::At) {
                    // `name @ pat` — bind the whole scrutinee, then match `pat`.
                    // `name @ _*` is the *named* sequence wildcard, which binds
                    // the trailing elements rather than the scrutinee.
                    self.advance();
                    self.skip_seps();
                    let inner = self.pattern_cons()?;
                    if inner == Pattern::Rest(None) {
                        return Ok(Pattern::Rest(Some(name)));
                    }
                    Ok(Pattern::At {
                        name,
                        pat: Box::new(inner),
                    })
                } else if name == "_" {
                    // `_*` — the trailing sequence wildcard.
                    if self.is(&Tok::Star) {
                        self.advance();
                        return Ok(Pattern::Rest(None));
                    }
                    Ok(Pattern::Wildcard)
                } else if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    // A capitalized bare identifier is a stable-identifier
                    // pattern (`case None =>`), matched by `==`, not a binding.
                    Ok(Pattern::Stable(name))
                } else {
                    Ok(Pattern::Bind(name))
                }
            }
            // `case (a, b) =>` — a tuple pattern. A single parenthesized pattern
            // is just grouping, as in Scala.
            Tok::LParen => {
                self.advance();
                self.skip_seps();
                let mut elems = vec![self.pattern()?];
                while self.is(&Tok::Comma) {
                    self.advance();
                    self.skip_seps();
                    elems.push(self.pattern()?);
                }
                self.eat(&Tok::RParen)?;
                if elems.len() == 1 {
                    Ok(elems.pop().unwrap())
                } else {
                    Ok(Pattern::Tuple(elems))
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

/// The method name of an operator token written in the dotted form (`n.+(1)`,
/// `"a".*(3)`, `a.<(b)`). Scala has no separate operator namespace — every
/// operator is a method — so the two spellings are the same call, dispatched by
/// [`crate::host`]'s `operator_method`. `None` for a token that cannot name a
/// method, which keeps the ordinary "expected an identifier" diagnostic.
fn dotted_operator(t: &Tok) -> Option<&'static str> {
    Some(match t {
        Tok::Plus => "+",
        Tok::Minus => "-",
        Tok::Star => "*",
        Tok::Slash => "/",
        Tok::Percent => "%",
        Tok::Lt => "<",
        Tok::Gt => ">",
        Tok::Le => "<=",
        Tok::Ge => ">=",
        Tok::EqEq => "==",
        Tok::NotEq => "!=",
        _ => return None,
    })
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
/// [`wrap_placeholders`] for one ARGUMENT of a call. Scala expands a placeholder
/// at the smallest expression that *properly contains* it, so an argument that is
/// nothing but `_` is not itself that expression — the enclosing call is. Wrapping
/// it here instead would make `xs.map(f(_))` pass `f` the identity function rather
/// than eta-expanding to `x => f(x)`; when `f` accepts any value that is a wrong
/// answer, not an error (`xs.map(m(_))` looked up the *function* as a map key).
fn wrap_arg_placeholders(e: Expr) -> Expr {
    if matches!(e, Expr::Placeholder) {
        return e;
    }
    wrap_placeholders(e)
}

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
        partial: false,
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

/// Destructure an infix range expression (`a to b`, `a until b by s`) into the
/// generator form a `for` lowers to a counted loop. Anything else is `None` — a
/// collection generator, or a `Range` value that stays a value.
fn as_range(e: &Expr) -> Option<(Expr, Expr, bool, Option<Expr>)> {
    match e {
        Expr::Method {
            recv, name, args, ..
        } if name == "by" && args.len() == 1 => {
            let (start, end, inclusive, _) = as_range(recv)?;
            Some((start, end, inclusive, Some(args[0].clone())))
        }
        Expr::Method {
            recv, name, args, ..
        } if (name == "to" || name == "until") && args.len() == 1 => {
            Some(((**recv).clone(), args[0].clone(), name == "to", None))
        }
        _ => None,
    }
}

/// Map a token to a compound-assignment operator, if it is one.
/// Render one token of a type-argument clause back into the type string.
///
/// Only the spellings that can carry a width are reproduced — names, nesting,
/// separators. Anything else (a variance annotation, a bound, a wildcard) is
/// dropped, which leaves a string that names no width and so narrows nothing.
fn type_tok_text(t: &Tok) -> String {
    match t {
        Tok::Ident(w) => w.clone(),
        Tok::LBracket => "[".to_string(),
        Tok::RBracket => "]".to_string(),
        Tok::Comma => ",".to_string(),
        Tok::Dot => ".".to_string(),
        _ => String::new(),
    }
}

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
/// The `(binding power, right-associative)` of a symbolic method operator, per
/// the SLS: precedence comes from the operator's first character — `:` sits with
/// `::` just below `+`/`-` — and an operator whose name ends in `:` is
/// right-associative.
fn symbolic_op(sym: &str) -> (u8, bool) {
    (first_char_prec(sym.chars().next()), sym.ends_with(':'))
}

/// The SLS operator-precedence table, keyed by an operator's first character
/// (lowest binding first):
///
/// ```text
///   |  ^  &  = !  < >  :  + -  * / %  (anything else)
/// ```
///
/// `&&` and `||` are not special-cased in Scala either — they take their
/// precedence from `&` and `|` like any other symbolic name, which is what makes
/// `a | b && c` read as `a | (b && c)`.
fn first_char_prec(c: Option<char>) -> u8 {
    match c {
        Some('|') => 1,
        Some('^') => 2,
        Some('&') => 3,
        Some('=' | '!') => 4,
        Some('<' | '>') => 5,
        Some(':') => 6,
        Some('+' | '-') => 7,
        Some('*' | '/' | '%') => 8,
        _ => 9,
    }
}

fn binop(t: &Tok) -> Option<(BinOp, u8)> {
    Some(match t {
        Tok::OrOr => (BinOp::Or, 1),
        Tok::AndAnd => (BinOp::And, 3),
        Tok::EqEq => (BinOp::Eq, 4),
        Tok::NotEq => (BinOp::Ne, 4),
        Tok::Lt => (BinOp::Lt, 5),
        Tok::Gt => (BinOp::Gt, 5),
        Tok::Le => (BinOp::Le, 5),
        Tok::Ge => (BinOp::Ge, 5),
        // `::` (cons) — Scala's `:`-family precedence sits just below `+`/`-`
        // and is right-associative (handled in `binary`).
        Tok::ColonColon => (BinOp::Cons, 6),
        Tok::Plus => (BinOp::Add, 7),
        Tok::Minus => (BinOp::Sub, 7),
        Tok::Star => (BinOp::Mul, 8),
        Tok::Slash => (BinOp::Div, 8),
        Tok::Percent => (BinOp::Mod, 8),
        _ => return None,
    })
}

/// The collection constructor an *unqualified* mutable name selects. Only the
/// names that cannot also mean an immutable collection are listed: imports are
/// skipped rather than tracked, so a bare `Set`/`Map` must keep meaning the
/// immutable one (see `BUGS.md`).
/// The package-path segments a `new scala.collection.mutable.X` may be spelled
/// through. `new` takes a type name, so the prefix is consumed and discarded.
const MUTABLE_PREFIXES: &[&str] = &["scala", "collection", "mutable"];

/// Whether `e` is the `scala.collection.mutable` package path — spelled
/// `mutable`, `collection.mutable` or `scala.collection.mutable`. The compiler's
/// `mutable_module` makes the same test on the lowering side; this one exists so
/// the PARSER can tell a factory call apart from a curried method call.
fn is_mutable_pkg(e: &Expr) -> bool {
    match e {
        Expr::Var(n) => n == "mutable",
        Expr::Method {
            recv, name, args, ..
        } if args.is_empty() && name == "mutable" => {
            matches!(&**recv, Expr::Var(p) if p == "collection")
                || matches!(&**recv, Expr::Method { recv, name, args, .. }
                    if args.is_empty() && name == "collection"
                        && matches!(&**recv, Expr::Var(p) if p == "scala"))
        }
        _ => false,
    }
}

/// Whether a member of that package names a collection FACTORY (`mutable.Set`,
/// `mutable.Queue`, …) rather than something curried.
fn mutable_factory_name(name: &str) -> bool {
    mutable_buffer_ctor(name).is_some() || matches!(name, "Set" | "HashSet" | "Map" | "HashMap")
}

fn mutable_buffer_ctor(name: &str) -> Option<&'static str> {
    Some(match name {
        "ListBuffer" => "ListBuffer",
        "ArrayBuffer" | "Buffer" => "ArrayBuffer",
        // `Queue`/`Stack`/`ArrayDeque`/`LinkedHash*` also name immutable or
        // package-qualified types, but none of them is in Scala's default scope,
        // so an unqualified use can only have come from a `scala.collection
        // .mutable` import — which is skipped rather than tracked (see BUGS.md).
        "Queue" => "Queue",
        "Stack" => "Stack",
        "ArrayDeque" => "ArrayDeque",
        "LinkedHashSet" => "LinkedHashSet",
        "LinkedHashMap" => "LinkedHashMap",
        // `StringBuilder` IS in the default scope (`scala.StringBuilder`), so
        // this spelling is exactly Scala's.
        "StringBuilder" => "StringBuilder",
        _ => return None,
    })
}
