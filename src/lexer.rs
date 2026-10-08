//! A hand-written Scala lexer with newline inference.
//!
//! Produces the token stream the parser consumes. Covers the slice-1 surface:
//! identifiers/keywords, integer/floating/string/char literals, and the
//! operators and punctuation used by bindings, expressions, and the control
//! statements. Line/block comments are skipped.
//!
//! Scala is newline-sensitive: a line break separates two statements only where
//! one can actually end and the next can begin. This lexer applies the standard
//! rule — a [`Tok::Newline`] separator is emitted for a source line break iff
//! (1) we are not inside `(...)`/`[...]`, (2) the previous token can *end* a
//! statement, and (3) the next token can *begin* one. Everywhere else the break
//! is whitespace. Explicit `;` is always a separator too.

use std::fmt;

/// A lexical token with its 1-based source line (for error reporting).
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: Tok,
    pub line: u32,
}

/// Token kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals & names
    Int(i64),
    /// An `L`-suffixed integer literal. Carries the same value as [`Tok::Int`];
    /// the suffix is kept (rather than dropped, as the float suffixes are)
    /// because `Long` and `Int` differ in WIDTH, and the compiler needs the
    /// static distinction to decide whether an expression wraps at 32 bits.
    Long(i64),
    Float(f64),
    /// An `f`/`F`-suffixed floating literal — a `Float`, not a `Double`.
    ///
    /// The value is ALREADY rounded to 32-bit precision here, because Scala
    /// rounds an `f` literal at the literal: `16777217.0f` is `1.6777216E7`
    /// (the odd integer has no `f32`) and `1.0e-45f` is `1.4E-45` (the
    /// smallest `f32` subnormal). Reading both at `f64` and narrowing later
    /// answers `1.6777217E7` and `1.0E-45`.
    ///
    /// Kept separate from [`Tok::Float`] for the same reason [`Tok::Long`] is
    /// kept separate from [`Tok::Int`]: the two share a runtime
    /// representation and differ only in the static type, which is what the
    /// compiler needs to decide where arithmetic happens at 32-bit width and
    /// where a value renders as a `Float`.
    Float32(f64),
    Str(String),
    /// A `Char` literal (`'a'`, `'\n'`). Separate from [`Tok::Str`] because
    /// `Char` is its own Scala type: it dispatches as a *number* in arithmetic
    /// (`'a' + 1 == 98`) and as *text* when printed (`println('a')` is `a`),
    /// and `'5'.toInt` (53) differs from `"5".toInt` (5).
    Char(char),
    /// An interpolated string literal — `s"…"`, `f"…"`, or `raw"…"`. Carries the
    /// literal segments (`parts`, length `exprs.len() + 1`), the raw source of
    /// each `$id` / `${expr}` splice (`exprs`), and, for the `f` interpolator,
    /// the per-splice format spec (`fmts`, `%d`/`%.2f`/… or `None` → `%s`). The
    /// parser re-parses each splice source and desugars to string concatenation.
    InterpStr {
        raw: bool,
        is_f: bool,
        parts: Vec<String>,
        exprs: Vec<String>,
        fmts: Vec<Option<String>>,
    },
    Ident(String),
    // keywords
    Object,
    Def,
    Val,
    Var,
    If,
    Else,
    While,
    For,
    Extends,
    New,
    Return,
    Match,
    Case,
    Try,
    Catch,
    Finally,
    Throw,
    True,
    False,
    Null,
    // punctuation
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semi,
    /// An inferred statement separator (a significant source line break).
    Newline,
    Comma,
    Dot,
    Colon,
    /// `<-` — a for-comprehension generator arrow.
    LArrow,
    /// `=>` — a `case` arm / function-literal arrow.
    FatArrow,
    /// `->` — the tuple-pair sugar (`k -> v`).
    RArrow,
    /// `::` — list cons.
    ColonColon,
    /// `#::` — `LazyList`'s cons. Distinct from [`Tok::ColonColon`] because its
    /// right operand is BY-NAME: that is what lets a `LazyList` be defined in
    /// terms of itself.
    HashColonColon,
    /// `@` — the pattern binder (`case n @ Some(v) =>`).
    At,
    /// A symbolic method operator with no dedicated token: `++`, `--`, `:+`, `+:`.
    /// Parsed as an infix method call, with SLS precedence taken from the first
    /// character and right-associativity from a trailing `:`.
    Op(String),
    // operators
    Assign,
    /// `~` — the unary bitwise complement.
    Tilde,
    /// A symbolic compound assignment that is a method call, not arithmetic:
    /// `++=` and `--=` on a growable collection.
    OpAssign(String),
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
    AndAnd,
    OrOr,
    Not,
    Eof,
}

impl Tok {
    /// Whether a token can be the *last* token of a statement/expression — a
    /// precondition for a following line break to act as a separator.
    fn can_end(&self) -> bool {
        matches!(
            self,
            Tok::Int(_)
                | Tok::Long(_)
                | Tok::Float(_)
                | Tok::Float32(_)
                | Tok::Str(_)
                | Tok::Char(_)
                | Tok::InterpStr { .. }
                | Tok::Ident(_)
                | Tok::True
                | Tok::False
                | Tok::Null
                | Tok::Return
                | Tok::RParen
                | Tok::RBracket
                | Tok::RBrace
        )
    }

