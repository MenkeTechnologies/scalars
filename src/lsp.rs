//! Language Server Protocol over stdio (`scala --lsp`).
//!
//! Self-contained and read-only: diagnostics come from the same `parser::parse`
//! the runtime uses (a syntax error maps to the reported line); hover and
//! completion draw on the keyword / Predef corpus below. No output ever reaches
//! the terminal — JSON-RPC on stdio only. Structure follows the sibling `-rs`
//! frontends' `lsp.rs` (see `pythonrs/src/lsp.rs`).

use std::collections::HashMap;

use lsp_server::{Connection, ErrorCode, ExtractError, Message, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Notification as _,
    PublishDiagnostics,
};
use lsp_types::request::{Completion, HoverRequest, Request as _};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse,
    Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, Hover, HoverContents, HoverParams, HoverProviderCapability,
    MarkupContent, MarkupKind, Position, PublishDiagnosticsParams, Range, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions, Uri,
};

/// The keyword / Predef corpus: (name, chapter, one-line doc, example). Single
/// source of truth for LSP completion, hover, and the generated
/// `docs/reference.html`. Every entry mirrors something the scalars runtime
/// actually recognizes:
///   * "Keyword"    → a reserved word in `lexer.rs` (`keyword_or_ident`).
///   * "Contextual" → a word the parser gives meaning in position (`until`/`to`
///     range bounds, `App`/`main` entry points, `package`/`import` prologue).
///   * "Predef"     → a print builtin dispatched in `host.rs` (`SPRINTLN`/`SPRINT`).
///   * "Operator"   → an operator token the parser lowers to a fusevm op / host
///     hook.
///
/// A name is documented here only if the runtime recognizes it, so the language
/// server and the static reference never drift from what `scala` can run.
const CORPUS: &[(&str, &str, &str, &str)] = &[
    // ── Keyword (lexer keyword set) ──
    (
        "object",
        "Keyword",
        "declare a singleton object; scalars runs its `main` or `extends App` body",
        "object T extends App { println(\"hi\") }",
    ),
    (
        "def",
        "Keyword",
        "define a method; scalars locates and runs `def main(args: Array[String])`. A `def` inside a block is scoped to that block and may shadow an outer one",
        "object T { def main(args: Array[String]): Unit = { println(1) } }",
    ),
    (
        "val",
        "Keyword",
        "an immutable binding: `val x = expr`",
        "val x = 41\nprintln(x + 1)   // => 42",
    ),
    (
        "var",
        "Keyword",
        "a mutable binding, reassignable with `=` / `+=` / `-=` / `*=` / `/=` / `%=`",
        "var n = 0\nn += 5\nprintln(n)   // => 5",
    ),
    (
        "if",
        "Keyword",
        "conditional branch: `if (cond) { .. } else { .. }`",
        "if (1 < 2) println(\"yes\") else println(\"no\")",
    ),
    (
        "else",
        "Keyword",
        "the fallback branch of an `if`",
        "if (false) println(1) else println(2)   // => 2",
    ),
    (
        "while",
        "Keyword",
        "loop while the condition is true: `while (cond) { .. }`",
        "var i = 0\nwhile (i < 3) { i += 1 }\nprintln(i)   // => 3",
    ),
    (
        "for",
        "Keyword",
        "range comprehension in statement position: `for (i <- a until b) { .. }`",
        "for (i <- 1 to 3) println(i)   // prints 1 2 3",
    ),
    (
        "extends",
        "Keyword",
        "name a supertype: its fields and concrete methods are inherited, and its constructor takes the `extends P(args)` arguments. `extends App` instead makes the object body the program entry point",
        "class A(val n: Int)\nclass B(m: Int) extends A(m)\nprintln(new B(2).n)   // => 2",
    ),
    (
        "trait",
        "Keyword",
        "declare an abstract type to mix in: abstract members (`def f: Int`, `val x: String`) plus concrete ones. Cannot be instantiated",
        "trait S { def area: Int; def show: String = \"a=\" + area }\nclass C(r: Int) extends S { def area = r * r }\nprintln(new C(3).show)   // => a=9",
    ),
    (
        "new",
        "Keyword",
        "construct an instance: a user `class`/`case class`, or a built-in throwable",
        "class P(val n: Int)\nprintln(new P(2).n)   // => 2",
    ),
    (
        "return",
        "Keyword",
        "early return from a `def`, exiting before the body's last expression",
        "def f(x: Int): Int = { if (x < 0) return 0; x * 2 }\nprintln(f(-1))   // => 0",
    ),
    (
        "try",
        "Keyword",
        "run a body with handlers: `try { .. } catch { case e: T => .. } finally { .. }`; its value is the body's, or the matching handler's",
        "println(try { 1 / 0 } catch { case _: ArithmeticException => -1 })   // => -1",
    ),
    (
        "catch",
        "Keyword",
        "the handler block of a `try`; arms are `case e: Type [if guard] => ..` and match the JVM throwable hierarchy",
        "try { \"z\".toInt } catch { case e: NumberFormatException => println(e.getMessage) }",
    ),
    (
        "finally",
        "Keyword",
        "a block that runs on both the normal and the exceptional exit of a `try`, before the exception continues unwinding",
        "try { println(1) } finally { println(\"done\") }   // => 1 done",
    ),
    (
        "throw",
        "Keyword",
        "raise an exception; an expression (type `Nothing`), so it may appear in operand position",
        "def pick(b: Boolean): Int = if (b) 7 else throw new RuntimeException(\"no\")",
    ),
    (
        "true",
        "Keyword",
        "the Boolean true literal",
        "println(true && false)   // => false",
    ),
    (
        "false",
        "Keyword",
        "the Boolean false literal",
        "println(false || true)   // => true",
    ),
    (
        "null",
        "Keyword",
        "the null reference literal; prints as `null`",
        "val x = null\nprintln(x)   // => null",
    ),
    // ── Contextual (parser-recognized in position) ──
    (
        "with",
        "Contextual",
        "mix another supertype into a `class`/`trait`: `extends P with T1 with T2`. Method lookup is self, then the parents right-to-left",
        "trait L { def tag = \"L\" }\nclass R extends L\nprintln(new R().tag)   // => L",
    ),
    (
        "override",
        "Contextual",
        "replace a supertype's concrete member; the call dispatches on the receiver's runtime class",
        "class A { def f = 1 }\nclass B extends A { override def f = 2 }\nval a: A = new B()\nprintln(a.f)   // => 2",
    ),
    (
        "super",
        "Contextual",
        "call the supertype's implementation of a member, skipping this type in the linearization",
        "class A { def f = \"a\" }\nclass B extends A { override def f = super.f + \"b\" }\nprintln(new B().f)   // => ab",
    ),
    (
        "isInstanceOf",
        "Contextual",
        "runtime type test against the registered class hierarchy; the same test `case x: T =>` performs",
        "trait S; class C extends S\nprintln(new C().isInstanceOf[S])   // => true",
    ),
    (
        "until",
        "Contextual",
        "exclusive range bound: `a until b` is the values a..b-1, as a `for` generator or as a first-class `Range`",
        "for (i <- 0 until 3) print(i)   // => 012",
    ),
    (
        "to",
        "Contextual",
        "inclusive range bound: `a to b` is the values a..b, as a `for` generator or as a first-class `Range`",
        "println((1 to 3).sum)   // => 6",
    ),
    (
        "by",
        "Contextual",
        "range step: `a until b by s`; a negative step counts down, and a zero step throws `IllegalArgumentException`",
        "for (i <- 10 to 1 by -3) print(i + \" \")   // => 10 7 4 1",
    ),
    (
        "Array",
        "Contextual",
        "the mutable sequence: `Array(a, b)`, `new Array[T](n)` (zero-filled), `a(i)` reads and `a(i) = v` writes",
        "val a = Array(1, 2, 3)\na(1) = 9\nprintln(a.mkString(\",\"))   // => 1,9,3",
    ),
    (
        "yield",
        "Contextual",
        "collect a `for` comprehension's body for each binding instead of running it for effect; a range generator yields a `Vector`, a collection generator the source's own kind",
        "println(for (x <- List(1, 2, 3) if x > 1) yield x * 10)   // => List(20, 30)",
    ),
    (
        "List",
        "Contextual",
        "the default immutable sequence (`Seq` is an alias for it): `List(a, b)`, `Nil`, `::` cons, indexing `xs(i)`, and the combinator set (`map`, `filter`, `flatMap`, `foldLeft`, `sorted`, `zip`, `groupBy`, `mkString`, …)",
        "println(List(3, 1, 2).sorted.map(_ * 2))   // => List(2, 4, 6)",
    ),
    (
        "Vector",
        "Contextual",
        "the immutable indexed sequence (`IndexedSeq` is an alias for it); the same combinators as `List`, and what a range comprehension yields",
        "println(Vector(1, 2, 3).map(_ + 1))   // => Vector(2, 3, 4)",
    ),
    (
        "Set",
        "Contextual",
        "the immutable set: duplicates dropped, `+`/`-`/`++`/`union`/`intersect`/`diff`/`subsetOf`. Up to four elements it prints in insertion order; beyond that it is a `HashSet` printed in hash-trie order",
        "println(Set(3, 1, 2))\nprintln(Set(9, 3, 1, 2, 7))   // => Set(3, 1, 2) then HashSet(1, 9, 2, 7, 3)",
    ),
    (
        "Map",
        "Contextual",
        "the immutable map of `k -> v` pairs: `apply`/`get`/`getOrElse`/`contains`/`keys`/`values`/`updated`/`+`/`-`. Up to four entries it prints in insertion order; beyond that it is a `HashMap` printed in hash-trie order",
        "val m = Map(\"a\" -> 1, \"b\" -> 2)\nprintln(m.getOrElse(\"z\", 0))   // => 0",
    ),
    (
        "mutable",
        "Contextual",
        "the `scala.collection.mutable` package: `mutable.ListBuffer`, `mutable.ArrayBuffer`, `mutable.Set` and `mutable.Map`, mutated with `+=`/`-=`/`++=`/`--=`. A mutable `Set`/`Map` prints `HashSet(…)`/`HashMap(…)` at every size, in its hash table's iteration order",
        "val s = mutable.Set(1, 2, 3)\ns += 4\nprintln(s)   // => HashSet(1, 2, 3, 4)",
    ),
    (
        "ListBuffer",
        "Contextual",
        "the growable `scala.collection.mutable` sequence: `+=`, `++=`, `-=`, `append`, `prepend`, `insert`, `remove`, `clear`, `b(i) = v`, plus every `List` combinator. A derived collection is another `ListBuffer`",
        "val b = mutable.ListBuffer(1, 2)\nb += 3\nprintln(b)   // => ListBuffer(1, 2, 3)",
    ),
    (
        "ArrayBuffer",
        "Contextual",
        "the growable indexed `scala.collection.mutable` sequence (`Buffer` is an alias); the same mutators and combinators as `ListBuffer`, printed `ArrayBuffer(…)`",
        "val a = mutable.ArrayBuffer(1, 2)\na ++= List(3, 4)\nprintln(a)   // => ArrayBuffer(1, 2, 3, 4)",
    ),
    (
        "PartialFunction",
        "Contextual",
        "a function defined only on some arguments — what a `{ case … }` literal builds. Besides `apply` it answers `isDefinedAt`, which is what `collect`/`collectFirst` use to skip a non-matching element; `applyOrElse`, `lift`, `orElse`, `andThen` and `compose` compose them",
        "val pf: PartialFunction[Int, String] = { case 1 => \"one\" }\nprintln(pf.isDefinedAt(2))   // => false\nprintln(pf.lift(1))          // => Some(one)",
    ),
    (
        "collect",
        "Contextual",
        "`filter` and `map` in one pass over a partial function: an element the function is not defined at is skipped instead of raising `MatchError`, and the arm body never runs for it. `collectFirst` answers the first result as an `Option`",
        "println(List(1, 2, 3, 4).collect { case x if x % 2 == 0 => x * 10 })   // => List(20, 40)",
    ),
    (
        "Nil",
        "Contextual",
        "the empty `List`; the tail a `::` chain terminates with",
        "println(1 :: 2 :: Nil)   // => List(1, 2)",
    ),
    (
        "math",
        "Contextual",
        "the `scala.math` module (also spelled `scala.math`, `Math`, `java.lang.Math`): `abs`, `min`/`max`, `sqrt`, `pow`, `floor`/`ceil`/`round`, the trig family, `Pi`, `E`",
        "println(math.sqrt(16.0))   // => 4.0",
    ),
    (
        "App",
        "Contextual",
        "the mixin whose object body scalars runs directly as the program",
        "object T extends App { println(\"hi\") }",
    ),
    (
        "main",
        "Contextual",
        "the entry method scalars runs: `def main(args: Array[String]): Unit`",
        "def main(args: Array[String]): Unit = { println(0) }",
    ),
    (
        "package",
        "Contextual",
        "a package prologue line; scalars skips it and runs the object entry",
        "package demo\nobject T extends App { println(1) }",
    ),
    (
        "import",
        "Contextual",
        "an import prologue line; tolerated and ignored (imports are skipped, not tracked)",
        "import scala.math._\nobject T extends App { println(1) }",
    ),
    // ── Predef (print builtins in host.rs) ──
    (
        "println",
        "Predef",
        "print one Scala-formatted argument followed by a newline; returns Unit",
        "println(3.0)   // prints 3.0",
    ),
    (
        "print",
        "Predef",
        "print one Scala-formatted argument with no trailing newline",
        "print(\"a\"); print(\"b\")   // prints ab",
    ),
    // ── Operator (lowered to a fusevm op or the numeric hook) ──
    (
        "+",
        "Operator",
        "numeric addition, or String concatenation when either operand is a String",
        "println(\"n=\" + 41)   // => n=41",
    ),
    (
        "/",
        "Operator",
        "division: truncating for two Ints (`7/2==3`), floating if either is a Double",
        "println(7 / 2); println(7 / 2.0)   // => 3 then 3.5",
    ),
    (
        "%",
        "Operator",
        "remainder of integer division",
        "println(7 % 3)   // => 1",
    ),
    (
        "==",
        "Operator",
        "structural equality (Scala `==` is value `equals`, so strings compare by content)",
        "println(\"a\" == \"a\")   // => true",
    ),
    (
        "&&",
        "Operator",
        "short-circuiting logical AND",
        "println(true && false)   // => false",
    ),
    (
        "||",
        "Operator",
        "short-circuiting logical OR",
        "println(false || true)   // => true",
    ),
    (
        "&",
        "Operator",
        "bitwise AND on `Int`, non-short-circuiting AND on `Boolean`, intersection on `Set`. It binds tighter than `^` and `|` (and `&~` is set difference)",
        "println(6 & 3)                     // => 2\nprintln(Set(1, 2, 3) & Set(2, 3, 4))   // => Set(2, 3)",
    ),
    (
        "|",
        "Operator",
        "bitwise OR on `Int`, non-short-circuiting OR on `Boolean`, union on `Set`. The loosest-binding symbolic operator, which is where `||` gets its precedence",
        "println(6 | 3)   // => 7\nprintln(5 & 3 | 2)   // => 3",
    ),
    (
        "^",
        "Operator",
        "bitwise XOR on `Int`, exclusive OR on `Boolean`. Binds between `|` and `&`",
        "println(6 ^ 3)   // => 5",
    ),
    (
        "~",
        "Operator",
        "bitwise complement of an `Int` (`unary_~`). Parenthesize a negative operand — Scala lexes `~-` as one operator name",
        "println(~(6))   // => -7",
    ),
    (
        "<<",
        "Operator",
        "left shift, evaluated at `Int` width: the distance masks to five bits and the result wraps at 32 bits, so `1 << 33` is `2`. `>>` is the arithmetic right shift and `>>>` the logical one",
        "println(1 << 4)     // => 16\nprintln(-16 >>> 2)   // => 1073741820",
    ),
];

