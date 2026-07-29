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
    let (out, ok) = run(&wrap(
        "println(\"a\")\ntry { println(\"t\") } catch { case e: NumberFormatException => println(\"c\") }\nthrow new IllegalStateException(\"uncaught\")\nprintln(\"never\")",
    ));
    assert!(!ok);
    assert_eq!(out, "a\nt\n");
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
fn a_try_without_catch_or_finally_is_rejected() {
    let (out, ok) = run(&wrap("try { println(1) }"));
    assert!(!ok);
    assert_eq!(out, "");
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
    let (out, ok) = run(&wrap(
        "def f(): Int = { var k = 0; def bump(): Unit = { k += 1 }; bump(); k }\nprintln(f())",
    ));
    assert!(!ok);
    assert_eq!(out, "");
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
    let (out, ok) = run("trait S { def f: Int = 1 }\nobject T extends App { println(new S().f) }");
    assert!(!ok);
    assert_eq!(out, "");
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
    let (out, ok) = run(&wrap("val xs = List(1, 2, 3)\nxs(1) = 9\nprintln(xs)"));
    assert!(!ok);
    assert_eq!(out, "");
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