    /// Whether a token can *begin* a statement/expression — the other
    /// precondition for a line break to act as a separator. Binary-operator,
    /// `.`, `)`/`]`/`}`, `,`, `=`, `else`, and `<-` continuations return false
    /// so a wrapped expression stays one statement.
    fn can_begin(&self) -> bool {
        matches!(
            self,
            Tok::Int(_)
                | Tok::Long(_)
                | Tok::Float(_)
                | Tok::Float32(_)
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
                | Tok::If
                | Tok::While
                | Tok::For
                | Tok::Val
                | Tok::Var
                | Tok::Def
                | Tok::Object
                | Tok::Return
                // `try`/`throw` open a statement; `catch`/`finally` deliberately
                // stay out so a line break before them continues the `try`.
                | Tok::Try
                | Tok::Throw
                // An annotation opens a declaration: `@main def …` on the line
                // after an `import` is two statements, not one. Without this the
                // parser's `import` scan runs past the line break and swallows
                // the `@main`, leaving the file with no entry point. The `@` of
                // a pattern binder (`case n @ 1`) never sits at the start of a
                // line, so it is unaffected.
                | Tok::At
        )
    }
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Lex `src` into a token vector terminated by `Tok::Eof`, with inferred
/// [`Tok::Newline`] statement separators.
///
/// Before tokenizing, every inline `rust { ... }` FFI block is rewritten to a
/// `__rust_compile("<base64>", <line>)` call by [`crate::rust_ffi::desugar`], so
/// the lexer/parser never see raw Rust source. This is the single source-level
/// choke point every path (`parse`, `--dump-tokens`) flows through; it is a
/// no-op when the source has no `rust` block.
pub fn lex(src: &str) -> Result<Vec<Token>, String> {
    let desugared = crate::rust_ffi::desugar(src);
    let src = desugared.as_str();
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut out: Vec<Token> = Vec::new();
    // Bracket nesting for `(` and `[` only — inside them, line breaks never
    // separate. `{ }` blocks do *not* suppress newlines.
    let mut paren_depth: i32 = 0;
    // A source line break has been seen since the last emitted token.
    let mut pending_newline = false;
    // For the optional-braces pass: the indentation of every token that is the
    // FIRST on its source line, index-aligned with `out` (`None` for the rest).
    // A token that follows a multi-line string on the string's last line is not
    // a line start — the string scanners never reset `at_line_start`.
    let mut starts: Vec<Option<u32>> = Vec::new();
    let mut at_line_start = true;
    let mut line_begin = 0usize;
    let mut line_indent = 0u32;
    let mut indent_taken = false;

    // Push a real token, first materializing a pending newline into a separator
    // when the inference rule fires.
    macro_rules! push {
        ($kind:expr, $line:expr) => {{
            let kind = $kind;
            if pending_newline
                && paren_depth == 0
                && out.last().map(|t| t.kind.can_end()).unwrap_or(false)
                && kind.can_begin()
            {
                out.push(Token {
                    kind: Tok::Newline,
                    line: $line,
                });
                starts.push(None);
            }
            pending_newline = false;
            match &kind {
                Tok::LParen | Tok::LBracket => paren_depth += 1,
                Tok::RParen | Tok::RBracket => paren_depth = (paren_depth - 1).max(0),
                _ => {}
            }
            out.push(Token { kind, line: $line });
            starts.push(at_line_start.then_some(line_indent));
            at_line_start = false;
        }};
    }

    while i < bytes.len() {
        let c = bytes[i] as char;

        // whitespace
        if c == '\n' {
            line += 1;
            pending_newline = true;
            i += 1;
            at_line_start = true;
            line_begin = i;
            indent_taken = false;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if at_line_start && !indent_taken {
            line_indent = (i - line_begin) as u32;
            indent_taken = true;
        }

        // comments
        if c == '/' && i + 1 < bytes.len() {
            match bytes[i + 1] as char {
                '/' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                '*' => {
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        if bytes[i] == b'\n' {
                            line += 1;
                            pending_newline = true;
                        }
                        i += 1;
                    }
                    i += 2; // consume closing */
                    continue;
                }
                _ => {}
            }
        }

        // identifiers & keywords
        if c.is_ascii_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < bytes.len() {
                let ch = bytes[i] as char;
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
                    i += 1;
                } else {
                    break;
                }
            }
            // An identifier whose last character is `_` may continue with
            // operator characters (SLS 1.1): `unary_-`, `unary_!`, `x_=`. A lone
            // `_` is the wildcard and never absorbs one: `case _: Int`, `_+1`.
            if i - start > 1 && bytes[i - 1] == b'_' {
                while i < bytes.len() && b"+-*/%<>=!&|^~?:\\".contains(&bytes[i]) {
                    i += 1;
                }
            }
            let word = &src[start..i];
            // Interpolator prefix: `s"…"` / `f"…"` / `raw"…"` — the ident is an
            // interpolator only when a `"` follows with no intervening whitespace
            // (Scala requires adjacency). Otherwise it is a plain identifier.
            if matches!(word, "s" | "f" | "raw") && i < bytes.len() && bytes[i] == b'"' {
                let (tok, ni, nl) = lex_interp(word, bytes, src, i, line)?;
                i = ni;
                line = nl;
                push!(tok, line);
                continue;
            }
            push!(keyword_or_ident(word), line);
            continue;
        }

