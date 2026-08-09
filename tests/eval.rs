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
        let (out, ok) = run(&wrap(src));
        assert!(!ok, "a Char range must be refused: {src}");
        assert_eq!(out, "", "a refused Char range must print nothing: {src}");
    }
    // The integer range it would be confused with is untouched.
    let (out, ok) = run(&wrap("println((1 to 4).toList); println((1 until 4).size)"));
    assert!(ok);
    assert_eq!(out, "List(1, 2, 3, 4)\n3\n");
}
