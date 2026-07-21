//! Inline Rust FFI (`rust { ... }` blocks) end-to-end.
//!
//! Each test runs a `.scala` program through the built `scala` binary. The FFI
//! path compiles the block to a real cdylib via `fusevm::ffi` and calls its
//! `extern "C"` exports by name, so a green run here proves the whole bridge —
//! source desugar, call-expression parsing, `has_ffi`-gated dispatch, and the
//! compile/marshal round-trip — actually works, not just that it builds.

use std::process::Command;

/// Run a Scala source string through the `scala` binary; return (stdout, ok).
fn run(src: &str) -> (String, bool) {
    let path = std::env::temp_dir().join(format!("scalars_ffi_{}.scala", fasthash(src)));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_scala"))
        .arg(&path)
        .output()
        .expect("spawn scala");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

/// Combined stdout + stderr, for asserting error diagnostics.
fn run_all(src: &str) -> (String, String, bool) {
    let path = std::env::temp_dir().join(format!("scalars_ffi_{}.scala", fasthash(src)));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_scala"))
        .arg(&path)
        .output()
        .expect("spawn scala");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn fasthash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[test]
fn ffi_block_export_is_called_by_name() {
    let src = "object M {\n  def main(args: Array[String]): Unit = {\n    rust { pub extern \"C\" fn scala_triple(x: i64) -> i64 { x * 3 } }\n    println(scala_triple(14))\n  }\n}\n";
    let (out, ok) = run(src);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "42\n");
}

#[test]
fn ffi_export_takes_two_args() {
    let src = "object M {\n  def main(args: Array[String]): Unit = {\n    rust { pub extern \"C\" fn scala_add(a: i64, b: i64) -> i64 { a + b } }\n    println(scala_add(40, 2))\n  }\n}\n";
    let (out, ok) = run(src);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "42\n");
}

#[test]
fn ffi_works_in_app_body() {
    let src = "object M extends App {\n  rust { pub extern \"C\" fn scala_sq(x: i64) -> i64 { x * x } }\n  println(scala_sq(7))\n}\n";
    let (out, ok) = run(src);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "49\n");
}

#[test]
fn unknown_call_without_ffi_block_is_a_compile_error() {
    // Without a `rust { ... }` block the program has no FFI, so an unknown call
    // keeps the normal "not found" diagnostic rather than a runtime dispatch.
    let src =
        "object M {\n  def main(args: Array[String]): Unit = {\n    println(nope(1))\n  }\n}\n";
    let (_out, err, ok) = run_all(src);
    assert!(!ok, "expected failure but run succeeded");
    assert!(err.contains("not found: nope"), "unexpected error: {err}");
}