        // numbers (int or float)
        if c.is_ascii_digit() {
            // Hexadecimal (`0x1F`) — the one radix Scala 3 still accepts. It
            // parses as an `Int`, so the value is read at 32 bits and sign-
            // extended, making `0xFFFFFFFF` the `-1` Scala reads it as.
            if c == '0'
                && i + 2 < bytes.len()
                && matches!(bytes[i + 1], b'x' | b'X')
                && (bytes[i + 2] as char).is_ascii_hexdigit()
            {
                let start = i + 2;
                i = start;
                while i < bytes.len() && (bytes[i] as char).is_ascii_hexdigit() {
                    i += 1;
                }
                let text = &src[start..i];
                let mut is_long = false;
                if i < bytes.len() && matches!(bytes[i], b'L' | b'l') {
                    is_long = true;
                    i += 1;
                }
                let v = u64::from_str_radix(text, 16)
                    .map_err(|_| format!("scalars: bad hex literal `{text}` on line {line}"))?;
                // An `Int`-width hex literal denotes a BIT PATTERN, so `0xFFFFFFFF`
                // is `-1`. The `L` suffix makes it `Long`-width instead, where the
                // same digits are the positive 4294967295 — so the sign extension
                // is exactly what the suffix suppresses.
                let v = if text.len() <= 8 && !is_long {
                    i64::from(v as u32 as i32)
                } else {
                    v as i64
                };
                push!(if is_long { Tok::Long(v) } else { Tok::Int(v) }, line);
                continue;
            }
            let start = i;
            let mut is_float = false;
            while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len()
                && bytes[i] == b'.'
                && i + 1 < bytes.len()
                && (bytes[i + 1] as char).is_ascii_digit()
            {
                is_float = true;
                i += 1;
                while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                    i += 1;
                }
            }
            // exponent part: (e|E)[+|-]digits — `1e7`, `1.5e300`, `2.5e-8`. Only
            // consume the `e` when a digit (after an optional sign) actually
            // follows, so `1 else` / a trailing `e` identifier is left intact.
            if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
                let mut j = i + 1;
                if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] as char).is_ascii_digit() {
                    is_float = true;
                    i = j;
                    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            // Every numeric suffix is KEPT, because each names a static type
            // the one runtime representation cannot: `Long` and `Int` are the
            // same `i64` and differ only in WIDTH, and `Float` and `Double` are
            // the same `f64` and differ only in PRECISION. Only the suffix says
            // which one a literal is. `d`/`D` is the exception — it names the
            // default, so it carries no information and is dropped.
            let mut is_long = false;
            let mut is_f32 = false;
            if i < bytes.len() && matches!(bytes[i], b'L' | b'l' | b'f' | b'F' | b'd' | b'D') {
                match bytes[i] {
                    b'f' | b'F' => {
                        is_float = true;
                        is_f32 = true;
                    }
                    b'd' | b'D' => is_float = true,
                    _ => is_long = true,
                }
                i += 1;
            }
            let text = src[start..i].trim_end_matches(|ch: char| ch.is_ascii_alphabetic());
            if is_float {
                let v: f64 = text
                    .parse()
                    .map_err(|_| format!("scalars: bad float literal `{text}` on line {line}"))?;
                // Scala rounds an `f` literal to single precision AT the
                // literal, before it is ever used, so the rounding belongs
                // here rather than at the first operation.
                if is_f32 {
                    push!(Tok::Float32(f64::from(v as f32)), line);
                } else {
                    push!(Tok::Float(v), line);
                }
            } else {
                // `Long.MinValue` is written `-9223372036854775808L`, and the
                // sign is a separate token — so the MAGNITUDE the lexer sees is
                // one past `i64::MAX`. Parse as `u64` and wrap, which lands on
                // `i64::MIN`; the parser's literal-negation fold then negates it
                // back to itself. Anything genuinely too large still errors.
                let v: i64 = match text.parse::<i64>() {
                    Ok(v) => v,
                    Err(_) => text.parse::<u64>().map(|u| u as i64).map_err(|_| {
                        format!("scalars: bad integer literal `{text}` on line {line}")
                    })?,
                };
                push!(if is_long { Tok::Long(v) } else { Tok::Int(v) }, line);
            }
            continue;
        }

        // Triple-quoted string literals. Everything up to the closing `"""` is
        // taken verbatim — no escape processing — which is why Scala regexes are
        // conventionally written this way (`"""(\d+)"""` needs no `\\d`). Must
        // be tested before the single-quote form, which would otherwise read the
        // opening `"""` as an empty string followed by a new literal.
        if src[i..].starts_with("\"\"\"") {
            let start_line = line;
            i += 3;
            let body_start = i;
            let end = src[i..].find("\"\"\"").ok_or_else(|| {
                format!("scalars: unterminated string literal on line {start_line}")
            })?;
            let body = &src[body_start..body_start + end];
            line += body.matches('\n').count() as u32;
            i = body_start + end;
            // Scala closes at the LAST of a run of quotes, so `"""a""""` is
            // `a"`; skip any extra quotes beyond the closing three.
            while src[i..].starts_with("\"\"\"\"") {
                i += 1;
            }
            i += 3;
            push!(Tok::Str(src[body_start..i - 3].to_string()), start_line);
            continue;
        }

        // string literals
        if c == '"' {
            let start_line = line;
            i += 1;
            let mut s = String::new();
            while i < bytes.len() && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 1;
                    match unicode_escape(bytes, &mut i) {
                        Some(c) => s.push(c),
                        None => {
                            s.push(unescape(bytes[i] as char));
                            i += 1;
                        }
                    }
                } else {
                    // Decode a full UTF-8 char so multibyte literals survive.
                    let ch = src[i..].chars().next().unwrap();
                    if ch == '\n' {
                        line += 1;
                    }
                    s.push(ch);
                    i += ch.len_utf8();
                }
            }
            if i >= bytes.len() {
                return Err(format!(
                    "scalars: unterminated string literal on line {start_line}"
                ));
            }
            i += 1; // closing quote
            push!(Tok::Str(s), start_line);
            continue;
        }

        // char literals — a distinct token, so `Char` stays its own type
        if c == '\'' {
            i += 1;
            let ch = if bytes[i] == b'\\' {
                i += 1;
                match unicode_escape(bytes, &mut i) {
                    Some(c) => c,
                    None => {
                        let c = unescape(bytes[i] as char);
                        i += 1;
                        c
                    }
                }
            } else {
                let c = src[i..].chars().next().unwrap();
                i += c.len_utf8();
                c
            };
            if i >= bytes.len() || bytes[i] != b'\'' {
                return Err(format!("scalars: unterminated char literal on line {line}"));
            }
            i += 1;
            push!(Tok::Char(ch), line);
            continue;
        }

        // operators & punctuation (longest match first)
        //
        // The lookahead slices by BYTE offset, and `i + 2` can land inside a
        // multi-byte character — `,"é` in `List("a", "é")` puts the third byte
        // of the window on the first continuation byte of `é`, and `&src[i..]`
        // panics rather than returning a short slice. Every operator matched
        // below is pure ASCII, so a window that is not a char boundary can never
        // be one of them: answer `""` and fall through to the one-byte arm.
        let two = peek(src, i, 2);
        let three = peek(src, i, 3);
        let (kind, adv) = match three {
            // The growable-collection bulk operators. Longest match first: `++=`
            // would otherwise lex as `++` then `=`.
            "++=" | "--=" => (Tok::OpAssign(three.to_string()), 3),
            // `>>>` — the logical right shift, longest match before `>>`.
            ">>>" => (Tok::Op(three.to_string()), 3),
            // `#::` — `LazyList`'s cons. Matched at three characters, before
            // the `::` below could take the last two and leave a stray `#`.
            "#::" => (Tok::HashColonColon, 3),
            // `:::` and `++:` prepend a whole collection, `+=:` one element in
            // place (a buffer's `prepend`). Each ends in `:`, so
            // they are right-associative and the RIGHT operand is the receiver.
            ":::" | "++:" | "+=:" => (Tok::Op(three.to_string()), 3),
            // `???` — `Predef.???`, an identifier made of operator characters;
            // the parser lowers it to the `NotImplementedError` throw it is.
            "???" => (Tok::Ident(three.to_string()), 3),
            _ if two == "##" => (Tok::Ident(two.to_string()), 2),
            _ => match two {
                "<-" => (Tok::LArrow, 2),
                "=>" => (Tok::FatArrow, 2),
                "->" => (Tok::RArrow, 2),
                "::" => (Tok::ColonColon, 2),
                // `/%` is `BigInt`'s quotient-and-remainder.
                "++" | "--" | ":+" | "+:" | "<<" | ">>" | "&~" | "/%" => {
                    (Tok::Op(two.to_string()), 2)
                }
                "+=" => (Tok::PlusAssign, 2),
                "-=" => (Tok::MinusAssign, 2),
                "*=" => (Tok::StarAssign, 2),
                "/=" => (Tok::SlashAssign, 2),
                "%=" => (Tok::PercentAssign, 2),
                "==" => (Tok::EqEq, 2),
                "!=" => (Tok::NotEq, 2),
                "<=" => (Tok::Le, 2),
                ">=" => (Tok::Ge, 2),
                "&&" => (Tok::AndAnd, 2),
                "||" => (Tok::OrOr, 2),
                _ => match c {
                    '{' => (Tok::LBrace, 1),
                    '}' => (Tok::RBrace, 1),
                    '(' => (Tok::LParen, 1),
                    ')' => (Tok::RParen, 1),
                    '[' => (Tok::LBracket, 1),
                    ']' => (Tok::RBracket, 1),
                    ';' => (Tok::Semi, 1),
                    ',' => (Tok::Comma, 1),
                    '.' => (Tok::Dot, 1),
                    ':' => (Tok::Colon, 1),
                    '=' => (Tok::Assign, 1),
                    // The bitwise / set operators, each an infix method call whose
                    // SLS precedence comes from this first character.
                    '&' | '|' | '^' => (Tok::Op(c.to_string()), 1),
                    '~' => (Tok::Tilde, 1),
                    // `@` — a pattern binder, or an annotation, which the
                    // parser skips where a member or statement starts.
                    '@' => (Tok::At, 1),
                    // `?` — Scala 3's wildcard type argument (`List[?]`), an
                    // identifier wherever a type is read.
                    '?' => (Tok::Ident("?".to_string()), 1),
                    '+' => (Tok::Plus, 1),
                    '-' => (Tok::Minus, 1),
                    '*' => (Tok::Star, 1),
                    '/' => (Tok::Slash, 1),
                    '%' => (Tok::Percent, 1),
                    '<' => (Tok::Lt, 1),
                    '>' => (Tok::Gt, 1),
                    '!' => (Tok::Not, 1),
                    other => {
                        return Err(format!(
                            "scalars: unexpected character `{other}` on line {line}"
                        ))
                    }
                },
            },
        };
        push!(kind, line);
        i += adv;
    }

    out.push(Token {
        kind: Tok::Eof,
        line,
    });
    starts.push(None);
    Ok(optional_braces(out, &starts))
}