/// The keyword/Predef corpus, exposed for offline doc generation.
pub fn corpus() -> &'static [(&'static str, &'static str, &'static str, &'static str)] {
    CORPUS
}

/// Open document text keyed by URI, kept current from the sync notifications so
/// hover can look up the identifier under the cursor.
type Docs = HashMap<String, String>;

/// Entry point for `scala --lsp`.
pub fn run() -> Result<(), String> {
    spawn_orphan_guard();
    let (conn, io_threads) = Connection::stdio();
    let (init_id, _params) = conn
        .initialize_start()
        .map_err(|e| format!("lsp initialize: {e}"))?;
    let init_result = serde_json::json!({
        "capabilities": server_capabilities(),
        "serverInfo": { "name": "scalars", "version": env!("CARGO_PKG_VERSION") },
    });
    conn.sender
        .send(Response::new_ok(init_id, init_result).into())
        .map_err(|e| format!("lsp send: {e}"))?;

    let mut docs: Docs = HashMap::new();
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn
                    .handle_shutdown(&req)
                    .map_err(|e| format!("lsp shutdown: {e}"))?
                {
                    break;
                }
                dispatch_request(&conn, &docs, req);
            }
            Message::Notification(not) => dispatch_notification(&conn, &mut docs, not),
            Message::Response(_) => {}
        }
    }
    drop(conn);
    io_threads.join().map_err(|_| "lsp io join".to_string())?;
    Ok(())
}

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                ..Default::default()
            },
        )),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(false),
            ..Default::default()
        }),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..Default::default()
    }
}

