//! Integration tests: run `.scala` programs through the built `scala` binary
//! and assert their stdout. Every expected output here was diffed byte-for-byte
//! against a reference Scala (`scala <file>`) during authoring, then frozen so
//! the suite is self-contained — CI needs no Scala toolchain installed.

use std::process::Command;

/// Run a Scala source string through the `scala` binary and return (stdout, ok).
fn run(src: &str) -> (String, bool) {
    let (out, _err, ok) = run_full(src);
    (out, ok)
}

/// As [`run`], but keeping stderr — for the cases where the DIAGNOSTIC is the
/// behaviour under test and a bare "it exited non-zero" would pass on any
/// failure at all.
fn run_full(src: &str) -> (String, String, bool) {
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
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Assert that `src` is refused AND that the diagnostic names `because`.
///
/// A bare `assert!(!ok)` is satisfied by ANY rejection — a parse error in the
/// test's own source, a construct that stopped working for an unrelated reason,
/// or the feature under test being deleted outright. It cannot distinguish the
/// correct behaviour from several incorrect ones, so it cannot fail for the
/// reason it was written. Naming the expected cause makes it able to.
fn rejects(src: &str, because: &str) {
    let (out, err, ok) = run_full(src);
    assert!(
        !ok,
        "must be refused, but it ran and printed {out:?}\n{src}"
    );
    assert!(
        err.contains(because),
        "refused for the wrong reason: wanted a diagnostic containing {because:?}, got {err:?}\n{src}"
    );
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
    rejects("object NoEntry { val x = 3 }", "no entry point");
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
    rejects(
        &wrap(r#"println(false + "a")"#),
        "`+` is not defined between `false` and `a`",
    );
    rejects(
        &wrap(r#"println(null + "a")"#),
        "`+` is not defined between `null` and `a`",
    );
}

// ── val immutability + arithmetic exceptions ──────────────────────────────

#[test]
fn reassigning_a_val_is_a_compile_error() {
    // Scala rejects `x = 2` when `x` is a `val`; scalars does too.
    rejects(
        &wrap("val x = 1; x = 2; println(x)"),
        "reassignment to val `x`",
    );
}

#[test]
fn compound_assign_to_a_val_is_a_compile_error() {
    // `+=` is not a member of an immutable binding's type — which is how Scala
    // reports it too, rather than as a reassignment.
    rejects(
        &wrap("val x = 1; x += 1; println(x)"),
        "value += is not a member of Int",
    );
}

#[test]
fn reassigning_a_var_is_allowed() {
    // The mirror of the above: a `var` still reassigns.
    let (out, ok) = run(&wrap("var x = 1; x = 2; println(x)"));
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn integer_division_by_zero_throws() {
    // Scala/JVM `idiv` traps: `java.lang.ArithmeticException: / by zero`.
    rejects(
        &wrap("println(1 / 0)"),
        "java.lang.ArithmeticException: / by zero",
    );
}

#[test]
fn integer_modulo_via_div_builtin_path_still_computes() {
    // Sanity: non-zero integer division is unaffected by the throw path.
    let (out, ok) = run(&wrap("println(7 / 2)"));
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn float_division_by_zero_is_infinity_not_an_error() {
    // IEEE-754 `/ 0.0` is not an exception in Scala — it is `Infinity`.
    let (out, ok) = run(&wrap("println(1.0 / 0.0); println(-1.0 / 0.0)"));
    assert!(ok);
    assert_eq!(out, "Infinity\n-Infinity\n");
}

// ── user-defined methods (`def`) ──────────────────────────────────────────

#[test]
fn recursive_def_computes_factorial() {
    // The canonical tail-`if` recursion. Exercises `Op::Call` frames, param
    // slots, and recursion.
    let (out, ok) = run(
        "object M { def fact(n: Int): Int = if (n <= 1) 1 else n * fact(n - 1)\n  def main(a: Array[String]): Unit = println(fact(5)) }",
    );
    assert!(ok);
    assert_eq!(out, "120\n");
}

#[test]
fn def_with_multiple_params() {
    let (out, ok) = run(
        "object M { def add(a: Int, b: Int): Int = a + b\n  def main(z: Array[String]): Unit = println(add(20, 22)) }",
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn def_early_return() {
    // `return` leaves the enclosing `def`; the fall-through path is the tail.
    let (out, ok) = run(
        "object M { def f(n: Int): Int = { if (n < 0) return 0; n * 2 }\n  def main(a: Array[String]): Unit = { println(f(-5)); println(f(5)) } }",
    );
    assert!(ok);
    assert_eq!(out, "0\n10\n");
}

#[test]
fn def_in_app_body() {
    // A `def` alongside statements in an `extends App` body is hoisted and
    // callable.
    let (out, ok) = run("object T extends App { def sq(x: Int): Int = x * x; println(sq(7)) }");
    assert!(ok);
    assert_eq!(out, "49\n");
}

#[test]
fn mutual_recursion_between_defs() {
    let (out, ok) = run(
        "object M {\n  def isEven(n: Int): Boolean = if (n == 0) true else isOdd(n - 1)\n  def isOdd(n: Int): Boolean = if (n == 0) false else isEven(n - 1)\n  def main(a: Array[String]): Unit = { println(isEven(10)); println(isOdd(7)) } }",
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn recursion_keeps_frame_local_state() {
    // Non-linear recursion: fib re-enters the same `def` twice per call, so a
    // shared (global) `n` would clobber. Correct output proves params are
    // frame-local slots.
    let (out, ok) = run(
        "object M { def fib(n: Int): Int = if (n < 2) n else fib(n - 1) + fib(n - 2)\n  def main(a: Array[String]): Unit = println(fib(10)) }",
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

#[test]
fn def_with_function_local_loop() {
    // A `while` loop with its own `var`s inside a `def`, called twice — the
    // loop locals must be frame-scoped, not leak between calls.
    let (out, ok) = run(
        "object M { def sumTo(n: Int): Int = { var s = 0; var i = 1; while (i <= n) { s += i; i += 1 }; s }\n  def main(a: Array[String]): Unit = { println(sumTo(5)); println(sumTo(10)) } }",
    );
    assert!(ok);
    assert_eq!(out, "15\n55\n");
}

#[test]
fn parameterless_def_is_a_parenless_call() {
    // Scala lets `def x = …` be referenced bare; a bare `answer` calls it.
    let (out, ok) = run("object T extends App { def answer: Int = 42; println(answer) }");
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn parameter_reassignment_is_a_compile_error() {
    // Scala method parameters are `val`s.
    rejects(
        "object M { def f(x: Int): Int = { x = 5; x }\n  \
         def main(a: Array[String]): Unit = println(f(1)) }",
        "reassignment to val `x`",
    );
}

// ── postfix `.` method dispatch (core stdlib) ─────────────────────────────

#[test]
fn string_length_and_case_methods() {
    let (out, ok) = run(&wrap(
        r#"val s = "hello"; println(s.length); println(s.toUpperCase); println(s.toLowerCase)"#,
    ));
    assert!(ok);
    assert_eq!(out, "5\nHELLO\nhello\n");
}

#[test]
fn tostring_on_any_value() {
    let (out, ok) = run(&wrap(
        "println(42.toString); println(true.toString); println(3.5.toString)",
    ));
    assert!(ok);
    assert_eq!(out, "42\ntrue\n3.5\n");
}

#[test]
fn string_substring_and_method_chaining() {
    let (out, ok) = run(&wrap(
        r#"println("hello world".substring(0, 6) + "scala"); println("  hi  ".trim.length)"#,
    ));
    assert!(ok);
    assert_eq!(out, "hello scala\n2\n");
}

#[test]
fn string_predicate_and_reverse_methods() {
    let (out, ok) = run(&wrap(
        r#"println("scala".contains("cal")); println("scala".startsWith("sc")); println("abc".reverse)"#,
    ));
    assert!(ok);
    assert_eq!(out, "true\ntrue\ncba\n");
}

#[test]
fn int_min_max_abs_methods() {
    let (out, ok) = run(&wrap(
        "println(3.max(7)); println(3.min(7)); println((-4).abs)",
    ));
    assert!(ok);
    assert_eq!(out, "7\n3\n4\n");
}

#[test]
fn string_to_int_conversion_method() {
    let (out, ok) = run(&wrap(r#"println("41".toInt + 1)"#));
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn unknown_method_is_an_error() {
    rejects(
        &wrap(r#"println("x".frobnicate)"#),
        "value frobnicate is not a member of String",
    );
}

#[test]
fn substring_out_of_range_throws() {
    // Faithful to Java `String.substring`'s bounds check.
    rejects(
        &wrap(r#"println("hi".substring(0, 9))"#),
        "java.lang.StringIndexOutOfBoundsException: Range [0, 9) out of bounds for length 2",
    );
}

// ── string interpolation (`s` / `f` / `raw`) ──────────────────────────────

#[test]
fn s_interpolator_id_and_block_splices() {
    // `$id` and `${expr}` splices; every expected output diffed against `scala`.
    let (out, ok) = run(&wrap(
        r#"val name = "Ada"; val n = 7; println(s"hi $name, n=${n * 2}")"#,
    ));
    assert!(ok);
    assert_eq!(out, "hi Ada, n=14\n");
}

#[test]
fn s_interpolator_method_call_in_splice() {
    // A `${…}` splice is a full expression: method calls, chaining.
    let (out, ok) = run(&wrap(
        r#"val name = "Ada"; println(s"len=${"abcd".length} up=${name.toUpperCase}")"#,
    ));
    assert!(ok);
    assert_eq!(out, "len=4 up=ADA\n");
}

#[test]
fn s_interpolator_adjacent_splices() {
    // `$n$name` — back-to-back splices with an empty literal between them.
    let (out, ok) = run(&wrap(r#"val name = "Ada"; val n = 7; println(s"$n$name")"#));
    assert!(ok);
    assert_eq!(out, "7Ada\n");
}

#[test]
fn f_interpolator_printf_formatting() {
    // `%.2f`, `%05d`, and left-justified `%-5s` — Java `Formatter` semantics.
    let (out, ok) = run(&wrap(
        r#"val n = 7; val name = "Ada"; println(f"${3.14159}%.2f|${n}%05d|${name}%-5s|end")"#,
    ));
    assert!(ok);
    assert_eq!(out, "3.14|00007|Ada  |end\n");
}

#[test]
fn f_interpolator_signed_and_hex_conversions() {
    let (out, ok) = run(&wrap(r#"println(f"${-3}%+d ${255}%x ${42}%d")"#));
    assert!(ok);
    assert_eq!(out, "-3 ff 42\n");
}

#[test]
fn f_interpolator_default_conversion_is_string() {
    // A bare `$id` in an `f"…"` defaults to `%s`.
    let (out, ok) = run(&wrap(r#"val name = "Ada"; println(f"hi $name!")"#));
    assert!(ok);
    assert_eq!(out, "hi Ada!\n");
}

#[test]
fn raw_interpolator_keeps_escapes_literal() {
    // `raw"…"` does not process `\t` / `\n` — they stay two characters each.
    let (out, ok) = run(&wrap(r#"println(raw"tab\tnl\n done")"#));
    assert!(ok);
    assert_eq!(out, "tab\\tnl\\n done\n");
}

#[test]
fn interpolator_prefix_needs_adjacent_quote() {
    // `s` with a space before the string is a plain identifier, not the `s`
    // interpolator — so a variable named `s` still works.
    let (out, ok) = run(&wrap(r#"val s = "plain"; println(s)"#));
    assert!(ok);
    assert_eq!(out, "plain\n");
}

// ── `if` / `else` as a value-producing expression ─────────────────────────

#[test]
fn if_expression_in_val_binding() {
    let (out, ok) = run(&wrap(
        r#"val x = 5; val r = if (x > 0) "pos" else "neg"; println(r)"#,
    ));
    assert!(ok);
    assert_eq!(out, "pos\n");
}

#[test]
fn if_expression_in_argument_position() {
    let (out, ok) = run(&wrap(
        r#"val x = 5; println(if (x % 2 == 0) "even" else "odd")"#,
    ));
    assert!(ok);
    assert_eq!(out, "odd\n");
}

#[test]
fn if_else_if_chain_as_expression() {
    let (out, ok) = run(&wrap(
        "val x = 5; val b = if (x > 10) 1 else if (x > 3) 2 else 3; println(b)",
    ));
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn if_expression_with_block_branches() {
    // A brace branch is a block expression: its last statement is the value.
    let (out, ok) = run(&wrap(
        "val x = 5; val v = if (x > 0) { val t = x * x; t + 1 } else 0; println(v)",
    ));
    assert!(ok);
    assert_eq!(out, "26\n");
}

// ── `match` (non-constructor patterns) ────────────────────────────────────

#[test]
fn match_literal_and_wildcard() {
    let (out, ok) = run(&wrap(
        r#"val x = 5; println(x match { case 0 => "zero"; case 5 => "five"; case _ => "other" })"#,
    ));
    assert!(ok);
    assert_eq!(out, "five\n");
}

#[test]
fn match_string_literals() {
    let (out, ok) = run(&wrap(
        r#"println("go" match { case "stop" => 0; case "go" => 1; case _ => -1 })"#,
    ));
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn match_guard_and_variable_binding() {
    let (out, ok) = run(&wrap(
        r#"val x = 5; println(x match { case k if k > 3 => "big:" + k; case _ => "small" })"#,
    ));
    assert!(ok);
    assert_eq!(out, "big:5\n");
}

#[test]
fn match_typed_patterns() {
    // `case s: String` / `case i: Int` / … — a runtime type test that binds.
    let src = "object M {\n  def kind(a: Any): String = a match {\n    case s: String => \"string:\" + s\n    case i: Int => \"int:\" + i\n    case d: Double => \"double:\" + d\n    case b: Boolean => \"bool:\" + b\n    case _ => \"unknown\"\n  }\n  def main(z: Array[String]): Unit = { println(kind(\"hey\")); println(kind(3)); println(kind(2.5)); println(kind(true)) } }";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "string:hey\nint:3\ndouble:2.5\nbool:true\n");
}

#[test]
fn match_without_matching_arm_throws() {
    // A non-exhaustive match that falls through raises `scala.MatchError`.
    rejects(
        &wrap(r#"val x = 5; println(x match { case 1 => "one"; case 2 => "two" })"#),
        "scala.MatchError: 5 (of class java.lang.Integer)",
    );
}

#[test]
fn match_option_constructor_pattern_binds() {
    // Constructor patterns on the built-in `Option` — `Some(v)` binds the value,
    // `None` is a stable-identifier pattern. Verified against `scala`.
    let src = "object T extends App {\n  val o: Option[Int] = Some(7)\n  val r = o match { case Some(v) => v * 2; case None => -1 }\n  println(r)\n  val e: Option[Int] = None\n  println(e match { case Some(v) => v; case None => 0 })\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "14\n0\n");
}

#[test]
fn match_case_class_constructor_pattern_binds_fields() {
    // `case Point(x, y)` binds each field in declared order.
    let src = "case class Point(x: Int, y: Int)\nobject T extends App {\n  val p = Point(3, 4)\n  p match { case Point(a, b) => println(a + b) }\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "7\n");
}

// ── `for … yield` comprehensions ──────────────────────────────────────────

#[test]
fn for_yield_range_makes_a_vector() {
    let (out, ok) = run(&wrap("println(for (i <- 1 to 4) yield i * i)"));
    assert!(ok);
    assert_eq!(out, "Vector(1, 4, 9, 16)\n");
}

#[test]
fn for_yield_multiple_generators_nest() {
    let (out, ok) = run(&wrap(
        "println(for (i <- 1 to 3; j <- 1 to 2) yield i * 10 + j)",
    ));
    assert!(ok);
    assert_eq!(out, "Vector(11, 12, 21, 22, 31, 32)\n");
}

#[test]
fn for_yield_guard_filters() {
    let (out, ok) = run(&wrap("println(for (i <- 1 to 10 if i % 3 == 0) yield i)"));
    assert!(ok);
    assert_eq!(out, "Vector(3, 6, 9)\n");
}

#[test]
fn for_yield_empty_range_is_empty_vector() {
    let (out, ok) = run(&wrap("println(for (i <- 0 until 0) yield i)"));
    assert!(ok);
    assert_eq!(out, "Vector()\n");
}

#[test]
fn for_yield_nested_produces_nested_vectors() {
    let (out, ok) = run(&wrap(
        "println(for (i <- 1 to 2) yield for (j <- 1 to 2) yield i * j)",
    ));
    assert!(ok);
    assert_eq!(out, "Vector(Vector(1, 2), Vector(2, 4))\n");
}

#[test]
fn for_yield_inside_a_def_is_frame_local() {
    // The comprehension's accumulator is a frame slot inside a `def`, so a
    // second call does not see the first call's collected values.
    let src = "object M { def sq(n: Int) = for (i <- 1 to n) yield i * i\n  def main(a: Array[String]): Unit = { println(sq(3)); println(sq(4)) } }";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "Vector(1, 4, 9)\nVector(1, 4, 9, 16)\n");
}

#[test]
fn for_side_effecting_with_assignment_body() {
    // A `for` without `yield` runs its body for effect; an unbraced assignment
    // body still parses as a statement.
    let (out, ok) = run(&wrap(
        "var acc = 0; for (i <- 1 to 4) acc += i; println(acc)",
    ));
    assert!(ok);
    assert_eq!(out, "10\n");
}

// ── classes, objects, case classes (host-side object model) ───────────────
// Every expected output below was diffed byte-for-byte against `scala` 1.15.0
// (Scala 3.8.4) during authoring, then frozen.

#[test]
fn class_new_field_access_and_method() {
    let src = "class Rect(val w: Int, val h: Int) {\n  def area = w * h\n  def scaled(k: Int) = new Rect(w * k, h * k)\n}\nobject T extends App {\n  val r = new Rect(3, 4)\n  println(r.w)\n  println(r.area)\n  println(r.scaled(2).area)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "3\n12\n48\n");
}

#[test]
fn class_body_var_field_mutates_in_place() {
    // A `var` field declared in the class body is initialized by the constructor
    // and mutated in place by a method (`n += 1`), persisting across calls.
    let src = "class Counter { var n = 0; def bump = { n += 1; n } }\nobject T extends App {\n  val c = new Counter\n  var i = 0\n  while (i < 3) { c.bump; i += 1 }\n  println(c.n)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn class_method_calls_sibling_method() {
    // An unqualified call to a sibling method resolves to `this.m(...)`.
    let src = "class Adder(val base: Int) {\n  def add(n: Int) = base + n\n  def twice(n: Int) = add(add(n))\n}\nobject T extends App {\n  println(new Adder(10).twice(1))\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "21\n");
}

#[test]
fn object_singleton_val_and_def_dispatch() {
    let src = "object Registry {\n  val name = \"reg\"\n  def greet(who: String) = \"hi \" + who\n  def total(a: Int, b: Int) = a + b\n}\nobject T extends App {\n  println(Registry.name)\n  println(Registry.greet(\"bob\"))\n  println(Registry.total(3, 4))\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "reg\nhi bob\n7\n");
}

#[test]
fn object_var_member_accumulates_across_calls() {
    let src = "object State { var total = 0; def addTo(k: Int) = { total += k; total } }\nobject T extends App {\n  println(State.addTo(5))\n  println(State.addTo(3))\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "5\n8\n");
}

#[test]
fn case_class_tostring_is_ordered_fields_no_space() {
    // Scala's synthesized `case` `toString` joins fields with a bare comma.
    let src = "case class Money(cents: Int, currency: String)\nobject T extends App {\n  println(Money(500, \"USD\"))\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "Money(500,USD)\n");
}

#[test]
fn case_class_equals_is_structural() {
    let src = "case class Point(x: Int, y: Int)\nobject T extends App {\n  println(Point(1, 2) == Point(1, 2))\n  println(Point(1, 2) == Point(3, 4))\n  println(Point(1, 2).equals(Point(1, 2)))\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "true\nfalse\ntrue\n");
}

#[test]
fn case_class_hashcode_agrees_with_equals() {
    // Equal instances hash equal; unequal instances (almost always) differ.
    let src = "case class Point(x: Int, y: Int)\nobject T extends App {\n  println(Point(1, 2).hashCode == Point(1, 2).hashCode)\n  println(Point(1, 2).hashCode == Point(2, 1).hashCode)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn plain_class_equality_is_reference_identity() {
    // A non-`case` class uses reference identity: two distinct instances with the
    // same field are unequal; an instance equals itself.
    let src = "class Foo(val a: Int)\nobject T extends App {\n  val f = new Foo(1)\n  println(f == new Foo(1))\n  println(f == f)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn case_class_copy_named_and_positional() {
    let src = "case class Money(cents: Int, currency: String)\nobject T extends App {\n  val m = Money(500, \"USD\")\n  println(m.copy(cents = 750))\n  println(m.copy(currency = \"EUR\"))\n  println(m.copy(1, \"GBP\"))\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "Money(750,USD)\nMoney(500,EUR)\nMoney(1,GBP)\n");
}

#[test]
fn case_class_apply_constructs_without_new() {
    let src = "case class Point(x: Int, y: Int)\nobject T extends App {\n  val p = Point(1, 2)\n  println(p.x + p.y)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn option_some_and_none_tostring() {
    // `Some(v)` renders with parens; the `None` case object renders bare.
    let src = "object T extends App {\n  println(Some(42))\n  println(None)\n  val o: Option[Int] = Some(1)\n  println(o.toString)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "Some(42)\nNone\nSome(1)\n");
}

#[test]
fn nested_constructor_pattern_binds_deeply() {
    let src = "case class Point(x: Int, y: Int)\ncase class Line(a: Point, b: Point)\nobject T extends App {\n  val ln = Line(Point(1, 2), Point(3, 4))\n  ln match {\n    case Line(Point(a, _), Point(_, d)) => println(a + d)\n  }\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn constructor_pattern_with_guard() {
    let src = "case class Box(n: Int)\nobject T extends App {\n  val b = Box(10)\n  val r = b match {\n    case Box(v) if v > 5 => \"big\"\n    case Box(_) => \"small\"\n  }\n  println(r)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "big\n");
}

#[test]
fn some_pattern_binds_none_pattern_matches() {
    let src = "object T extends App {\n  val o: Option[Int] = Some(7)\n  println(o match { case Some(v) => v * 2; case None => -1 })\n  val e: Option[Int] = None\n  println(e match { case Some(v) => v; case None => 0 })\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "14\n0\n");
}

// ── Lambdas / first-class functions (byte-verified vs scala 3.8.4) ───────────

#[test]
fn lambda_single_and_multi_param() {
    let (out, ok) = run(&wrap(
        "val inc = (x: Int) => x + 1\nval mul = (a: Int, b: Int) => a * b\nprintln(inc(41))\nprintln(mul(6, 7))",
    ));
    assert!(ok);
    assert_eq!(out, "42\n42\n");
}

#[test]
fn lambda_function_type_annotation() {
    // `val sq: Int => Int = x => x * x` — a function-type-annotated binding whose
    // literal has an unparenthesized single parameter.
    let (out, ok) = run(&wrap("val sq: Int => Int = x => x * x\nprintln(sq(9))"));
    assert!(ok);
    assert_eq!(out, "81\n");
}

#[test]
fn lambda_block_body() {
    let (out, ok) = run(&wrap(
        "val blk = (x: Int) => { val y = x * 2; y + 1 }\nprintln(blk(20))",
    ));
    assert!(ok);
    assert_eq!(out, "41\n");
}

#[test]
fn lambda_passed_as_argument() {
    let (out, ok) = run(&wrap(
        "def applyTo(f: Int => Int, v: Int): Int = f(v)\nval inc = (x: Int) => x + 1\nprintln(applyTo(inc, 100))",
    ));
    assert!(ok);
    assert_eq!(out, "101\n");
}

#[test]
fn lambda_returned_and_captures_enclosing_param() {
    // A returned closure over the enclosing `def`'s parameter (`n`) — the upvalue
    // must survive after `adder` has returned.
    let (out, ok) = run(&wrap(
        "def adder(n: Int): Int => Int = (x: Int) => x + n\nval add10 = adder(10)\nprintln(add10(5))\nprintln(adder(100)(1))",
    ));
    assert!(ok);
    assert_eq!(out, "15\n101\n");
}

#[test]
fn lambda_composition_captures_two_params() {
    let (out, ok) = run(&wrap(
        "def compose(f: Int => Int, g: Int => Int): Int => Int = (x: Int) => f(g(x))\nval inc = (x: Int) => x + 1\nval dbl = (x: Int) => x * 2\nprintln(compose(inc, dbl)(10))",
    ));
    assert!(ok);
    assert_eq!(out, "21\n");
}

#[test]
fn underscore_placeholder_forms() {
    // `_ * 2` (one placeholder), `_ + _` (two placeholders → two-param lambda).
    let (out, ok) = run(&wrap(
        "println(List(1,2,3,4).map(_ * 2))\nprintln(List(1,2,3,4).filter(_ % 2 == 0))\nprintln(List(1,2,3,4).foldLeft(0)(_ + _))",
    ));
    assert!(ok);
    assert_eq!(out, "List(2, 4, 6, 8)\nList(2, 4)\n10\n");
}

// ── List (byte-verified vs scala 3.8.4) ──────────────────────────────────────

#[test]
fn list_literal_nil_and_basic_accessors() {
    let (out, ok) = run(&wrap(
        "val xs = List(10, 20, 30)\nprintln(xs)\nprintln(Nil)\nprintln(List())\nprintln(xs.head)\nprintln(xs.tail)\nprintln(xs.length)\nprintln(xs.isEmpty)\nprintln(xs.reverse)\nprintln(xs.contains(20))\nprintln(xs(2))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "List(10, 20, 30)\nList()\nList()\n10\nList(20, 30)\n3\nfalse\nList(30, 20, 10)\ntrue\n30\n"
    );
}

#[test]
fn list_cons_is_right_associative() {
    let (out, ok) = run(&wrap(
        "val xs = List(10, 20)\nprintln(0 :: xs)\nprintln(1 :: 2 :: 3 :: Nil)",
    ));
    assert!(ok);
    assert_eq!(out, "List(0, 10, 20)\nList(1, 2, 3)\n");
}

#[test]
fn list_map_filter_foldleft_chain() {
    let (out, ok) = run(&wrap(
        "println(List(1,2,3).map(x => x + 10))\nprintln(List(1,2,3,4,5).filter(_ > 2).map(_ * 10).sum)\nprintln(List(\"a\",\"b\",\"c\").foldLeft(\"\")((acc, s) => acc + s))",
    ));
    assert!(ok);
    assert_eq!(out, "List(11, 12, 13)\n120\nabc\n");
}

#[test]
fn list_foreach_side_effects_in_order() {
    // `foreach` runs for effect (its `Unit` result is discarded, not printed).
    let (out, ok) = run(&wrap("List(1,2,3).foreach(x => print(x + \" \"))"));
    assert!(ok);
    assert_eq!(out, "1 2 3 ");
}

#[test]
fn list_flatmap_flattens() {
    let (out, ok) = run(&wrap(
        "println(List(1,2,3).flatMap(x => List(x, x * 10)))\nprintln(List(List(1,2), List(3,4)).flatMap(x => x))",
    ));
    assert!(ok);
    assert_eq!(out, "List(1, 10, 2, 20, 3, 30)\nList(1, 2, 3, 4)\n");
}

// ── Map (byte-verified vs scala 3.8.4) ───────────────────────────────────────

#[test]
fn map_literal_and_lookups() {
    let (out, ok) = run(&wrap(
        "val m = Map(\"a\" -> 1, \"b\" -> 2, \"c\" -> 3)\nprintln(m)\nprintln(m(\"b\"))\nprintln(m.get(\"a\"))\nprintln(m.get(\"z\"))\nprintln(m.contains(\"c\"))\nprintln(m.size)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "Map(a -> 1, b -> 2, c -> 3)\n2\nSome(1)\nNone\ntrue\n3\n"
    );
}

#[test]
fn map_keys_values_and_plus() {
    let (out, ok) = run(&wrap(
        "val m = Map(\"a\" -> 1, \"b\" -> 2)\nprintln(m.keys)\nprintln(m.values)\nprintln(m + (\"c\" -> 3))\nprintln(Map[String,Int]())",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "Set(a, b)\nIterable(1, 2)\nMap(a -> 1, b -> 2, c -> 3)\nMap()\n"
    );
}

#[test]
fn arrow_builds_a_tuple() {
    let (out, ok) = run(&wrap("println(\"k\" -> 99)\nprintln((1, 2, 3))"));
    assert!(ok);
    assert_eq!(out, "(k,99)\n(1,2,3)\n");
}

// ── for-comprehensions over collections (byte-verified vs scala 3.8.4) ────────

#[test]
fn for_yield_over_list_maps() {
    let (out, ok) = run(&wrap(
        "println(for (x <- List(1,2,3)) yield x * 2)\nprintln(for (x <- List(1,2,3,4) if x % 2 == 0) yield x)",
    ));
    assert!(ok);
    assert_eq!(out, "List(2, 4, 6)\nList(2, 4)\n");
}

#[test]
fn for_yield_multi_generator_flatmaps() {
    let (out, ok) = run(&wrap(
        "println(for (x <- List(1,2); y <- List(10,20)) yield x + y)",
    ));
    assert!(ok);
    assert_eq!(out, "List(11, 21, 12, 22)\n");
}

#[test]
fn for_foreach_over_collection_runs_for_effect() {
    let (out, ok) = run(&wrap(
        "var s = 0\nfor (x <- List(5,10,15)) s += x\nprintln(s)",
    ));
    assert!(ok);
    assert_eq!(out, "30\n");
}

#[test]
fn def_reference_eta_expands_to_a_function_value() {
    // A bare `def` used as an argument is eta-expanded (`sq` ⇒ `x => sq(x)`).
    let (out, ok) = run(&wrap(
        "def sq(x: Int): Int = x * x\nprintln(List(1,2,3,4).map(sq))",
    ));
    assert!(ok);
    assert_eq!(out, "List(1, 4, 9, 16)\n");
}

#[test]
fn lambda_body_may_be_an_assignment_statement() {
    let (out, ok) = run(&wrap(
        "var total = 0\nList(1,2,3,4,5).foreach(x => total += x)\nprintln(total)",
    ));
    assert!(ok);
    assert_eq!(out, "15\n");
}

#[test]
fn range_yield_result_supports_collection_methods() {
    // A range `for … yield` (a `Vector`) can be chained through `.toList`/`.map`.
    let (out, ok) = run(&wrap(
        "println((for (i <- 1 to 5) yield i).toList.map(_ * 10))",
    ));
    assert!(ok);
    assert_eq!(out, "List(10, 20, 30, 40, 50)\n");
}

#[test]
fn for_yield_range_led_generator_is_a_vector() {
    // A range-led comprehension mixing a collection generator yields a `Vector`
    // (Scala's `Range.flatMap` result type), not a `List`.
    let (out, ok) = run(&wrap(
        "println(for (i <- 1 to 3; c <- List(\"a\",\"b\")) yield i + c)",
    ));
    assert!(ok);
    assert_eq!(out, "Vector(1a, 1b, 2a, 2b, 3a, 3b)\n");
}

// ── `by`-step ranges ────────────────────────────────────────────────────────

#[test]
fn range_by_step_ascends_and_descends() {
    // A negative step flips the bound test; getting that wrong yields either an
    // empty range or a non-terminating loop.
    let (out, ok) = run(&wrap(
        "for (i <- 0 until 10 by 3) print(i + \" \")\nprintln()\nfor (i <- 10 to 1 by -3) print(i + \" \")\nprintln()",
    ));
    assert!(ok);
    assert_eq!(out, "0 3 6 9 \n10 7 4 1 \n");
}

#[test]
fn range_by_step_overshooting_bound_yields_one_or_zero_elements() {
    // `0 until 5 by 7` stops after the first element; `5 until 0 by 1` is empty
    // because the step is positive but the range descends.
    let (out, ok) = run(&wrap(
        "for (i <- 0 until 5 by 7) print(i)\nprintln(\"|\")\nfor (i <- 5 until 0 by 1) print(i)\nprintln(\"|\")",
    ));
    assert!(ok);
    assert_eq!(out, "0|\n|\n");
}

#[test]
fn range_by_step_from_a_runtime_value_picks_direction_at_runtime() {
    // A non-literal step compiles to the sign-branching bound test, a different
    // lowering from the literal (compile-time direction) case.
    let (out, ok) = run(&wrap(
        "val k = -2\nfor (i <- 8 to 0 by k) print(i + \",\")\nprintln()",
    ));
    assert!(ok);
    assert_eq!(out, "8,6,4,2,0,\n");
}

#[test]
fn range_by_step_works_in_yield_and_nested_generators() {
    let (out, ok) = run(&wrap(
        "println((for (i <- 1 to 9 by 4) yield i * 2).mkString(\",\"))\nfor (i <- 0 until 6 by 2; j <- 0 until 4 by 3) print(s\"$i$j \")\nprintln()",
    ));
    assert!(ok);
    assert_eq!(out, "2,10,18\n00 03 20 23 40 43 \n");
}

#[test]
fn range_by_zero_step_throws_illegal_argument() {
    let (out, ok) = run(&wrap(
        "try { for (i <- 1 to 5 by 0) println(i) } catch { case e: IllegalArgumentException => println(e.getMessage) }",
    ));
    assert!(ok);
    assert_eq!(out, "step cannot be 0.\n");
}

// ── `try` / `catch` / `finally` / `throw` ───────────────────────────────────

#[test]
fn catch_intercepts_a_runtime_exception_and_finally_runs() {
    let (out, ok) = run(&wrap(
        "try { println(1 / 0) } catch { case e: ArithmeticException => println(\"A:\" + e.getMessage) } finally { println(\"fin\") }",
    ));
    assert!(ok);
    assert_eq!(out, "A:/ by zero\nfin\n");
}

#[test]
fn catch_matches_a_supertype_of_the_thrown_exception() {
    // `IllegalArgumentException` reaches `Exception` through `RuntimeException`;
    // this is the throwable-hierarchy walk, not an exact-name test.
    let (out, ok) = run(&wrap(
        "try { throw new IllegalArgumentException(\"bad\") } catch { case e: Exception => println(e.toString) }",
    ));
    assert!(ok);
    assert_eq!(out, "java.lang.IllegalArgumentException: bad\n");
}

#[test]
fn unmatched_catch_rethrows_to_the_enclosing_try_after_finally() {
    // The inner handler's type does not match, so the exception keeps unwinding
    // — but the inner `finally` still runs first, before the outer `catch`.
    let (out, ok) = run(&wrap(
        "try { try { throw new NumberFormatException(\"nf\") } catch { case e: IllegalStateException => println(\"no\") } finally { println(\"innerFin\") } } catch { case e: Throwable => println(\"outer:\" + e.getMessage) }",
    ));
    assert!(ok);
    assert_eq!(out, "innerFin\nouter:nf\n");
}

#[test]
fn try_is_an_expression_whose_value_comes_from_either_path() {
    let (out, ok) = run(&wrap(
        "println(try { 10 / 2 } catch { case _: Throwable => -1 })\nprintln(try { 10 / 0 } catch { case _: Throwable => -1 })",
    ));
    assert!(ok);
    assert_eq!(out, "5\n-1\n");
}

#[test]
fn a_catch_guard_reads_the_bound_exception_and_falls_through_when_false() {
    // The guard must see a real binding: while an exception is in flight the
    // host suppresses method dispatch, so a naive lowering reads `null` here and
    // silently takes the wrong arm.
    let (out, ok) = run(&wrap(
        "try { throw new RuntimeException(\"abc\") } catch { case e: RuntimeException if e.getMessage.length > 5 => println(\"long\"); case e: RuntimeException => println(\"short:\" + e.getMessage) }",
    ));
    assert!(ok);
    assert_eq!(out, "short:abc\n");
}

#[test]
fn an_exception_unwinds_through_two_def_frames() {
    let (out, ok) = run(&wrap(
        "def lo(x: Int): Int = 100 / x\ndef hi(x: Int): Int = lo(x) + 1\ntry { println(hi(0)) } catch { case e: ArithmeticException => println(\"deep:\" + e.getMessage) }\nprintln(hi(5))",
    ));
    assert!(ok);
    assert_eq!(out, "deep:/ by zero\n21\n");
}

#[test]
fn an_exception_abandons_the_remaining_loop_iterations() {
    let (out, ok) = run(&wrap(
        "try { for (i <- 0 to 5) { println(i); if (i == 2) throw new RuntimeException(\"stop\") } } catch { case e: Exception => println(\"L:\" + e.getMessage) }",
    ));
    assert!(ok);
    assert_eq!(out, "0\n1\n2\nL:stop\n");
}

#[test]
fn an_exception_inside_a_lambda_reaches_the_enclosing_catch() {
    // `map` re-enters the VM to run the lambda, so the raise has to survive a
    // host-driven nested invocation.
    let (out, ok) = run(&wrap(
        "val xs = List(1, 2, 0, 4)\ntry { println(xs.map(v => 12 / v)) } catch { case e: ArithmeticException => println(\"map:\" + e.getMessage) }\nprintln(xs.map(v => v * 2))",
    ));
    assert!(ok);
    assert_eq!(out, "map:/ by zero\nList(2, 4, 0, 8)\n");
}

#[test]
fn no_output_escapes_between_a_raise_and_its_handler() {
    // Statements after the raise in the same `try` body must not run at all.
    let (out, ok) = run(&wrap(
        "try { println(\"before\"); throw new RuntimeException(\"x\"); println(\"after\") } catch { case _: Throwable => println(\"caught\") }",
    ));
    assert!(ok);
    assert_eq!(out, "before\ncaught\n");
}

#[test]
fn finally_runs_on_the_normal_path_before_the_value_is_used() {
    let (out, ok) = run(&wrap(
        "def g(): Int = try { 1 } finally { println(\"gfin\") }\nprintln(g())",
    ));
    assert!(ok);
    assert_eq!(out, "gfin\n1\n");
}

#[test]
fn no_arg_throwable_has_a_null_message() {
    // `Throwable.toString` drops the `: message` suffix when the message is null.
    let (out, ok) = run(&wrap(
        "println(new RuntimeException().getMessage)\nprintln(new RuntimeException().toString)",
    ));
    assert!(ok);
    assert_eq!(out, "null\njava.lang.RuntimeException\n");
}

#[test]
fn library_exceptions_carry_their_jdk_messages() {
    let (out, ok) = run(&wrap(
        "try { \"zz\".toInt } catch { case e: Throwable => println(e.toString) }\ntry { \"ab\".charAt(9) } catch { case e: Throwable => println(e.toString) }\ntry { List(1,2,3,4)(99) } catch { case e: Throwable => println(e.toString) }",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "java.lang.NumberFormatException: For input string: \"zz\"\n\
         java.lang.StringIndexOutOfBoundsException: Index 9 out of bounds for length 2\n\
         java.lang.IndexOutOfBoundsException: 99\n"
    );
}

#[test]
fn an_uncaught_throw_stops_the_run_with_a_failing_status() {
    // Output before the raise is kept; nothing after it runs.
    let (out, err, ok) = run_full(&wrap(
        "println(\"a\")\ntry { println(\"t\") } catch { case e: NumberFormatException => println(\"c\") }\nthrow new IllegalStateException(\"uncaught\")\nprintln(\"never\")",
    ));
    assert!(!ok);
    assert_eq!(out, "a\nt\n");
    // The status alone would be satisfied by ANY failure, including the `catch`
    // arm swallowing the wrong exception and something else aborting later.
    assert_eq!(err, "scalars: java.lang.IllegalStateException: uncaught\n");
}

#[test]
fn throw_is_an_expression_usable_in_operand_position() {
    let (out, ok) = run(&wrap(
        "def pick(b: Boolean): Int = if (b) 7 else throw new RuntimeException(\"nope\")\nprintln(pick(true))\ntry { println(pick(false)) } catch { case e: RuntimeException => println(\"pick:\" + e.getMessage) }",
    ));
    assert!(ok);
    assert_eq!(out, "7\npick:nope\n");
}

#[test]
fn a_try_without_catch_or_finally_runs_its_body_and_handles_nothing() {
    // This test used to assert `try { … }` with no handler was REJECTED. That
    // was wrong about Scala: reference `scala` 3.8.4 compiles it, warns "A try
    // without catch or finally is equivalent to putting its body in a block; no
    // exceptions are handled", and prints `1` then `2`. Re-scoped to what the
    // construct actually does — the body runs, the `try` answers the body's
    // value, and nothing is caught, so a raise inside it still aborts.
    let (out, ok) = run(&wrap("try { println(1) }; println(2)"));
    assert!(ok);
    assert_eq!(out, "1\n2\n");

    let (out, ok) = run(&wrap("val v = try { 41 + 1 }; println(v)"));
    assert!(ok);
    assert_eq!(out, "42\n");

    // "no exceptions are handled" is the load-bearing half: the bare `try` must
    // not swallow anything the way a `catch`-bearing one would.
    let (out, err, ok) = run_full(&wrap("try { 1 / 0 }; println(\"unreached\")"));
    assert!(!ok);
    assert_eq!(out, "");
    assert!(
        err.contains("ArithmeticException"),
        "a bare `try` must let the raise through, got stderr {err:?}"
    );
}

#[test]
fn a_raise_does_not_commit_garbage_to_a_var_the_handler_reads() {
    // The assignment's value expression raises part-way through. Without a check
    // *before* the store, `acc` would be `null` when the handler runs and
    // `acc += 100` would fail with a `+`-on-null type error instead of printing.
    let (out, ok) = run(&wrap(
        "var acc = 7\ntry { acc += 10 / 0 } catch { case _: ArithmeticException => acc += 100 }\nprintln(acc)",
    ));
    assert!(ok);
    assert_eq!(out, "107\n");
}

#[test]
fn an_exception_in_a_finally_replaces_the_one_it_was_unwinding() {
    let (out, ok) = run(&wrap(
        "try { try { throw new RuntimeException(\"orig\") } finally { throw new IllegalStateException(\"fromFinally\") } } catch { case e: Throwable => println(e.getMessage) }",
    ));
    assert!(ok);
    assert_eq!(out, "fromFinally\n");
}

#[test]
fn catch_arms_are_tried_in_order_and_a_failing_guard_falls_through() {
    let (out, ok) = run(&wrap(
        "try { throw new RuntimeException(\"q\") } catch { case e: RuntimeException if e.getMessage == \"zzz\" => println(\"g1\"); case e: RuntimeException if e.getMessage == \"q\" => println(\"g2\"); case e: RuntimeException => println(\"g3\") }",
    ));
    assert!(ok);
    assert_eq!(out, "g2\n");
}

// ── block-local `def` scoping (crate::resolve) ──────────────────────────────

#[test]
fn same_named_local_defs_in_sibling_methods_do_not_collide() {
    // The regression this pass exists for: hoisting every `def` into one flat
    // table keyed by name kept only the first `h`, so `b()` silently returned
    // `a()`'s answer (6) instead of 103 — a wrong answer, not an error.
    let (out, ok) = run(&wrap(
        "def a(): Int = { def h(x: Int): Int = x * 2; h(3) }\n\
         def b(): Int = { def h(x: Int): Int = x + 100; h(3) }\n\
         println(a() + \",\" + b())",
    ));
    assert!(ok);
    assert_eq!(out, "6,103\n");
}

#[test]
fn an_inner_block_def_shadows_an_outer_one_and_the_outer_returns_after() {
    let (out, ok) = run(&wrap(
        "def f(x: Int): Int = x + 1\nprintln(f(2))\n\
         val r = { def f(x: Int): Int = x * 10; f(2) }\nprintln(r)\nprintln(f(2))",
    ));
    assert!(ok);
    assert_eq!(out, "3\n20\n3\n");
}

#[test]
fn a_val_shadows_a_local_def_of_the_same_name_after_its_block() {
    // The lifted `def` must not steal the global slot the `val` occupies, or
    // the trailing `g` reads a function value instead of 5.
    let (out, ok) = run(&wrap(
        "val g = 5\nval q = { def g(x: Int): Int = x; g(7) }\nprintln(q + \"/\" + g)",
    ));
    assert!(ok);
    assert_eq!(out, "7/5\n");
}

#[test]
fn a_local_def_captures_the_enclosing_methods_locals() {
    let (out, ok) = run(&wrap(
        "def s(n: Int): Int = { val k = 3\ndef go(i: Int, acc: Int): Int = if (i > n) acc else go(i + 1, acc + i * k)\ngo(1, 0) }\nprintln(s(5))",
    ));
    assert!(ok);
    assert_eq!(out, "45\n");
}

#[test]
fn mutually_recursive_local_defs_share_a_capture() {
    // `ev` and `od` reference each other (so the block's `def`s must be bound
    // before either body is walked) and both capture `t` from the frame above.
    let (out, ok) = run(&wrap(
        "def p(n: Int): String = { val t = \"t\"\n\
         def ev(k: Int): String = if (k == 0) t + \"E\" else od(k - 1)\n\
         def od(k: Int): String = if (k == 0) t + \"O\" else ev(k - 1)\nev(n) }\n\
         println(p(4) + \" \" + p(7))",
    ));
    assert!(ok);
    assert_eq!(out, "tE tO\n");
}

#[test]
fn a_capturing_local_def_used_as_a_function_value_still_gets_its_capture() {
    let (out, ok) = run(&wrap(
        "def m(xs: List[Int]): List[Int] = { val k = 3; def f(x: Int): Int = x * k; xs.map(f) }\nprintln(m(List(1, 2, 3)))",
    ));
    assert!(ok);
    assert_eq!(out, "List(3, 6, 9)\n");
}

#[test]
fn assigning_to_a_captured_binding_is_rejected_not_silently_lost() {
    // A capture travels by value, so a write inside the lifted body could not
    // reach the enclosing frame. Reject it rather than drop it.
    let (out, err, ok) = run_full(&wrap(
        "def f(): Int = { var k = 0; def bump(): Unit = { k += 1 }; bump(); k }\nprintln(f())",
    ));
    assert!(!ok);
    assert_eq!(out, "");
    assert!(
        err.contains("local `def bump` assigns to `k` from the enclosing method"),
        "the refusal must name the write it refused, not merely fail: {err:?}"
    );
}

#[test]
fn a_brace_block_is_an_expression_in_value_position() {
    let (out, ok) = run(&wrap(
        "println(3 match { case x => { x + 10 } })\nval z = { val a = 2; a * 3 }\nprintln(z)",
    ));
    assert!(ok);
    assert_eq!(out, "13\n6\n");
}

// ── traits, inheritance, virtual dispatch ───────────────────────────────────

#[test]
fn a_trait_method_dispatches_to_each_subclasss_override() {
    let (out, ok) = run(
        "trait S { def area: Int; def name: String = \"s\"; def show: String = name + \"=\" + area }\n\
         class C(val r: Int) extends S { def area: Int = r * r; override def name: String = \"c\" }\n\
         class D(val w: Int) extends S { def area: Int = w + w }\n\
         object T extends App { val xs: List[S] = List(new C(3), new D(4)); xs.foreach(x => println(x.show)) }",
    );
    assert!(ok);
    assert_eq!(out, "c=9\ns=8\n");
}

#[test]
fn a_three_level_class_chain_forwards_constructor_args_and_super() {
    let (out, ok) = run(
        "class A(val n: String) { def speak: String = \"...\"; def intro: String = n + \":\" + speak }\n\
         class B(m: String) extends A(m) { override def speak: String = \"woof\" }\n\
         class E(m: String) extends B(m) { override def speak: String = super.speak + \"!\" }\n\
         object T extends App { val xs: List[A] = List(new A(\"g\"), new B(\"r\"), new E(\"p\")); xs.foreach(x => println(x.intro)) }",
    );
    assert!(ok);
    assert_eq!(out, "g:...\nr:woof\np:woof!\n");
}

#[test]
fn a_case_classs_derived_members_see_the_constructor_prefix_only() {
    // `z` is a body `val`, not a primary-constructor parameter, so it appears
    // in neither `toString` nor the extractor's arity.
    let (out, ok) = run(
        "case class P(x: Int, y: Int) { val z = x + y }\n\
         object T extends App { val p = P(1, 2); println(p); println(p.z); println(p == P(1, 2)); p match { case P(i, j) => println(i * j) } }",
    );
    assert!(ok);
    assert_eq!(out, "P(1,2)\n3\ntrue\n2\n");
}

#[test]
fn a_typed_pattern_matches_a_user_class_and_its_supertypes() {
    let (out, ok) = run(
        "trait Shape; class Circle(val r: Int) extends Shape; case class Sq(s: Int) extends Shape\n\
         object T extends App { val xs: List[Any] = List(new Circle(1), Sq(2), 3)\n\
         for (x <- xs) x match { case c: Circle => println(\"C\" + c.r); case s: Sq => println(\"S\" + s.s); case i: Int => println(\"I\" + i) }\n\
         println(new Circle(1).isInstanceOf[Shape]) }",
    );
    assert!(ok);
    assert_eq!(out, "C1\nS2\nI3\ntrue\n");
}

#[test]
fn a_mixin_override_can_call_super_into_the_trait_it_refines() {
    let (out, ok) = run(
        "trait G { val pre: String; def greet(n: String): String = pre + n }\n\
         trait L extends G { override def greet(n: String): String = super.greet(n).toUpperCase }\n\
         class R extends G with L { val pre = \"yo \" }\n\
         object T extends App { println(new R().greet(\"ann\")) }",
    );
    assert!(ok);
    assert_eq!(out, "YO ANN\n");
}

#[test]
fn a_trait_cannot_be_instantiated() {
    let (out, err, ok) =
        run_full("trait S { def f: Int = 1 }\nobject T extends App { println(new S().f) }");
    assert!(!ok);
    assert_eq!(out, "");
    assert!(
        err.contains("trait S is abstract; it cannot be instantiated"),
        "{err:?}"
    );
}

// ── Range as a value, Array, scala.math ─────────────────────────────────────

#[test]
fn a_range_is_a_value_with_scalas_tostring() {
    let (out, ok) = run(&wrap(
        "println(1 to 5); println(1 until 5); println(1 to 10 by 3); println(10 to 1 by -2); println(5 to 1)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "Range 1 to 5\nRange 1 until 5\nRange 1 to 10 by 3\ninexact Range 10 to 1 by -2\nempty Range 5 to 1\n"
    );
}

#[test]
fn range_sequence_ops_match_scalas_result_types() {
    // `map`/`filter` on a `Range` yield a `Vector`; `reverse` yields a `Range`.
    let (out, ok) = run(&wrap(
        "val r = 1 to 5\nprintln(r.sum + \"/\" + r.length)\nprintln(r.toList)\nprintln(r.map(_ * 2))\nprintln(r.reverse)\nprintln(r.filter(_ % 2 == 0))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "15/5\nList(1, 2, 3, 4, 5)\nVector(2, 4, 6, 8, 10)\nRange 5 to 1 by -1\nVector(2, 4)\n"
    );
}

#[test]
fn an_array_is_mutable_through_indexed_assignment() {
    let (out, ok) = run(&wrap(
        "val a = Array(1, 2, 3)\nprintln(a.length + \":\" + a(0))\na(1) = 9\nprintln(a.mkString(\",\"))\nprintln(a.map(_ * 2).mkString(\",\"))\nprintln(a.sum)",
    ));
    assert!(ok);
    assert_eq!(out, "3:1\n1,9,3\n2,18,6\n13\n");
}

#[test]
fn new_array_fills_with_the_element_types_zero() {
    let (out, ok) = run(&wrap(
        "println(new Array[Int](3).mkString(\"|\"))\nprintln(new Array[Double](2).mkString(\"|\"))\nprintln(new Array[Boolean](2).mkString(\"|\"))",
    ));
    assert!(ok);
    assert_eq!(out, "0|0|0\n0.0|0.0\nfalse|false\n");
}

#[test]
fn a_list_is_immutable_so_it_has_no_update() {
    let (out, err, ok) = run_full(&wrap("val xs = List(1, 2, 3)\nxs(1) = 9\nprintln(xs)"));
    assert!(!ok);
    assert_eq!(out, "");
    assert!(err.contains("value update is not a member"), "{err:?}");
}

#[test]
fn math_members_keep_scalas_int_and_double_overloads_apart() {
    let (out, ok) = run(&wrap(
        "println(math.abs(-4) + \" \" + math.abs(-3.5))\nprintln(math.max(2, 7) + \" \" + math.min(2.5, 1.5))\nprintln(math.round(2.5) + \" \" + math.round(-2.5))\nprintln(math.signum(-5) + \" \" + Math.signum(5))",
    ));
    assert!(ok);
    // `java.lang.Math` has no `signum(int)`, so `Math.signum(5)` widens to 1.0.
    assert_eq!(out, "4 3.5\n7 1.5\n3 -2\n-1 1.0\n");
}

#[test]
fn math_is_reachable_under_every_spelling() {
    let (out, ok) = run(&wrap(
        "println(math.sqrt(16.0))\nprintln(scala.math.pow(2.0, 10.0))\nprintln(Math.hypot(3.0, 4.0))\nprintln(math.Pi)",
    ));
    assert!(ok);
    assert_eq!(out, "4.0\n1024.0\n5.0\n3.141592653589793\n");
}

// ── collections ────────────────────────────────────────────────────────────

#[test]
fn seq_literals_carry_their_collection_kind() {
    // Scala 3's `Seq` *is* `List`, and `IndexedSeq` is `Vector`, so all three
    // spellings must render as the class they alias.
    let (out, ok) = run(&wrap(
        "println(Seq(1, 2, 3))\nprintln(Vector(1, 2, 3))\nprintln(IndexedSeq(1, 2))\nprintln(Vector(1, 2).map(_ + 1))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "List(1, 2, 3)\nVector(1, 2, 3)\nVector(1, 2)\nVector(2, 3)\n"
    );
}

#[test]
fn a_set_becomes_a_hashset_past_four_elements() {
    // Up to four entries Scala uses `Set1`..`Set4`, which keep insertion order;
    // the fifth switches to a CHAMP `HashSet`, printed in trie order. Getting
    // the trie order wrong is the only way this test can pass the first line and
    // fail the second.
    let (out, ok) = run(&wrap(
        "println(Set(3, 1, 2))\nprintln(Set(9, 3, 1, 2, 7))\nprintln(Set(\"e\", \"a\", \"b\", \"c\", \"d\"))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "Set(3, 1, 2)\nHashSet(1, 9, 2, 7, 3)\nHashSet(e, a, b, c, d)\n"
    );
}

#[test]
fn a_hashed_set_stays_hashed_however_small_the_result() {
    // `HashSet.filter` rebuilds the trie rather than going through the small-set
    // builder, so a four-element result still prints `HashSet`.
    let (out, ok) = run(&wrap(
        "println(Set(1, 2, 3, 4, 5).filter(_ > 1))\nprintln(Set(1, 2, 3, 4, 5).map(_ % 2))\nprintln(Set(1, 2, 3).map(_ * 2))",
    ));
    assert!(ok);
    assert_eq!(out, "HashSet(5, 2, 3, 4)\nHashSet(0, 1)\nSet(2, 4, 6)\n");
}

#[test]
fn a_map_becomes_a_hashmap_past_four_entries() {
    let (out, ok) = run(&wrap(
        "println(Map(3 -> \"c\", 1 -> \"a\"))\nprintln(Map(1 -> 1, 2 -> 2, 3 -> 3, 4 -> 4, 5 -> 5))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "Map(3 -> c, 1 -> a)\nHashMap(5 -> 5, 1 -> 1, 2 -> 2, 3 -> 3, 4 -> 4)\n"
    );
}

#[test]
fn group_by_always_answers_a_hashmap() {
    // `groupBy` builds through a `HashMap` builder, so even a single group is a
    // `HashMap` — not the `Map1` a two-entry literal would give.
    let (out, ok) = run(&wrap(
        "println(List(1).groupBy(x => x))\nprintln(List(1, 2, 3, 4).groupBy(_ % 2))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "HashMap(1 -> List(1))\nHashMap(0 -> List(2, 4), 1 -> List(1, 3))\n"
    );
}

#[test]
fn a_case_class_hashes_as_scalas_murmurhash3_product_hash() {
    // The exact bits matter: a `Set` of records is ordered by them.
    let (out, ok) = run(
        "case class Pt(x: Int, y: Int)\nobject T extends App {\n  println(Pt(1, 2).hashCode)\n  println((1, 2).hashCode)\n  println(Set((1,2), (3,4), (5,6), (7,8), (9,10)))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "2081183297\n1316541600\nHashSet((5,6), (3,4), (7,8), (1,2), (9,10))\n"
    );
}

#[test]
fn sequence_combinators_preserve_order_and_kind() {
    let (out, ok) = run(&wrap(
        "val xs = List(5, 3, 9, 1)\nprintln(xs.sorted)\nprintln(xs.sortBy(x => -x))\nprintln(xs.zip(List(\"a\", \"b\")))\nprintln(xs.zipWithIndex)\nprintln(xs.partition(_ > 4))\nprintln(xs.grouped(3).toList)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "List(1, 3, 5, 9)\nList(9, 5, 3, 1)\nList((5,a), (3,b))\nList((5,0), (3,1), (9,2), (1,3))\n(List(5, 9),List(3, 1))\nList(List(5, 3, 9), List(1))\n"
    );
}

#[test]
fn sorting_is_stable_and_orders_strings_by_code_unit() {
    // Equal keys keep input order in both languages, so the tie between the two
    // two-character words is the observable part (`ax` was written after `fig`
    // but sorts before it only because its key is smaller).
    let (out, ok) = run(&wrap(
        "println(List(\"pear\", \"fig\", \"ax\", \"by\", \"apple\").sortBy(_.length))\nprintln(List(\"B\", \"a\", \"A\", \"b\").sorted)",
    ));
    assert!(ok);
    assert_eq!(out, "List(ax, by, fig, pear, apple)\nList(A, B, a, b)\n");
}

#[test]
fn map_methods_keep_the_receivers_representation() {
    let (out, ok) = run(&wrap(
        "val m = Map(\"a\" -> 1, \"b\" -> 2)\nprintln(m.map { case (k, v) => (k, v * 10) })\nprintln(m.filter { case (_, v) => v > 1 })\nprintln(m + (\"c\" -> 3))\nprintln(m - \"a\")\nprintln(m.updated(\"a\", 9))\nprintln(m.keys)\nprintln(m.values)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "Map(a -> 10, b -> 20)\nMap(b -> 2)\nMap(a -> 1, b -> 2, c -> 3)\nMap(b -> 2)\nMap(a -> 9, b -> 2)\nSet(a, b)\nIterable(1, 2)\n"
    );
}

#[test]
fn symbolic_collection_operators_pick_scalas_associativity() {
    // `+:` ends in `:`, so it is right-associative and dispatches on its RIGHT
    // operand — `0 +: xs` is `xs.+:(0)`, not `0.+:(xs)`.
    let (out, ok) = run(&wrap(
        "println(List(1, 2) ++ List(3))\nprintln(List(1, 2) :+ 3)\nprintln(0 +: List(1, 2))\nprintln(1 +: 2 +: List(3))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "List(1, 2, 3)\nList(1, 2, 3)\nList(0, 1, 2)\nList(1, 2, 3)\n"
    );
}

// ── infix method syntax and `for` comprehensions ────────────────────────────

#[test]
fn any_single_argument_method_can_be_written_infix() {
    // An alphanumeric operator binds looser than every symbolic one and chains
    // left-associatively, so `xs map f mkString s` is `(xs.map(f)).mkString(s)`
    // and `1 to n - 1` is `1 to (n - 1)`.
    let (out, ok) = run(&wrap(
        "println(List(1, 2, 3) contains 2)\nprintln(1 max 2)\nprintln(List(1, 2, 3) map (_ * 2) mkString \",\")\nprintln((1 to 4 - 1).toList)",
    ));
    assert!(ok);
    assert_eq!(out, "true\n2\n2,4,6\nList(1, 2, 3)\n");
}

#[test]
fn a_line_break_stops_an_infix_reading() {
    // `xs` and `println` on separate lines are two statements, not `xs.println`.
    let (out, ok) = run(&wrap("val xs = List(1, 2)\nxs\nprintln(xs.length)"));
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn a_soft_keyword_is_never_read_as_an_infix_operator() {
    // `yield` follows a complete enumerator group but is not a method call on it.
    let (out, ok) = run(&wrap("println(for (x <- List(1, 2)) yield x * 3)"));
    assert!(ok);
    assert_eq!(out, "List(3, 6)\n");
}

#[test]
fn a_comprehension_accepts_braces_guards_and_destructuring() {
    let (out, ok) = run(&wrap(
        "println(for { x <- List(1, 2, 3); if x > 1 } yield x)\nval m = Map(\"a\" -> 1, \"b\" -> 2)\nfor ((k, v) <- m) println(k + \"=\" + v)\nprintln(for ((k, v) <- m.toList) yield v)",
    ));
    assert!(ok);
    assert_eq!(out, "List(2, 3)\na=1\nb=2\nList(1, 2)\n");
}

#[test]
fn a_case_block_is_a_pattern_matching_anonymous_function() {
    // The same literal serves a one-argument `map` and a two-argument
    // `foldLeft`, because Scala tuples the arguments of the latter.
    let (out, ok) = run(&wrap(
        "println(List((1, 2), (3, 4)).map { case (a, b) => a * b })\nprintln(List((1, 2), (3, 4)).foldLeft(0) { case (acc, (a, b)) => acc + a + b })",
    ));
    assert!(ok);
    assert_eq!(out, "List(2, 12)\n10\n");
}

// ── singleton objects inheriting concrete members ──────────────────────────

#[test]
fn an_object_inherits_its_traits_method_bodies() {
    let (out, ok) = run(
        "trait Greeter {\n  def name: String\n  def greet: String = \"hello, \" + name\n  def loud: String = greet.toUpperCase\n  val punct: String = \"!\"\n}\nobject Bob extends Greeter { def name = \"bob\" }\nobject Carol extends Greeter {\n  def name = \"carol\"\n  override def greet: String = \"hi, \" + name\n}\nobject T extends App {\n  println(Bob.greet)\n  println(Bob.loud)\n  println(Bob.punct)\n  println(Carol.greet)\n  println(Carol.loud)\n}",
    );
    assert!(ok);
    assert_eq!(out, "hello, bob\nHELLO, BOB\n!\nhi, carol\nHI, CAROL\n");
}

#[test]
fn an_inherited_object_method_dispatches_through_the_supertype() {
    // The `Shape`-typed binding proves the inherited body is reachable by
    // virtual dispatch, not only by the object's own name.
    let (out, ok) = run(
        "sealed trait Shape { def area: Double; def label: String = \"shape:\" + area }\ncase object One extends Shape { def area = 1.0 }\nobject T extends App {\n  val s: Shape = One\n  println(s.label)\n  println(One.label)\n}",
    );
    assert!(ok);
    assert_eq!(out, "shape:1.0\nshape:1.0\n");
}

// ── generics (type-erased) ─────────────────────────────────────────────────

#[test]
fn type_parameters_are_erased_but_never_rejected() {
    let (out, ok) = run(
        "class Box[A](val v: A) { def get: A = v; def map[B](f: A => B): Box[B] = new Box(f(v)) }\ncase class Pair[A, B](a: A, b: B) { def swap: Pair[B, A] = Pair(b, a) }\ntrait Cont[A] { def item: A; def show: String = \"<\" + item + \">\" }\nclass Cell[A](val item: A) extends Cont[A]\nobject T extends App {\n  def id[A](x: A): A = x\n  println(new Box[Int](3).get)\n  println(new Box(3).map(x => x * 2).get)\n  println(Pair(1, \"a\").swap)\n  println(id[String](\"hi\"))\n  println(new Cell[String](\"z\").show)\n  val m: Map[String, List[Int]] = Map(\"a\" -> List(1, 2))\n  println(m)\n}",
    );
    assert!(ok);
    assert_eq!(out, "3\n6\nPair(a,1)\nhi\n<z>\nMap(a -> List(1, 2))\n");
}

// ── partial functions (`{ case … }` as a PartialFunction) ──────────────────

#[test]
fn collect_skips_elements_no_arm_matches_and_never_runs_their_body() {
    // The `seen` counter is the load-bearing assertion: `isDefinedAt` must
    // evaluate the pattern and guard only, so two of four elements increment it
    // and a bare `isDefinedAt` call increments nothing.
    let (out, ok) = run(
        "object T extends App {\n  var seen = 0\n  val pf: PartialFunction[Int, Int] = { case x if x > 2 => seen += 1; x * 10 }\n  println(List(1, 2, 3, 4).collect(pf))\n  println(seen)\n  println(List(1, 2, 3, 4).collectFirst { case x if x > 2 => x })\n  println(List(1, 2).collectFirst { case x if x > 9 => x })\n  println(pf.isDefinedAt(1))\n  println(pf.isDefinedAt(3))\n  println(seen)\n}",
    );
    assert!(ok);
    assert_eq!(out, "List(30, 40)\n2\nSome(3)\nNone\nfalse\ntrue\n2\n");
}

#[test]
fn partial_functions_compose_through_orelse_lift_andthen_and_compose() {
    let (out, ok) = run(
        "object T extends App {\n  val a: PartialFunction[Int, String] = { case 1 => \"one\" }\n  val b: PartialFunction[Int, String] = { case x if x > 5 => \"big\" }\n  val both = a orElse b\n  println(List(1, 3, 7).map(both.lift))\n  println(both.applyOrElse(3, (i: Int) => \"?\" + i))\n  val f = (x: Int) => x + 1\n  println(List(1, 2).map(f andThen ((y: Int) => y * 3)))\n  println(List(1, 2).map(f compose ((y: Int) => y * 3)))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "List(Some(one), None, Some(big))\n?3\nList(6, 9)\nList(4, 7)\n"
    );
}

#[test]
fn collect_matches_type_constructor_and_guard_patterns() {
    // A derived collection keeps the receiver's representation, so the six-element
    // `Set` stays a CHAMP `HashSet` and prints in trie order.
    let (out, ok) = run(
        "case class P(x: Int, y: Int)\nobject T extends App {\n  val any: List[Any] = List(1, \"hi\", 2.5, true)\n  println(any.collect { case s: String => s.toUpperCase })\n  println(any.collectFirst { case d: Double => d })\n  println(List(P(1, 2), P(5, 6)).collect { case P(a, b) if a < 3 => a + b })\n  println(List(Some(1), None, Some(3)).collect { case Some(v) => v })\n  println(Set(1, 2, 3, 4, 5, 6).collect { case x if x % 2 == 0 => x })\n  println((1 to 6).collect { case x if x % 3 == 0 => x })\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "List(HI)\nSome(2.5)\nList(3)\nList(1, 3)\nHashSet(6, 2, 4)\nVector(3, 6)\n"
    );
}

#[test]
fn map_collect_picks_its_builder_from_the_result_shape_even_when_empty() {
    // Scala reads the builder off the function's static result type: a pair
    // rebuilds a `Map`, anything else falls back to `immutable.Iterable`'s
    // builder, which is `List`. With no surviving entry the run-time results
    // cannot say which, so the compile-time result shape decides.
    let (out, ok) = run(
        "object T extends App {\n  val m = Map(\"a\" -> 1, \"b\" -> 2, \"c\" -> 3)\n  println(m.collect { case (k, v) if v > 1 => k })\n  println(m.collect { case (k, v) if v > 1 => k -> v * 10 })\n  println(m.collect { case (k, v) if v > 9 => k })\n  println(m.collect { case (k, v) if v > 9 => k -> v })\n  println(m.collectFirst { case (k, v) if v > 1 => k })\n  println(m.map { case (k, v) => v })\n  println(m.values.map(_ * 2))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "List(b, c)\nMap(b -> 20, c -> 30)\nList()\nMap()\nSome(b)\nList(1, 2, 3)\nList(2, 4, 6)\n"
    );
}

// ── mutable collections ───────────────────────────────────────────────────

#[test]
fn buffers_grow_shrink_and_keep_their_own_prefix() {
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  val b = mutable.ListBuffer(3, 1, 2)\n  b += 4; b ++= List(5, 6); b -= 1\n  println(b); println(b.size); println(b.toList); println(b.sorted); println(b.map(_ * 2))\n  b(0) = 99; println(b)\n  val a = mutable.ArrayBuffer[Int]()\n  a += 1; a.append(2); a.prepend(0)\n  println(a); println(a.remove(0)); println(a)\n  a.insert(1, 9); println(a)\n  a.clear(); println(a); println(a.isEmpty)\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "ListBuffer(3, 2, 4, 5, 6)\n5\nList(3, 2, 4, 5, 6)\nListBuffer(2, 3, 4, 5, 6)\nListBuffer(6, 4, 8, 10, 12)\nListBuffer(99, 2, 4, 5, 6)\nArrayBuffer(0, 1, 2)\n0\nArrayBuffer(1, 2)\nArrayBuffer(1, 9, 2)\nArrayBuffer()\ntrue\n"
    );
}

#[test]
fn a_mutable_hashset_prints_in_its_tables_order_not_insertion_order() {
    // Every line here is a *different* table length, which is what the order
    // turns on: `HashSet.from` sizes the table from the argument count, `add`
    // doubles it at three quarters full, and the bucket is
    // `(h ^ (h >>> 16)) & (len - 1)` with each bucket sorted by that hash. The
    // seventeen-element line is the one that has grown past its initial table.
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  println(mutable.Set(1, 2, 3, 4, 5, 6, 7, 8, 9, 10))\n  println(mutable.Set(-7, 42, 3, -1, 100, 0))\n  println(mutable.Set(\"apple\", \"banana\", \"cherry\", \"date\"))\n  println(mutable.Set(100, 200, 300, 400, 500, 600, 700, 800, 900, 1000, 1100, 1200, 1300, 1400, 1500, 1600, 1700))\n  println(mutable.Set(5, 3, 1) ++ List(9, 7))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "HashSet(1, 2, 3, 4, 5, 6, 7, 8, 9, 10)\nHashSet(-1, 0, 3, 100, -7, 42)\nHashSet(banana, date, cherry, apple)\nHashSet(800, 1600, 100, 900, 1700, 200, 1000, 300, 1100, 400, 1200, 500, 1300, 600, 1400, 700, 1500)\nHashSet(1, 3, 5, 7, 9)\n"
    );
}

#[test]
fn mutable_set_and_map_mutators_answer_what_scala_answers() {
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  val s = mutable.Set(1, 2)\n  println(s.add(3)); println(s.add(3)); println(s.remove(1)); println(s.remove(99)); println(s)\n  s += 5; println(s)\n  s --= List(2); println(s)\n  val m = mutable.Map(1 -> \"a\")\n  println(m.put(2, \"b\")); println(m.put(2, \"c\")); println(m.remove(1)); println(m)\n  m.update(3, \"z\"); println(m)\n  println(m.getOrElseUpdate(9, \"n\")); println(m)\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "true\nfalse\ntrue\nfalse\nHashSet(2, 3)\nHashSet(2, 3, 5)\nHashSet(3, 5)\nNone\nSome(b)\nSome(a)\nHashMap(2 -> c)\nHashMap(2 -> c, 3 -> z)\nn\nHashMap(9 -> n, 2 -> c, 3 -> z)\n"
    );
}

#[test]
fn plus_equals_picks_the_growable_method_or_arithmetic_at_run_time() {
    // Scala chooses statically from the receiver's type; there are no static
    // types here, so a program that builds a mutable collection anywhere emits
    // a run-time test. `n += 5` and `s += "b"` must still be arithmetic and
    // concatenation, and a `var`-held buffer must still mutate in place.
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  var b = mutable.ListBuffer(1, 2)\n  b += 3; println(b)\n  var n = 0; n += 5; n -= 2; println(n)\n  var s = \"a\"; s += \"b\"; println(s)\n  val lb = mutable.ListBuffer[Int]()\n  for (i <- 1 to 4) lb += i * i\n  println(lb); println(lb.foldLeft(0)(_ + _))\n  val c = mutable.Map[String, Int]()\n  for (w <- List(\"a\", \"b\", \"a\", \"c\", \"a\")) c(w) = c.getOrElse(w, 0) + 1\n  println(c); println(c.toList.sorted)\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "ListBuffer(1, 2, 3)\n3\nab\nListBuffer(1, 4, 9, 16)\n30\nHashMap(a -> 3, b -> 1, c -> 1)\nList((a,3), (b,1), (c,1))\n"
    );
}

// ── MurmurHash3 seq/set/map hashes ────────────────────────────────────────

#[test]
fn collections_hash_with_scalas_own_murmurhash3_values() {
    // Every number here is the reference `scala`'s. The first four agreeing is
    // the point of `MurmurHash3.seqHash`: an ordered hash with the `Seq` seed,
    // reached through three different loops in the library but one value, with
    // the arithmetic-progression case that makes a `Range` hash as its
    // elements. A `Set`/`Map` hashes symmetrically instead, so the mutable and
    // immutable ones agree too.
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  println(List(1, 2, 3).hashCode)\n  println(Vector(1, 2, 3).hashCode)\n  println((1 to 3).hashCode)\n  println(mutable.ListBuffer(1, 2, 3).hashCode)\n  println(List(1, 5, 2).hashCode)\n  println(List[Int]().hashCode)\n  println(List(7).hashCode)\n  println(Set(1, 2, 3).hashCode)\n  println(mutable.Set(1, 2, 3).hashCode)\n  println(Map(\"a\" -> 1, \"b\" -> 2).hashCode)\n  println(Set[Int]().hashCode)\n  println(Map[String, Int]().hashCode)\n  println(List(List(1), List(2)).hashCode)\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "1836368899\n1836368899\n1836368899\n1836368899\n759968518\n473519988\n-2080959496\n1510543636\n1510543636\n2006323191\n835491922\n-1609326920\n-1127282338\n"
    );
}

#[test]
fn a_hashed_set_or_map_keyed_by_a_collection_prints_in_trie_order() {
    // Before the seq/set/map hashes were ported these fell back to insertion
    // order, because the CHAMP trie's position needs the key's JVM hash.
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  println(Set(List(1), List(2), List(3), List(4), List(5)))\n  println(Map(List(1) -> \"a\", List(2) -> \"b\", List(3) -> \"c\", List(4) -> \"d\", List(5) -> \"e\"))\n  println(Set((1, 2), (3, 4), (5, 6), (7, 8), (9, 10)))\n  println(Set(Set(1), Set(2), Set(3), Set(4), Set(5)))\n  println(mutable.Set(List(1), List(2), List(3), List(4), List(5)))\n  println(List(List(1), List(2), List(3), List(4), List(5)).groupBy(_.head % 2))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "HashSet(List(1), List(3), List(5), List(4), List(2))\nHashMap(List(1) -> a, List(3) -> c, List(5) -> e, List(4) -> d, List(2) -> b)\nHashSet((5,6), (3,4), (7,8), (1,2), (9,10))\nHashSet(Set(2), Set(5), Set(3), Set(4), Set(1))\nHashSet(List(5), List(1), List(2), List(4), List(3))\nHashMap(0 -> List(List(2), List(4)), 1 -> List(List(1), List(3), List(5)))\n"
    );
}

// ── bitwise / shift operators ─────────────────────────────────────────────

#[test]
fn bitwise_and_shift_operators_evaluate_at_int_width() {
    // `1 << 33 == 2` and `1 << 31 == Int.MinValue` are the observable part of
    // Scala's 32-bit `Int`: the shift distance masks to five bits and the
    // result wraps, which a 64-bit shift would not do.
    let (out, ok) = run(&wrap(
        "println(6 & 3); println(6 | 3); println(6 ^ 3); println(~(6))\nprintln(1 << 4); println(-16 >> 2); println(-16 >>> 2)\nprintln(1 << 33); println(1 << 31); println(255 & 0x0F); println(0x1F)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "2\n7\n5\n-7\n16\n-4\n1073741820\n2\n-2147483648\n15\n31\n"
    );
}

#[test]
fn bitwise_operator_precedence_follows_the_sls_first_character_table() {
    // `|` binds loosest, then `^`, then `&` — which is also where `&&`/`||`
    // get their precedence — and every arithmetic operator binds tighter than
    // a shift, so `1 << 2 + 1` is `1 << 3`.
    let (out, ok) = run(&wrap(
        "println(5 & 3 | 2); println(1 << 2 + 1); println(3 + 1 & 6); println(1 | 0 ^ 3 & 2)\nprintln(true & false); println(true | false); println(true ^ true)\nprintln(Set(1, 2, 3) & Set(2, 3, 4)); println(Set(1, 2) | Set(3)); println(Set(1, 2, 3) &~ Set(2))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "3\n8\n4\n3\nfalse\ntrue\nfalse\nSet(2, 3)\nSet(1, 2, 3)\nSet(1, 3)\n"
    );
}

#[test]
fn a_prefix_minus_on_a_numeric_literal_is_part_of_the_literal() {
    // Scala reads `-3.abs` as `(-3).abs`, so the sign is what the postfix chain
    // applies to; a non-literal receiver keeps the unary reading, so `-x.abs`
    // stays `-(x.abs)`.
    let (out, ok) = run(&wrap(
        "println(-3.abs); println(-3.0.abs); val x = 3; println(-x.abs); println(-3.toString); println(1 - 3.abs); println(-3.max(1))",
    ));
    assert!(ok);
    assert_eq!(out, "3\n3.0\n-3\n-3\n-2\n1\n");
}

#[test]
fn an_import_line_does_not_swallow_a_following_case_class() {
    // Newline inference emits no statement separator before `case`, so the
    // prologue skip has to stop at a declaration keyword as well as at a line
    // end — otherwise the whole `case class` line was consumed as part of the
    // import and every use of it failed with "not found".
    let (out, ok) = run(
        "import scala.collection.mutable\ncase class Q(x: Int)\nobject T extends App { println(Q(1)); println(Q(1) == Q(1)) }",
    );
    assert!(ok);
    assert_eq!(out, "Q(1)\ntrue\n");
}

// ── Pattern-matching forms beyond literal/type/tuple (byte-verified vs scala
//    3.8.4 via the `patmatch` fuzz mode) ──────────────────────────────────────

#[test]
fn at_binder_binds_the_whole_scrutinee_alongside_its_parts() {
    // `w @ Pt(x, y)` must bind BOTH the record and its fields, and the binder
    // must be visible to the guard's arm body, not just to the pattern.
    let src = "case class Pt(x: Int, y: Int)\nobject T extends App {\n  def f(p: Pt): String = p match { case w @ Pt(0, _) => \"z\" + w; case w @ Pt(x, y) if x > y => \"gt\" + w.x; case w => \"o\" + w }\n  println(f(Pt(0, 4))); println(f(Pt(9, 1))); println(f(Pt(1, 9)))\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "zPt(0,4)\ngt9\noPt(1,9)\n");
}

#[test]
fn alternation_tries_every_branch_before_falling_through() {
    // The regression this guards: a `|` arm that only ever tested its FIRST
    // branch would still answer "low" for 0 and drop 1 and 2 to the catch-all.
    let (out, ok) = run(&wrap(
        "def f(x: Int): String = x match { case 0 | 1 | 2 => \"low\"; case 7 | 9 => \"odd\"; case v if v < 0 => \"neg\"; case v => \"hi\" + v }\nList(0, 1, 2, 7, 9, -4, 12).foreach(x => println(f(x)))",
    ));
    assert!(ok);
    assert_eq!(out, "low\nlow\nlow\nodd\nodd\nneg\nhi12\n");
}

#[test]
fn cons_and_nil_patterns_destructure_a_list_by_shape() {
    let (out, ok) = run(&wrap(
        "def f(l: List[Int]): String = l match { case Nil => \"nil\"; case x :: Nil => \"one\" + x; case x :: y :: Nil => \"two\" + (x + y); case h :: t => \"n\" + h + \"/\" + t.length }\nprintln(f(Nil)); println(f(List(4))); println(f(List(4, 5))); println(f(List(1, 2, 3, 4)))",
    ));
    assert!(ok);
    assert_eq!(out, "nil\none4\ntwo9\nn1/3\n");
}

#[test]
fn sequence_pattern_matches_on_length_and_binds_the_rest() {
    // `List(x, y)` is an EXACT-length test; the trailing `_*` turns it into a
    // minimum, and a named `_*` binds the remainder as a `List`.
    let (out, ok) = run(&wrap(
        "def f(l: List[Int]): String = l match { case List() => \"e\"; case List(x) => \"1:\" + x; case List(x, y) => \"2:\" + (x * y); case List(x, rest @ _*) => \"r:\" + x + rest }\nprintln(f(Nil)); println(f(List(3))); println(f(List(3, 4))); println(f(List(1, 2, 3)))",
    ));
    assert!(ok);
    assert_eq!(out, "e\n1:3\n2:12\nr:1List(2, 3)\n");
}

#[test]
fn a_vector_does_not_match_a_list_pattern() {
    // The shape test is per-representation, as in Scala: `case List(a, b)` on a
    // `Vector` must fall through rather than destructure it. Both non-matching
    // cases answer the same catch-all, so the assertion pins that the shape test
    // runs BEFORE the length test (a `Vector` of the right length still misses).
    let (out, ok) = run(&wrap(
        "def f(s: Seq[Int]): String = s match { case List(a, b) => \"list\" + a + b; case _ => \"?\" }\nprintln(f(List(1, 2))); println(f(Vector(1, 2))); println(f(List(1)))",
    ));
    assert!(ok);
    assert_eq!(out, "list12\n?\n?\n");
}

#[test]
fn pattern_definitions_bind_into_the_enclosing_scope() {
    let src = "case class D(x: Int, y: String)\nobject T extends App {\n  val (p, q) = (3, \"k\")\n  println(p + q)\n  val D(dx, dy) = D(7, \"m\")\n  println(dx + dy)\n  val hd :: tl = List(1, 2, 3)\n  println(hd); println(tl)\n  val ((m, n) , o) = ((1, 2), \"t\")\n  println(m + n + o)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "3k\n7m\n1\nList(2, 3)\n3t\n");
}

// ── `scala.Option` (byte-verified vs scala 3.8.4 via the `option` fuzz mode) ──

#[test]
fn option_combinators_answer_both_the_some_and_the_none_case() {
    let (out, ok) = run(&wrap(
        "val s: Option[Int] = Some(5)\nval e: Option[Int] = None\nprintln(s.getOrElse(0)); println(e.getOrElse(0))\nprintln(s.map(_ * 2)); println(e.map(_ * 2))\nprintln(s.flatMap(x => Some(x + 1))); println(e.flatMap(x => Some(x + 1)))\nprintln(s.fold(-1)(_ + 1)); println(e.fold(-1)(_ + 1))\nprintln(s.filter(_ > 9)); println(s.exists(_ > 3)); println(e.forall(_ > 3))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "5\n0\nSome(10)\nNone\nSome(6)\nNone\n6\n-1\nNone\ntrue\ntrue\n"
    );
}

#[test]
fn option_conversions_and_the_null_factory() {
    // `Option(null)` is the one factory case that is NOT `Some`, and
    // `List[Option[A]].flatten` must drop the empties rather than fault.
    let (out, ok) = run(&wrap(
        "val e: Option[Int] = None\nprintln(Option(3)); println(Option(null))\nprintln(List(Some(1), None, Some(3)).flatten)\nprintln(Some(Some(1)).flatten)\nprintln(Some(5).toRight(\"z\")); println(e.toRight(\"z\"))\nprintln(Some(5).toLeft(\"z\")); println(e.toLeft(\"z\"))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "Some(3)\nNone\nList(1, 3)\nSome(1)\nRight(5)\nLeft(z)\nLeft(5)\nRight(z)\n"
    );
}

#[test]
fn either_cases_construct_and_destructure() {
    let (out, ok) = run(&wrap(
        "val rs: List[Either[String, Int]] = List(Right(1), Left(\"e\"), Right(4))\nprintln(rs)\nprintln(rs.collect { case Right(v) => v })\nprintln(rs.map { case Right(v) => v * 2; case Left(m) => m.length })",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "List(Right(1), Left(e), Right(4))\nList(1, 4)\nList(2, 1, 8)\n"
    );
}

// ── `Product` on a case class (byte-verified vs scala 3.8.4) ─────────────────

#[test]
fn product_members_expose_the_constructor_prefix_only() {
    // A body `val` is a member but NOT a product element — the same prefix
    // `toString`/`unapply` use.
    let src = "case class C(x: Int, y: String) { val extra = x * 2 }\nobject T extends App {\n  val c = C(3, \"q\")\n  println(c.productArity); println(c.productPrefix)\n  println(c.productElement(0)); println(c.productElement(1))\n  println(c.productIterator.toList)\n  println(c.extra)\n}";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "2\nC\n3\nq\nList(3, q)\n6\n");
}

#[test]
fn tuple_swap_and_product_members() {
    let (out, ok) = run(&wrap(
        "println((1, \"a\").swap); println((1, 2, 3).productArity); println((1, 2).productPrefix)\nprintln(List(3, 1, 2).iterator.toList); println(List(3, 1, 2).reverseIterator.toList)",
    ));
    assert!(ok);
    assert_eq!(out, "(a,1)\n3\nTuple2\nList(3, 1, 2)\nList(2, 1, 3)\n");
}

// ── Wider `String`/`StringOps` surface (byte-verified vs scala 3.8.4) ────────

#[test]
fn string_index_search_and_total_slicing() {
    let (out, ok) = run(&wrap(
        "val s = \"Hello, World\"\nprintln(s.indexOf(\"o\")); println(s.lastIndexOf(\"o\")); println(s.indexOf(\"z\"))\nprintln(s.take(3)); println(s.drop(3)); println(s.slice(1, 4)); println(s.splitAt(3))\nprintln(s.take(99)); println(s.drop(99))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "4\n8\n-1\nHel\nlo, World\nell\n(Hel,lo, World)\nHello, World\n\n"
    );
}

#[test]
fn string_char_combinators_rebuild_a_string() {
    // A `Char => Char` map answers a `String`; the predicates and the searches
    // answer scalars/`Option`, so this pins both result shapes at once.
    let (out, ok) = run(&wrap(
        "val s = \"abcabc\"\nprintln(s.distinct); println(s.sorted)\nprintln(s.filter(_ != 'a')); println(s.count(_ == 'a'))\nprintln(s.map(c => c.toUpper))\nprintln(s.takeWhile(_ != 'b')); println(s.dropWhile(_ != 'b'))\nprintln(s.find(_ == 'b')); println(s.indexWhere(_ == 'b'))\nprintln(s.partition(_ < 'c')); println(s.span(_ != 'b'))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "abc\naabbcc\nbcbc\n2\nABCABC\na\nbcabc\nSome(b)\n1\n(abab,cc)\n(a,bcabc)\n"
    );
}

// ── Non-local `return` and `finally` ordering (byte-verified vs scala 3.8.4
//    via the `nlr` fuzz mode) ─────────────────────────────────────────────────

#[test]
fn return_inside_a_for_leaves_the_enclosing_def() {
    // The `for` desugars to a `foreach` CLOSURE, so a frame-local return would
    // only end one iteration and the method would fall through to "none".
    let (out, ok) = run(&wrap(
        "def f(l: List[Int]): String = { for (x <- l) { if (x > 2) return \"hit\" + x }; \"none\" }\nprintln(f(List(1, 5, 9))); println(f(Nil))",
    ));
    assert!(ok);
    assert_eq!(out, "hit5\nnone\n");
}

#[test]
fn return_inside_an_explicit_lambda_leaves_the_enclosing_def() {
    // `return` in expression position (a brace-less lambda body) is also the
    // parse shape this covers.
    let (out, ok) = run(&wrap(
        "def f(l: List[Int]): Int = { l.foreach(x => if (x > 2) return x); -1 }\nprintln(f(List(1, 5, 9))); println(f(Nil))",
    ));
    assert!(ok);
    assert_eq!(out, "5\n-1\n");
}

#[test]
fn finally_runs_before_a_return_leaves_the_try() {
    // Output ORDER is the assertion: the finalizer prints before the caller's
    // `println` of the returned value.
    let (out, ok) = run(&wrap(
        "def f(n: Int): Int = { try { if (n > 0) return n * 10; 0 } finally { println(\"fin\") } }\nprintln(f(9)); println(f(-9))",
    ));
    assert!(ok);
    assert_eq!(out, "fin\n90\nfin\n0\n");
}

#[test]
fn nested_finalizers_run_innermost_first_on_a_return() {
    let (out, ok) = run(&wrap(
        "def f(n: Int): Int = { try { try { if (n > 0) return n; 0 } finally { println(\"inner\") } } finally { println(\"outer\") } }\nprintln(f(4))",
    ));
    assert!(ok);
    assert_eq!(out, "inner\nouter\n4\n");
}

#[test]
fn return_from_a_catch_arm_still_runs_the_finalizer() {
    let (out, ok) = run(&wrap(
        "def f(n: Int): Int = { try { if (n > 0) throw new RuntimeException(\"x\"); 1 } catch { case e: RuntimeException => return 7 } finally { println(\"fin\") } }\nprintln(f(1)); println(f(-1))",
    ));
    assert!(ok);
    assert_eq!(out, "fin\n7\nfin\n1\n");
}

#[test]
fn return_inside_a_try_inside_a_while_terminates_the_loop() {
    // A `return` that only ended the `try` region would leave the `while`
    // spinning forever; this test fails as a HANG if that regresses.
    let (out, ok) = run(&wrap(
        "def f(n: Int): Int = { var i = 0; while (i < 5) { try { if (i == n) return i * 100 } catch { case e: RuntimeException => println(\"c\") }; i += 1 }; -1 }\nprintln(f(2)); println(f(99))",
    ));
    assert!(ok);
    assert_eq!(out, "200\n-1\n");
}

#[test]
fn a_catch_arm_never_intercepts_a_non_local_return() {
    // The non-local return travels the same in-flight slot an exception uses,
    // so this pins that a `catch` cannot swallow it.
    let (out, ok) = run(&wrap(
        "def f(l: List[Int]): String = { try { for (x <- l) { if (x > 2) return \"y\" + x }; \"n\" } catch { case e: RuntimeException => \"caught\" } }\nprintln(f(List(1, 5))); println(f(List(1)))",
    ));
    assert!(ok);
    assert_eq!(out, "y5\nn\n");
}

#[test]
fn unit_literal_prints_as_scala_prints_it() {
    // `()` is a value, not just a statement separator: it binds, compares and
    // renders. Rendering is the part a `null` stand-in would get wrong.
    let (out, ok) = run(&wrap(
        "val u = (); println(u); println(()); println(u == ()); val f = (x: Int) => (); println(f(1))",
    ));
    assert!(ok);
    assert_eq!(out, "()\n()\ntrue\n()\n");
}

#[test]
fn type_ascription_widens_a_numeric_and_is_otherwise_transparent() {
    // The runtime is dynamically typed, so the only ascription that may change
    // an answer is the numeric widening — and it must, or `(3: Double)` prints
    // `3` where Scala prints `3.0`.
    let (out, ok) = run(&wrap(
        "println((3: Double)); println((3: Long)); println((2.5: Double) + 1); println((\"ab\": String).length); println((None: Option[Int]))",
    ));
    assert!(ok);
    assert_eq!(out, "3.0\n3\n3.5\n2\nNone\n");
}

#[test]
fn for_comprehension_value_definition_over_a_collection_and_a_range() {
    // The two lowerings differ: a range generator stores inline (keeping the
    // counted loop), a collection generator takes Scala's pairing translation.
    let (out, ok) = run(&wrap(
        "val xs = List(1, 2, 3)\nprintln(for { x <- xs; y = x * 2 } yield y)\nprintln(for { i <- 1 to 3; s = i * i } yield s)",
    ));
    assert!(ok);
    assert_eq!(out, "List(2, 4, 6)\nVector(1, 4, 9)\n");
}

#[test]
fn a_for_value_definition_is_visible_to_a_later_guard() {
    // The pairing translation exists precisely so a guard after the definition
    // can read BOTH the generator's binding and the defined name; a naive
    // `withFilter`-on-the-source lowering would not have `y` in scope.
    let (out, ok) = run(&wrap(
        "val xs = List(1, 2, 3, 4)\nprintln(for { x <- xs; y = x * 10; if y > 15 } yield (x, y))\nprintln(for { x <- xs; y = x + 1; z = y + 1 } yield x + y + z)",
    ));
    assert!(ok);
    assert_eq!(out, "List((2,20), (3,30), (4,40))\nList(6, 9, 12, 15)\n");
}

#[test]
fn string_repetition_works_infix_and_dotted() {
    // The infix form reaches the arithmetic hook and the dotted form reaches
    // method dispatch; both are `StringOps.*` and must agree.
    let (out, ok) = run(&wrap(
        "val s = \"ab\"; println(s * 3); println(s * 0); println(s * -2); println(s.*(2))",
    ));
    assert!(ok);
    assert_eq!(out, "ababab\n\n\nabab\n");
}

#[test]
fn dotted_operators_dispatch_like_their_infix_spelling() {
    // Including `/`'s Int-vs-Double split, which is decided at run time.
    let (out, ok) = run(&wrap(
        "println(1.+(2)); println(7./(2)); println(7.0./(2)); println(7.%(3)); println(3.<(4)); println(1.==(1))",
    ));
    assert!(ok);
    assert_eq!(out, "3\n3\n3.5\n1\ntrue\ntrue\n");
}

#[test]
fn string_regex_methods_match_replace_and_splice_groups() {
    let (out, ok) = run(&wrap(
        r##"println("a1b2".matches("[a-z0-9]+")); println("a1b2".matches("[a-z]+")); println("a1b2".replaceAll("[0-9]", "#")); println("a1b2".replaceFirst("[0-9]", "#")); println("2024-01-02".replaceAll("(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1"))"##,
    ));
    assert!(ok);
    assert_eq!(out, "true\nfalse\na#b#\na#b2\n02/01/2024\n");
}

#[test]
fn split_is_regex_based_with_javas_match_iteration() {
    // Three separate JDK rules, each observable only here: `split` takes a
    // REGEX (so `.` is not a literal dot), trailing empty fields are dropped,
    // and `Matcher.find` allows an empty match at the end of a non-empty one
    // (which is the field Rust's own iterator would drop from `"xx9"`).
    let (out, ok) = run(&wrap(
        r##"println("a,b,,".split(",").toList); println("a.b".split(".").toList); println("abc".split("").toList); println("xx9".split("x*").toList)"##,
    ));
    assert!(ok);
    assert_eq!(out, "List(a, b)\nList()\nList(a, b, c)\nList(, , 9)\n");
}

#[test]
fn regex_object_finds_replaces_and_exposes_match_groups() {
    let (out, ok) = run(&wrap(
        r##"val r = "([a-z])([0-9])".r; println(r); println(r.findFirstIn("xa1y")); println(r.findFirstIn("xyz")); println(r.findAllIn("a1b2").toList); println(r.replaceAllIn("a1b2", "-")); println(r.findFirstMatchIn("xa1y").map(_.group(2)))"##,
    ));
    assert!(ok);
    assert_eq!(
        out,
        "([a-z])([0-9])\nSome(a1)\nNone\nList(a1, b2)\n--\nSome(1)\n"
    );
}

#[test]
fn a_replacement_referencing_a_missing_group_throws_like_java() {
    let (out, ok) = run(&wrap(
        r##"try { println("a1".replaceAll("[0-9]", "$1")) } catch { case e: Exception => println(e.getMessage) }"##,
    ));
    assert!(ok);
    assert_eq!(out, "No group 1\n");
}

#[test]
fn string_map_picks_its_result_type_from_the_functions_static_type() {
    // `Char => Char` rebuilds a `String`; `Char => B` answers an `ArraySeq`.
    // `Char` is its own runtime type, so the results classify themselves; the
    // empty receiver is the one case with no results to read, and falls back to
    // the body's static type.
    let (out, ok) = run(&wrap(
        r##"println("abc".map(_.toUpper)); println("abc".map(_.toString)); println("abc".map(c => c + "!")); println("".map(_.toString)); println("abc".filter(_ != 'b'))"##,
    ));
    assert!(ok);
    assert_eq!(
        out,
        "ABC\nArraySeq(a, b, c)\nArraySeq(a!, b!, c!)\nArraySeq()\nac\n"
    );
}

#[test]
fn a_closure_assigning_an_enclosing_var_reaches_the_declaring_frame() {
    // Captures are threaded by value, so without boxing every one of these
    // reads back the initial value instead of the accumulated one.
    let (out, ok) = run(&wrap(
        "def f(): Int = { var c = 0; List(1, 2, 3).foreach(x => c += x); c }\ndef g(): String = { var s = \"\"; List(\"a\", \"b\").foreach(x => s = s + x); s }\ndef h(): Int = { var t = 0; List(1, 2).foreach(a => List(10, 20).foreach(b => t += a * b)); t }\nprintln(f()); println(g()); println(h())",
    ));
    assert!(ok);
    assert_eq!(out, "6\nab\n90\n");
}

#[test]
fn a_boxed_var_outlives_the_frame_that_declared_it() {
    // The cell is heap-allocated, so a closure returned from the frame keeps
    // mutating the same binding across calls.
    let (out, ok) = run(&wrap(
        "def mk(): () => Int = { var i = 0; () => { i += 1; i } }\nval c = mk(); val d = mk()\nprintln(c() + \",\" + c() + \",\" + c() + \",\" + d())",
    ));
    assert!(ok);
    assert_eq!(out, "1,2,3,1\n");
}

#[test]
fn an_if_branch_may_be_an_assignment_statement() {
    // `if (p) c += x` as a brace-less lambda body — the idiomatic conditional
    // accumulate, whose branch is a `Unit` statement rather than an expression.
    let (out, ok) = run(&wrap(
        "def f(): Int = { var c = 0; List(1, 2, 3, 4).foreach(x => if (x % 2 == 0) c += x); c }\nprintln(f())",
    ));
    assert!(ok);
    assert_eq!(out, "6\n");
}

#[test]
fn a_for_value_definition_carries_a_destructuring_generator_through() {
    // The generator's element is threaded through the pairing map unchanged and
    // re-destructured by the SAME pattern, so a tuple binder — including one
    // with a `_` or a nested tuple — keeps every name it bound.
    let (out, ok) = run(&wrap(
        "val m = Map(\"a\" -> 1, \"b\" -> 2)\nprintln(for { (k, v) <- m; d = v * 10 } yield k + d)\nprintln(for { (a, _) <- List((1, \"x\"), (2, \"y\")); t = a * 5 } yield t)\nprintln(for { (a, (b, c)) <- List((1, (2, 3))); s = a + b + c } yield s)",
    ));
    assert!(ok);
    assert_eq!(out, "List(a10, b20)\nList(5, 10)\nList(6)\n");
}

#[test]
fn a_char_dispatches_as_a_number_and_prints_as_text() {
    // The two faces of `Char`, and the distinction a one-character `String`
    // could not encode: `'5'.toInt` is the CODE POINT 53 where `"5".toInt`
    // PARSES to 5. `+` stays concatenation as soon as a `String` is involved.
    let (out, ok) = run(&wrap(
        "println('5'.toInt); println(\"5\".toInt); println('5'.asDigit)\nprintln('a' + 1); println('a' + \"b\"); println(\"a\" + 'b')",
    ));
    assert!(ok);
    assert_eq!(out, "53\n5\n5\n98\nab\nab\n");
}

#[test]
fn char_ness_survives_a_lambda_and_a_collection() {
    // No static type reaches inside these lambdas, so the element must carry
    // its own type: `_.toUpper` keeps a `String`, `_.toInt` answers code points
    // through a `List` the host built and through a `List` the user wrote.
    let (out, ok) = run(&wrap(
        "println(\"abc\".map(_.toUpper))\nprintln(\"abc\".toList.map(_.toInt))\nprintln(List('a', 'b').map(_.toInt))\nprintln(\"abc\".head.toInt)\nprintln(('a' + 2).toChar)",
    ));
    assert!(ok);
    assert_eq!(out, "ABC\nList(97, 98, 99)\nList(97, 98)\n97\nc\n");
}

#[test]
fn an_empty_string_combinator_falls_back_to_the_bodys_static_type() {
    // With no results to classify, only the syntax of the body says whether the
    // overload was `Char => Char` (a `String`) or `Char => B` (a sequence) —
    // and `flatMap` splits on the `String` side rather than the `Char` side.
    let (out, ok) = run(&wrap(
        "println(\"\".map(_.toUpper))\nprintln(\"\".map(_.toString))\nprintln(\"\".collect { case c => c })\nprintln(\"\".flatMap(c => List(c)))",
    ));
    assert!(ok);
    assert_eq!(out, "\nArraySeq()\n\nVector()\n");
}

#[test]
fn a_regex_in_pattern_position_matches_the_whole_input() {
    // `Regex.unapplySeq` is anchored: `"a1"` does NOT match `"([0-9]+)".r`
    // even though it contains a digit run. A group that did not participate
    // binds `null`.
    let (out, ok) = run(&wrap(
        "val n = \"([0-9]+)\".r\nprintln(\"a1\" match { case n(x) => x; case _ => \"none\" })\nprintln(\"123\" match { case n(x) => x; case _ => \"none\" })\nval o = \"([0-9]+)?-([0-9]+)\".r\nprintln(\"-5\" match { case o(a, b) => \"\" + a + \"|\" + b; case _ => \"no\" })",
    ));
    assert!(ok);
    assert_eq!(out, "none\n123\nnull|5\n");
}

#[test]
fn a_case_generator_filters_instead_of_raising() {
    // Scala 3's `for (case pat <- xs)`: the refutable pattern desugars to a
    // `withFilter`, so a non-matching element is SKIPPED rather than raising a
    // `MatchError`. An irrefutable tuple binder still needs no `case`.
    let (out, ok) = run(&wrap(
        "val n = \"([0-9]+)\".r\nprintln(for (case Some(x) <- List(Some(1), None, Some(3))) yield x * 2)\nprintln(for (case n(x) <- List(\"a1\", \"22\", \"zz\", \"7\")) yield x)\nprintln(for ((k, v) <- List((1, 2), (3, 4))) yield k + v)",
    ));
    assert!(ok);
    assert_eq!(out, "List(2, 6)\nList(22, 7)\nList(3, 7)\n");
}

#[test]
fn a_triple_quoted_literal_is_taken_verbatim() {
    // No escape processing, which is why a Scala regex is written this way; the
    // literal closes at the LAST of a run of quotes, so `"""a""""` is `a"`.
    let (out, ok) = run(&wrap(
        "println(\"\"\"(\\w+)@(\\w+)\"\"\")\nprintln(\"\"\"(\\d+)\"\"\".length)\nprintln(\"\"\"a\"b\"\"\")\nval p = \"\"\"(\\d+)-(\\d+)\"\"\".r\nprintln(\"1-2\" match { case p(a, b) => a + b; case _ => \"no\" })",
    ));
    assert!(ok);
    assert_eq!(out, "(\\w+)@(\\w+)\n5\na\"b\n12\n");
}

// ── parameter lists: defaults, named arguments, varargs, by-name ───────────
//
// All four are decided at the CALL site from the callee's signature. The
// by-name tests count evaluations with a mutable counter on purpose: a by-name
// parameter that is quietly passed by value still returns a plausible number,
// just one built from the wrong number of evaluations.

#[test]
fn default_parameter_values_fill_omitted_trailing_arguments() {
    let (out, ok) = run(&wrap(
        r#"def f(x: Int, y: Int = 10, z: Int = 3): String = x + ":" + y + ":" + z
           println(f(1)); println(f(1, 2)); println(f(1, 2, 5))"#,
    ));
    assert!(ok);
    assert_eq!(out, "1:10:3\n1:2:3\n1:2:5\n");
}

#[test]
fn a_default_runs_at_the_call_site_only_when_the_argument_is_omitted() {
    // Scala evaluates a default where the call is written, and not at all when
    // the caller supplies the argument — so the counter moves exactly once.
    let (out, ok) = run(&wrap(
        r#"var c = 0
           def d(): Int = { c += 1; 7 }
           def f(x: Int, y: Int = d()): Int = x + y
           println(f(1)); println(c); println(f(1, 2)); println(c)"#,
    ));
    assert!(ok);
    assert_eq!(out, "8\n1\n3\n1\n");
}

#[test]
fn named_arguments_move_to_their_parameters_position() {
    let (out, ok) = run(&wrap(
        r#"def f(x: Int, y: Int, z: Int = 3): String = x + "/" + y + "/" + z
           println(f(y = 1, x = 2)); println(f(2, y = 1)); println(f(z = 9, x = 2, y = 1))"#,
    ));
    assert!(ok);
    assert_eq!(out, "2/1/3\n2/1/3\n2/1/9\n");
}

#[test]
fn a_repeated_parameter_arrives_as_an_arrayseq() {
    // The runtime class is observable through `toString`, and Scala hands a
    // varargs method an `ArraySeq` — not a `List`.
    let (out, ok) = run(&wrap(
        r#"def f(xs: Int*): String = xs.toString + "|" + xs.length + "|" + xs.sum
           println(f()); println(f(4)); println(f(1, 2, 3))"#,
    ));
    assert!(ok);
    assert_eq!(
        out,
        "ArraySeq()|0|0\nArraySeq(4)|1|4\nArraySeq(1, 2, 3)|3|6\n"
    );
}

#[test]
fn a_by_name_parameter_is_re_evaluated_at_every_use() {
    // `x + x` reads the thunk twice, so the counter ends at 2 and the sum is
    // 1 + 2. Passing by value would answer 2 with the counter at 1.
    let (out, ok) = run(&wrap(
        r#"var c = 0
           def f(x: => Int): Int = x + x
           println(f({ c += 1; c })); println(c)"#,
    ));
    assert!(ok);
    assert_eq!(out, "3\n2\n");
}

#[test]
fn an_unused_by_name_argument_never_runs() {
    let (out, ok) = run(&wrap(
        r#"var c = 0
           def f(x: => Int, on: Boolean): Int = if (on) x else -1
           println(f({ c += 1; c }, false)); println(c)
           println(f({ c += 1; c }, true)); println(c)"#,
    ));
    assert!(ok);
    assert_eq!(out, "-1\n0\n1\n1\n");
}

#[test]
fn a_by_name_parameter_read_inside_a_lambda_still_forces() {
    // The thunk is captured by the closure, so the force has to happen inside
    // the closure body rather than at the enclosing frame's parameter bind.
    let (out, ok) = run(&wrap(
        r#"var c = 0
           def f(x: => Int): Int = List(1, 2).map(k => k * x).sum
           println(f({ c += 1; c })); println(c)"#,
    ));
    assert!(ok);
    assert_eq!(out, "5\n2\n");
}

// ── scala.util.control.Breaks ──────────────────────────────────────────────

#[test]
fn breakable_stops_a_loop_and_execution_continues_after_it() {
    let (out, ok) = run("import scala.util.control.Breaks._\n\
         object T extends App { var a = 0\n\
         breakable { for (i <- 1 to 5) { if (i == 3) break(); a += i } }\n\
         println(a); println(\"after\") }");
    assert!(ok);
    assert_eq!(out, "3\nafter\n");
}

#[test]
fn a_finally_between_break_and_breakable_still_runs() {
    let (out, ok) = run("import scala.util.control.Breaks._\n\
         object T extends App { var a = 0\n\
         breakable { for (i <- 1 to 5) { try { if (i == 3) break() } finally { a += 1 } } }\n\
         println(a) }");
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn a_catch_on_exception_does_not_swallow_a_break() {
    // Scala's `BreakControl` is a `ControlThrowable`, which hangs off
    // `Throwable` rather than `Exception` precisely so a catch-all handler
    // cannot eat a control-flow signal.
    let (out, ok) = run("import scala.util.control.Breaks._\n\
         object T extends App { var a = 0\n\
         breakable { for (i <- 1 to 4) { try { if (i == 3) break(); a += 1 } \
         catch { case e: Exception => println(\"swallowed\") } } }\n\
         println(a) }");
    assert!(ok);
    assert_eq!(out, "2\n");
}

#[test]
fn a_break_raised_inside_a_def_unwinds_to_the_enclosing_breakable() {
    let (out, ok) = run("import scala.util.control.Breaks._\n\
         object T extends App { def step(i: Int): Int = { if (i == 3) break(); i }\n\
         var a = 0\n\
         breakable { for (i <- 1 to 5) a += step(i) }\n\
         println(a) }");
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn nested_breakables_bind_a_break_to_the_innermost_one() {
    let (out, ok) = run("import scala.util.control.Breaks._\n\
         object T extends App { var a = 0\n\
         breakable { for (i <- 1 to 3) { breakable { for (j <- 1 to 4) \
         { if (j == 3) break(); a += 1 } }; a += 100 } }\n\
         println(a) }");
    assert!(ok);
    assert_eq!(out, "306\n");
}

// ── java.util.Formatter conversions ────────────────────────────────────────

#[test]
fn float_conversions_round_half_up_like_java_not_half_to_even() {
    // Rust's `{:.*}` rounds half-to-even and would answer 0.12 / 2 / 0.1 here.
    let (out, ok) = run(&wrap(
        r#"println(f"${0.125}%.2f"); println(f"${2.5}%.0f"); println(f"${0.375}%.2f")"#,
    ));
    assert!(ok);
    assert_eq!(out, "0.13\n3\n0.38\n");
}

#[test]
fn float_conversions_round_the_shortest_decimal_not_the_exact_binary_value() {
    // The stored double for 1.005 is 1.00499999999999989…, so rounding the
    // exact expansion answers 1.00. Java rounds the digits `Double.toString`
    // would print, which are "1.005", and so answers 1.01.
    let (out, ok) = run(&wrap(
        r#"println(f"${1.005}%.2f"); println(f"${0.15}%.1f")"#,
    ));
    assert!(ok);
    assert_eq!(out, "1.01\n0.2\n");
}

#[test]
fn scientific_notation_signs_the_exponent_and_pads_it_to_two_digits() {
    let (out, ok) = run(&wrap(
        r#"println(f"${1.0}%e"); println(f"${1234.5}%.3e"); println(f"${0.0001}%.2E")"#,
    ));
    assert!(ok);
    assert_eq!(out, "1.000000e+00\n1.235e+03\n1.00E-04\n");
}

#[test]
fn negative_zero_and_the_non_finite_doubles_use_javas_spellings() {
    let (out, ok) = run(&wrap(
        r#"println(f"${-0.0}%.2f"); println(f"${1.0 / 0.0}%.2f")
           println(f"${-1.0 / 0.0}%.1E"); println(f"${0.0 / 0.0}%.1E")
           println(f"${1.0 / 0.0}%+.2f")"#,
    ));
    assert!(ok);
    assert_eq!(out, "-0.00\nInfinity\n-INFINITY\nNAN\n+Infinity\n");
}

#[test]
fn format_is_available_on_a_string_statically_and_as_formatted() {
    let (out, ok) = run(&wrap(
        r#"println("%s-%d".format("a", 7)); println(String.format("%05d|%.2f", 42, 3.14159))
           println(3.14159.formatted("%.3f")); println("%.1f%%".format(50.0))"#,
    ));
    assert!(ok);
    assert_eq!(out, "a-7\n00042|3.14\n3.142\n50.0%\n");
}

#[test]
fn radix_conversions_use_int_width_for_a_negative_value() {
    // Java prints the two's-complement pattern at the operand's type width;
    // every value here fits in `Int`, so that width is 32 bits.
    let (out, ok) = run(&wrap(
        r#"println("%x".format(-1)); println("%X".format(-42)); println("%o".format(-7))
           println("%x".format(255)); println("%08x".format(-2))"#,
    ));
    assert!(ok);
    assert_eq!(out, "ffffffff\nFFFFFFD6\n37777777771\nff\nfffffffe\n");
}

#[test]
fn padto_segmentlength_and_lastindexwhere_on_a_sequence() {
    let (out, ok) = run(&wrap(
        r#"println(List(1, 2, 3).padTo(5, 9)); println(List(1, 2, 3).padTo(2, 9))
           println(List(1, 2, 5, 3).segmentLength(_ < 5)); println(List(1, 4, 2, 4).lastIndexWhere(_ == 4))"#,
    ));
    assert!(ok);
    assert_eq!(out, "List(1, 2, 3, 9, 9)\nList(1, 2, 3)\n2\n3\n");
}

// ── apply: `receiver(args)` in every position (byte-verified vs scala 3.8.4) ──

#[test]
fn a_binding_is_applied_from_inside_a_lambda_and_a_def() {
    // The receiver is named where it is neither a frame slot nor a capture — the
    // shape that told a top-level binding apart from an undefined function.
    let (out, ok) = run(&wrap(
        r#"val xs = List(10, 20, 30)
           val ar = Array(4, 5, 6)
           val m = Map("a" -> 1, "b" -> 2)
           println(List(0, 2).map(i => xs(i)))
           println(List(0, 2).map(i => ar(i)).sum)
           println(List("a", "b").map(k => m(k)))
           def at(i: Int) = xs(i)
           println(at(1))
           val inc = (x: Int) => x + 1
           println(List(1, 2).map(i => inc(i)))"#,
    ));
    assert!(ok);
    assert_eq!(out, "List(10, 30)\n10\nList(1, 2)\n20\nList(2, 3)\n");
}

#[test]
fn a_string_binding_indexes_like_a_string_literal() {
    let (out, ok) = run(&wrap(
        r#"val s = "pear"
           println(s(0))
           println(s(3).toUpper)
           println((0 until s.length).map(i => s(i)).mkString("-"))
           def initial(w: String) = w(0)
           println(initial("kiwi"))"#,
    ));
    assert!(ok);
    assert_eq!(out, "p\nR\np-e-a-r\nk\n");
}

#[test]
fn a_bare_placeholder_argument_eta_expands_the_enclosing_call() {
    // `f(_)` is `x => f(x)`. Expanding it at the argument instead would pass `f`
    // the identity function, which for a receiver that accepts any value is a
    // wrong answer rather than an error (`m(_)` looked the function up as a key).
    let (out, ok) = run(&wrap(
        r#"def dbl(x: Int) = x * 2
           def add(x: Int, y: Int) = x + y
           val m = Map("a" -> 1, "b" -> 2)
           val s = "abcd"
           println(List(1, 2).map(dbl(_)))
           println(List("a", "b").map(m(_)))
           println(List(1, 2).map(add(_, 10)))
           println(List(0, 2).map(s(_)))
           println(List(1, 2, 3).map(_ * 2))"#,
    ));
    assert!(ok);
    assert_eq!(
        out,
        "List(2, 4)\nList(1, 2)\nList(11, 12)\nList(a, c)\nList(2, 4, 6)\n"
    );
}

#[test]
fn a_placeholder_can_itself_be_applied() {
    let (out, ok) = run(&wrap(
        r#"println(List(List(1, 2), List(3, 4)).map(_(1)))
           println(List("kiwi", "pear").map(_(1)))"#,
    ));
    assert!(ok);
    assert_eq!(out, "List(2, 4)\nList(i, e)\n");
}

#[test]
fn a_selector_chain_continues_after_an_apply_on_a_literal() {
    let (out, ok) = run(&wrap(
        r#"println("pear"(1).toUpper)
           println("pear"(1).toInt)
           println("pear"(0).toString.length)"#,
    ));
    assert!(ok);
    assert_eq!(out, "E\n101\n1\n");
}

#[test]
fn applying_a_field_reads_the_field_and_applies_that() {
    // `r.name(0)` is `r.name.apply(0)` — a field of the record, not a method of
    // its class. An unknown name stays an error.
    let (out, ok) = run(
        r#"case class Row(name: String, cells: List[Int], f: Int => Int)
           object T extends App {
             val r = Row("melon", List(7, 8, 9), (x: Int) => x * 3)
             println(r.name(0))
             println(r.cells(2))
             println(r.f(5))
             println(r.name.length)
           }"#,
    );
    assert!(ok);
    assert_eq!(out, "m\n9\n15\n5\n");

    let (_, bad) = run(r#"case class Row(name: String)
           object T extends App { println(Row("x").nope(1)) }"#);
    assert!(!bad, "an unknown member must still be rejected");
}

#[test]
fn a_char_range_is_refused_rather_than_read_as_integers() {
    // `'a' to 'e'` is a `NumericRange[Char]`. Reading the endpoints as integers
    // answered `List(0)` — a silent wrong answer, which this frontend does not
    // ship. Every spelling is refused, including endpoints held in bindings.
    for src in [
        "println(('a' to 'e').toList)",
        "println(('a' until 'e').size)",
        "for (c <- 'a' to 'd') print(c)",
        "val a = 'a'; val z = 'e'; println((a to z).toList)",
    ] {
        let (out, err, ok) = run_full(&wrap(src));
        assert!(!ok, "a Char range must be refused: {src}");
        assert_eq!(out, "", "a refused Char range must print nothing: {src}");
        // Without this, a parse error anywhere in `src` would pass the test.
        assert!(
            err.contains("a Char range"),
            "refused for the wrong reason on {src}: {err:?}"
        );
    }
    // The integer range it would be confused with is untouched.
    let (out, ok) = run(&wrap("println((1 to 4).toList); println((1 until 4).size)"));
    assert!(ok);
    assert_eq!(out, "List(1, 2, 3, 4)\n3\n");
}

// ── Int is 32 bits ──────────────────────────────────────────────────────────
// Scala's `Int` wraps at 32 bits while this frontend computes in `i64`, so every
// expected value below was diffed against `scala-cli` (Scala 3.8.4) rather than
// reasoned about. The `Long` cases are as load-bearing as the `Int` ones: they
// are what fails if narrowing is applied unconditionally instead of by width.

#[test]
fn int_addition_wraps_at_thirty_two_bits() {
    let (out, _) = run(&wrap(
        "println(2147483647 + 1); println(-2147483648 - 1); println(2147483647 * 2)",
    ));
    assert_eq!(out, "-2147483648\n2147483647\n-2\n");
}

#[test]
fn int_multiplication_wraps_to_zero_at_two_to_the_thirty_two() {
    let (out, _) = run(&wrap("println(65536 * 65536)"));
    assert_eq!(out, "0\n");
}

#[test]
fn negating_int_min_value_answers_itself() {
    // The one unary operator that overflows: `Int.MinValue` has no positive.
    let (out, _) = run(&wrap("println(-(-2147483648)); println((-2147483648).abs)"));
    assert_eq!(out, "-2147483648\n-2147483648\n");
}

#[test]
fn int_companion_bounds_are_available_and_wrap() {
    let (out, _) = run(&wrap(
        "println(Int.MaxValue); println(Int.MinValue); println(Int.MaxValue + 1)",
    ));
    assert_eq!(out, "2147483647\n-2147483648\n-2147483648\n");
}

#[test]
fn long_arithmetic_does_not_wrap_at_thirty_two_bits() {
    // A `Long` anywhere in the expression promotes the whole thing, so none of
    // these narrow — including the two mixed forms.
    let (out, _) = run(&wrap(
        "println(2147483647L + 1L); println(2147483647L + 1); println(2147483647 + 1L)",
    ));
    assert_eq!(out, "2147483648\n2147483648\n2147483648\n");
}

#[test]
fn declared_long_binding_keeps_full_width() {
    let (out, _) = run(&wrap(
        "val big: Long = 2147483647L; println(big + 1); val small = 2147483647; println(small + 1)",
    ));
    assert_eq!(out, "2147483648\n-2147483648\n");
}

#[test]
fn def_parameter_types_decide_the_width() {
    // Scala requires a type on every `def` parameter, which is what makes the
    // width knowable inside a function body.
    let src = "object T { def bump(n: Int): Int = n + 1\n\
               def bumpL(n: Long): Long = n + 1\n\
               def main(a: Array[String]): Unit = { println(bump(2147483647)); println(bumpL(2147483647L)) } }";
    let (out, _) = run(src);
    assert_eq!(out, "-2147483648\n2147483648\n");
}

#[test]
fn compound_assignment_wraps() {
    let (out, _) = run(&wrap(
        "var x = 2147483647; x += 1; println(x); var m = 2147483647; m *= 2; println(m)",
    ));
    assert_eq!(out, "-2147483648\n-2\n");
}

#[test]
fn int_min_value_divided_by_negative_one_overflows_to_itself() {
    let (out, _) = run(&wrap(
        "println(-2147483648 / -1); println(-2147483648 % -1); println(2147483647 / -1)",
    ));
    assert_eq!(out, "-2147483648\n0\n-2147483647\n");
}

#[test]
fn to_int_truncates_and_double_to_int_saturates() {
    // `Long.toInt` keeps the low 32 bits; `Double.toInt` is the JVM's `d2i`,
    // which CLAMPS instead of truncating.
    let (out, _) = run(&wrap(
        "println(5000000000L.toInt); println(3000000000.0.toInt); println((-3000000000.0).toInt)",
    ));
    assert_eq!(out, "705032704\n2147483647\n-2147483648\n");
}

#[test]
fn shifts_use_the_receivers_width() {
    // `1 << 40` masks the distance to five bits, `1L << 40` to six.
    let (out, _) = run(&wrap(
        "println(1 << 40); println(1L << 40); println(1 << 31); println(-1 >>> 1)",
    ));
    assert_eq!(out, "256\n1099511627776\n-2147483648\n2147483647\n");
}

#[test]
fn long_arithmetic_wraps_at_sixty_four_bits() {
    // Scala has no bignum in the primitive tower — `Long` overflow wraps rather
    // than widening or failing.
    let (out, _) = run(&wrap(
        "println(4294967296L * 4294967296L); println(9223372036854775807L + 1L)",
    ));
    assert_eq!(out, "0\n-9223372036854775808\n");
}

#[test]
fn long_min_value_literal_parses() {
    let (out, _) = run(&wrap(
        "println(-9223372036854775808L); println(Long.MinValue); println(Long.MaxValue)",
    ));
    assert_eq!(
        out,
        "-9223372036854775808\n-9223372036854775808\n9223372036854775807\n"
    );
}

#[test]
fn narrowing_leaves_doubles_and_strings_alone() {
    // The wrap is a shift pair, which would destroy a `Double` or a `String`, so
    // it must fire only where the result is provably an `Int`.
    let (out, _) = run(&wrap(
        r#"println(1.5 + 2.0); println("a" + 1); println(2147483647 + 1.0); println('a' + 1)"#,
    ));
    assert_eq!(out, "3.5\na1\n2.147483648E9\n98\n");
}

#[test]
fn a_hot_loop_wraps_the_same_way_the_interpreter_does() {
    // Two hundred thousand iterations is well past the tracing JIT's threshold,
    // so this pins the compiled tier to the interpreter's answer.
    let (out, _) = run(&wrap(
        "var s = 0; for (k <- 1 to 200000) { s = s + 100000 }; println(s)",
    ));
    assert_eq!(out, "-1474836480\n");
}

#[test]
fn a_captured_accumulator_still_wraps_inside_a_lambda() {
    // A lambda body compiles in a fresh scope; the enclosing widths have to
    // travel with it or `t` loses its `Int` width the moment a closure writes it.
    let (out, _) = run(&wrap(
        "var t = 0; List(1,2,3).foreach(x => t += 2147483647); println(t)",
    ));
    assert_eq!(out, "2147483645\n");
}

#[test]
fn math_abs_of_int_min_value_stays_negative() {
    let (out, _) = run(&wrap(
        "println(math.abs(-2147483648)); println(java.lang.Math.abs(-2147483648))",
    ));
    assert_eq!(out, "-2147483648\n-2147483648\n");
}

// ── Unit is `()`, null is `null` ────────────────────────────────────────────

#[test]
fn a_unit_returning_def_renders_as_the_unit_literal() {
    // `Unit` and `null` are different values with different renderings, so they
    // cannot share one representation.
    let src = "object T { def u(): Unit = {}\n\
               def s(): Unit = println(\"s\")\n\
               def main(a: Array[String]): Unit = { println(u()); println(s()); val v = u(); println(v) } }";
    let (out, _) = run(src);
    assert_eq!(out, "()\ns\n()\n()\n");
}

#[test]
fn unit_from_statements_and_empty_branches() {
    let (out, _) = run(&wrap(
        "println(if (false) 1); println({ }); println(List(1).foreach(x => x)); println(for (i <- 1 to 2) { })",
    ));
    assert_eq!(out, "()\n()\n()\n()\n");
}

#[test]
fn null_still_renders_as_null_and_compares_as_null() {
    let (out, _) = run(&wrap(
        "println(null); val n: String = null; println(n); println(n == null); println(List(null, \"a\"))",
    ));
    assert_eq!(out, "null\nnull\ntrue\nList(null, a)\n");
}

#[test]
fn unit_values_compare_equal() {
    let src = "object T { def u(): Unit = {}\n\
               def main(a: Array[String]): Unit = { println(() == ()); println(u() == ()) } }";
    let (out, _) = run(src);
    assert_eq!(out, "true\ntrue\n");
}

// ── scala.math.Ordering ─────────────────────────────────────────────────────

#[test]
fn sorted_takes_an_explicit_ordering() {
    let (out, _) = run(&wrap(
        "println(List(3,1,2).sorted(Ordering.Int.reverse)); println(List(3,1,2).sorted(Ordering.Int)); println(List(\"b\",\"a\",\"c\").sorted(Ordering.String.reverse))",
    ));
    assert_eq!(out, "List(3, 2, 1)\nList(1, 2, 3)\nList(c, b, a)\n");
}

#[test]
fn an_ordering_reverses_max_min_and_compare() {
    let (out, _) = run(&wrap(
        "println(List(3,1,2).max(Ordering.Int.reverse)); println(List(3,1,2).min(Ordering.Int.reverse)); println(Ordering.Int.compare(1,2)); println(Ordering.Int.reverse.compare(1,2))",
    ));
    assert_eq!(out, "1\n3\n-1\n1\n");
}

#[test]
fn reversing_an_ordering_twice_restores_it() {
    let (out, _) = run(&wrap(
        "println(List(1,2).sorted(Ordering.Int.reverse.reverse)); println(Ordering.Int.lt(1,2)); println(Ordering.Int.max(3,5))",
    ));
    assert_eq!(out, "List(1, 2)\ntrue\n5\n");
}

// ── Widths Scala infers from context ────────────────────────────────────────
// `Int` is 32 bits everywhere, including the positions where the width is never
// written on the expression itself: it comes from the collection a lambda
// parameter traverses, from a class's field declarations, from a `def`'s return
// annotation, or from a collection's element type. Every expectation below was
// diffed against `scala-cli` (Scala 3.8.4). The cases that must NOT wrap carry
// as much weight as the ones that must: they are what fails if a width is
// claimed for an expression whose type was never proven.

#[test]
fn a_lambda_parameter_takes_the_element_width_of_what_it_traverses() {
    let (out, _) = run(&wrap(
        "println(List(2147483647, 2).map(x => x * 2).mkString(\",\"))",
    ));
    assert_eq!(out, "-2,4\n");
}

#[test]
fn a_placeholder_lambda_narrows_the_same_way_a_named_one_does() {
    let (out, _) = run(&wrap(
        "println(List(2147483647, 2).map(_ * 2).mkString(\",\"))",
    ));
    assert_eq!(out, "-2,4\n");
}

#[test]
fn a_lambda_over_a_long_collection_keeps_sixty_four_bits() {
    let (out, _) = run(&wrap(
        "println(List(2147483647L, 2L).map(x => x * 2).mkString(\",\"))",
    ));
    assert_eq!(out, "4294967294,4\n");
}

#[test]
fn a_lambda_over_a_non_integer_collection_is_left_alone() {
    // The shift pair would truncate a `Double` and mangle a `String`, so an
    // element type that is neither `Int` nor `Long` must narrow nothing.
    let (out, _) = run(&wrap(
        "println(List(1.5).map(d => d * 2.0).mkString(\",\") + \"|\" + List(\"a\").map(s => s + s).mkString(\",\"))",
    ));
    assert_eq!(out, "3.0|aa\n");
}

#[test]
fn nested_lambdas_each_take_their_own_element_width() {
    // The inner traversal is over a `List[Long]` and the outer over a
    // `List[Int]`; neither may inherit the other's width.
    let (out, _) = run(&wrap(
        "println(List(2147483647).map(x => List(2L).map(y => y * 2).sum + x * 2).mkString(\",\"))",
    ));
    assert_eq!(out, "2\n");
}

#[test]
fn reduce_binds_both_of_its_parameters_to_the_element_width() {
    let (out, _) = run(&wrap(
        "println(List(2147483647, 2147483647).reduce((a, b) => a + b))",
    ));
    assert_eq!(out, "-2\n");
}

#[test]
fn a_declared_collection_parameter_types_the_lambda_inside_the_def() {
    let (out, _) = run(&wrap(
        "def viaSig(zs: List[Int]): Int = zs.map(z => z * 2).head; println(viaSig(List(2147483647)))",
    ));
    assert_eq!(out, "-2\n");
}

#[test]
fn a_constructor_field_carries_its_declared_width_to_a_use_site() {
    let (out, _) = run(
        "class Counter(val step: Int, val total: Long)\n\
         object T extends App { val c = new Counter(2147483647, 2147483647L); println(c.step * 2); println(c.total * 2) }",
    );
    assert_eq!(out, "-2\n4294967294\n");
}

#[test]
fn a_bare_field_reference_inside_a_method_narrows() {
    let (out, _) = run(
        "class Counter(val step: Int) { val doubled: Int = step * 2; def bump: Int = step * 2147483647 }\n\
         object T extends App { val c = new Counter(2147483647); println(c.bump); println(c.doubled) }",
    );
    assert_eq!(out, "1\n-2\n");
}

#[test]
fn an_inherited_field_carries_its_width_into_the_subclass() {
    let (out, _) = run("class Base(val seed: Int)\n\
         class Derived(s: Int) extends Base(s) { def grow: Int = seed * 1000000 }\n\
         object T extends App { println(new Derived(2147483647).grow) }");
    assert_eq!(out, "-1000000\n");
}

#[test]
fn a_user_field_named_like_a_stdlib_int_method_keeps_its_declared_width() {
    // `size` and `length` are `Int` on every stdlib receiver, and were typed
    // `Int` here on ANY receiver — so a `case class` field of that name had its
    // `Long` value wrapped to 32 bits. A name-based rule cannot outrank a
    // declaration.
    let (out, _) = run(
        "case class FileRec(name: String, size: Long)\n\
         case class Sample(name: String, length: Long)\n\
         object T extends App { println(FileRec(\"a\", 10000000000L).size * 2); println(Sample(\"a\", 10000000000L).length + 1) }",
    );
    assert_eq!(out, "20000000000\n10000000001\n");
}

#[test]
fn a_def_return_annotation_types_its_call_site() {
    let (out, _) = run(&wrap(
        "def mk(): Int = 2147483647; def mkL(): Long = 2147483647L; println(mk() * 2); println(mkL() * 2)",
    ));
    assert_eq!(out, "-2\n4294967294\n");
}

#[test]
fn sum_and_product_wrap_over_an_int_collection() {
    let (out, _) = run(&wrap(
        "println(List(2147483647, 2147483647).sum); println(List(2147483647, 2).product)",
    ));
    assert_eq!(out, "-2\n-2\n");
}

#[test]
fn summing_a_range_wraps_at_thirty_two_bits() {
    // Every element is small; the ACCUMULATOR is what overflows, which is why
    // `sum` has to narrow even when no operand is near the boundary.
    let (out, _) = run(&wrap("println((1 to 100000).sum)"));
    assert_eq!(out, "705082704\n");
}

#[test]
fn summing_a_long_or_double_collection_does_not_wrap() {
    let (out, _) = run(&wrap(
        "println(List(2147483647L, 2147483647L).sum); println(List(1.5, 2.5).sum)",
    ));
    assert_eq!(out, "4294967294\n4.0\n");
}

#[test]
fn a_long_accumulator_wraps_rather_than_raising() {
    // The 64-bit counterpart of `sum_and_product_wrap_over_an_int_collection`.
    // Folding with `Iterator::sum` made this depend on the build profile —
    // checked `+` raised in debug, and only a release build wrapped into
    // agreement — so `ok` is asserted alongside the values.
    let (out, ok) = run(&wrap(
        "println(List(9223372036854775807L, 1L).sum)\n\
         println(List(-9223372036854775808L, -1L).sum)\n\
         println(List(9223372036854775807L, 2L).product)",
    ));
    assert!(ok, "a wrapping accumulator must not raise: {out}");
    assert_eq!(out, "-9223372036854775808\n9223372036854775807\n-2\n");
}

#[test]
fn an_element_width_survives_a_filter_but_an_empty_literal_has_none() {
    let (out, _) = run(&wrap(
        "println(List(2147483647, 2147483647).filter(x => x > 0).sum); println(List[Int]().sum)",
    ));
    assert_eq!(out, "-2\n0\n");
}

#[test]
fn a_declared_long_collection_keeps_sixty_four_bits_through_a_lambda() {
    let (out, _) = run(&wrap(
        "val big: List[Long] = List(2147483647L, 2147483647L); println(big.map(x => x * 2).sum)",
    ));
    assert_eq!(out, "8589934588\n");
}

#[test]
fn a_user_member_name_does_not_disarm_narrowing_on_a_primitive() {
    // A user declaration outranks a stdlib name-based width, but only on a
    // receiver that could be that user type. A receiver already known to be a
    // primitive is not, so one `def abs` anywhere in the program must not stop
    // `Int.MinValue.abs` from wrapping.
    let (out, _) = run("class Thing { def abs: Int = 5 }\n\
         object T extends App { println((-2147483648).abs); println(\"abcd\".length * 2) }");
    assert_eq!(out, "-2147483648\n8\n");
}

// ── scala.collection.mutable: Queue / Stack / ArrayDeque / LinkedHash* /
//    StringBuilder ───────────────────────────────────────────────────────────

#[test]
fn queue_stack_and_arraydeque_mutate_at_the_ends_scala_does() {
    // The three buffers differ only in WHICH end each operation touches, and
    // that is the whole point of having them: `Queue.enqueue` appends while
    // `Stack.push` prepends (a `Stack`'s head is its top), yet `+=` is
    // `Growable.addOne` on both and therefore appends on both — so
    // `Stack(1,2,3) += 8` is `Stack(1, 2, 3, 8)` where `push(8)` would have been
    // `Stack(8, 1, 2, 3)`. Getting that backwards is the mistake this pins.
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  val q = mutable.Queue(1, 2, 3)\n  q.enqueue(4); println(q); println(q.dequeue()); println(q.front); println(q)\n  q += 9; println(q); println(q.map(_ * 2))\n  val s = mutable.Stack(1, 2, 3)\n  s.push(4); println(s); println(s.pop()); println(s.top)\n  s += 8; println(s)\n  val d = mutable.ArrayDeque(1, 2, 3)\n  d.prepend(0); d += 4; println(d)\n  println(d.removeHead()); println(d.removeLast()); println(d)\n  d(1) = 9; println(d)\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "Queue(1, 2, 3, 4)\n1\n2\nQueue(2, 3, 4)\nQueue(2, 3, 4, 9)\nQueue(4, 6, 8, 18)\nStack(4, 1, 2, 3)\n4\n1\nStack(1, 2, 3, 8)\nArrayDeque(0, 1, 2, 3, 4)\n0\n4\nArrayDeque(1, 2, 3)\nArrayDeque(1, 9, 3)\n"
    );
}

#[test]
fn a_removal_from_an_empty_buffer_raises_the_jdk_message() {
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  try { mutable.Queue[Int]().dequeue() } catch { case e: Throwable => println(e) }\n  try { mutable.Stack[Int]().pop() } catch { case e: Throwable => println(e) }\n  try { mutable.Stack[Int]().top } catch { case e: Throwable => println(e) }\n  try { mutable.Queue[Int]().front } catch { case e: Throwable => println(e) }\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "java.util.NoSuchElementException: empty collection\njava.util.NoSuchElementException: empty collection\njava.util.NoSuchElementException: head of empty Stack\njava.util.NoSuchElementException: head of empty Queue\n"
    );
}

#[test]
fn linked_hash_collections_keep_insertion_order_not_table_order() {
    // The whole reason `LinkedHashSet`/`LinkedHashMap` are not aliases for the
    // table-ordered ones: `LinkedHashSet(3, 1, 2)` prints in the order it was
    // given, a re-added element keeps its ORIGINAL position, and an element
    // removed and re-added moves to the END. A `mutable.HashSet` of the same
    // elements would print in bucket order instead.
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  val s = mutable.LinkedHashSet(3, 1, 2)\n  s += 9; s += 1; println(s); println(s.toList); println(s.map(_ * 2))\n  s -= 1; println(s); s += 1; println(s)\n  val m = mutable.LinkedHashMap(3 -> \"c\", 1 -> \"a\")\n  m(2) = \"b\"; println(m); println(m.toList)\n  m -= 3; println(m); println(m.filter(_._1 > 1))\n  println(mutable.LinkedHashSet[Int]()); println(mutable.LinkedHashMap[Int, String]())\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "LinkedHashSet(3, 1, 2, 9)\nList(3, 1, 2, 9)\nLinkedHashSet(6, 2, 4, 18)\nLinkedHashSet(3, 2, 9)\nLinkedHashSet(3, 2, 9, 1)\nLinkedHashMap(3 -> c, 1 -> a, 2 -> b)\nList((3,c), (1,a), (2,b))\nLinkedHashMap(1 -> a, 2 -> b)\nLinkedHashMap(2 -> b)\nLinkedHashSet()\nLinkedHashMap()\n"
    );
}

#[test]
fn stringbuilder_is_a_char_sequence_that_prints_its_contents() {
    // `StringBuilder` wears two faces at once and the test pins both: it is a
    // `Seq[Char]` (`length`, `apply`, `take`, `mkString`) AND a `CharSequence`
    // (`substring`, `indexOf`), its `toString` is the CONTENTS with no
    // `StringBuilder(…)` wrapper, `append` takes `String.valueOf` of anything
    // (so `append(7)` appends the digit), and `map` — whose element type can
    // change — falls back to `mutable.IndexedSeq`'s builder and so answers an
    // `ArrayBuffer` where the selecting `take` answers a `StringBuilder`.
    let (out, ok) = run(
        "object T extends App {\n  val b = new StringBuilder\n  b.append(\"ab\"); b += 'c'; b ++= \"de\"; b.append(7)\n  println(b); println(b.length); println(b(0)); println(b.reverse)\n  println(b.take(2)); println(b.map(_.toUpper)); println(b.indexOf(\"cd\"))\n  println(b.substring(1, 3)); println(b.mkString(\"-\")); println(b.result())\n  val c = new StringBuilder(\"abc\")\n  println(c.insert(1, \"ZZ\")); println(c.setCharAt(0, 'Q')); println(c.deleteCharAt(1))\n  c.setLength(2); println(c); println(c.isEmpty)\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "abcde7\n6\na\n7edcba\nab\nArrayBuffer(A, B, C, D, E, 7)\n2\nbc\na-b-c-d-e-7\nabcde7\naZZbc\nQZZbc\nQZbc\nQZ\nfalse\n"
    );
}

#[test]
fn a_user_type_shadows_the_built_in_collection_of_the_same_name() {
    // The parser recognizes these constructors by NAME, so a program with its
    // own `Stack`/`Queue` must construct that one — not silently build the
    // library collection.
    let (out, ok) = run(
        "case class Stack(top: Int)\nclass Queue(val n: Int)\nobject T extends App {\n  println(Stack(1)); println(Stack(1).top); println(new Queue(2).n)\n}",
    );
    assert!(ok);
    assert_eq!(out, "Stack(1)\n1\n2\n");
}

// ── Boxed-primitive / `String` companion statics ───────────────────────────

#[test]
fn boxed_primitive_companions_answer_their_own_namespaces_members() {
    // `scala.Int` and `java.lang.Integer` are different objects with disjoint
    // members, and the width a rendering uses follows the box: `Integer
    // .toHexString(-1)` is 32 bits and `Long.toHexString(-1)` is 64.
    let (out, ok) = run(
        "object T extends App {\n  println(Int.MaxValue); println(Long.MinValue); println(Short.MaxValue); println(Byte.MinValue)\n  println(Double.MaxValue); println(Double.PositiveInfinity); println(Char.MaxValue.toInt)\n  println(Integer.MAX_VALUE); println(Integer.parseInt(\"42\")); println(Integer.parseInt(\"ff\", 16))\n  println(Integer.toBinaryString(-1)); println(Integer.toHexString(255)); println(Integer.toString(255, 16))\n  println(java.lang.Long.toHexString(-1L)); println(Integer.compare(1, 2)); println(Integer.bitCount(7))\n  println(java.lang.Double.parseDouble(\"1.5\")); println(Character.isDigit('5')); println(Character.toUpperCase('a'))\n  println(Character.getNumericValue('7')); println(String.valueOf(42)); println(String.valueOf(List(1, 2)))\n  try { Integer.parseInt(\"zz\") } catch { case e: Throwable => println(e) }\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "2147483647\n-9223372036854775808\n32767\n-128\n1.7976931348623157E308\nInfinity\n65535\n2147483647\n42\n255\n11111111111111111111111111111111\nff\nff\nffffffffffffffff\n-1\n3\n1.5\ntrue\nA\n7\n42\nList(1, 2)\njava.lang.NumberFormatException: For input string: \"zz\"\n"
    );
}

#[test]
fn a_member_of_the_other_boxed_namespace_is_rejected() {
    // `Double.parseDouble` really is "not a member of object Double" in Scala —
    // the split between the two namespaces is faithful, not a superset.
    let (out, err, ok) = run_full(&wrap(r#"println(Double.parseDouble("1.5"))"#));
    assert!(!ok);
    assert_eq!(out, "");
    assert!(
        err.contains("value parseDouble is not a member of Double"),
        "{err:?}"
    );
}

// ── `getClass` ─────────────────────────────────────────────────────────────

#[test]
fn getclass_names_the_receivers_class() {
    // `Class.toString` is `class <name>` for a reference type and the bare name
    // for a primitive, and an exception's is the fully-qualified JDK name —
    // which is the reason to model `getClass` at all.
    let (out, ok) = run(
        "case class P(n: String, a: Int)\nclass Q\nobject T extends App {\n  println(\"x\".getClass); println(\"x\".getClass.getName); println(\"x\".getClass.getSimpleName)\n  println(1.getClass); println(1.5.getClass); println(true.getClass); println('c'.getClass)\n  println(P(\"x\", 1).getClass); println(P(\"x\", 1).getClass.getSimpleName); println(new Q().getClass)\n  try { 1 / 0 } catch { case e: Throwable => println(e.getClass.getName); println(e.getClass.getSimpleName) }\n  try { \"z\".toInt } catch { case e: Throwable => println(e.getClass.getSimpleName) }\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "class java.lang.String\njava.lang.String\nString\nint\ndouble\nboolean\nchar\nclass P\nP\nclass Q\njava.lang.ArithmeticException\nArithmeticException\nNumberFormatException\n"
    );
}

#[test]
fn getclass_on_a_collection_is_an_error_rather_than_a_guess() {
    // A collection's runtime class is a private implementation detail
    // (`$colon$colon`, `Vector1`, `Tuple2$mcII$sp`), so it fails loudly instead
    // of answering something plausible and wrong.
    let (out, err, ok) = run_full(&wrap("println(List(1).getClass.getName)"));
    assert!(!ok);
    assert_eq!(out, "");
    assert!(err.contains("value getClass is not a member"), "{err:?}");
}

// ── An explicit `Ordering` ─────────────────────────────────────────────────

#[test]
fn an_explicit_ordering_drives_sorted_max_min_and_sortby() {
    // Scala passes the `Ordering` in a second (implicit) parameter list, which
    // this frontend flattens into the same call — so `xs.sorted(ord)` arrives
    // with one argument and `xs.sortBy(f)(ord)` with two, and both must find it.
    let (out, ok) = run(
        "case class P(n: String, a: Int)\nobject T extends App {\n  val xs = List(3, 1, 2)\n  println(xs.sorted(Ordering.Int.reverse)); println(xs.sorted(Ordering.Int))\n  println(List(\"b\", \"a\").sorted(Ordering.String.reverse))\n  println(xs.sortBy(x => x)(Ordering.Int.reverse))\n  println(Ordering.Int.compare(1, 2)); println(Ordering.Int.reverse.compare(1, 2))\n  println(xs.max(Ordering.Int.reverse)); println(xs.min(Ordering.Int.reverse))\n  println(xs.sorted(Ordering.by((x: Int) => -x)))\n  println(Ordering[Int].compare(2, 1)); println(Ordering.Int.lt(1, 2))\n  println(List(P(\"x\", 2), P(\"y\", 1)).sortBy(_.a)(Ordering.Int.reverse))\n  println(xs.sorted(Ordering.fromLessThan[Int](_ > _)))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "List(3, 2, 1)\nList(1, 2, 3)\nList(b, a)\nList(3, 2, 1)\n-1\n1\n1\n3\nList(3, 2, 1)\n1\ntrue\nList(P(x,2), P(y,1))\nList(3, 2, 1)\n"
    );
}

// ── Varargs spread, and an apply on a literal ──────────────────────────────

#[test]
fn a_varargs_spread_hands_the_sequence_to_the_repeated_parameter() {
    // `f(xs: _*)` must pass `xs` ITSELF, not an `ArraySeq` holding it — the
    // difference shows up immediately in `xs.sum` and in the empty case.
    let (out, ok) = run(
        "object T extends App {\n  def f(xs: Int*) = xs.sum\n  def g(a: Int, xs: String*) = a + xs.mkString\n  def h(xs: Int*) = xs\n  println(f(List(1, 2, 3): _*)); println(f(1, 2, 3)); println(f(List(): _*))\n  println(g(1, List(\"a\", \"b\"): _*)); println(h(Vector(1, 2): _*))\n}",
    );
    assert!(ok);
    assert_eq!(out, "6\n6\n0\n1ab\nVector(1, 2)\n");
}

#[test]
fn a_spread_outside_a_repeated_parameter_is_rejected() {
    // Scala: "Sequence argument type annotation `*` cannot be used here: the
    // corresponding parameter has type Int which is not a repeated parameter
    // type." Both sides refuse to compile it.
    let (out, err, ok) = run_full(&wrap("def k(a: Int) = a; println(k(List(1): _*))"));
    assert!(!ok);
    assert_eq!(out, "");
    assert!(
        err.contains("a `: _*` argument is only allowed for a repeated parameter"),
        "{err:?}"
    );
}

#[test]
fn an_apply_on_a_literal_continues_the_selector_chain() {
    // Selection and application ALTERNATE, so `"abc"(1).toInt` is an apply and
    // then a selection. A `mutable` FACTORY is also not curried, so its second
    // group is an index rather than three more elements.
    let (out, ok) = run(
        "object T extends App {\n  println(\"abc\"(1).toInt); println(List(1, 2, 3)(1).toString)\n  println(scala.collection.mutable.ArrayBuffer(1, 2, 3)(1))\n  println(List(List(1, 2))(0)(1))\n}",
    );
    assert!(ok);
    assert_eq!(out, "98\n2\n2\n2\n");
}

#[test]
fn a_collection_factory_takes_a_spread_too() {
    // Every collection factory is varargs in Scala, so `List(xs: _*)` is legal —
    // but the element count is only known at run time, so it cannot go through
    // the fixed-arity `MAKE_*` builtins. Each kind must still come out as
    // itself, and a `Map` factory must still see `(k, v)` pairs.
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  val xs = List(3, 1, 2)\n  println(List(xs: _*)); println(Vector(xs: _*)); println(Set(xs: _*))\n  println(mutable.ListBuffer(xs: _*)); println(mutable.Queue(xs: _*)); println(mutable.Stack(xs: _*))\n  println(mutable.ArrayDeque(xs: _*)); println(mutable.LinkedHashSet(xs: _*))\n  println(mutable.LinkedHashMap(xs.map(i => (i, i)): _*))\n  println(Map(xs.map(i => (i, i)): _*)); println(List(List(): _*))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "List(3, 1, 2)\nVector(3, 1, 2)\nSet(3, 1, 2)\nListBuffer(3, 1, 2)\nQueue(3, 1, 2)\nStack(3, 1, 2)\nArrayDeque(3, 1, 2)\nLinkedHashSet(3, 1, 2)\nLinkedHashMap(3 -> 3, 1 -> 1, 2 -> 2)\nMap(3 -> 3, 1 -> 1, 2 -> 2)\nList()\n"
    );
}

#[test]
fn ordered_on_a_user_class_drives_every_ordering_op() {
    // `compare` is REVERSED and scaled by 7 on purpose. A natural `n - that.n`
    // would agree with the structural fallback for a one-field `case class`, so
    // a frontend that ignored the user's method entirely would still print the
    // right answer; reversing it means only a real call can pass. The scale
    // proves `compareTo` forwards the user's value verbatim (14, not 1) while
    // the relational operators reduce it to a sign.
    let (out, ok) = run(
        "case class V(n: Int) extends Ordered[V] {\n  def compare(that: V): Int = (that.n - n) * 7\n}\nobject T extends App {\n  val xs = List(V(3), V(1), V(2))\n  println(xs.sorted)\n  println(xs.max)\n  println(xs.min)\n  println(xs.sortBy(v => v))\n  println(xs.maxBy(v => v))\n  println(xs.minBy(v => v))\n  println(V(1) < V(2))\n  println(V(1) > V(2))\n  println(V(2) <= V(2))\n  println(V(2) >= V(3))\n  println(V(1).compareTo(V(3)))\n  println(List((0, V(1)), (0, V(2))).sorted)\n  println(xs.sorted(Ordering[V].reverse))\n  println(1 < 2)\n  println(\"a\" < \"b\")\n  println(List(3, 1, 2).sorted)\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        // The last three lines are the control: a program that defines `compare`
        // still compares `Int`s and `String`s numerically/lexicographically, and
        // still sorts a plain `List[Int]` naturally.
        "List(V(3), V(2), V(1))\nV(1)\nV(3)\nList(V(3), V(2), V(1))\nV(1)\nV(3)\nfalse\ntrue\ntrue\ntrue\n14\nList((0,V(2)), (0,V(1)))\nList(V(1), V(2), V(3))\ntrue\ntrue\nList(1, 2, 3)\n"
    );
}

#[test]
fn a_user_ordered_sort_is_stable_on_every_path() {
    // Scala's sorts are stable, so elements whose `compare` answers 0 come out
    // in INPUT order. Every one of these runs through `host::merge_sort_idx`:
    // the implicit ordering (`sorted`), a key function (`sortBy`), and an
    // explicit `Ordering` — a merge that took from the right run on a tie would
    // scramble the tags while leaving the keys looking correctly sorted.
    // `reverse` flips the key order but must still keep ties in input order.
    let (out, ok) = run(
        "case class K(k: Int, tag: String) extends Ordered[K] {\n  def compare(that: K): Int = k - that.k\n}\nobject T extends App {\n  val ties = List(K(1,\"a\"), K(1,\"b\"), K(1,\"c\"), K(1,\"d\"), K(1,\"e\"), K(1,\"f\"), K(1,\"g\"))\n  println(ties.sorted)\n  println(ties.sortBy(x => x))\n  println(ties.sorted(Ordering[K]))\n  val mixed = List(K(2,\"a\"), K(1,\"b\"), K(2,\"c\"), K(1,\"d\"), K(3,\"e\"), K(2,\"f\"))\n  println(mixed.sorted)\n  println(mixed.sortBy(x => x))\n  println(mixed.sortBy(_.k))\n  println(mixed.sorted(Ordering[K].reverse))\n  println((1 to 40).toList.map(i => K(41 - i, \"t\")).sorted.map(_.k).take(8))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "List(K(1,a), K(1,b), K(1,c), K(1,d), K(1,e), K(1,f), K(1,g))\nList(K(1,a), K(1,b), K(1,c), K(1,d), K(1,e), K(1,f), K(1,g))\nList(K(1,a), K(1,b), K(1,c), K(1,d), K(1,e), K(1,f), K(1,g))\nList(K(1,b), K(1,d), K(2,a), K(2,c), K(2,f), K(3,e))\nList(K(1,b), K(1,d), K(2,a), K(2,c), K(2,f), K(3,e))\nList(K(1,b), K(1,d), K(2,a), K(2,c), K(2,f), K(3,e))\nList(K(3,e), K(2,a), K(2,c), K(2,f), K(1,b), K(1,d))\nList(1, 2, 3, 4, 5, 6, 7, 8)\n"
    );
}

#[test]
fn set_equality_ignores_iteration_order() {
    // A `Set` is unordered, so `equals` is "same size, same members". The stored
    // `Vec` is only an iteration order — and for a `mutable.HashSet` or a
    // `LinkedHashSet` it is not even the order an equal set would arrive at — so
    // comparing positionally answered `false` for sets Scala calls equal.
    // A set is equal only to another set: `Set(1, 2).equals(List(1, 2))` is
    // `false`, and `List` equality stays positional.
    let (out, ok) = run(
        "import scala.collection.mutable\nobject T extends App {\n  println(Set(1, 2) == Set(2, 1))\n  println(Set(1, 2) == Set(1, 2, 3))\n  println(List(3, 1, 2).toSet == Set(1, 2, 3))\n  println(Set(1, 2, 3, 4, 5, 6) == Set(6, 5, 4, 3, 2, 1))\n  println(mutable.Set(1, 2) == Set(2, 1))\n  println(mutable.LinkedHashSet(2, 1) == Set(1, 2))\n  println(Set(Set(1, 2)) == Set(Set(2, 1)))\n  println(Map(1 -> Set(1, 2)) == Map(1 -> Set(2, 1)))\n  println(Set(1, 2).equals(List(1, 2)))\n  println(List(1, 2) == List(2, 1))\n  println(Set(\"a\", \"b\") == Set(\"b\", \"a\"))\n}",
    );
    assert!(ok);
    assert_eq!(
        out,
        "true\nfalse\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\nfalse\nfalse\ntrue\n"
    );
}

#[test]
fn every_builtin_id_is_distinct() {
    // Builtin ids are hand-assigned `pub const`s, and two slices adding one at
    // the same time pick the same number without any conflict marker: the file
    // merges cleanly and the later `register_builtin` silently REPLACES the
    // earlier handler. That is how `MAKE_QUEUE` and `MAKE_ORDERING` both became
    // 754 — `mutable.Queue(1, 2)` started answering "value 2 is not a member of
    // object Ordering". Nothing else in the build catches it, so this does.
    let src = include_str!("../src/host.rs");
    let mut seen: Vec<(u16, &str)> = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once(": u16 = ") else {
            continue;
        };
        let Some(num) = tail.strip_suffix(';') else {
            continue;
        };
        let Ok(id) = num.parse::<u16>() else { continue };
        if let Some((_, other)) = seen.iter().find(|(n, _)| *n == id) {
            panic!("builtin id {id} is used by both `{other}` and `{name}`");
        }
        seen.push((id, name));
    }
    // A guard that found nothing would pass silently forever.
    assert!(
        seen.len() > 50,
        "expected to scan the builtin id table, found {} consts",
        seen.len()
    );
}

// ── The same hazard, one key type over: registries keyed by NAME ────────────
//
// `every_builtin_id_is_distinct` guards the integer-keyed builtin table. The
// tables below are keyed by a STRING, and they fail the same way and just as
// silently: two entries under one key, the build clean, and one of the two
// handlers simply never reached. Which one loses depends on the lookup —
// `throwable_fqn` and `Chunk::find_sub` take the FIRST match, while
// `VM::register_builtin` and a `HashMap::insert` keep the LAST — so a duplicate
// is never a compile error and never a panic, only a wrong answer somewhere
// else in the program.
//
// These read source text rather than the consts, for the same reason the id
// guard does: a table can be private to its module, and a duplicate should be
// caught where it is WRITTEN rather than only where it happens to be reachable.

/// Every `("key", "value"),` pair inside `const NAME: &[(&str, &str)] = &[…];`
/// in `src`, in source order. Returns `None` when the table is not found, so a
/// renamed table fails the guard instead of silently scanning nothing.
fn str_pair_table_keys<'a>(src: &'a str, table: &str) -> Option<Vec<&'a str>> {
    let start = src.find(&format!("const {table}: &[(&str, &str)] = &["))?;
    let body = &src[start..];
    let end = body.find("\n];")?;
    Some(
        quoted_strings(&body[..end])
            .into_iter()
            .step_by(2)
            .collect(),
    )
}

/// Every string literal in `s`, in source order. The tables scanned here hold
/// only plain literals — no escapes, no raw strings — so a scan to the next
/// unescaped quote is exact for them.
fn quoted_strings(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'"' {
            i += 1;
            continue;
        }
        let from = i + 1;
        let mut j = from;
        while j < b.len() && b[j] != b'"' {
            j += if b[j] == b'\\' { 2 } else { 1 };
        }
        if j >= b.len() {
            break;
        }
        out.push(&s[from..j]);
        i = j + 1;
    }
    out
}

/// Report the first key that appears twice in `keys`.
fn first_repeat<'a>(keys: &[&'a str]) -> Option<&'a str> {
    let mut seen: Vec<&str> = Vec::new();
    for k in keys {
        if seen.contains(k) {
            return Some(k);
        }
        seen.push(k);
    }
    None
}

#[test]
fn every_throwable_table_key_is_distinct() {
    // `throwable_fqn` is a `.iter().find()`, so a second row under a simple name
    // is dead — and a second row is exactly what an "add the JDK's package"
    // edit produces. Adding `("NoSuchElementException", "java.lang.…")` above
    // the real `java.util.…` row builds clean, and the only symptom is that a
    // caught `NoSuchElementException` prints the wrong package. Same for
    // `THROWABLE_PARENTS`, whose walk takes the first parent it finds: a second
    // row re-parenting a class silently loses, so `case e: Exception` either
    // stops catching something or starts catching a `ControlThrowable`.
    let src = include_str!("../src/host.rs");
    for table in ["BUILTIN_THROWABLES", "THROWABLE_PARENTS"] {
        let keys = str_pair_table_keys(src, table)
            .unwrap_or_else(|| panic!("`{table}` not found in src/host.rs — was it renamed?"));
        assert!(
            keys.len() > 10,
            "expected to scan `{table}`, found {} rows",
            keys.len()
        );
        if let Some(dup) = first_repeat(&keys) {
            panic!("`{table}` has two rows keyed `{dup}`; only the first is ever reached");
        }
    }
}

#[test]
fn every_builtin_is_registered_once() {
    // The id guard checks the `pub const` DECLARATIONS. This checks the CALL
    // SITES: `register_builtin` overwrites its slot, so registering one id twice
    // — the shape a copy-pasted line takes — silently drops the first handler.
    // Two consts with distinct ids and a mistyped second line pass the id guard
    // and fail here.
    let src = include_str!("../src/host.rs");
    let ids: Vec<&str> = src
        .lines()
        .filter_map(|l| l.trim().strip_prefix("vm.register_builtin("))
        .filter_map(|l| l.split_once(',').map(|(id, _)| id))
        .collect();
    assert!(
        ids.len() > 50,
        "expected to scan the register_builtin calls, found {}",
        ids.len()
    );
    if let Some(dup) = first_repeat(&ids) {
        panic!("builtin `{dup}` is registered twice; the first handler is unreachable");
    }
}

#[test]
fn every_method_name_list_is_distinct() {
    // The compiler's static-width tables are `&[&str]` membership sets. A
    // repeated name is not a wrong answer today, but it is the tell that a name
    // was added to a list that already had it — usually because the same method
    // was classified twice, in two different lists, with two different widths.
    // Cheap to hold to zero, and it keeps the lists auditable by eye.
    let src = include_str!("../src/compiler.rs");
    let mut scanned = 0;
    for (i, line) in src.lines().enumerate() {
        let Some(rest) = line.strip_prefix("const ") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once(": &[&str] = ") else {
            continue;
        };
        // Multi-line lists run to the closing `];` on its own line.
        let body: String = if tail.trim_end().ends_with("];") {
            tail.to_string()
        } else {
            src.lines()
                .skip(i + 1)
                .take_while(|l| l.trim() != "];")
                .collect::<Vec<_>>()
                .join("\n")
        };
        let names = quoted_strings(&body);
        scanned += 1;
        if let Some(dup) = first_repeat(&names) {
            panic!("`{name}` lists `{dup}` twice");
        }
    }
    assert!(
        scanned >= 8,
        "expected to scan the compiler's method-name lists, found {scanned}"
    );
}

#[test]
fn every_reference_corpus_entry_is_distinct() {
    // `lsp::hover` renders EVERY corpus row matching the word under the cursor,
    // and `lsp::completions` emits one item per row, so a duplicated row is a
    // doubled completion entry and a hover that says the same thing twice — and
    // it ships, because `gen-docs` renders the same table into
    // `docs/reference.html`. A name legitimately recurs across chapters (`map`
    // is both a sequence and a map method), so the key is `(name, chapter)`.
    let src = include_str!("../src/corpus.rs");
    let Some(start) = src.find("pub const CORPUS: &[Entry] = &[") else {
        panic!("`CORPUS` not found in src/corpus.rs — was it renamed?");
    };
    let mut seen: Vec<(&str, &str)> = Vec::new();
    // rustfmt puts each field of a wrapped tuple on its own line, and every
    // corpus row wraps: the row opens with `    (` and its first two fields are
    // the name and the chapter.
    let lines: Vec<&str> = src[start..].lines().collect();
    for w in lines.windows(3) {
        if w[0] != "    (" {
            continue;
        }
        let (Some(name), Some(chapter)) = (
            quoted_strings(w[1]).first().copied(),
            quoted_strings(w[2]).first().copied(),
        ) else {
            continue;
        };
        if seen.contains(&(name, chapter)) {
            panic!("the reference corpus documents `{name}` twice under `{chapter}`");
        }
        seen.push((name, chapter));
    }
    assert!(
        seen.len() > 300,
        "expected to scan the reference corpus, found {} entries",
        seen.len()
    );
}

// ── Overloads: the one name-keyed registry a USER PROGRAM can collide ───────
//
// The tables above are collided by an edit to this repo. `Class$method` is
// collided by an edit to the SCALA program: two `def g`s in one class both
// registered under `C$g`, and `Chunk::find_sub` answers the first, so every
// `g` call landed in whichever was written first whatever its argument count —
// `c.g(5)` and `c.g(5, 6)` both silently ran `def g()`. Every expected string
// below was diffed byte-for-byte against `scala` 3.8.4.

#[test]
fn an_overload_dispatches_on_its_argument_count() {
    let (out, ok) = run("class Box(val w: Int, val h: Int) {\n\
         \x20 def g(): String = \"g0\"\n\
         \x20 def g(x: Int): String = \"g1 \" + x\n\
         \x20 def g(x: Int, y: Int): String = \"g2 \" + (x + y)\n\
         }\n\
         object Fmt {\n\
         \x20 def show(): String = \"s0\"\n\
         \x20 def show(x: Int): String = \"s1 \" + x\n\
         \x20 def show(x: Int, y: Int): String = \"s2 \" + x + y\n\
         \x20 def viaBare: String = show() + \"|\" + show(3) + \"|\" + show(4, 5)\n\
         }\n\
         object T extends App {\n\
         \x20 val b = new Box(2, 3)\n\
         \x20 println(b.g()); println(b.g(5)); println(b.g(5, 6))\n\
         \x20 println(Fmt.show()); println(Fmt.show(1)); println(Fmt.show(1, 2))\n\
         \x20 println(Fmt.viaBare)\n\
         }\n");
    assert!(ok);
    assert_eq!(out, "g0\ng1 5\ng2 11\ns0\ns1 1\ns2 12\ns0|s1 3|s2 45\n");
}

#[test]
fn an_overload_survives_override_super_and_upcast_dispatch() {
    // Each of these reaches `Owner$method` by a DIFFERENT route — the direct
    // call on a known class, `super.m` against the supertype that owns the
    // implementation, the tag-test chain for a receiver typed as the base, an
    // unqualified self-call inside a method body, and the same chain over a
    // heterogeneous list. All five have to agree on which arity they mean.
    let (out, ok) = run("class Base {\n\
         \x20 def tag(): String = \"base0\"\n\
         \x20 def tag(n: Int): String = \"base1 \" + n\n\
         }\n\
         class Derived extends Base {\n\
         \x20 override def tag(): String = \"derived0/\" + super.tag()\n\
         \x20 override def tag(n: Int): String = \"derived1 \" + n + \"/\" + super.tag(n)\n\
         }\n\
         class Acc(var n: Int) {\n\
         \x20 def add(): Unit = { n += 1 }\n\
         \x20 def add(k: Int): Unit = { n += k }\n\
         \x20 def add(k: Int, j: Int): Unit = { n += k + j }\n\
         \x20 def bump(): Int = { add(); add(2); add(3, 4); n }\n\
         }\n\
         object T extends App {\n\
         \x20 val d = new Derived\n\
         \x20 println(d.tag()); println(d.tag(7))\n\
         \x20 val b: Base = d\n\
         \x20 println(b.tag()); println(b.tag(8))\n\
         \x20 println(new Acc(0).bump())\n\
         \x20 println(List(new Derived, new Base).map(x => x.tag(1)))\n\
         }\n");
    assert!(ok);
    assert_eq!(
        out,
        "derived0/base0\nderived1 7/base1 7\nderived0/base0\nderived1 8/base1 8\n10\n\
         List(derived1 1/base1 1, base1 1)\n"
    );
}

#[test]
fn an_overload_that_differs_only_in_parameter_type_is_refused() {
    // Argument COUNT is the part of Scala's overload resolution this frontend
    // can decide; `f(Int)` vs `f(String)` needs the argument TYPE. Both would
    // register as `C$f$1`, so the collision is exactly the one the arity
    // mangling removes — and the pre-fix behaviour was to run `f(Int)` for
    // `c.f("a")` and print `int a`. Refusing says so; printing does not.
    let (out, err, ok) = run_full(
        "class C {\n\
         \x20 def f(x: Int): String = \"int \" + x\n\
         \x20 def f(x: String): String = \"str \" + x\n\
         }\n\
         object T extends App { println(new C().f(1)); println(new C().f(\"a\")) }\n",
    );
    assert!(!ok, "a type-only overload must be refused, not guessed");
    assert_eq!(out, "", "nothing may run before the refusal");
    assert!(
        err.contains("declares `f` twice with 1 parameter(s)"),
        "the diagnostic must name the colliding method, got: {err}"
    );
}

#[test]
fn a_call_matching_no_overload_arity_is_refused() {
    // With `g` overloaded there is no unmangled `C$g` left to fall back to, so
    // an unmatched arity has to be caught where it is compiled — otherwise it
    // becomes a run-time "no such subroutine" naming a synthetic `C$g$2`.
    let (_out, err, ok) = run_full(
        "class C { def g(): Int = 0; def g(x: Int): Int = x }\n\
         object T extends App { println(new C().g(1, 2)) }\n",
    );
    assert!(!ok);
    assert!(
        err.contains("no overload of `C.g` takes 2 argument(s)")
            && err.contains("declared taking 0 or 1"),
        "the diagnostic must name the arities that DO exist, got: {err}"
    );
}

#[test]
fn an_overloaded_compare_still_reaches_the_hosts_ordering_path() {
    // `host::user_method_entry` resolves `Class$compare` by NAME to run a user
    // `Ordered` from inside `sorted`. Overloading `compare` renames the real one
    // to `V$compare$1`, so the host has to probe the arity-mangled name too or
    // every user ordering silently falls back to the structural order — which
    // for `V(3), V(1), V(2)` sorts ASCENDING and looks plausible.
    let (out, ok) = run("case class V(n: Int) extends Ordered[V] {\n\
         \x20 def compare(that: V): Int = that.n - n\n\
         \x20 def compare(a: Int, b: Int): Int = a - b\n\
         }\n\
         object T extends App {\n\
         \x20 println(List(V(3), V(1), V(2)).sorted)\n\
         \x20 println(V(1) < V(2))\n\
         \x20 println(V(1).compare(5, 4))\n\
         }\n");
    assert!(ok);
    assert_eq!(out, "List(V(3), V(2), V(1))\nfalse\n1\n");
}

// ── A user `toString` override, everywhere a value is rendered ──────────────
//
// Scala renders EVERY value through its `toString`, so an override has to be
// honoured by `println`, by interpolation, by `+` concatenation and at every
// depth of a collection. Only an explicit `p.toString` used to reach it — the
// compiler resolves that one statically — so `println(p)`, `s"$p"`, `"x" + p`
// and `List(p)` all printed `P@0`. Every expected string below was diffed
// byte-for-byte against `scala` 3.8.4.

/// A class whose `toString` is overridden, for the rendering tests below.
const P_CLASS: &str = "class P(val a: Int, val b: Int) {\n\
                       \x20 override def toString: String = \"P[\" + a + \",\" + b + \"]\"\n\
                       }\n";

#[test]
fn a_tostring_override_reaches_every_rendering_surface() {
    let (out, ok) = run(&format!(
        "{P_CLASS}\
         case class Q(n: Int) {{ override def toString: String = \"Q<\" + n + \">\" }}\n\
         object T extends App {{\n\
         \x20 val p = new P(1, 2)\n\
         \x20 println(p.toString); println(p); println(s\"$p\"); println(\"x\" + p)\n\
         \x20 println(List(p)); println(Map(1 -> p)); println(Some(p)); println((p, p))\n\
         \x20 println(String.valueOf(p)); println(f\"$p%s\")\n\
         \x20 val q = Q(5)\n\
         \x20 println(q); println(s\"$q\"); println(List(q))\n\
         }}\n"
    ));
    assert!(ok);
    assert_eq!(
        out,
        "P[1,2]\nP[1,2]\nP[1,2]\nxP[1,2]\nList(P[1,2])\nMap(1 -> P[1,2])\nSome(P[1,2])\n\
         (P[1,2],P[1,2])\nP[1,2]\nP[1,2]\nQ<5>\nQ<5>\nList(Q<5>)\n"
    );
}

#[test]
fn a_tostring_override_reaches_the_string_building_methods() {
    // These four reach the renderer by different routes than `println` does:
    // `mkString` from inside the sequence dispatcher, `format` from the string
    // dispatcher, `.toString` on a COLLECTION from the universal one, and
    // `StringBuilder.append` through the character expansion of
    // `String.valueOf`. All four held no VM before.
    let (out, ok) = run(&format!(
        "{P_CLASS}\
         object T extends App {{\n\
         \x20 val p = new P(1, 2)\n\
         \x20 println(List(p, p).mkString(\"[\", \";\", \"]\"))\n\
         \x20 println(Array(p).mkString(\",\"))\n\
         \x20 println(\"%s!\".format(p))\n\
         \x20 println(List(p).toString)\n\
         \x20 val sb = new StringBuilder; sb.append(p); println(sb.toString)\n\
         }}\n"
    ));
    assert!(ok);
    assert_eq!(
        out,
        "[P[1,2];P[1,2]]\nP[1,2]\nP[1,2]!\nList(P[1,2])\nP[1,2]\n"
    );
}

#[test]
fn a_tostring_inherited_from_a_trait_is_found() {
    // The subroutine is registered under the type that IMPLEMENTS it
    // (`Named$toString`), not under the receiver's tag, so resolving only
    // `Impl$toString` missed every trait-provided rendering. An `object`
    // override and a nested one are the same lookup by a different route.
    let (out, ok) = run(
        "trait Named { override def toString: String = \"named:\" + tag; def tag: String }\n\
         class Impl(val tag: String) extends Named\n\
         class Nested(val inner: Impl) { override def toString: String = \"N{\" + inner + \"}\" }\n\
         object Solo { override def toString: String = \"SOLO\" }\n\
         object T extends App {\n\
         \x20 println(new Impl(\"z\"))\n\
         \x20 println(Solo)\n\
         \x20 println(new Nested(new Impl(\"y\")))\n\
         \x20 println(List(new Impl(\"a\"), new Impl(\"b\")))\n\
         }\n",
    );
    assert!(ok);
    assert_eq!(out, "named:z\nSOLO\nN{named:y}\nList(named:a, named:b)\n");
}

#[test]
fn a_tostring_override_may_print_and_may_raise() {
    // Running an override re-enters the VM from inside the renderer, which is
    // where two things can go wrong. `println` renders BEFORE taking the stdout
    // lock, or an override that prints would deadlock — and the interleaving is
    // observable, since Scala runs each `toString` as it reaches it. An override
    // that RAISES leaves the exception in flight, and the `println` it was being
    // rendered for must not run: the pre-render unwinding check happens before
    // the override does, so it is re-checked after.
    let (out, ok) = run(
        "class Loud(val n: Int) {\n\
         \x20 override def toString: String = { println(\"side\"); \"L\" + n }\n\
         }\n\
         class Boom { override def toString: String = throw new RuntimeException(\"bad\") }\n\
         object T extends App {\n\
         \x20 println(new Loud(1))\n\
         \x20 println(List(new Loud(2), new Loud(3)))\n\
         \x20 try { println(new Boom) } catch { case e: RuntimeException => println(\"caught \" + e.getMessage) }\n\
         \x20 println(\"after\")\n\
         }\n",
    );
    assert!(ok);
    assert_eq!(
        out,
        "side\nL1\nside\nside\nList(L2, L3)\ncaught bad\nafter\n"
    );
}

#[test]
fn a_program_without_an_override_renders_exactly_as_it_did() {
    // The control for the whole feature. Both the compiler's concat rerouting
    // and the host's per-value method lookup are gated on the program declaring
    // an override at all; with none declared, every one of these has to answer
    // the derived rendering, on the same `Op::Add` bytecode as before.
    let (out, ok) = run(
        "case class C(n: Int)\n\
         class Plain(val n: Int)\n\
         object T extends App {\n\
         \x20 val c = C(1)\n\
         \x20 println(c); println(s\"$c\"); println(\"x\" + c); println(List(c)); println((c, c))\n\
         \x20 println(Map(\"k\" -> c)); println(List(c).mkString(\"|\")); println(\"%s\".format(c))\n\
         \x20 println(new Plain(1).toString.startsWith(\"Plain@\"))\n\
         \x20 println(1 + 2); println(\"n=\" + 3); println(s\"${1 + 2}\")\n\
         }\n",
    );
    assert!(ok);
    assert_eq!(
        out,
        "C(1)\nC(1)\nxC(1)\nList(C(1))\n(C(1),C(1))\nMap(k -> C(1))\nC(1)\nC(1)\ntrue\n3\nn=3\n3\n"
    );
}

#[test]
fn an_override_is_reached_through_a_string_that_is_not_a_literal() {
    // The reroute used to fire only when one side of the `+` was SYNTACTICALLY
    // a String — a literal, a `.toString`, a `.mkString`. A `val`, a parameter
    // and an element are none of those, so `pre + p` rendered `P@0`. The
    // decision is a run-time one now, and `acc += p` — the `x = x + e`
    // expansion, whose far operand is the target and so has no syntax to read
    // at all — takes the same path.
    //
    // The last four lines are the control: `n += 2`, `d += 0.25` and a `+` the
    // width analysis has already typed must stay on the native `Op::Add`, and
    // must still answer what Scala answers.
    let (out, ok) = run(
        "class P(val n: Int) { override def toString: String = \"P<\" + n + \">\" }\n\
         object T extends App {\n\
         \x20 val p = new P(3)\n\
         \x20 val pre = \"pre:\"\n\
         \x20 println(pre + p)\n\
         \x20 def tag(s: String): String = s + p\n\
         \x20 println(tag(\"fn:\"))\n\
         \x20 val parts = List(\"el:\")\n\
         \x20 println(parts(0) + p)\n\
         \x20 var acc = \"acc:\"\n\
         \x20 acc += p\n\
         \x20 acc += 1\n\
         \x20 println(acc)\n\
         \x20 var n = 5; n += 2; println(n)\n\
         \x20 var d = 1.5; d += 0.25; println(d)\n\
         \x20 println(List(1, 2).map(i => \"i\" + i).mkString(\",\"))\n\
         }\n",
    );
    assert!(ok);
    assert_eq!(
        out,
        "pre:P<3>\nfn:P<3>\nel:P<3>\nacc:P<3>1\n7\n1.75\ni1,i2\n"
    );
}

// ── mutable.PriorityQueue ───────────────────────────────────────────────────
//
// Its `toString` and its iteration expose the RAW heap array, so every string
// below is an artifact of Scala's exact sift algorithm rather than of the
// abstract "max-heap" idea. Diffed byte-for-byte against `scala` 3.8.4.

#[test]
fn a_priority_queue_prints_its_raw_heap_array() {
    // `PriorityQueue(3,1,4,1,5,9,2,6)` is `9, 6, 4, 1, 5, 3, 2, 1`: neither the
    // input order, nor sorted, nor `9, 6, 5, 4, 1, 3, 2, 1` — which is what
    // repeated sift-up insertion leaves. The factory appends every element raw
    // and then runs ONE bottom-up `fixDown` sweep, and only that reproduces it.
    let (out, ok) = run("import scala.collection.mutable\n\
         object T extends App {\n\
         \x20 println(mutable.PriorityQueue(3,1,4,1,5,9,2,6))\n\
         \x20 println(mutable.PriorityQueue(\"b\",\"a\",\"c\"))\n\
         \x20 println(mutable.PriorityQueue[Int]())\n\
         \x20 println(mutable.PriorityQueue(3,1,2).toList)\n\
         \x20 println(mutable.PriorityQueue(5,3,8).dequeueAll)\n\
         }\n");
    assert!(ok);
    assert_eq!(
        out,
        "PriorityQueue(9, 6, 4, 1, 5, 3, 2, 1)\nPriorityQueue(c, a, b)\nPriorityQueue()\n\
         List(3, 1, 2)\nArraySeq(8, 5, 3)\n"
    );
}

#[test]
fn a_priority_queue_add_sifts_up_and_addall_reheapifies() {
    // The two are NOT the same operation and do not leave the same array:
    // `+= x` sifts the one new arrival up, while `++= xs` appends them all and
    // heapifies from the first new position. Implementing `++=` as repeated
    // `+=` answers a different (still valid) heap, which is why both are here.
    // `dequeue` moves the LAST element to the root and sifts it down.
    let (out, ok) = run("import scala.collection.mutable\n\
         object T extends App {\n\
         \x20 val q = mutable.PriorityQueue(3,1,4,1,5,9,2,6)\n\
         \x20 println(q.dequeue()); println(q)\n\
         \x20 q += 7; println(q)\n\
         \x20 q ++= List(0, 8); println(q)\n\
         \x20 println(q.dequeue()); println(q.dequeue()); println(q)\n\
         \x20 val e = mutable.PriorityQueue[Int]()\n\
         \x20 e.enqueue(2); e.enqueue(5); e.enqueue(1)\n\
         \x20 println(e); println(e.dequeue()); println(e.size); println(e.isEmpty)\n\
         }\n");
    assert!(ok);
    assert_eq!(
        out,
        "9\nPriorityQueue(6, 5, 4, 1, 1, 3, 2)\n\
         PriorityQueue(7, 6, 4, 5, 1, 3, 2, 1)\n\
         PriorityQueue(8, 7, 4, 5, 6, 3, 2, 1, 0, 1)\n\
         8\n7\nPriorityQueue(6, 5, 4, 1, 1, 3, 2, 0)\n\
         PriorityQueue(5, 2, 1)\n5\n2\nfalse\n"
    );
}

#[test]
fn a_priority_queue_uses_a_user_ordering_and_the_right_result_factory() {
    // The heap consults the user's `compare`, and ties keep arrival order
    // because both sift comparisons are STRICT. `map` answers an `ArrayBuffer`
    // — a `PriorityQueue`'s builder needs an `Ordering` for the RESULT element
    // type, which `map` cannot supply — where the selecting `filter` stays one.
    // `clone` is independent of the original.
    let (out, ok) = run(
        "import scala.collection.mutable\n\
         case class J(p: Int, n: String) extends Ordered[J] { def compare(that: J): Int = p - that.p }\n\
         object T extends App {\n\
         \x20 val q = mutable.PriorityQueue(J(1,\"a\"), J(5,\"b\"), J(3,\"c\"), J(5,\"d\"))\n\
         \x20 println(q); println(q.dequeue()); println(q.dequeue()); println(q)\n\
         \x20 println(mutable.PriorityQueue(1,2,3).map(_ * 10))\n\
         \x20 println(mutable.PriorityQueue(4,2,9).filter(_ > 3))\n\
         \x20 val c = mutable.PriorityQueue(3,1,2); val cc = c.clone(); cc.dequeue()\n\
         \x20 println(c); println(cc)\n\
         }\n",
    );
    assert!(ok);
    assert_eq!(
        out,
        "PriorityQueue(J(5,b), J(5,d), J(3,c), J(1,a))\nJ(5,b)\nJ(5,d)\n\
         PriorityQueue(J(3,c), J(1,a))\nArrayBuffer(30, 20, 10)\nPriorityQueue(9, 4)\n\
         PriorityQueue(3, 1, 2)\nPriorityQueue(2, 1)\n"
    );
}

// ── Compound assignment to a target that is not a plain name ────────────────
//
// Scala resolves `l op= r` by preferring an `op=` MEMBER on `l` and falling
// back to `l = l op r`, which for an application target expands to
// `l.update(args, l.apply(args) op r)` (SLS 6.12.4). Every expected string
// below was diffed byte-for-byte against `scala` 3.8.4 (JVM 26) during
// authoring.

#[test]
fn an_indexed_target_takes_the_update_expansion() {
    let (out, ok) = run(&wrap(
        "val a = Array(1,2,3); a(0) += 10; a(1) -= 5; a(2) *= 4; \
         println(a(0)); println(a(1)); println(a(2)); \
         val d = Array(7); d(0) /= 2; println(d(0)); \
         val md = Array(7); md(0) %= 4; println(md(0)); \
         val g = Array(Array(1,2), Array(3,4)); g(0)(1) += 9; println(g(0)(1))",
    ));
    assert!(ok);
    assert_eq!(out, "11\n-3\n12\n3\n3\n11\n");
}

#[test]
fn an_indexed_map_and_buffer_target_updates_in_place() {
    let (out, ok) = run(&wrap(
        "val ab = scala.collection.mutable.ArrayBuffer(1,2); ab(0) += 10; println(ab); \
         val m = scala.collection.mutable.Map(\"k\" -> 1); m(\"k\") += 10; println(m); \
         val neg = scala.collection.mutable.Map(1 -> 5); neg(1) -= 8; println(neg)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "ArrayBuffer(11, 2)\nHashMap(k -> 11)\nHashMap(1 -> -3)\n"
    );
}

#[test]
fn a_growable_element_takes_the_member_call_not_the_arithmetic() {
    // `+=` on a `ListBuffer` element is the growable METHOD — it mutates the
    // buffer the slot already holds. The arithmetic expansion would need a `+`
    // on `ListBuffer`, which Scala does not have, so picking the wrong branch
    // here is a hard failure rather than a subtly wrong number.
    let (out, ok) = run(&wrap(
        "val m = scala.collection.mutable.Map(\"k\" -> scala.collection.mutable.ListBuffer(1)); \
         m(\"k\") += 7; println(m); \
         m(\"k\") ++= List(5,6); println(m); \
         val nb = scala.collection.mutable.ListBuffer(scala.collection.mutable.ListBuffer(1)); \
         nb.head += 3; println(nb)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "HashMap(k -> ListBuffer(1, 7))\n\
         HashMap(k -> ListBuffer(1, 7, 5, 6))\n\
         ListBuffer(ListBuffer(1, 3))\n"
    );
}

#[test]
fn a_field_compound_assign_reaches_an_explicit_receiver() {
    let src = "import scala.collection.mutable\n\
               class C(var n: Int, val items: mutable.ListBuffer[Int])\n\
               object T extends App {\n\
                 val c = new C(1, mutable.ListBuffer(9))\n\
                 c.n += 5; println(c.n)\n\
                 c.items += 4; println(c.items)\n\
                 c.n *= 3; println(c.n)\n\
               }\n";
    let (out, ok) = run(src);
    assert!(ok);
    assert_eq!(out, "6\nListBuffer(9, 4)\n18\n");
}

#[test]
fn a_compound_assign_evaluates_its_target_exactly_once() {
    // The index expression has a side effect, so a lowering that re-evaluates
    // the target instead of parking it in a temporary answers `2` for the call
    // count. Scala evaluates it once.
    let (out, ok) = run(&wrap(
        "val log = scala.collection.mutable.ListBuffer[Int](); \
         val a = Array(1,2); \
         def k(): Int = { log += 1; 0 }; \
         a(k()) += 5; println(a(0)); println(log.size)",
    ));
    assert!(ok);
    assert_eq!(out, "6\n1\n");
}

#[test]
fn a_field_is_written_through_its_receiver_with_a_plain_equals() {
    // `recv.field = v` used to be a parse error ("unexpected token Assign"),
    // which left a `var` field writable only from inside its own class. It is
    // the `op=` target with `=` as the operator, and the receiver is subject to
    // the same evaluate-once rule: `mk(b).n = 3` calls `mk` once, and so does
    // `mk(b).n += 4`, so the call counter reads 1 then 2.
    let (out, ok) = run("class Box(var n: Int, var label: String)\n\
         object T extends App {\n\
         \x20 var calls = 0\n\
         \x20 def mk(b: Box): Box = { calls += 1; b }\n\
         \x20 val b = new Box(1, \"L\")\n\
         \x20 b.n = 7; println(b.n)\n\
         \x20 b.label = \"M\"; println(b.label)\n\
         \x20 mk(b).n = 3; println(b.n); println(calls)\n\
         \x20 mk(b).n += 4; println(b.n); println(calls)\n\
         \x20 val bs = Array(new Box(0, \"a\"), new Box(0, \"b\"))\n\
         \x20 bs(1).n = 5; println(bs(0).n); println(bs(1).n)\n\
         }\n");
    assert!(ok);
    assert_eq!(out, "7\nM\n3\n1\n7\n2\n0\n5\n");
}

#[test]
fn a_singleton_var_is_written_through_the_object_name() {
    // A singleton's `var` is a GLOBAL, not a heap-record field — reading `Cfg.n`
    // lowers to `GetVar("Cfg.n")` — so writing it through the record builtins
    // used to fail at run time with `value n is not a member of Cfg` even once
    // the parse succeeded. Every operator form has to reach the same global the
    // object's own `def`s write, or `Cfg.bump()` would not see the update.
    let (out, ok) = run(
        "object Cfg { var n = 1; var s: String = \"c\"; val fixed = 9; \
         def bump(): Int = { n += 1; n } }\n\
         object T extends App {\n\
         \x20 Cfg.n = 10; println(Cfg.n)\n\
         \x20 Cfg.n += 5; println(Cfg.n)\n\
         \x20 Cfg.n *= 2; println(Cfg.n)\n\
         \x20 Cfg.s = Cfg.s + \"!\"; println(Cfg.s)\n\
         \x20 Cfg.s += \"?\"; println(Cfg.s)\n\
         \x20 println(Cfg.bump()); println(Cfg.n)\n\
         \x20 println(Cfg.fixed)\n\
         }\n",
    );
    assert!(ok);
    assert_eq!(out, "10\n15\n30\nc!\nc!?\n31\n31\n9\n");
}

#[test]
fn a_class_declared_in_the_app_body_is_a_member() {
    // Scala compiles a type declared inside `object T extends App { … }` as a
    // member of the object. Declaring it beside the object is a PLACEMENT, not
    // a requirement, and this form used to be a parse error.
    let (out, ok) = run(&wrap(
        "class C(var n: Int); val c = new C(1); c.n += 4; println(c.n)",
    ));
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn a_member_case_class_gets_its_generated_members() {
    let (out, ok) = run(&wrap(
        "case class P(x: Int, y: Int); val p = P(1, 2); \
         println(p); println(p.copy(y = 9)); println(p == P(1, 2))",
    ));
    assert!(ok);
    assert_eq!(out, "P(1,2)\nP(1,9)\ntrue\n");
}

#[test]
fn a_member_trait_dispatches_polymorphically() {
    let (out, ok) = run(&wrap(
        "trait S { def area: Int }; class Sq(s: Int) extends S { def area = s * s }; \
         class Rc(w: Int, h: Int) extends S { def area = w * h }; \
         val xs: List[S] = List(new Sq(3), new Rc(2, 5)); \
         println(xs.map(_.area)); println(xs.map(_.area).sum)",
    ));
    assert!(ok);
    assert_eq!(out, "List(9, 10)\n19\n");
}

#[test]
fn a_member_object_is_a_singleton() {
    let (out, ok) = run(&wrap(
        "object U { val k = 3; def f(n: Int) = n * k }; println(U.f(4)); println(U.k)",
    ));
    assert!(ok);
    assert_eq!(out, "12\n3\n");
}

#[test]
fn a_member_class_reads_an_app_body_val() {
    // The `App` body's `val`s are program globals, so a member class's method
    // sees `k` the way Scala's would. A member class inside a `def` frame
    // cannot — see `BUGS.md`.
    let (out, ok) = run(&wrap(
        "val k = 10; class C(val n: Int) { def g = n + k }; println(new C(1).g)",
    ));
    assert!(ok);
    assert_eq!(out, "11\n");
}

#[test]
fn a_class_declared_inside_a_def_body_is_hoisted() {
    let (out, ok) = run(&wrap(
        "def f(): Int = { class C(val n: Int); new C(7).n }; println(f())",
    ));
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn a_member_class_declared_after_its_use_still_resolves() {
    // Members are not ordered the way statements are: Scala resolves `C` in the
    // first statement against a declaration that comes later in the body.
    let (out, ok) = run(&wrap(
        "val c = new C(4); println(c.twice); class C(val n: Int) { def twice = n + n }",
    ));
    assert!(ok);
    assert_eq!(out, "8\n");
}

#[test]
fn member_case_objects_form_an_adt() {
    let (out, ok) = run(&wrap(
        "sealed trait E; case object A extends E; case object B extends E; \
         val es: List[E] = List(A, B, A); println(es.map { case A => 1; case B => 2 })",
    ));
    assert!(ok);
    assert_eq!(out, "List(1, 2, 1)\n");
}

#[test]
fn a_redeclared_type_is_refused_rather_than_silently_shadowed() {
    // Scala scopes same-named types (`class Q` in the body shadows the one
    // beside the object, and answers 200 here). Every declaration lands in ONE
    // flat namespace here, so the second would silently replace the first and
    // the program would print 20 — a wrong answer, not a refusal. The
    // diagnostic is asserted, because "it exited non-zero" would pass on any
    // failure at all.
    let (_out, err, ok) = run_full(
        "class Q(val v: Int) { def g = v * 10 }\n\
         object T extends App { class Q(val v: Int) { def g = v * 100 }; println(new Q(2).g) }\n",
    );
    assert!(!ok, "a redeclared type must not run");
    assert!(
        err.contains("type `Q` is already declared"),
        "expected the redeclaration diagnostic, got: {err}"
    );
}

#[test]
fn a_companion_object_is_not_a_redeclaration() {
    // `class P` beside `object P` is the companion idiom, which Scala allows —
    // so the redeclaration check counts the two kinds apart.
    let (out, ok) = run("case class P(x: Int)\n\
         object P { def zero = P(0) }\n\
         object T extends App { println(P.zero); println(P(5)) }\n");
    assert!(ok);
    assert_eq!(out, "P(0)\nP(5)\n");
}

#[test]
fn a_compound_assign_in_expression_position_answers_the_growable_receiver() {
    // Scala's `+=` is an expression. On a receiver that HAS the method the
    // value is that receiver, so `println(b += 3)` prints the buffer.
    let (out, ok) = run(&wrap(
        "val b = scala.collection.mutable.ListBuffer(1, 2); println(b += 3); println(b)",
    ));
    assert!(ok);
    assert_eq!(out, "ListBuffer(1, 2, 3)\nListBuffer(1, 2, 3)\n");
}

#[test]
fn a_compound_assign_in_expression_position_answers_unit_when_it_is_arithmetic() {
    // The other half of the same SLS 6.12.4 choice: with no `+=` member the
    // expansion is `n = n + 1`, an assignment, whose value is `()`. Both halves
    // are branches of ONE lowering, so the update must still land.
    let (out, ok) = run(&wrap(
        "var n = 5; println(n += 1); println(n); val r = (n *= 2); println(r); println(n)",
    ));
    assert!(ok);
    assert_eq!(out, "()\n6\n()\n12\n");
}

#[test]
fn an_expression_position_compound_assign_reaches_an_element_and_a_field() {
    // An `Int` element goes through `update`, so the value is `()`; a growable
    // element takes the member call, so the value is the ELEMENT.
    let (out, ok) = run(&wrap(
        "val a = Array(1, 2, 3); println(a(1) += 10); println(a.mkString(\",\")); \
         val m = scala.collection.mutable.Map(\"k\" -> scala.collection.mutable.ListBuffer(1)); \
         println(m(\"k\") += 5); \
         class C(var f: Int); val c = new C(3); println(c.f *= 4); println(c.f)",
    ));
    assert!(ok);
    assert_eq!(out, "()\n1,12,3\nListBuffer(1, 5)\n()\n12\n");
}

#[test]
fn a_chained_compound_assign_feeds_its_own_result() {
    // `(b += 2) += 3` has no assignable target for the outer `+=`, so only the
    // member reading exists — and it only works if the inner one answered the
    // buffer rather than `()`.
    let (out, ok) = run(&wrap(
        "val b = scala.collection.mutable.ListBuffer(1); println((b += 2) += 3); println(b)",
    ));
    assert!(ok);
    assert_eq!(out, "ListBuffer(1, 2, 3)\nListBuffer(1, 2, 3)\n");
}

#[test]
fn an_expression_position_compound_assign_still_boxes_the_var_it_writes() {
    // The value is read AND the write escapes the closure: `n` is written from
    // inside a lambda, so it has to be boxed exactly as the statement form
    // boxes it, or the closure would update a copy and `n` would stay 0.
    let (out, ok) = run(&wrap(
        "var n = 0; val xs = List(1, 2, 3).map(x => n += x); println(xs); println(n)",
    ));
    assert!(ok);
    assert_eq!(out, "List((), (), ())\n6\n");
}

#[test]
fn an_expression_position_compound_assign_evaluates_its_target_once() {
    // Reading the value must not cost a second evaluation of the target: the
    // index has a side effect, so a re-evaluating lowering answers 2 here.
    let (out, ok) = run(&wrap(
        "val a = Array(1, 2); var i = 0; def next(): Int = { i += 1; i - 1 }; \
         println(a(next()) += 9); println(a.mkString(\",\")); println(i)",
    ));
    assert!(ok);
    assert_eq!(out, "()\n10,2\n1\n");
}

#[test]
fn a_growable_operator_assign_in_expression_position_answers_its_receiver() {
    let (out, ok) = run(&wrap(
        "val sb = new StringBuilder(\"a\"); println(sb += 'b'); println(sb ++= \"cd\")",
    ));
    assert!(ok);
    assert_eq!(out, "ab\nabcd\n");
}

#[test]
fn a_subnormal_double_renders_java_two_significant_digits() {
    // Java's `Double.toString` takes the shortest decimal that round-trips
    // EXCEPT that when one digit suffices it takes the closest decimal of
    // length one OR two. `Double.MinPositiveValue` is 4.9406…E-324, so the
    // answer is `4.9E-324` — not the one-digit `5E-324` padded to `5.0E-324`.
    // The padded form is what this printed before.
    let (out, ok) = run(&wrap(
        "val mp = Double.MinPositiveValue; println(mp); println(mp * 2); \
         println(mp * 20); println(mp * 128); println(mp * 2048)",
    ));
    assert!(ok);
    assert_eq!(out, "4.9E-324\n9.9E-324\n9.9E-323\n6.3E-322\n1.012E-320\n");
}

#[test]
fn the_two_digit_rule_leaves_every_normal_double_alone() {
    // The rule can only move the second digit off zero when the ULP is of the
    // same order as the value, which no normal double reaches — so these must
    // render exactly as they did.
    let (out, ok) = run(&wrap(
        "println(1.0E7); println(1.0E-4); println(3.0); println(0.001); \
         println(100.0); println(1e23); println(Double.MaxValue)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "1.0E7\n1.0E-4\n3.0\n0.001\n100.0\n1.0E23\n1.7976931348623157E308\n"
    );
}

#[test]
fn an_exact_decimal_tie_goes_to_the_even_digit() {
    // `5 * 2^-23` is exactly `5.9604644775390625E-7`, equidistant between the
    // two 16-digit decimals around it, and Java takes the even significand.
    // Rust's shortest form takes the other, so this printed `…063E-7`.
    let (out, ok) = run(&wrap(
        "var a = 1.0; var i = 0; while (i < 23) { a = a / 2.0; i += 1 }; println(a * 5.0); \
         var b = 1.0; var j = 0; while (j < 25) { b = b / 2.0; j += 1 }; println(b)",
    ));
    assert!(ok);
    assert_eq!(out, "5.960464477539062E-7\n2.9802322387695312E-8\n");
}

#[test]
fn a_tie_needs_both_candidates_to_round_trip() {
    // `2^-24` carries the SAME digits as `5 * 2^-23` above
    // (`5.9604644775390625`) and yet answers the ODD one, because at an exact
    // power of two the gap below is half the gap above: the lower candidate
    // falls outside the rounding interval, so it is not a candidate and there
    // is no tie to break. A digits-only tie rule answers `…062E-8` here.
    let (out, ok) = run(&wrap(
        "var c = 1.0; var k = 0; while (k < 24) { c = c / 2.0; k += 1 }; println(c); \
         var d = 1.0; var l = 0; while (l < 26) { d = d / 2.0; l += 1 }; \
         println(d); println(d * 3.0)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "5.960464477539063E-8\n1.4901161193847656E-8\n4.470348358154297E-8\n"
    );
}

// ── entry points: `@main`, top-level definitions, and `main`'s `args` ────────
//
// Every expectation below was diffed against Scala 3.8.4 on JDK 26.0.2 before
// being frozen; the `Illegal command line …` wordings are
// `scala.util.CommandLineParser.showError`'s, which writes to STDOUT and leaves
// the exit status 0 — unlike every other failure in this frontend.

/// Run a Scala source string with program arguments, as `scala file.scala a b`
/// would supply them.
fn run_args(src: &str, argv: &[&str]) -> (String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("scalars_argv_{}.scala", fasthash(src)));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_scala"))
        .arg(&path)
        .args(argv)
        .output()
        .expect("spawn scala");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

#[test]
fn a_main_annotated_def_is_an_entry_point() {
    let (out, ok) = run("@main def go(): Unit = { println(\"hi\"); println(6 * 7) }");
    assert!(ok);
    assert_eq!(out, "hi\n42\n");
}

#[test]
fn an_annotation_after_an_import_still_starts_a_declaration() {
    // Newline inference used not to separate before `@`, so the parser's
    // `import` scan ran past the line break and swallowed the `@main` — leaving
    // a file whose only entry point had silently become an ordinary top-level
    // `def`. Every fuzz program carries three imports, so this shape is the
    // common one, not a corner.
    let (out, ok) = run("import scala.collection.mutable\n@main def go(): Unit = println(\"ok\")");
    assert!(ok);
    assert_eq!(out, "ok\n");
}

#[test]
fn top_level_defs_and_vals_are_visible_to_the_entry_point() {
    let (out, ok) = run("val base = 10\ndef twice(x: Int): Int = x * 2\n\
         @main def go(): Unit = { println(base); println(twice(base)) }");
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

#[test]
fn a_top_level_val_initializes_before_the_entry_body() {
    // And before the command line is read at all: the initializer's output
    // precedes the `Illegal command line` diagnostic for the missing argument.
    let (out, ok) =
        run("val v = { println(\"init\"); 1 }\n@main def go(n: Int): Unit = println(n)");
    assert!(ok);
    assert_eq!(out, "init\nIllegal command line: more arguments expected\n");
}

#[test]
fn main_parameters_are_read_from_the_command_line() {
    let (out, ok) = run_args(
        "@main def go(n: Int, s: String, d: Double, b: Boolean): Unit = \
         { println(n + 1); println(s.toUpperCase); println(d * 2); println(!b) }",
        &["7", "hey", "2.5", "true"],
    );
    assert!(ok);
    assert_eq!(out, "8\nHEY\n5.0\nfalse\n");
}

#[test]
fn extra_command_line_arguments_are_ignored() {
    let (out, ok) = run_args(
        "@main def go(n: Int): Unit = println(n)",
        &["7", "spare", "spare"],
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn a_missing_argument_names_the_position_it_stopped_at() {
    // Scala's wording changes with the index: nothing for the first, the word
    // `first` for the second, a count from there on.
    let src = "@main def go(a: Int, b: Int, c: Int): Unit = println(a + b + c)";
    for (argv, expected) in [
        (&[][..], "Illegal command line: more arguments expected\n"),
        (
            &["1"][..],
            "Illegal command line after first argument: more arguments expected\n",
        ),
        (
            &["1", "2"][..],
            "Illegal command line after 2 arguments: more arguments expected\n",
        ),
    ] {
        let (out, ok) = run_args(src, argv);
        assert!(ok, "a command-line error still exits 0 in Scala");
        assert_eq!(out, expected, "argv {argv:?}");
    }
}

#[test]
fn an_unreadable_argument_carries_the_jdk_conversion_message() {
    let src = "@main def go(y: Byte, i: Int, l: Long, d: Double, b: Boolean): Unit = \
               println(\"\" + y + i + l + d + b)";
    for (argv, expected) in [
        (
            ["300", "1", "1", "1.0", "true"],
            "Illegal command line: java.lang.NumberFormatException: \
             Value out of range. Value:\"300\" Radix:10\n",
        ),
        (
            ["1", "zz", "1", "1.0", "true"],
            "Illegal command line after first argument: \
             java.lang.NumberFormatException: For input string: \"zz\"\n",
        ),
        (
            ["1", "1", "99999999999999999999", "1.0", "true"],
            "Illegal command line after 2 arguments: java.lang.NumberFormatException: \
             For input string: \"99999999999999999999\"\n",
        ),
        (
            ["1", "1", "1", "1.0", "yes"],
            "Illegal command line after 4 arguments: java.lang.IllegalArgumentException: \
             For input string: \"yes\"\n",
        ),
    ] {
        let (out, ok) = run_args(src, &argv);
        assert!(ok);
        assert_eq!(out, expected, "argv {argv:?}");
    }
}

#[test]
fn a_main_parameter_type_with_no_command_line_reader_is_refused() {
    // Scala rejects these at COMPILE time for want of a
    // `CommandLineParser.FromString[T]` given; `Float` is refused here for the
    // separate reason that it is not a distinct type in this frontend, so
    // reading one would answer a `Double`'s rendering (`BUGS.md`).
    for ty in ["Char", "Float"] {
        let (out, err, ok) = run_full(&format!("@main def go(c: {ty}): Unit = println(c)"));
        assert!(!ok, "`{ty}` must not be read from the command line");
        assert!(out.is_empty());
        assert!(
            err.contains(&format!(
                "`@main def go` cannot read a `{ty}` from the command line"
            )),
            "the refusal must name the offending type: {err:?}"
        );
    }
}

#[test]
fn a_second_main_annotation_is_refused_rather_than_guessed() {
    // Scala does not pick either: it asks for `--main-class`.
    let (_, err, ok) =
        run_full("@main def a(): Unit = println(1)\n@main def b(): Unit = println(2)");
    assert!(!ok);
    assert!(err.contains("--main-class"), "stderr was {err:?}");
}

#[test]
fn mains_args_parameter_is_the_command_line() {
    // It used to be unbound, so `args.length` dereferenced `null` and the
    // program died where Scala prints `0`.
    let src = "object T { def main(args: Array[String]): Unit = \
               { println(args.length); println(args.mkString(\"|\")); println(args.isEmpty) } }";
    let (out, ok) = run_args(src, &[]);
    assert!(ok);
    assert_eq!(out, "0\n\ntrue\n");
    let (out, ok) = run_args(src, &["a", "b", "c"]);
    assert!(ok);
    assert_eq!(out, "3\na|b|c\nfalse\n");
}

#[test]
fn a_non_ascii_character_next_to_an_operator_lexes() {
    // The operator lookahead sliced `&src[i..i + 3]` by BYTE offset, so the
    // `,"é` window in a list of strings split a multi-byte character and the
    // lexer PANICKED on valid Scala. Only a program with a non-ASCII literal
    // within two bytes of an operator reaches it, and the fuzz corpus is
    // ASCII-only by construction.
    let (out, ok) = run(&wrap(
        "println(List(\"b\", \"A\", \"é\", \"a\", \"Z\").sorted); println(\"naïve\".toUpperCase); \
         println(Map(\"é\" -> 1).size)",
    ));
    assert!(ok);
    assert_eq!(out, "List(A, Z, a, b, é)\nNAÏVE\n1\n");
}

// ── three silent wrong answers that are now honest refusals ─────────────────
//
// Each was a construct `BUGS.md` already described as unsupported while the
// runtime answered it anyway — with the wrong value and no diagnostic, which is
// the one thing the README says never happens.

#[test]
fn a_block_level_def_overload_is_refused_not_silently_shadowed() {
    // The same-block pair is an OVERLOAD in Scala; the resolver's
    // inner-shadows-outer rule turned it into a redefinition, so BOTH calls ran
    // the second body — `def g(x: Int) = "int"; def g(x: String) = "str"`
    // answered `str` twice, and even the statically decidable
    // `def h(); def h(x: Int)` answered `h1` twice.
    for src in [
        "def g(x: Int): String = \"int\"\ndef g(x: String): String = \"str\"\nprintln(g(1))",
        "def h(): String = \"h0\"\ndef h(x: Int): String = \"h1\"\nprintln(h())",
    ] {
        let (out, err, ok) = run_full(&wrap(src));
        assert!(!ok, "a block-level overload must not run: {out:?}");
        assert!(
            err.contains("declared twice in one block"),
            "stderr {err:?}"
        );
    }
}

#[test]
fn a_def_in_a_nested_block_still_shadows_the_outer_one() {
    // The guard above must not touch the rule it sits next to: two `def`s of one
    // name at DIFFERENT nesting depths are a shadow, not an overload.
    let (out, ok) = run(&wrap(
        "def q(x: Int): Int = x + 1\n{ def q(x: Int): Int = x + 100; println(q(1)) }\nprintln(q(1))",
    ));
    assert!(ok);
    assert_eq!(out, "101\n2\n");
}

#[test]
fn a_named_regex_group_is_refused_rather_than_answered_wrongly() {
    // `m.group("y")` went through `to_int`, which reads a `String` as 0 — group
    // 0, the whole match. `"(?<y>[0-9]{4})-(?<m>[0-9]{2})"` on `2026-08` gave
    // `2026-08` for both names instead of `2026` and `08`.
    let (_, err, ok) = run_full(&wrap(
        "val r = \"(?<y>[0-9]{4})-(?<m>[0-9]{2})\".r\n\
         println(r.findFirstMatchIn(\"2026-08\").get.group(\"y\"))",
    ));
    assert!(!ok);
    assert!(err.contains("named regex group"), "stderr {err:?}");
    // The numbered form is modeled and must keep working.
    let (out, ok) = run(&wrap(
        "val r = \"(?<y>[0-9]{4})-(?<m>[0-9]{2})\".r\n\
         println(r.findFirstMatchIn(\"2026-08\").get.group(1))",
    ));
    assert!(ok);
    assert_eq!(out, "2026\n");
}

#[test]
fn a_named_group_in_a_replacement_is_refused() {
    // `${d}` was copied through verbatim: `"a1b2".replaceAll("(?<d>[0-9])",
    // "<${d}>")` answered `a<${d}>b<${d}>` where Java splices `a<1>b<2>`.
    let (_, err, ok) = run_full(&wrap(
        r##"println("a1b2".replaceAll("(?<d>[0-9])", "<${d}>"))"##,
    ));
    assert!(!ok);
    assert!(err.contains("named regex group"), "stderr {err:?}");
    let (out, ok) = run(&wrap(
        r##"println("a1b2".replaceAll("(?<d>[0-9])", "<$1>"))"##,
    ));
    assert!(ok);
    assert_eq!(out, "a<1>b<2>\n");
}

#[test]
fn a_forward_reference_reads_the_jvm_field_default() {
    // An `extends App` body's `val`s are FIELDS, and a JVM field holds its
    // type's default from the moment the object exists. Every one of these read
    // `null` before, whatever the declared type. The `Char` line is the NUL
    // character, which is what Scala prints for the `Char` default.
    let (out, ok) = run("object T extends App {\n  \
         println(i); println(l); println(d); println(b); println(s); println(c); println(xs)\n  \
         println(ii); println(dd); println(bb)\n  \
         val i: Int = 7\n  val l: Long = 8L\n  val d: Double = 1.5\n  \
         val b: Boolean = true\n  val s: String = \"s\"\n  val c: Char = 'x'\n  \
         val xs: List[Int] = List(1)\n  \
         val ii = 7\n  val dd = 1.5\n  val bb = true\n}");
    assert!(ok);
    assert_eq!(out, "0\n0\n0.0\nfalse\nnull\n\u{0}\nnull\n0\n0.0\nfalse\n");
}

#[test]
fn a_declared_binding_still_holds_its_initializer_afterwards() {
    // The default is a PRE-store; the declaration must overwrite it, or every
    // annotated top-level `val` in the program would read zero.
    let (out, ok) = run(
        "object T extends App { val i: Int = 7; val d: Double = 1.5; val b: Boolean = true\n  \
         println(i); println(d); println(b); println(i + 1) }",
    );
    assert!(ok);
    assert_eq!(out, "7\n1.5\ntrue\n8\n");
}

/// Print `<class>|<message>` for each expression, the way the oracle probe that
/// captured these expectations did. `null` is what a JVM exception built with no
/// message answers from `getMessage`, and several of these have exactly that.
fn faults(exprs: &[&str]) -> String {
    let body: String = exprs
        .iter()
        .map(|e| {
            format!(
                "try {{ println(\"OK|\" + ({e})) }} \
                 catch {{ case ex: Throwable => println(ex.getClass.getName + \"|\" + ex.getMessage) }}\n"
            )
        })
        .collect();
    let (out, ok) = run(&format!("object T extends App {{\n{body}}}"));
    assert!(ok, "program was rejected outright:\n{body}\n{out}");
    out
}

#[test]
fn an_out_of_range_index_reports_the_receivers_own_exception() {
    // Scala does not have ONE out-of-bounds message. A linear sequence passes
    // the bare index, an indexed one formats the legal span, an `Array` hits the
    // JVM's own array check, and a `StringBuilder` hits `String`'s. Answering
    // `List`'s message for all four was wrong three times over — and note the
    // exception CLASS differs too, so a `catch` selecting on the class behaved
    // differently from Scala.
    assert_eq!(
        faults(&[
            "List(1,2)(5)",
            "Seq(1,2)(5)",
            "Vector(1,2)(5)",
            "IndexedSeq(1,2)(5)",
            "Array(1,2)(5)",
            "scala.collection.mutable.ListBuffer(1,2)(5)",
            "scala.collection.mutable.ArrayBuffer(1,2)(5)",
            "scala.collection.mutable.Queue(1,2)(5)",
            "(1 to 2)(5)",
            "new StringBuilder(\"ab\")(5)",
        ]),
        "java.lang.IndexOutOfBoundsException|5\n\
         java.lang.IndexOutOfBoundsException|5\n\
         java.lang.IndexOutOfBoundsException|5 is out of bounds (min 0, max 1)\n\
         java.lang.IndexOutOfBoundsException|5 is out of bounds (min 0, max 1)\n\
         java.lang.ArrayIndexOutOfBoundsException|Index 5 out of bounds for length 2\n\
         java.lang.IndexOutOfBoundsException|5\n\
         java.lang.IndexOutOfBoundsException|5 is out of bounds (min 0, max 1)\n\
         java.lang.IndexOutOfBoundsException|5 is out of bounds (min 0, max 1)\n\
         java.lang.IndexOutOfBoundsException|5 is out of bounds (min 0, max 1)\n\
         java.lang.StringIndexOutOfBoundsException|Index 5 out of bounds for length 2\n"
    );
}

#[test]
fn an_indexed_write_fails_the_way_the_matching_read_does() {
    // `a(i) = v` used to answer the JVM's ARRAY message for every receiver, the
    // mirror image of the read bug: an `ArrayBuffer` write is a library check,
    // not an array store.
    assert_eq!(
        faults(&[
            "{ val a = Array(1,2); a(5) = 7; a.length }",
            "{ val b = scala.collection.mutable.ArrayBuffer(1,2); b(5) = 7; b.length }",
        ]),
        "java.lang.ArrayIndexOutOfBoundsException|Index 5 out of bounds for length 2\n\
         java.lang.IndexOutOfBoundsException|5 is out of bounds (min 0, max 1)\n"
    );
}

#[test]
fn head_and_last_on_an_empty_receiver_name_that_receiver() {
    // Every one of these was `head of empty list` before, including for a `Map`
    // and a `Set` — which have no head at all and report the ITERATOR they could
    // not advance. `Vector.last` reporting `empty.tail` is not a typo: it is
    // implemented as one, and the JVM prints what ran.
    assert_eq!(
        faults(&[
            "List[Int]().head",
            "Vector[Int]().head",
            "Vector[Int]().last",
            "Array[Int]().head",
            "scala.collection.mutable.ListBuffer[Int]().head",
            "scala.collection.mutable.ArrayBuffer[Int]().last",
            "scala.collection.mutable.Stack[Int]().head",
            "Set[Int]().head",
            "Map[Int,Int]().head",
            "Map[Int,Int]().last",
            "(1 until 1).head",
            "\"\".last",
        ]),
        "java.util.NoSuchElementException|head of empty list\n\
         java.util.NoSuchElementException|empty.head\n\
         java.util.NoSuchElementException|empty.tail\n\
         java.util.NoSuchElementException|head of empty array\n\
         java.util.NoSuchElementException|next on empty iterator\n\
         java.util.NoSuchElementException|last of empty ArrayBuffer\n\
         java.util.NoSuchElementException|head of empty Stack\n\
         java.util.NoSuchElementException|next on empty iterator\n\
         java.util.NoSuchElementException|next on empty iterator\n\
         java.util.NoSuchElementException|next on empty iterator\n\
         java.util.NoSuchElementException|head on empty Range\n\
         java.util.NoSuchElementException|last of empty String\n"
    );
}

#[test]
fn tail_and_init_on_an_empty_receiver_raise_rather_than_answer_empty() {
    // `init` answered an empty collection for EVERY kind, and `"".tail`
    // answered `""` — neither is a Scala behaviour. The mutable buffers raise
    // with no message at all, which `getMessage` reads as `null`; that is a real
    // observable, not a gap in the capture.
    assert_eq!(
        faults(&[
            "List[Int]().tail",
            "List[Int]().init",
            "Vector[Int]().init",
            "Array[Int]().init",
            "scala.collection.mutable.ArrayBuffer[Int]().init",
            "Set[Int]().init",
            "(1 until 1).init",
            "\"\".tail",
            "\"\".init",
        ]),
        "java.lang.UnsupportedOperationException|tail of empty list\n\
         java.lang.UnsupportedOperationException|init of empty list\n\
         java.lang.UnsupportedOperationException|empty.init\n\
         java.lang.UnsupportedOperationException|init of empty array\n\
         java.lang.UnsupportedOperationException|null\n\
         java.lang.UnsupportedOperationException|null\n\
         java.util.NoSuchElementException|init on empty Range\n\
         java.lang.UnsupportedOperationException|tail of empty String\n\
         java.lang.UnsupportedOperationException|init of empty String\n"
    );
}

#[test]
fn a_priority_queue_reports_its_root_read_and_its_iterator_separately() {
    assert_eq!(
        faults(&[
            "scala.collection.mutable.PriorityQueue[Int]().head",
            "scala.collection.mutable.PriorityQueue[Int]().max",
            "scala.collection.mutable.PriorityQueue[Int]().dequeue()",
            "(1 until 1).max",
            "(1 until 1).min",
        ]),
        "java.util.NoSuchElementException|queue is empty\n\
         java.lang.UnsupportedOperationException|empty.max\n\
         java.util.NoSuchElementException|no element to remove from heap\n\
         java.util.NoSuchElementException|last on empty Range\n\
         java.util.NoSuchElementException|head on empty Range\n"
    );
}

#[test]
fn integer_remainder_traps_on_a_zero_divisor_like_the_jvms_irem() {
    // fusevm's native `Op::Mod` answers `0`, so `%` has to route through the
    // host the way `/` does. The JVM's message names `/` for BOTH operators.
    assert_eq!(
        faults(&[
            "1 % 0",
            "1L % 0L",
            "{ val z = 0; 5 % z }",
            "1 / 0",
            "Int.MinValue % -1",
            "-7 % 2",
            "7 % -2",
            "5.5 % 2.0",
            "5.0 % 0.0",
        ]),
        "java.lang.ArithmeticException|/ by zero\n\
         java.lang.ArithmeticException|/ by zero\n\
         java.lang.ArithmeticException|/ by zero\n\
         java.lang.ArithmeticException|/ by zero\n\
         OK|0\nOK|-1\nOK|1\nOK|1.5\nOK|NaN\n"
    );
}

#[test]
fn grouped_and_sliding_report_the_step_their_argument_implies() {
    // Both go through one `require(size > 0 && step > 0)`, which prints BOTH
    // numbers — and `grouped(n)` passes `n` as the step where `sliding(n)`
    // leaves it at 1, so the same argument produces different text.
    assert_eq!(
        faults(&[
            "List(1,2,3).grouped(0).toList",
            "List(1,2,3).sliding(0).toList",
            "List(1,2,3).grouped(-1).toList",
            "List(1,2,3).sliding(-1).toList",
        ]),
        "java.lang.IllegalArgumentException|requirement failed: size=0 and step=0, but both must be positive\n\
         java.lang.IllegalArgumentException|requirement failed: size=0 and step=1, but both must be positive\n\
         java.lang.IllegalArgumentException|requirement failed: size=-1 and step=-1, but both must be positive\n\
         java.lang.IllegalArgumentException|requirement failed: size=-1 and step=1, but both must be positive\n"
    );
}

#[test]
fn a_regex_group_index_and_a_replacement_group_reference_fail_differently() {
    // `Match.group(i)` indexes the match's own arrays; `replaceAll`'s `$n` goes
    // through `Matcher`. Two different exceptions from two different APIs — the
    // frontend used to answer `Matcher`'s wording for both. The array holds
    // group 0, so its length is one more than the capture count.
    assert_eq!(
        faults(&[
            "\"(a)(b)\".r.findFirstMatchIn(\"ab\").get.group(7)",
            "\"ab\".r.findFirstMatchIn(\"ab\").get.group(1)",
            "\"(a)(b)\".r.findFirstMatchIn(\"ab\").get.group(0)",
            "\"a1\".replaceAll(\"[0-9]\", \"$1\")",
        ]),
        "java.lang.ArrayIndexOutOfBoundsException|Index 7 out of bounds for length 3\n\
         java.lang.ArrayIndexOutOfBoundsException|Index 1 out of bounds for length 1\n\
         OK|ab\n\
         java.lang.IndexOutOfBoundsException|No group 1\n"
    );
}

#[test]
fn a_missing_key_and_a_bad_conversion_are_catchable_jvm_exceptions() {
    // Both used to abort the program with wording of the frontend's own, so no
    // `catch` could see them and no JDK ever printed them.
    assert_eq!(
        faults(&[
            "Map(1 -> 2)(5)",
            "Map(\"a\" -> 2)(\"z\")",
            "\"%q\".format(\"x\")",
            "String.format(\"%s %s\", \"x\")",
            "new Array[Int](-1)",
        ]),
        "java.util.NoSuchElementException|key not found: 5\n\
         java.util.NoSuchElementException|key not found: z\n\
         java.util.UnknownFormatConversionException|Conversion = 'q'\n\
         java.util.MissingFormatArgumentException|Format specifier '%s'\n\
         java.lang.NegativeArraySizeException|-1\n"
    );
}

#[test]
fn round_is_the_jdk_algorithm_not_floor_of_x_plus_a_half_nor_rusts_round() {
    // Two shortcuts, each wrong somewhere the other is right. `floor(x + 0.5)`
    // answers 1 for 0.49999999999999994 (adding 0.5 rounds up to exactly 1.0
    // first — JDK-6430675, fixed in Java 7). Rust's `f64::round` is
    // half-AWAY-from-zero, so it answers -3 for -2.5 where `Math.round` is
    // half-UP and answers -2.
    let (out, ok) = run(&wrap(
        "println(List(2.5, -2.5, 3.5, -3.5, 0.5, -0.5, -1.5, 0.49999999999999994, \
         -0.49999999999999994).map(_.round))\n  \
         println(math.round(-2.5)); println(math.round(0.49999999999999994))\n  \
         println(math.round(Double.NaN)); println(math.round(Double.PositiveInfinity))\n  \
         println(math.round(1e300))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "List(3, -2, 4, -3, 1, 0, -1, 0, 0)\n-2\n0\n0\n9223372036854775807\n9223372036854775807\n"
    );
}

#[test]
fn nan_propagates_through_max_and_min_and_sorts_last() {
    // Rust's `f64::max`/`min` IGNORE a NaN operand; Java's propagate it. And the
    // implicit `Ordering[Double]` is `TotalOrdering` — `Double.compare` — under
    // which NaN is above every other value and `-0.0` is below `0.0`. Reading a
    // `partial_cmp` `None` as `Equal` got both wrong: `List(1.0, NaN, 2.0).max`
    // answered 2.0, and a NaN stayed wherever `sorted` first found it.
    let (out, ok) = run(&wrap(
        "println(math.max(1.0, Double.NaN)); println(math.min(Double.NaN, 1.0))\n  \
         println(1.0.max(Double.NaN)); println(1.0.min(Double.NaN))\n  \
         println(List(1.0, Double.NaN, 2.0).max); println(List(1.0, Double.NaN, 2.0).min)\n  \
         println(List(1.0, Double.NaN, 2.0).maxBy(x => x))\n  \
         println(List(Double.NaN, 1.0, -0.0, 0.0, 2.0).sorted)\n  \
         println(Double.NaN.compareTo(1.0)); println((-0.0).compareTo(0.0))\n  \
         println(scala.math.Ordering.Double.TotalOrdering.compare(-0.0, 0.0))\n  \
         println(scala.math.Ordering.Double.IeeeOrdering.compare(Double.NaN, 1.0))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "NaN\nNaN\nNaN\nNaN\nNaN\n1.0\nNaN\nList(-0.0, 0.0, 1.0, 2.0, NaN)\n1\n-1\n-1\n1\n"
    );
}

#[test]
fn the_total_order_does_not_leak_into_the_ieee_comparisons() {
    // `sorted` is a total order; `==` and `<` are not, and must stay IEEE. If
    // `double_total_cmp` had been wired into equality instead, every one of
    // these would flip.
    let (out, ok) = run(&wrap(
        "println(Double.NaN == Double.NaN); println(-0.0 == 0.0)\n  \
         println(1.0 < Double.NaN); println(Double.NaN > 1.0)\n  \
         println(List(Double.NaN).contains(Double.NaN))\n  \
         println(List(0.0, -0.0).distinct.length); println(Set(0.0, -0.0).size)",
    ));
    assert!(ok);
    assert_eq!(out, "false\ntrue\nfalse\nfalse\nfalse\n1\n1\n");
}

#[test]
fn trim_cuts_by_code_point_and_strip_cuts_by_unicode_whitespace() {
    // `String.trim` predates Unicode-aware trimming: it cuts everything at or
    // below U+0020 — including control characters Rust's `trim` leaves — and
    // nothing above it, including the no-break space Rust's `trim` removes.
    // `strip` is the Unicode-aware one, and it disagrees with `trim` in BOTH
    // directions: it keeps U+00A0 (`Character.isWhitespace` excludes the
    // non-breaking separators) and it keeps U+0001 (a control character, but not
    // a whitespace one) while still cutting the tab and the file separator.
    let (out, ok) = run(&wrap(
        "println(\"  a  \".trim + \"|\")\n  \
         println((160.toChar + \"a\").trim + \"|\")\n  \
         println((1.toChar + \"a\").trim + \"|\")\n  \
         println((160.toChar + \"a\").strip + \"|\")\n  \
         println((1.toChar + \"a\").strip + \"|\")\n  \
         println((9.toChar + \"a\").strip + \"|\")\n  \
         println((28.toChar + \"a\").strip + \"|\")",
    ));
    assert!(ok);
    assert_eq!(out, "a|\n\u{a0}a|\na|\n\u{a0}a|\n\u{1}a|\na|\na|\n");
}

#[test]
fn the_numeric_methods_that_only_look_like_their_rust_namesakes() {
    let (out, ok) = run(&wrap(
        "println(List(-0.0, 0.0, -3.0, 3.0, Double.NaN).map(_.signum))\n  \
         println(List(-5, 0, 5).map(_.signum))\n  \
         println(math.signum(-0.0)); println(math.signum(-3.0))\n  \
         println(3.compareTo(5)); println(5.compareTo(3)); println(3.compareTo(3))\n  \
         println(Int.MinValue / -1); println(Int.MinValue % -1); println(Int.MinValue.abs)\n  \
         println(-7 / 2); println(7 / -2); println(-7 % 2); println(7 % -2)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "List(0, 0, -1, 1, 0)\nList(-1, 0, 1)\n-0.0\n-1.0\n-1\n1\n0\n\
         -2147483648\n0\n-2147483648\n-3\n-3\n-1\n1\n"
    );
}

#[test]
fn strip_margin_drops_the_margin_and_leaves_lines_without_one() {
    let (out, ok) = run(&wrap(
        "println(\"|x\\n  |y\".stripMargin)\n  \
         println(\"#x\\n  #y\".stripMargin('#'))\n  \
         println(\"a\\n  |b\\n  c\".stripMargin)",
    ));
    assert!(ok);
    assert_eq!(out, "x\ny\nx\ny\na\nb\n  c\n");
}

#[test]
fn require_and_assert_raise_the_predef_exceptions_with_their_prefixes() {
    // Three functions, two exception classes, three prefixes. `assume` is the
    // one most likely to be assumed identical to `assert` — it is not, it says
    // "assumption failed".
    assert_eq!(
        faults(&[
            "{ require(false); 1 }",
            "{ require(false, \"boom\"); 1 }",
            "{ require(1 > 2, 40 + 2); 1 }",
            "{ assert(false); 1 }",
            "{ assert(false, \"m\"); 1 }",
            "{ assume(false); 1 }",
            "{ require(true); 7 }",
        ]),
        "java.lang.IllegalArgumentException|requirement failed\n\
         java.lang.IllegalArgumentException|requirement failed: boom\n\
         java.lang.IllegalArgumentException|requirement failed: 42\n\
         java.lang.AssertionError|assertion failed\n\
         java.lang.AssertionError|assertion failed: m\n\
         java.lang.AssertionError|assumption failed\n\
         OK|7\n"
    );
}

#[test]
fn the_require_message_is_by_name_and_an_assertion_error_is_not_an_exception() {
    // `message: => Any` is not evaluated when the condition holds — which is why
    // this desugars to an `if`/`throw` rather than to a two-argument builtin, and
    // it is the half a builtin could not have got right. And `AssertionError`
    // extends `Error`, so `catch { case e: Exception }` must NOT see one.
    let (out, ok) = run(&wrap(
        "var n = 0\n  require(true, { n = 1; \"m\" })\n  assert(true, { n = n + 10; \"m\" })\n  \
         println(n)\n  \
         println(try { assert(false); \"ran\" } catch { case e: Error => \"Error\" })\n  \
         println(try { \
           try { assert(false); \"ran\" } catch { case e: Exception => \"Exception\" } \
         } catch { case e: Throwable => \"passed through to \" + e.getClass.getName })",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "0\nError\npassed through to java.lang.AssertionError\n"
    );
}

#[test]
fn string_to_long_parses_and_fails_the_way_to_int_does() {
    let (out, ok) = run(&wrap(
        "println(\"12\".toLong); println(\"9999999999\".toLong)\n  \
         println(try { \"x\".toLong } catch { case e: Throwable => e.getMessage })",
    ));
    assert!(ok);
    assert_eq!(out, "12\n9999999999\nFor input string: \"x\"\n");
}

#[test]
fn the_narrowing_conversions_truncate_rather_than_clamp() {
    // `toByte`/`toShort` are the JVM's `i2b`/`i2s`: the low bits of the two's
    // complement, sign-extended. Nothing here saturates, so 300 becomes 44 and
    // 128 changes SIGN — the opposite of `Double.toInt`, which clamps.
    // A `Double` receiver composes the two rules in that order (`d2i` then
    // `i2b`), which is why 1e20 narrows to -1 rather than to 0, and a `Char` is
    // unsigned and sixteen bits wide, so a high code point also turns negative.
    let (out, ok) = run(&wrap(
        "println(300.toByte); println(128.toByte); println((-129).toByte)\n  \
         println(70000.toShort); println(65535.toShort)\n  \
         println(1000L.toByte); println(100000L.toShort)\n  \
         println(3.7.toByte); println(1e20.toByte); println(Double.NaN.toShort)\n  \
         println(Char.MaxValue.toByte); println('z'.toByte)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "44\n-128\n127\n4464\n-1\n-24\n-31072\n3\n-1\n0\n-1\n122\n"
    );
}

#[test]
fn a_narrowed_value_re_enters_arithmetic_at_int_width() {
    // `Byte` and `Short` have no arithmetic of their own — both widen to `Int`
    // immediately — so `128.toByte + 1` is -127, not 129 and not a second
    // truncation. The `Char` round trip is the same rule at sixteen bits:
    // `200.toByte` is -56, whose low sixteen bits as a code point are 65480.
    let (out, ok) = run(&wrap(
        "println(128.toByte + 1); println(200.toByte.toChar.toInt); println(255.toByte.toInt)",
    ));
    assert!(ok);
    assert_eq!(out, "-127\n65480\n-1\n");
}

#[test]
fn the_radix_renderings_show_the_bit_pattern_at_the_receivers_width() {
    // `toHexString` and its siblings print the two's complement, NOT a sign, so
    // the answer's length is the receiver's width — and the width is the one
    // thing a single `i64` per integer cannot carry. `(-1)` and `(-1L)` are the
    // same runtime value and must still render eight digits and sixteen.
    let (out, ok) = run(&wrap(
        "println(255.toHexString); println((-1).toHexString); println((-8).toBinaryString)\n  \
         println((-1).toOctalString); println(255L.toHexString); println((-1L).toHexString)\n  \
         println((-1L).toOctalString)",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "ff\nffffffff\n11111111111111111111111111111000\n37777777777\nff\n\
         ffffffffffffffff\n1777777777777777777777\n"
    );
}

#[test]
fn the_integer_parses_do_not_trim_and_check_their_own_range() {
    // `StringOps.toInt` IS `Integer.parseInt`, which rejects the padding that
    // `trim` would have removed and refuses a value outside the box's range even
    // when every digit is legal. The two failures are DIFFERENT exceptions'
    // messages: an `Int` out of range is still a format error, while a `Byte`
    // out of range names the value and the radix.
    let (out, ok) = run(&wrap(
        "println(\"42\".toInt); println(\"  42  \".trim.toInt); println(\"127\".toByte)\n  \
         println(\"-32768\".toShort)\n  \
         println(try { \" 42\".toInt } catch { case e: Throwable => e.getMessage })\n  \
         println(try { \"2147483648\".toInt } catch { case e: Throwable => e.getMessage })\n  \
         println(try { \"128\".toByte } catch { case e: Throwable => e.getMessage })\n  \
         println(try { \"7f\".toByte } catch { case e: Throwable => e.getMessage })",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "42\n42\n127\n-32768\n\
         For input string: \" 42\"\n\
         For input string: \"2147483648\"\n\
         Value out of range. Value:\"128\" Radix:10\n\
         For input string: \"7f\"\n"
    );
}

#[test]
fn string_to_boolean_ignores_case_and_fails_with_an_illegal_argument() {
    // `toBoolean` is the one conversion on `String` whose failure is NOT a
    // `NumberFormatException`, and it does not trim either — so `" true"` throws
    // where `"TRUE"` succeeds. A handler that only catches `NumberFormatException`
    // must therefore NOT see it.
    let (out, ok) = run(&wrap(
        "println(\"true\".toBoolean); println(\"FALSE\".toBoolean); println(\"True\".toBoolean)\n  \
         println(try { \"x\".toBoolean } catch { case e: IllegalArgumentException => e.getMessage })\n  \
         println(try { \" true\".toBoolean } catch { case e: IllegalArgumentException => e.getMessage })\n  \
         println(try { \"\".toBoolean } catch \
           { case e: NumberFormatException => \"wrong handler\"; case e: Throwable => e.getMessage })",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "true\nfalse\ntrue\n\
         For input string: \"x\"\n\
         For input string: \" true\"\n\
         For input string: \"\"\n"
    );
}

#[test]
fn a_radix_outside_the_character_bounds_throws_instead_of_aborting() {
    // The JDK checks `Character.MIN_RADIX`/`MAX_RADIX` BEFORE it reads a digit,
    // and each bound has its own message that never quotes the input. This is
    // also the one arm here that was not a wrong answer: Rust's `from_str_radix`
    // PANICS outside `2..=36`, so `Integer.parseInt("12", 40)` used to abort the
    // process with a Rust backtrace rather than raise anything catchable.
    let (out, ok) = run(&wrap(
        "println(Integer.parseInt(\"ff\", 16)); println(Integer.parseInt(\"z\", 36))\n  \
         println(java.lang.Long.parseLong(\"777\", 8))\n  \
         println(try { Integer.parseInt(\"12\", 40) } catch { case e: Throwable => e.getMessage })\n  \
         println(try { Integer.parseInt(\"12\", 1) } catch { case e: Throwable => e.getMessage })\n  \
         println(try { Integer.parseInt(\"zz\", 16) } catch { case e: Throwable => e.getMessage })",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "255\n35\n511\n\
         radix 40 greater than Character.MAX_RADIX\n\
         radix 1 less than Character.MIN_RADIX\n\
         For input string: \"zz\" under radix 16\n"
    );
}

#[test]
fn integer_decode_reads_every_prefix_and_blames_the_subject() {
    // `decode` has a grammar `parseInt` does not: a sign, then `0x`/`0X`/`#`, or
    // a bare leading `0` that makes the REST octal. Its failure quotes what is
    // left after the prefix under the radix the prefix chose — so `"08"` blames
    // `"8" under radix 8`, not `"08"` — and a sign in the wrong place and an
    // empty string each get a message of their own.
    let (out, ok) = run(&wrap(
        "println(Integer.decode(\"0x1f\")); println(Integer.decode(\"0X1F\")); \
         println(Integer.decode(\"#1f\"))\n  \
         println(Integer.decode(\"017\")); println(Integer.decode(\"0\")); \
         println(Integer.decode(\"-0x10\")); println(Integer.decode(\"+0x10\"))\n  \
         println(java.lang.Long.decode(\"0xff\"))\n  \
         println(try { Integer.decode(\"08\") } catch { case e: Throwable => e.getMessage })\n  \
         println(try { Integer.decode(\"0x-1\") } catch { case e: Throwable => e.getMessage })\n  \
         println(try { Integer.decode(\"\") } catch { case e: Throwable => e.getMessage })\n  \
         println(try { Integer.decode(\"0xffffffff\") } catch { case e: Throwable => e.getMessage })",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "31\n31\n31\n15\n0\n-16\n16\n255\n\
         For input string: \"8\" under radix 8\n\
         Sign character in wrong position\n\
         Zero length string\n\
         For input string: \"ffffffff\" under radix 16\n"
    );
}

#[test]
fn the_java_lang_bit_statics_work_at_the_boxs_own_width() {
    // Every one of these is defined on a fixed number of bits, so the SAME
    // argument answers differently through `Integer` and through `Long`:
    // reversing 1 lands on each width's minimum value, and counting the leading
    // zeros of 1 gives 31 or 63. The rotations mask their distance to the width,
    // which is what makes a negative distance well defined.
    let (out, ok) = run(&wrap(
        "println(Integer.reverse(1)); println(java.lang.Long.reverse(1L))\n  \
         println(Integer.numberOfLeadingZeros(1)); println(java.lang.Long.numberOfLeadingZeros(1L))\n  \
         println(Integer.numberOfTrailingZeros(0)); println(java.lang.Long.numberOfTrailingZeros(0L))\n  \
         println(Integer.reverseBytes(1)); println(Integer.highestOneBit(100)); \
         println(Integer.lowestOneBit(12))\n  \
         println(Integer.rotateLeft(1, -1)); println(Integer.rotateRight(1, 1)); \
         println(java.lang.Long.rotateLeft(1L, 1))",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "-2147483648\n-9223372036854775808\n31\n63\n32\n64\n\
         16777216\n64\n4\n-2147483648\n-2147483648\n2\n"
    );
}

#[test]
fn the_unsigned_surface_reads_the_bits_without_a_sign() {
    // The unsigned family reinterprets the same bits: -1 renders as 4294967295
    // through `Integer` and as 18446744073709551615 through `Long`, and an
    // unsigned quotient whose high bit is set comes back NEGATIVE because the
    // result is a bit pattern again. `parseUnsignedInt` runs the trip in
    // reverse, and it is the only parse here that tells bad DIGITS
    // (`For input string:`) apart from a legal value that is too large — however
    // large, so a 26-digit number is still a range error rather than a format one.
    let (out, ok) = run(&wrap(
        "println(java.lang.Integer.toUnsignedString(-1)); \
         println(java.lang.Long.toUnsignedString(-1L))\n  \
         println(java.lang.Integer.toUnsignedString(-1, 16)); \
         println(java.lang.Integer.toUnsignedLong(-1))\n  \
         println(java.lang.Integer.divideUnsigned(-1, 2)); \
         println(java.lang.Integer.remainderUnsigned(-1, 3)); \
         println(java.lang.Integer.compareUnsigned(-1, 1))\n  \
         println(Integer.parseUnsignedInt(\"4294967295\")); \
         println(Integer.parseUnsignedInt(\"ffffffff\", 16))\n  \
         println(try { Integer.parseUnsignedInt(\"4294967296\") } \
           catch { case e: Throwable => e.getMessage })\n  \
         println(try { Integer.parseUnsignedInt(\"99999999999999999999999999\") } \
           catch { case e: Throwable => e.getMessage })\n  \
         println(try { Integer.parseUnsignedInt(\"zz\") } catch { case e: Throwable => e.getMessage })\n  \
         println(try { Integer.parseUnsignedInt(\"-1\") } catch { case e: Throwable => e.getMessage })",
    ));
    assert!(ok);
    assert_eq!(
        out,
        "4294967295\n18446744073709551615\nffffffff\n4294967295\n\
         2147483647\n0\n1\n-1\n-1\n\
         String value 4294967296 exceeds range of unsigned int.\n\
         String value 99999999999999999999999999 exceeds range of unsigned int.\n\
         For input string: \"zz\"\n\
         Illegal leading minus sign on unsigned string -1.\n"
    );
}

#[test]
fn the_float_companions_non_finite_constants_are_the_doubles() {
    // `Float.NaN` and the two infinities have ONE spelling at both widths, so
    // they are exactly representable in this frontend's single floating value
    // type. The finite `Float` constants are deliberately not: they would have
    // to render through `Float.toString`, which needs a single-precision value
    // (BUGS.md). `Double.toString` is the same shortest-round-trip rendering the
    // implicit `"" + d` uses, so a negative zero keeps its sign.
    let (out, ok) = run(&wrap(
        "println(\"\" + Float.NaN + \" \" + Float.PositiveInfinity + \" \" + Float.NegativeInfinity)\n  \
         println(java.lang.Double.toString(-0.0)); println(java.lang.Double.toString(1.5))",
    ));
    assert!(ok);
    assert_eq!(out, "NaN Infinity -Infinity\n-0.0\n1.5\n");
}

#[test]
fn unicode_escapes_decode_in_char_string_and_interpolated_literals() {
    // A `\uXXXX` escape is read by the LEXER, in all three literal forms,
    // and Java allows a run of `u`s before the digits (`\uuu0041` is the same
    // `A`). It is also the only way to write a `Char` this frontend cannot get
    // from a keystroke: `'\uffff'` is the code point `Char.MaxValue` names, and
    // narrowing it is what makes the sixteen-bit-unsigned rule visible.
    // A `\u00e9` is ONE character, not the six that spell it.
    let (out, ok) = run(&wrap(
        r#"println("\u0041b"); println("\uuu0041"); println(s"x\u0041y"); println("\u00e9".length); println('\u0041'); println('\uffff'.toInt); println('\u00ff'.toShort); println('\u0100'.toByte)"#,
    ));
    assert!(ok);
    assert_eq!(out, "Ab\nA\nxAy\n1\nA\n65535\n255\n0\n");
}

#[test]
fn a_brace_group_is_an_argument_and_a_placeholder_inside_it_expands_there() {
    // Scala's two spellings of one argument: `xs.map(_ * 2)` puts it in
    // parentheses, `xs.map { _ * 2 }` puts it in a BLOCK whose value is the
    // argument. The placeholder expands at the smallest enclosing expression,
    // and a block statement is one — so both spellings are `x => x * 2`.
    // Successive placeholders take successive parameters left to right, which
    // is what makes `{ _ - _ }` an asymmetric fold and not `{ _ + _ }` in
    // disguise.
    let (out, ok) = run(&wrap(
        "println(List(1, 2, 3).map { _ * 2 })\n  \
         println(List(1, 2, 3).filter { _ > 1 })\n  \
         println(List(1, 2, 3).foldLeft(0) { _ - _ })\n  \
         println(List(\"a\", \"bb\").map { _.length })\n  \
         println(List(3, 1, 2).sortBy { -_ })",
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "List(2, 4, 6)\nList(2, 3)\n-6\nList(1, 2)\nList(3, 2, 1)\n");
}

#[test]
fn each_brace_group_is_its_own_placeholder_scope() {
    // Nested brace arguments must NOT pool their placeholders into one lambda:
    // the inner `_` belongs to the inner block. Pooling them would make the
    // outer function two-parameter and the inner one zero-parameter, which
    // `map` would still accept from a dynamically typed runtime — and answer
    // wrongly rather than fail.
    let (out, ok) = run(&wrap(
        "println(List(List(1, 2), List(3)).flatMap { _.map { _ * 2 } })\n  \
         println(List(1, 2).map { _ * 2 }.map { _ + 100 })",
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "List(2, 4, 6)\nList(102, 104)\n");
}

#[test]
fn a_brace_argument_is_evaluated_once_not_once_per_element() {
    // The block produces the FUNCTION; the traversal then calls that function
    // per element. A lowering that spliced the block into the lambda body
    // instead would run the leading statement four times here and still print
    // the same list — only the counter can tell the two apart.
    let (out, ok) = run(&wrap(
        "var c = 0\n  \
         println(List(1, 2, 3, 4).map { c += 1; _ * 2 })\n  \
         println(c)",
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "List(2, 4, 6, 8)\n1\n");
}

#[test]
fn a_brace_group_can_stand_in_for_a_plain_calls_argument_clause() {
    // `once { 7 }` is the brace form of `once(7)`, and `use(3) { f }` is the
    // TRAILING CLAUSE of a curried `def` — not an `apply` on what `use(3)`
    // returned. The distinction is the whole test: `def mk(n: Int): Int => Int`
    // called `mk(1)(2)` IS a complete call whose result is applied, and it must
    // keep working.
    let (out, ok) = run(&wrap(
        "def once(v: Int): Int = v * 2\n  \
         def use(n: Int)(f: Int => Int): Int = f(n)\n  \
         def mk(n: Int): Int => Int = (x: Int) => x + n\n  \
         println(once { 7 })\n  \
         println(once { val a = 3; a + 1 })\n  \
         println(use(3) { _ + 1 })\n  \
         println(use(3) { x => x * x })\n  \
         println(use(3)(_ * 2))\n  \
         println(mk(1)(2))",
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "14\n8\n4\n9\n6\n3\n");
}

#[test]
fn a_placeholder_expands_at_a_val_initializer_and_under_an_ascription() {
    // A `val`'s initializer is an expression boundary too, so `_ * 3` is a
    // function there. `(_: Int) + 1` is Scala's TYPED placeholder: the
    // parentheses carry the ascription, NOT the function boundary, so the
    // expansion still belongs to the enclosing expression — reading them as the
    // boundary made the group the identity function and then added 1 to a
    // function.
    let (out, ok) = run(&wrap(
        "val f: Int => Int = _ * 3\n  \
         val g = (_: Int) + 1\n  \
         println(f(4)); println(g(5)); println(List(1, 2).map(f)); println(List(1, 2).map(g))",
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "12\n6\nList(3, 6)\nList(2, 3)\n");
}

#[test]
fn a_bare_placeholder_block_is_refused_the_way_scala_refuses_it() {
    // Scala 3 rejects `xs.map { _ }` with "Unbound placeholder parameter": a
    // block statement that is NOTHING BUT a placeholder is not an expression
    // that CONTAINS one, so there is nothing to expand. Wrapping it into the
    // identity function would answer where the reference refuses.
    rejects(
        &wrap("println(List(1, 2).map { _ })"),
        "`_` placeholder outside an argument",
    );
}

#[test]
fn a_bare_placeholder_argument_expands_at_the_call_around_it() {
    // The other half of the same rule: an argument that is nothing but `_` is
    // not the expression that contains it — the CALL is. So `math.abs(_)` is
    // `x => math.abs(x)` wherever it appears, and `xs.map(f(_))` passes
    // `x => f(x)` rather than `f` itself (which a dynamically typed runtime
    // would accept and answer wrongly, not reject). Expanding at the enclosing
    // STATEMENT instead would lift the whole `println(…)` into a function and
    // print nothing at all.
    let (out, ok) = run(&wrap(
        "val f: Int => Int = math.abs(_)\n  \
         def g(x: Int): Int = x * 10\n  \
         println(f(-3))\n  \
         println(List(-1, 2).map(math.abs(_)))\n  \
         println(List(1, 2).map(g(_)))\n  \
         List(1, 2).foreach(println(_))",
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "3\nList(1, 2)\nList(10, 20)\n1\n2\n");
}

#[test]
fn a_blocks_value_is_its_trailing_if() {
    // Scala's `if` is an expression everywhere, so a block that ends in one has
    // the branch's value — including the block a brace-argument lambda body
    // becomes, which is where this matters most (`xs.map { x => if (p) a else b }`
    // is the ordinary way to write a conditional mapping). Reading the trailing
    // `if` as a statement run for effect made every one of those answer `()`,
    // silently. A branch-less `if` still yields `Unit`, as in Scala.
    let (out, ok) = run(&wrap(
        "println(List(1, 2, 3).map { x => if (x > 1) \"hi\" else \"lo\" })\n  \
         println({ val a = 1; if (a > 0) \"p\" else \"n\" })\n  \
         println({ if (true) 7 else 8 })\n  \
         println({ if (false) 1 })\n  \
         println(List(1, 2, 3).map { x => val d = x * 3; if (d > 5) d else -d })",
    ));
    assert!(ok, "{out}");
    assert_eq!(out, "List(lo, hi, hi)\np\n7\n()\nList(-3, 6, 9)\n");
}