/// One open construct while [`optional_braces`] walks the stream: a bracket the
/// source wrote, or an indentation region the pass opened.
enum Open {
    /// `(` or `[` — indentation is not significant inside either.
    Paren,
    /// A `{` the source wrote.
    Brace,
    /// An indentation region, with the indentation of its first line.
    Region(u32),
    /// The `(` a fewer-braces argument (`xs.foreach: x =>`) stands for; it
    /// closes with the region of the lambda's body.
    ColonArg,
}

/// Scala 3's optional braces, as a token-stream rewrite: an indentation region
/// is turned into the `{ … }` block the parser already reads.
///
/// A region opens after a token that ENDS its line and may begin one — `=`,
/// `=>`, `match`, `try`, `catch`, `finally`, `else`, `for`, `then`, `do`, `yield`, and
/// the `:` of an `object`/`class`/`trait`/`enum` header, and the `)` closing an
/// `extension`'s receiver — when the next line is
/// indented deeper than the opener's own line and does not start with `{`. It
/// closes before the first line indented less than the region, before the
/// bracket that encloses it closes, and at the end of the file. Inside `( )` and
/// `[ ]` no region opens, since indentation is not significant there.
///
/// Brace-style code is unaffected: its bodies either start with `{` or are
/// single indented expressions, and wrapping those in a block does not change
/// what they compute.
fn optional_braces(toks: Vec<Token>, starts: &[Option<u32>]) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(toks.len());
    let mut stack: Vec<Open> = Vec::new();
    let mut line_indent = 0u32;
    let mut line_head: Option<Tok> = None;
    let brace = |kind: Tok, line: u32| Token { kind, line };

    // Emit a `}` for every region on top of the stack that `keep` rejects.
    // A separator already emitted before the current token goes AFTER the
    // braces, so the closed block ends the statement it belongs to.
    fn close_regions(
        out: &mut Vec<Token>,
        stack: &mut Vec<Open>,
        line: u32,
        keep: impl Fn(u32) -> bool,
    ) {
        let sep = if matches!(out.last().map(|t| &t.kind), Some(Tok::Newline)) {
            out.pop()
        } else {
            None
        };
        while let Some(Open::Region(r)) = stack.last() {
            if keep(*r) {
                break;
            }
            stack.pop();
            out.push(Token {
                kind: Tok::RBrace,
                line,
            });
            // A lambda body closing ends the argument it was passed as.
            if matches!(stack.last(), Some(Open::ColonArg)) {
                stack.pop();
                out.push(Token {
                    kind: Tok::RParen,
                    line,
                });
            }
        }
        if let Some(sep) = sep {
            out.push(sep);
        }
    }

    let mut skip = 0usize;
    // Whether the current line has had a `case` yet: its `x: T =>` is a typed
    // pattern, never a fewer-braces argument.
    let mut case_on_line = false;
    for (i, tok) in toks.iter().enumerate() {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        if let Some(ind) = starts[i] {
            close_regions(&mut out, &mut stack, tok.line, |r| ind >= r);
            line_indent = ind;
            line_head = Some(tok.kind.clone());
            case_on_line = false;
            // An end marker (`end match`, `end run`) only documents where a
            // region ends; closing it is the outdent's job, done above.
            if is_end_marker(&toks, starts, i) {
                skip = 1;
                continue;
            }
        }
        match tok.kind {
            Tok::RParen | Tok::RBracket | Tok::RBrace => {
                close_regions(&mut out, &mut stack, tok.line, |_| false);
                stack.pop();
            }
            Tok::Eof => close_regions(&mut out, &mut stack, tok.line, |_| false),
            _ => {}
        }
        case_on_line |= tok.kind == Tok::Case;
        // Scala 3's fewer-braces argument: `f: x =>` with the lambda's body on
        // the deeper lines below is `f(x => { … })`.
        if tok.kind == Tok::Colon
            && !case_on_line
            && !matches!(stack.last(), Some(Open::Paren))
            && i > 0
            && toks[i - 1].kind.can_end()
            && colon_lambda_ahead(&toks, starts, i, line_indent)
        {
            out.push(brace(Tok::LParen, tok.line));
            stack.push(Open::ColonArg);
            continue;
        }
        // The fewer-braces BLOCK argument: `f:` ending its line, with the
        // argument on the deeper lines below, is the brace argument `f { … }`,
        // which the parser already reads as a block, a lambda (`xs.map { x =>`),
        // a placeholder function (`xs.map { _ + 1 }`) or a `{ case … }`. A
        // template header's `:` is a region of its own and is left to
        // `opens_region`.
        if tok.kind == Tok::Colon
            && !case_on_line
            && !matches!(stack.last(), Some(Open::Paren))
            && i > 0
            && toks[i - 1].kind.can_end()
            && !opens_region(&tok.kind, line_head.as_ref())
        {
            if let Some(ind) = colon_block_ahead(&toks, starts, i, line_indent) {
                out.push(brace(Tok::LBrace, tok.line));
                stack.push(Open::Region(ind));
                continue;
            }
        }
        out.push(tok.clone());
        match tok.kind {
            Tok::LParen | Tok::LBracket => stack.push(Open::Paren),
            Tok::LBrace => stack.push(Open::Brace),
            _ => {}
        }
        if matches!(stack.last(), Some(Open::Paren)) || !opens_region(&tok.kind, line_head.as_ref())
        {
            continue;
        }
        // The next real token must start a deeper line, and not with `{`.
        let next = (i + 1..toks.len()).find(|&j| toks[j].kind != Tok::Newline);
        if let Some(j) = next {
            if let Some(ind) = starts[j] {
                if ind > line_indent && toks[j].kind != Tok::LBrace && toks[j].kind != Tok::Eof {
                    // A template's `:` IS the brace: `object A:` is `object A {`.
                    if tok.kind == Tok::Colon {
                        out.pop();
                    }
                    out.push(brace(Tok::LBrace, tok.line));
                    stack.push(Open::Region(ind));
                }
            }
        }
    }
    out
}