fn handle<P, R>(conn: &Connection, req: Request, f: impl FnOnce(P) -> R)
where
    P: serde::de::DeserializeOwned,
    R: serde::Serialize,
{
    let method = req.method.clone();
    let id = req.id.clone();
    match req.extract::<P>(&method) {
        Ok((id, params)) => {
            let value = serde_json::to_value(f(params)).unwrap_or(serde_json::Value::Null);
            let _ = conn.sender.send(Response::new_ok(id, value).into());
        }
        Err(ExtractError::JsonError { error, .. }) => {
            let _ = conn.sender.send(
                Response::new_err(id, ErrorCode::InvalidParams as i32, error.to_string()).into(),
            );
        }
        Err(ExtractError::MethodMismatch(_)) => unreachable!("method matched before extract"),
    }
}

fn dispatch_request(conn: &Connection, docs: &Docs, req: Request) {
    match req.method.as_str() {
        Completion::METHOD => handle(conn, req, |_p: CompletionParams| completions()),
        HoverRequest::METHOD => handle(conn, req, |p: HoverParams| hover(docs, &p)),
        _ => {
            let _ = conn.sender.send(
                Response::new_err(req.id, ErrorCode::MethodNotFound as i32, "unhandled".into())
                    .into(),
            );
        }
    }
}

