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

// ── val immutability + arithmetic exceptions ──────────────────────────────

#[test]
fn reassigning_a_val_is_a_compile_error() {
    // Scala rejects `x = 2` when `x` is a `val`; scalars does too.
    let (_out, ok) = run(&wrap("val x = 1; x = 2; println(x)"));
    assert!(!ok, "reassignment to a `val` must be rejected");
}

#[test]
fn compound_assign_to_a_val_is_a_compile_error() {
    let (_out, ok) = run(&wrap("val x = 1; x += 1; println(x)"));
    assert!(!ok, "compound reassignment to a `val` must be rejected");
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
    let (_out, ok) = run(&wrap("println(1 / 0)"));
    assert!(!ok, "integer `/ 0` must throw, not yield null");
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
    let (_out, ok) = run(
        "object M { def f(x: Int): Int = { x = 5; x }\n  def main(a: Array[String]): Unit = println(f(1)) }",
    );
    assert!(!ok, "reassigning a method parameter must be rejected");
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
    let (_out, ok) = run(&wrap(r#"println("x".frobnicate)"#));
    assert!(
        !ok,
        "an unresolved method must be rejected, not silently null"
    );
}

#[test]
fn substring_out_of_range_throws() {
    // Faithful to Java `String.substring`'s bounds check.
    let (_out, ok) = run(&wrap(r#"println("hi".substring(0, 9))"#));
    assert!(!ok, "an out-of-range substring must throw");
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
    let (_out, ok) = run(&wrap(
        r#"val x = 5; println(x match { case 1 => "one"; case 2 => "two" })"#,
    ));
    assert!(!ok, "a non-exhaustive match must throw scala.MatchError");
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