/// The indentation of a fewer-braces block argument's first line, when the `:`
/// at `colon` ends its line and the next line is indented deeper than `indent`
/// and does not open a brace of its own.
fn colon_block_ahead(
    toks: &[Token],
    starts: &[Option<u32>],
    colon: usize,
    indent: u32,
) -> Option<u32> {
    // The next real token has to START a line (`starts` is `None` for one that
    // continues the colon's line); the lexer infers no separator after a `:`.
    let next = (colon + 1..toks.len()).find(|&k| toks[k].kind != Tok::Newline)?;
    let ind = starts[next]?;
    (ind > indent && !matches!(toks[next].kind, Tok::LBrace | Tok::Eof)).then_some(ind)
}

/// Whether the `:` at `colon` introduces a fewer-braces lambda argument: it is
/// followed, on the same line, by lambda parameters — one name, or a
/// parenthesized list — and an `=>` that ends the line, with the body on a
/// deeper line.
fn colon_lambda_ahead(toks: &[Token], starts: &[Option<u32>], colon: usize, indent: u32) -> bool {
    let mut j = colon + 1;
    match toks.get(j).map(|t| &t.kind) {
        Some(Tok::Ident(_)) => j += 1,
        Some(Tok::LParen) => {
            let mut depth = 0;
            loop {
                match toks.get(j).map(|t| &t.kind) {
                    Some(Tok::LParen) => depth += 1,
                    Some(Tok::RParen) => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    Some(Tok::Eof) | None => return false,
                    _ => {}
                }
                j += 1;
            }
        }
        _ => return false,
    }
    if toks.get(j).map(|t| &t.kind) != Some(&Tok::FatArrow) || starts.get(j) != Some(&None) {
        return false;
    }
    let body = (j + 1..toks.len()).find(|&k| toks[k].kind != Tok::Newline);
    body.is_some_and(|k| starts[k].is_some_and(|ind| ind > indent))
}