fn dispatch_notification(conn: &Connection, docs: &mut Docs, not: lsp_server::Notification) {
    match not.method.as_str() {
        DidOpenTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidOpenTextDocumentParams>(not.params) {
                let uri = p.text_document.uri;
                docs.insert(uri.as_str().to_string(), p.text_document.text.clone());
                publish_diagnostics(conn, &uri, &p.text_document.text);
            }
        }
        DidChangeTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidChangeTextDocumentParams>(not.params) {
                if let Some(change) = p.content_changes.into_iter().last() {
                    let uri = p.text_document.uri;
                    docs.insert(uri.as_str().to_string(), change.text.clone());
                    publish_diagnostics(conn, &uri, &change.text);
                }
            }
        }
        DidCloseTextDocument::METHOD => {
            if let Ok(p) = serde_json::from_value::<DidCloseTextDocumentParams>(not.params) {
                let uri = p.text_document.uri;
                docs.remove(uri.as_str());
                publish_diagnostics(conn, &uri, "");
            }
        }
        _ => {}
    }
}

fn completions() -> CompletionResponse {
    let items = CORPUS
        .iter()
        .map(|(name, chapter, doc, _example)| CompletionItem {
            label: name.to_string(),
            kind: Some(match *chapter {
                "Keyword" => CompletionItemKind::KEYWORD,
                "Predef" => CompletionItemKind::FUNCTION,
                "Operator" => CompletionItemKind::OPERATOR,
                _ => CompletionItemKind::CONSTANT,
            }),
            detail: Some((*doc).to_string()),
            ..Default::default()
        })
        .collect();
    CompletionResponse::Array(items)
}

