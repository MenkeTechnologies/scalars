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
    Float(f64),
    Str(String),
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
                | Tok::Float(_)
                | Tok::Str(_)
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
                | Tok::Float(_)
                | Tok::Str(_)
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
            }
            pending_newline = false;
            match &kind {
                Tok::LParen | Tok::LBracket => paren_depth += 1,
                Tok::RParen | Tok::RBracket => paren_depth = (paren_depth - 1).max(0),
                _ => {}
            }
            out.push(Token { kind, line: $line });
        }};
    }

    while i < bytes.len() {
        let c = bytes[i] as char;

        // whitespace
        if c == '\n' {
            line += 1;
            pending_newline = true;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
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
                if i < bytes.len() && matches!(bytes[i], b'L' | b'l') {
                    i += 1;
                }
                let v = u64::from_str_radix(text, 16)
                    .map_err(|_| format!("scalars: bad hex literal `{text}` on line {line}"))?;
                let v = if text.len() <= 8 {
                    i64::from(v as u32 as i32)
                } else {
                    v as i64
                };
                push!(Tok::Int(v), line);
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
            // long/float/double suffixes are accepted and dropped
            if i < bytes.len() && matches!(bytes[i], b'L' | b'l' | b'f' | b'F' | b'd' | b'D') {
                if matches!(bytes[i], b'f' | b'F' | b'd' | b'D') {
                    is_float = true;
                }
                i += 1;
            }
            let text = src[start..i].trim_end_matches(|ch: char| ch.is_ascii_alphabetic());
            if is_float {
                let v: f64 = text
                    .parse()
                    .map_err(|_| format!("scalars: bad float literal `{text}` on line {line}"))?;
                push!(Tok::Float(v), line);
            } else {
                let v: i64 = text
                    .parse()
                    .map_err(|_| format!("scalars: bad integer literal `{text}` on line {line}"))?;
                push!(Tok::Int(v), line);
            }
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
                    s.push(unescape(bytes[i] as char));
                    i += 1;
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

        // char literals — modeled as a one-char string
        if c == '\'' {
            i += 1;
            let ch = if bytes[i] == b'\\' {
                i += 1;
                let c = unescape(bytes[i] as char);
                i += 1;
                c
            } else {
                let c = src[i..].chars().next().unwrap();
                i += c.len_utf8();
                c
            };
            if i >= bytes.len() || bytes[i] != b'\'' {
                return Err(format!("scalars: unterminated char literal on line {line}"));
            }
            i += 1;
            push!(Tok::Str(ch.to_string()), line);
            continue;
        }

        // operators & punctuation (longest match first)
        let two = if i + 1 < bytes.len() {
            &src[i..i + 2]
        } else {
            ""
        };
        let three = if i + 2 < bytes.len() {
            &src[i..i + 3]
        } else {
            ""
        };
        let (kind, adv) = match three {
            // The growable-collection bulk operators. Longest match first: `++=`
            // would otherwise lex as `++` then `=`.
            "++=" | "--=" => (Tok::OpAssign(three.to_string()), 3),
            // `>>>` — the logical right shift, longest match before `>>`.
            ">>>" => (Tok::Op(three.to_string()), 3),
            _ => match two {
                "<-" => (Tok::LArrow, 2),
                "=>" => (Tok::FatArrow, 2),
                "->" => (Tok::RArrow, 2),
                "::" => (Tok::ColonColon, 2),
                "++" | "--" | ":+" | "+:" | "<<" | ">>" | "&~" => (Tok::Op(two.to_string()), 2),
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
    Ok(out)
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

/// Lex an interpolated string `s"…"` / `f"…"` / `raw"…"`. `i` points at the
/// opening `"`; returns the [`Tok::InterpStr`] token, the index past the closing
/// `"`, and the updated line. Splits the body into literal `parts` and `$id` /
/// `${expr}` splices; for `f`, captures each splice's trailing `%…` format spec.
/// `raw` keeps backslash escapes literal (`raw"\n"` is a backslash then `n`).
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
    i += 1; // opening quote
    let mut parts: Vec<String> = Vec::new();
    let mut exprs: Vec<String> = Vec::new();
    let mut fmts: Vec<Option<String>> = Vec::new();
    let mut cur = String::new();

    while i < bytes.len() && bytes[i] != b'"' {
        let c = bytes[i] as char;
        // Escapes: raw keeps them verbatim, others decode.
        if c == '\\' && i + 1 < bytes.len() {
            if raw {
                cur.push('\\');
                let ch = src[i + 1..].chars().next().unwrap();
                cur.push(ch);
                i += 1 + ch.len_utf8();
            } else {
                cur.push(unescape(bytes[i + 1] as char));
                i += 2;
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
    i += 1; // closing quote
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

fn unescape(c: char) -> char {
    match c {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        '\\' => '\\',
        '"' => '"',
        '\'' => '\'',
        other => other,
    }
}