/// Whether the line starting at `i` is a Scala 3 end marker: `end` and one
/// name or keyword, alone on the line.
fn is_end_marker(toks: &[Token], starts: &[Option<u32>], i: usize) -> bool {
    let is_end = matches!(&toks[i].kind, Tok::Ident(w) if w == "end");
    let named = toks.get(i + 1).is_some_and(|t| {
        matches!(
            t.kind,
            Tok::Ident(_)
                | Tok::If
                | Tok::While
                | Tok::For
                | Tok::Match
                | Tok::Try
                | Tok::New
                | Tok::Val
        )
    }) && starts.get(i + 1) == Some(&None);
    let alone = match toks.get(i + 2) {
        Some(t) => {
            matches!(t.kind, Tok::Newline | Tok::Eof | Tok::RBrace) || starts[i + 2].is_some()
        }
        None => true,
    };
    is_end && named && alone
}

/// Whether `kind`, ending a line whose first token is `head`, opens an
/// indentation region (see [`optional_braces`]).
fn opens_region(kind: &Tok, head: Option<&Tok>) -> bool {
    match kind {
        // `type T =` names a type, not a block.
        Tok::Assign => !matches!(head, Some(Tok::Ident(w)) if w == "type"),
        Tok::FatArrow
        | Tok::Match
        | Tok::Try
        | Tok::Catch
        | Tok::Finally
        | Tok::Else
        | Tok::For => true,
        // `given Sh[Int] with` and the instance's members on the deeper lines.
        Tok::Ident(w) if w == "with" => matches!(head, Some(Tok::Ident(h)) if h == "given"),
        Tok::Ident(w) => matches!(w.as_str(), "then" | "do" | "yield"),
        Tok::Colon => match head {
            Some(Tok::Object | Tok::Case) => true,
            Some(Tok::Ident(w)) => matches!(w.as_str(), "class" | "trait" | "enum"),
            _ => false,
        },
        // `extension (x: T)` with its methods on the deeper lines below.
        Tok::RParen => matches!(head, Some(Tok::Ident(w)) if w == "extension"),
        _ => false,
    }
}