/// Hover: look up the identifier under the cursor in the corpus and render its
/// chapter, doc, and example. Falls back to a short banner when the cursor is
/// not on a known name.
fn hover(docs: &Docs, params: &HoverParams) -> Hover {
    let pos = params.text_document_position_params.position;
    let uri = params
        .text_document_position_params
        .text_document
        .uri
        .as_str();
    let word = docs
        .get(uri)
        .and_then(|text| word_at(text, pos))
        .unwrap_or_default();

    let matches: Vec<&(&str, &str, &str, &str)> =
        CORPUS.iter().filter(|(name, ..)| *name == word).collect();

    let body = if matches.is_empty() {
        "**scalars** — Scala on the fusevm bytecode VM + Cranelift JIT.".to_string()
    } else {
        let mut out = String::new();
        for (name, chapter, doc, example) in matches {
            out.push_str(&format!(
                "**`{name}`** — _{chapter}_\n\n{doc}\n\n```scala\n{example}\n```\n\n"
            ));
        }
        out.trim_end().to_string()
    };

    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: body,
        }),
        range: None,
    }
}

/// Extract the identifier (`[A-Za-z0-9_]+`) spanning the given position, if any.
fn word_at(text: &str, pos: Position) -> Option<String> {
    let line = text.lines().nth(pos.line as usize)?;
    let chars: Vec<char> = line.chars().collect();
    let col = (pos.character as usize).min(chars.len());
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';

    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some(chars[start..end].iter().collect())
}

