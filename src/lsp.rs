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
        "define a method; scalars locates and runs `def main(args: Array[String])`",
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
        "name a parent; `extends App` makes the object body the program entry point",
        "object T extends App { println(\"run\") }",
    ),
    (
        "new",
        "Keyword",
        "instance-construction keyword (reserved; instantiation lands in a later slice)",
        "new T()   // reserved",
    ),
    (
        "return",
        "Keyword",
        "early return from a method (reserved; slice-1 `main` bodies fall off the end)",
        "return x   // reserved",
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
        "until",
        "Contextual",
        "exclusive range bound in a `for`: `a until b` iterates a..b-1",
        "for (i <- 0 until 3) print(i)   // => 012",
    ),
    (
        "to",
        "Contextual",
        "inclusive range bound in a `for`: `a to b` iterates a..b",
        "for (i <- 1 to 3) print(i)   // => 123",
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
        "an import prologue line; skipped in slice 1",
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