/// The `n`-byte window at `i`, or `""` when it would run past the end of `src`
/// or split a multi-byte character. Only ASCII operator spellings are matched
/// against the result, so a split window and an absent one are equivalent.
fn peek(src: &str, i: usize, n: usize) -> &str {
    let end = i + n;
    if end <= src.len() && src.is_char_boundary(end) {
        &src[i..end]
    } else {
        ""
    }
}

fn keyword_or_ident(word: &str) -> Tok {
    match word {
        "object" => Tok::Object,
        "def" => Tok::Def,
        "val" => Tok::Val,
        "var" => Tok::Var,
        "if" => Tok::If,
        "else" => Tok::Else,
        "while" => Tok::While,
        "for" => Tok::For,
        "extends" => Tok::Extends,
        "new" => Tok::New,
        "return" => Tok::Return,
        "match" => Tok::Match,
        "case" => Tok::Case,
        "try" => Tok::Try,
        "catch" => Tok::Catch,
        "finally" => Tok::Finally,
        "throw" => Tok::Throw,
        "true" => Tok::True,
        "false" => Tok::False,
        "null" => Tok::Null,
        _ => Tok::Ident(word.to_string()),
    }
}

/// Lex an interpolated string `s"…"` / `f"…"` / `raw"…"`, in either the
/// single-quoted or the TRIPLE-quoted form. `i` points at the first `"`; returns
/// the [`Tok::InterpStr`] token, the index past the closing delimiter, and the
/// updated line. Splits the body into literal `parts` and `$id` / `${expr}`
/// splices; for `f`, captures each splice's trailing `%…` format spec.
/// `raw` keeps backslash escapes literal (`raw"\n"` is a backslash then `n`).
///
/// The triple-quoted form is the SAME interpolator, not the verbatim literal the
/// unprefixed `"""…"""` is. Measured against the reference: `s"""a\tb"""` has
/// length 6 (the `\t` decoded) where `"""q\tw"""` has length 4 (kept literal),
/// and `raw"""r\td"""` keeps its backslash exactly as `raw"r\td"` does. What the
/// third quote changes is only the delimiter — a bare `"` inside is literal, a
/// newline inside is literal, and the string closes at the LAST of a run of
/// quotes, so `s"""a"""""` is `a""`.
///
/// Without this arm the prefix was recognised and the body was not: `s"""x $v"""`
/// lexed as an EMPTY interpolation (`s""`) immediately followed by a plain
/// `"x $v"` literal, and the parser then rejected the program at the stray
/// string. Every multi-line interpolated string was unparseable.
fn lex_interp(
    prefix: &str,
    bytes: &[u8],
    src: &str,
    mut i: usize,
    mut line: u32,
) -> Result<(Tok, usize, u32), String> {
    let raw = prefix == "raw";
    let is_f = prefix == "f";
    let start_line = line;
    let triple = src[i..].starts_with("\"\"\"");
    i += if triple { 3 } else { 1 }; // opening delimiter
    let mut parts: Vec<String> = Vec::new();
    let mut exprs: Vec<String> = Vec::new();
    let mut fmts: Vec<Option<String>> = Vec::new();
    let mut cur = String::new();

    while i < bytes.len() && !at_close(src, i, triple) {
        let c = bytes[i] as char;
        // Escapes: raw keeps them verbatim, others decode.
        if c == '\\' && i + 1 < bytes.len() {
            if raw {
                cur.push('\\');
                let ch = src[i + 1..].chars().next().unwrap();
                cur.push(ch);
                i += 1 + ch.len_utf8();
            } else {
                let mut j = i + 1;
                match unicode_escape(bytes, &mut j) {
                    Some(ch) => {
                        cur.push(ch);
                        i = j;
                    }
                    None => {
                        cur.push(unescape(bytes[i + 1] as char));
                        i += 2;
                    }
                }
            }
            continue;
        }
        // Splices.
        if c == '$' && i + 1 < bytes.len() {
            let n = bytes[i + 1] as char;
            if n == '$' {
                cur.push('$');
                i += 2;
                continue;
            }
            if n == '{' {
                // `${ balanced-braces }` — capture the inner source.
                i += 2;
                let estart = i;
                let mut depth = 1i32;
                while i < bytes.len() && depth > 0 {
                    match bytes[i] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        b'\n' => line += 1,
                        _ => {}
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(format!(
                        "scalars: unterminated `${{`-interpolation on line {start_line}"
                    ));
                }
                let esrc = src[estart..i].to_string();
                i += 1; // closing `}`
                parts.push(std::mem::take(&mut cur));
                exprs.push(esrc);
                fmts.push(if is_f { read_fmt(bytes, &mut i) } else { None });
                continue;
            }
            if n.is_ascii_alphabetic() || n == '_' {
                // `$identifier` — a bare Scala identifier splice.
                let estart = i + 1;
                i += 1;
                while i < bytes.len() {
                    let ch = bytes[i] as char;
                    if ch.is_ascii_alphanumeric() || ch == '_' {
                        i += 1;
                    } else {
                        break;
                    }
                }
                let esrc = src[estart..i].to_string();
                parts.push(std::mem::take(&mut cur));
                exprs.push(esrc);
                fmts.push(if is_f { read_fmt(bytes, &mut i) } else { None });
                continue;
            }
            return Err(format!(
                "scalars: invalid interpolation after `$` on line {line}"
            ));
        }
        // `f`-string literal `%%` decodes to a single `%` (Java Formatter).
        if is_f && c == '%' && i + 1 < bytes.len() && bytes[i + 1] == b'%' {
            cur.push('%');
            i += 2;
            continue;
        }
        let ch = src[i..].chars().next().unwrap();
        if ch == '\n' {
            line += 1;
        }
        cur.push(ch);
        i += ch.len_utf8();
    }
    if i >= bytes.len() {
        return Err(format!(
            "scalars: unterminated interpolated string on line {start_line}"
        ));
    }
    if triple {
        // Scala closes at the LAST of a run of quotes, so the extra ones belong
        // to the string: `s"""a"""""` is `a""`.
        while src[i..].starts_with("\"\"\"\"") {
            cur.push('"');
            i += 1;
        }
        i += 3;
    } else {
        i += 1;
    }
    parts.push(cur);
    Ok((
        Tok::InterpStr {
            raw,
            is_f,
            parts,
            exprs,
            fmts,
        },
        i,
        line,
    ))
}