fn publish_diagnostics(conn: &Connection, uri: &Uri, text: &str) {
    let params = PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics: compute_diagnostics(text),
        version: None,
    };
    let not = lsp_server::Notification::new(PublishDiagnostics::METHOD.to_string(), params);
    let _ = conn.sender.send(not.into());
}

/// Parse the whole document with the runtime's own parser; a syntax error maps
/// to a single diagnostic on the line named in its `on line N` / `(line N)`
/// suffix.
fn compute_diagnostics(text: &str) -> Vec<Diagnostic> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    match crate::parser::parse(text) {
        Ok(_) => Vec::new(),
        Err(e) => {
            let line = parse_error_line(&e).saturating_sub(1);
            vec![Diagnostic {
                range: Range {
                    start: Position { line, character: 0 },
                    end: Position {
                        line,
                        character: 200,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: e,
                ..Default::default()
            }]
        }
    }
}

/// Extract the (1-based) line number from a scalars parser error, which embeds
/// it as `… on line N` or `… (line N)`. Defaults to line 1 when no such marker
/// is present.
fn parse_error_line(e: &str) -> u32 {
    for sep in ["on line ", "(line "] {
        if let Some((_, rest)) = e.rsplit_once(sep) {
            if let Some(n) = rest
                .split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
                .and_then(|n| n.parse().ok())
            {
                return n;
            }
        }
    }
    1
}

/// Exit if reparented to pid 1 (the editor died) so we never leak.
fn spawn_orphan_guard() {
    std::thread::spawn(|| {
        #[cfg(target_os = "linux")]
        // SAFETY: prctl(PR_SET_PDEATHSIG, ...) only registers a signal disposition.
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            );
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            // SAFETY: getppid takes no arguments and never fails.
            if unsafe { libc::getppid() } == 1 {
                std::process::exit(0);
            }
        }
    });
}
