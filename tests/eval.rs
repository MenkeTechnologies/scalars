//! Integration tests: run `.scala` programs through the built `scala` binary
//! and assert their stdout. Every expected output here was diffed byte-for-byte
//! against a reference Scala (`scala <file>`) during authoring, then frozen so
//! the suite is self-contained — CI needs no Scala toolchain installed.

use std::process::Command;

/// Run a Scala source string through the `scala` binary and return (stdout, ok).
fn run(src: &str) -> (String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("scalars_test_{}.scala", fasthash(src)));
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

fn fasthash(s: &str) -> u64 {
    // A tiny FNV-1a so concurrent tests use distinct temp files.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Wrap a statement body in the `object … extends App` entry form (the body
/// runs directly).
fn wrap(body: &str) -> String {
    format!("object T extends App {{ {body} }}")
}

#[test]
fn prints_a_string_literal() {
    let (out, ok) = run(&wrap(r#"println("hello")"#));
    assert!(ok);
    assert_eq!(out, "hello\n");
}

#[test]
fn integer_arithmetic_and_precedence() {
    let (out, _) = run(&wrap("println(2 + 3 * 4 - 1)"));
    assert_eq!(out, "13\n");
}

#[test]
fn integer_division_truncates_and_modulo() {
    let (out, _) = run(&wrap("println(7 / 2); println(7 % 3)"));
    assert_eq!(out, "3\n1\n");
}

#[test]
fn division_floats_when_an_operand_is_double() {
    let (out, _) = run(&wrap("println(7 / 2.0); println(7.0 / 2)"));
    assert_eq!(out, "3.5\n3.5\n");
}

#[test]
fn negative_integer_division_truncates_toward_zero() {
    let (out, _) = run(&wrap("println(-7 / 2); println(-7 % 3)"));
    assert_eq!(out, "-3\n-1\n");
}

#[test]
fn string_plus_int_concatenation() {
    let (out, _) = run(&wrap(r#"val x = 21; println("x=" + x * 2)"#));
    assert_eq!(out, "x=42\n");
}

#[test]
fn boolean_prints_scala_style() {
    let (out, _) = run(&wrap("println(3 > 2); println(1 == 2)"));
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn string_equality_is_structural() {
    let (out, _) = run(&wrap(
        r#"val s = "hi"; println(s == "hi"); println(s != "bye")"#,
    ));
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn double_prints_with_trailing_point_zero() {
    let (out, _) = run(&wrap("val d = 3.0; println(d)"));
    assert_eq!(out, "3.0\n");
}

#[test]
fn if_else_chain() {
    let (out, _) = run(&wrap(
        r#"val n = 5; if (n < 0) println("neg") else if (n == 0) println("zero") else println("pos")"#,
    ));
    assert_eq!(out, "pos\n");
}

#[test]
fn while_loop_accumulates() {
    let (out, _) = run(&wrap(
        "var sum = 0; var i = 1; while (i <= 5) { sum += i; i += 1 }; println(sum)",
    ));
    assert_eq!(out, "15\n");
}

#[test]
fn for_until_is_exclusive() {
    let (out, _) = run(&wrap("for (i <- 0 until 3) println(i)"));
    assert_eq!(out, "0\n1\n2\n");
}

#[test]
fn for_to_is_inclusive() {
    let (out, _) = run(&wrap("for (i <- 1 to 3) println(i)"));
    assert_eq!(out, "1\n2\n3\n");
}

#[test]
fn nested_for_bounds_do_not_alias() {
    let (out, _) = run(&wrap(
        "for (a <- 0 until 2) { for (b <- 0 until 2) { println(a * 10 + b) } }",
    ));
    assert_eq!(out, "0\n1\n10\n11\n");
}

#[test]
fn short_circuit_and_or() {
    let (out, _) = run(&wrap(
        "val x = 5; println(x > 0 && x < 10); println(x < 0 || x == 5)",
    ));
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn unary_negation_and_not() {
    let (out, _) = run(&wrap("val x = 3; println(-x); println(!(x > 5))"));
    assert_eq!(out, "-3\ntrue\n");
}

#[test]
fn fizzbuzz_first_five() {
    let (out, _) = run(&wrap(
        r#"for (i <- 1 to 5) { if (i % 15 == 0) println("FizzBuzz") else if (i % 3 == 0) println("Fizz") else if (i % 5 == 0) println("Buzz") else println(i) }"#,
    ));
    assert_eq!(out, "1\n2\nFizz\n4\nBuzz\n");
}

#[test]
fn utf8_string_literal_survives() {
    let (out, _) = run(&wrap(r#"println("café — ☕")"#));
    assert_eq!(out, "café — ☕\n");
}

#[test]
fn newline_separated_statements_need_no_semicolons() {
    let (out, _) = run(
        "object T extends App {\n  val a = 1\n  val b = 2\n  println(a + b)\n  println(a * b)\n}",
    );
    assert_eq!(out, "3\n2\n");
}

#[test]
fn def_main_entry_form() {
    let (out, ok) = run(
        "object M { def main(args: Array[String]): Unit = { println(\"from main\"); println(40 + 2) } }",
    );
    assert!(ok);
    assert_eq!(out, "from main\n42\n");
}

#[test]
fn missing_entry_point_is_an_error() {
    let (_out, ok) = run("object NoEntry { val x = 3 }");
    assert!(
        !ok,
        "an object with no main and no `extends App` should fail"
    );
}

#[test]
fn double_uses_java_scientific_notation() {
    // `Double.toString`: decimal in [1e-3, 1e7), scientific outside. Verified
    // byte-for-byte against reference scala (found by the parity fuzzer).
    let (out, _) = run(&wrap(
        "println(1e7); println(1e-4); println(123456789.0); println(1.5e300); println(9999999.0); println(0.001)",
    ));
    assert_eq!(
        out,
        "1.0E7\n1.0E-4\n1.23456789E8\n1.5E300\n9999999.0\n0.001\n"
    );
}

#[test]
fn exponent_float_literals_parse() {
    let (out, ok) = run(&wrap(
        "println(6.022e23); println(1E10); println(2.5e-8 * 2.0)",
    ));
    assert!(ok);
    assert_eq!(out, "6.022E23\n1.0E10\n5.0E-8\n");
}

#[test]
fn string_left_plus_concatenates_anything() {
    let (out, _) = run(&wrap(
        r#"println("a" + false); println("a" + null); println("n=" + 3 + "!")"#,
    ));
    assert_eq!(out, "afalse\nanull\nn=3!\n");
}

#[test]
fn numeric_plus_string_concatenates() {
    let (out, _) = run(&wrap(r#"println(1 + "a"); println(1.5 + "b")"#));
    assert_eq!(out, "1a\n1.5b\n");
}

#[test]
fn boolean_or_null_plus_string_is_rejected() {
    // Scala 3 removed the universal `any2stringadd`, so these do not compile.
    let (_o1, ok1) = run(&wrap(r#"println(false + "a")"#));
    assert!(!ok1, "`Boolean + String` must be rejected (no Scala 3 `+`)");
    let (_o2, ok2) = run(&wrap(r#"println(null + "a")"#));
    assert!(!ok2, "`null + String` must be rejected (no Scala 3 `+`)");
}