/// Whether the closing delimiter of an interpolated string starts at `i`.
fn at_close(src: &str, i: usize, triple: bool) -> bool {
    if triple {
        src[i..].starts_with("\"\"\"")
    } else {
        src.as_bytes()[i] == b'"'
    }
}

/// Read an `f`-interpolator format spec `%[flags][width][.precision]conv`
/// immediately after a splice, advancing `i` past it. Returns `None` (leaving `i`
/// unmoved) when no `%`-spec follows, or when a `%%` (a literal percent) does.
fn read_fmt(bytes: &[u8], i: &mut usize) -> Option<String> {
    if *i >= bytes.len() || bytes[*i] != b'%' {
        return None;
    }
    if *i + 1 < bytes.len() && bytes[*i + 1] == b'%' {
        return None; // `%%` is a literal percent, not this splice's format
    }
    let start = *i;
    *i += 1; // `%`
    while *i < bytes.len()
        && matches!(
            bytes[*i],
            b'0'..=b'9' | b'.' | b'-' | b'+' | b' ' | b'#' | b','
        )
    {
        *i += 1;
    }
    if *i < bytes.len() && (bytes[*i] as char).is_ascii_alphabetic() {
        *i += 1; // conversion letter
        Some(String::from_utf8_lossy(&bytes[start..*i]).into_owned())
    } else {
        *i = start; // not a valid spec — leave the `%` as literal text
        None
    }
}

/// Read a `\uXXXX` escape's code point, with the backslash already consumed and
/// `i` on the `u`.
///
/// Java — and so Scala's literals — allows a RUN of `u`s before the four hex
/// digits, which is why `"\uuu0041"` is the same `A` as `"A"`. On success
/// `i` is left past the last digit; on failure it is untouched and the caller
/// falls back to the one-character [`unescape`] table, so a literal that merely
/// starts with `\u` (or names a lone UTF-16 surrogate, which is not a Rust
/// `char`) still lexes rather than aborting the parse.
fn unicode_escape(bytes: &[u8], i: &mut usize) -> Option<char> {
    let mut j = *i;
    if bytes.get(j) != Some(&b'u') {
        return None;
    }
    while bytes.get(j) == Some(&b'u') {
        j += 1;
    }
    let hex = std::str::from_utf8(bytes.get(j..j + 4)?).ok()?;
    let c = char::from_u32(u32::from_str_radix(hex, 16).ok()?)?;
    *i = j + 4;
    Some(c)
}

fn unescape(c: char) -> char {
    match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        // Backspace and form feed — Scala's other two letter escapes.
        'b' => '\u{8}',
        'f' => '\u{c}',
        '0' => '\0',
        '\\' => '\\',
        '"' => '"',
        '\'' => '\'',
        other => other,
    }
}
