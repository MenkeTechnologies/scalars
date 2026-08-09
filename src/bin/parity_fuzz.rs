//! Differential parity fuzzer: reference `scala <file>` vs our `scala <file>`.
//!
//! Generates grammar-driven, deterministic-output Scala programs, runs each
//! through both the reference Scala toolchain and this frontend, and reports
//! every case whose stdout OR success/failure diverges. Each program is produced
//! from a per-index seed so any divergence replays exactly:
//! `parity-fuzz --seed <N> --once`.
//!
//! The reference `scala` pays a ~3s JVM/compile cost per invocation, so every
//! program packs many independent probe statements (`--probes`, default 40) into
//! one `object T extends App { … }`; a single invocation therefore exercises
//! dozens of probes. On divergence, [`minimize`] bisects the probe list down to
//! the single offending probe before reporting.
//!
//! The generator is biased toward the historically weak areas of a from-scratch
//! Scala frontend: `Int`-vs-`Double` division dispatch (`7/2==3`, `7/2.0==3.5`),
//! `+` string-concatenation coercion, structural `==`/`!=`, `Double.toString`
//! notation (the decimal/scientific threshold), the range `for` (including `by`
//! steps and their sign-dependent bound test), IEEE division by zero
//! (`x/0.0 == Infinity`, `0.0/0.0 == NaN`), `try`/`catch`/`finally`/`throw`
//! unwinding, block-local `def` scoping and capture, trait/class inheritance
//! and virtual dispatch, `Range` values, `Array` mutation, the `scala.math`
//! vs. `java.lang.Math` overload split, the collection combinators, the
//! `Set`/`Map` representation split (an insertion-ordered `Set1`..`Set4` up to
//! four entries, a CHAMP `HashSet`/`HashMap` in trie order beyond it), and
//! infix method syntax with its `for`-comprehension forms. Pure random bytes only produce mutual
//! parse errors that agree on both sides and teach nothing.
//!
//! Scope + determinism invariants (mirroring the node-js/pythonrs harnesses):
//!   * Only constructs scalars actually implements are emitted — an unsupported
//!     construct would be a known gap, not a parity signal.
//!   * No nondeterministic output (no `Random`, `System.currentTimeMillis`,
//!     identity hashes, unordered collections). Every probe's output is a pure
//!     function of its source.
//!   * Documented known gaps are NOT generated: integer overflow (operands and
//!     products are kept well inside `Int` range), and *uncaught* integer
//!     division/modulo by zero (divisors are non-zero outside the `exc` mode,
//!     where a `try` makes the throw catchable and therefore comparable).
//!     Generating a gap would only reproduce a `BUGS.md` entry.
//!   * Probes that declare top-level types suffix every name with a per-probe
//!     id, so two probes in one program cannot collide. The `localdef` mode is
//!     the deliberate exception: it REUSES one `def` name at different nesting
//!     depths, because block-local shadowing is exactly what it probes.
//!   * `Array.toString` is never printed: Scala emits the JVM identity form
//!     (`[I@1b6d3586`), which is unreproducible by construction (`BUGS.md`).
//!
//! Subprocess-only: this binary never links the scalars library — it compares
//! two `scala` processes, exactly as a user would observe them.
//!
//! Build:  cargo build --bin parity-fuzz
//! Run:    ./target/debug/parity-fuzz --count 300 --probes 40

use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ───────────────────────── deterministic PRNG (splitmix64) ─────────────────

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
}

fn pick<'a, T>(rng: &mut Rng, xs: &'a [T]) -> &'a T {
    &xs[rng.below(xs.len() as u64) as usize]
}

// ───────────────────────── value pools ─────────────────────────────────────

// Small ints so arithmetic stays well inside 32-bit `Int` (no overflow gap).
const INTS: &[&str] = &[
    "0", "1", "2", "3", "5", "7", "9", "-1", "-3", "-7", "12", "42", "-42",
];
// Non-zero divisors (avoids the documented integer /0 gap).
const DIVS: &[&str] = &["1", "2", "3", "4", "5", "-2", "-3", "7"];
// Doubles spanning the decimal and scientific `Double.toString` ranges, plus
// repr edge cases.
const DBLS: &[&str] = &[
    "0.0",
    "-0.0",
    "0.5",
    "1.5",
    "2.5",
    "3.0",
    "3.14",
    "10.0",
    "-1.5",
    "100.0",
    "0.1",
    "0.2",
    "0.001",
    "0.0001",
    "1e7",
    "1e-4",
    "1e21",
    "1.5e300",
    "123456789.0",
    "9999999.0",
    "1e-300",
    "6.022e23",
];
const STRS: &[&str] = &[
    "\"hi\"",
    "\"World\"",
    "\"abc\"",
    "\"\"",
    "\"a\"",
    "\"x=\"",
    "\"café\"",
];
const BOOLS: &[&str] = &["true", "false"];
const AOPS: &[&str] = &["+", "-", "*"];
const FOPS: &[&str] = &["+", "-", "*", "/"];
const DIVMOD: &[&str] = &["/", "%"];
const CMPOPS: &[&str] = &["==", "!=", "<", ">", "<=", ">="];
const EQOPS: &[&str] = &["==", "!="];
const LOGOPS: &[&str] = &["&&", "||"];

// ───────────────────────── generators (one probe each) ─────────────────────
//
// Each returns ONE self-contained statement whose stdout is deterministic and
// which scalars implements. Probes never share state, so [`minimize`] can drop
// any subset and each survivor still reproduces.

fn g_arith(r: &mut Rng) -> String {
    // small operands, depth<=3 -> products stay < 2^31
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let c = pick(r, INTS);
    let (o1, o2) = (pick(r, AOPS), pick(r, AOPS));
    match r.below(4) {
        0 => format!("println({a} {o1} {b} {o2} {c})"),
        1 => format!("println(({a} {o1} {b}) {o2} {c})"),
        2 => format!("println({a} {o1} ({b} {o2} {c}))"),
        _ => format!("println(-{a} {o1} {b})", a = a.trim_start_matches('-')),
    }
}

fn g_intdiv(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, DIVS);
    let op = pick(r, DIVMOD);
    format!("println({a} {op} {b})")
}

fn g_floatarith(r: &mut Rng) -> String {
    let a = pick(r, DBLS);
    let b = pick(r, DBLS);
    let op = pick(r, FOPS);
    // one mixed Int/Double form to exercise the division dispatch
    if r.below(3) == 0 {
        let i = pick(r, INTS);
        let d = pick(r, DBLS);
        return format!("println({i} {op} {d})");
    }
    format!("println({a} {op} {b})")
}

fn g_floatfmt(r: &mut Rng) -> String {
    // raw literal, or a whole-number-producing op, or negation
    let a = pick(r, DBLS);
    match r.below(4) {
        0 => format!("println({a})"),
        1 => format!("println(-{})", pick(r, DBLS).trim_start_matches('-')),
        2 => format!("println({a} + 0.0)"),
        _ => format!("println({} * 2.0)", pick(r, DBLS)),
    }
}

fn g_concat(r: &mut Rng) -> String {
    // Scala 3 has no universal `any2stringadd`: `String + Any` and
    // `numeric + String` are the only valid mixed `+`. `Boolean + String` and
    // `null + String` do NOT compile, so never put a non-numeric value on the
    // LEFT of a `+ String` — that would be invalid Scala, not a parity probe.
    let s = *pick(r, STRS);
    // Any value is legal on the RIGHT of `String +`.
    let any = match r.below(4) {
        0 => *pick(r, INTS),
        1 => *pick(r, DBLS),
        2 => *pick(r, BOOLS),
        _ => "null",
    };
    // Only a numeric value is legal on the LEFT of `+ String`.
    let numeric = if r.below(2) == 0 {
        *pick(r, INTS)
    } else {
        *pick(r, DBLS)
    };
    match r.below(3) {
        0 => format!("println({s} + {any})"),
        1 => format!("println({numeric} + {s})"),
        _ => format!("println({s} + {any} + {s})"),
    }
}

fn g_compare(r: &mut Rng) -> String {
    let op = pick(r, CMPOPS);
    match r.below(3) {
        0 => format!("println({} {op} {})", pick(r, INTS), pick(r, INTS)),
        1 => format!("println({} {op} {})", pick(r, DBLS), pick(r, DBLS)),
        _ => {
            let eq = pick(r, EQOPS);
            format!("println({} {eq} {})", pick(r, STRS), pick(r, STRS))
        }
    }
}

fn g_bool(r: &mut Rng) -> String {
    let a = pick(r, BOOLS);
    let b = pick(r, BOOLS);
    let op = pick(r, LOGOPS);
    match r.below(3) {
        0 => format!("println({a} {op} {b})"),
        1 => format!("println(!{a} {op} {b})"),
        _ => {
            // comparison-driven booleans
            let x = pick(r, INTS);
            let y = pick(r, INTS);
            format!("println({x} > {y} {op} {x} < {y})")
        }
    }
}

fn g_cond(r: &mut Rng) -> String {
    let n = pick(r, INTS);
    let a = pick(r, STRS);
    let b = pick(r, STRS);
    let c = pick(r, STRS);
    format!("{{ val n = {n}; if (n < 0) println({a}) else if (n == 0) println({b}) else println({c}) }}")
}

fn g_loop(r: &mut Rng) -> String {
    let lo = r.below(4) as i64;
    let hi = lo + 1 + r.below(6) as i64;
    match r.below(3) {
        0 => format!("{{ var s = 0; for (i <- {lo} until {hi}) {{ s += i }}; println(s) }}"),
        1 => format!("{{ var s = 0; for (i <- {lo} to {hi}) {{ s += i * i }}; println(s) }}"),
        _ => format!(
            "{{ var s = 0; var i = {lo}; while (i < {hi}) {{ s += i; i += 1 }}; println(s) }}"
        ),
    }
}

/// `by`-step ranges. Both directions and both bound kinds (`until`/`to`), with
/// the step as a literal (compile-time direction) and as a `val` (runtime
/// direction) — those take different lowerings, so both must be probed. Empty
/// ranges (`5 until 0 by 1`) are included deliberately: getting the flipped
/// bound test wrong yields an infinite loop or an off-by-one, and only an empty
/// range catches the "always run once" flavour of that bug.
fn g_step(r: &mut Rng) -> String {
    let lo = r.below(12) as i64;
    let hi = r.below(12) as i64;
    // Non-zero, both signs; `by 0` is generated only by `g_exc` (it throws).
    let step = *pick(r, &["1", "2", "3", "4", "-1", "-2", "-3", "5"]);
    let bound = if r.below(2) == 0 { "until" } else { "to" };
    match r.below(4) {
        0 => format!("{{ var s = \"\"; for (i <- {lo} {bound} {hi} by {step}) s += i + \",\"; println(s) }}"),
        1 => format!("{{ var s = 0; for (i <- {lo} {bound} {hi} by {step}) s += i; println(s) }}"),
        // A `val` step: the direction is only known at runtime.
        2 => format!("{{ val k = {step}; var s = \"\"; for (i <- {lo} {bound} {hi} by k) s += i + \";\"; println(s) }}"),
        // A `yield` over a stepped range, which materializes through a Vector.
        _ => format!("println((for (i <- {lo} {bound} {hi} by {step}) yield i * 2).mkString(\"|\"))"),
    }
}

/// IEEE floating-point division by zero: unlike integer `/ 0` (which throws),
/// `x / 0.0` is `±Infinity` and `0.0 / 0.0` is `NaN`. The sign of the zero and
/// of the dividend both matter (`-1.0 / 0.0` is `-Infinity`, `1.0 / -0.0` is
/// too), so the probe enumerates the sign combinations rather than sampling one.
fn g_ieee(r: &mut Rng) -> String {
    let num = *pick(r, &["1.0", "-1.0", "2.5", "-2.5", "0.0", "-0.0", "1e300"]);
    let den = if r.below(2) == 0 { "0.0" } else { "-0.0" };
    match r.below(5) {
        0 => format!("println({num} / {den})"),
        1 => format!("println(({num} / {den}).isNaN)"),
        // Int-numerator over a Double zero still takes the floating path.
        2 => format!("println({} / {den})", pick(r, INTS)),
        3 => format!("println({num} / {den} + 1.0)"),
        // An Infinity/NaN result flowing through `toString` and comparison.
        _ => format!(
            "println(\"v=\" + ({num} / {den}) + \" eq=\" + ({num} / {den} == {num} / {den}))"
        ),
    }
}

/// `try`/`catch`/`finally`/`throw`. Every probe prints on both the normal and
/// the exceptional path so a handler that silently never fires (or one that
/// fires when it should not) shows up as a stdout difference rather than as an
/// equal-but-empty result.
///
/// Integer `/ 0` and a zero range step ARE generated here, unlike elsewhere in
/// this file: inside a `try` they are no longer the documented uncatchable-abort
/// gap but ordinary catchable exceptions, and they are the cheapest way to raise
/// a *runtime* (rather than user-thrown) exception.
fn g_exc(r: &mut Rng) -> String {
    let msg = *pick(r, &["\"boom\"", "\"x\"", "\"a b\"", "\"\""]);
    let n = pick(r, INTS);
    let d = *pick(r, &["0", "2", "3"]);
    match r.below(13) {
        // Runtime exception (integer /0) caught by its exact type, vs. not raised.
        0 => format!(
            "try {{ println({n} / {d}) }} catch {{ case e: ArithmeticException => println(\"AE:\" + e.getMessage) }}"
        ),
        // Caught by a supertype — exercises the throwable hierarchy walk.
        1 => format!(
            "try {{ println({n} / {d}) }} catch {{ case e: RuntimeException => println(\"RE:\" + e) }}"
        ),
        2 => format!(
            "try {{ println({n} / {d}) }} catch {{ case e: Throwable => println(\"T:\" + e.getMessage) }} finally {{ println(\"fin\") }}"
        ),
        // User `throw`, conditional on a value so both paths are reachable.
        3 => format!(
            "try {{ if ({d} == 0) throw new RuntimeException({msg}); println(\"ok\" + {d}) }} catch {{ case e: RuntimeException => println(\"UT:\" + e.getMessage) }}"
        ),
        // `try` as a value.
        4 => format!(
            "println(try {{ {n} / {d} }} catch {{ case _: ArithmeticException => -1 }})"
        ),
        // Non-matching handler: the exception escapes the inner `try` (running
        // its `finally`) and is caught outside.
        5 => format!(
            "try {{ try {{ println({n} / {d}) }} catch {{ case e: NumberFormatException => println(\"wrong\") }} finally {{ println(\"innerFin\") }} }} catch {{ case e: Throwable => println(\"outer:\" + e.getMessage) }}"
        ),
        // A guard on the handler.
        6 => format!(
            "try {{ throw new IllegalStateException({msg}) }} catch {{ case e: IllegalStateException if e.getMessage.length > 1 => println(\"long:\" + e.getMessage); case e: IllegalStateException => println(\"short:\" + e.getMessage) }}"
        ),
        // Raised inside a loop: the loop must abandon its remaining iterations.
        7 => format!(
            "try {{ for (i <- 0 to 4) {{ println(\"i\" + i); if (i == {}) throw new RuntimeException({msg}) }} }} catch {{ case e: Throwable => println(\"loop:\" + e.getMessage) }}",
            r.below(6)
        ),
        // Raised two frames deep, so both `def` frames must unwind. The `def`
        // names carry a per-probe suffix: scalars hoists every `def` into one
        // flat table, so two probes declaring the same name would collide (a
        // documented gap, not a parity signal).
        8 => {
            let u = r.next_u64() % 100_000;
            format!(
                "{{ def lo{u}(x: Int): Int = 10 / x; def hi{u}(x: Int): Int = lo{u}(x) + 1; try {{ println(hi{u}({d})) }} catch {{ case e: ArithmeticException => println(\"deep:\" + e.getMessage) }} }}"
            )
        }
        // `finally` runs on the normal path too, before the value is produced.
        9 => {
            let u = r.next_u64() % 100_000;
            format!(
                "{{ def f{u}(): Int = try {{ {n} + 1 }} finally {{ println(\"ffin\") }}; println(f{u}()) }}"
            )
        }
        // A zero range step throws IllegalArgumentException.
        10 => format!(
            "try {{ for (i <- 0 until 3 by {}) println(i) }} catch {{ case e: IllegalArgumentException => println(\"step:\" + e.getMessage) }}",
            if r.below(2) == 0 { 0 } else { 2 }
        ),
        // A `var` mutated *inside* the `try` and then read by the handler: a
        // raise part-way through the assignment must not commit garbage to the
        // binding. This is the shape that caught the missing store guard.
        11 => format!(
            "{{ var acc{u} = 7; try {{ acc{u} += 10 / {d} }} catch {{ case _: ArithmeticException => acc{u} += 100 }}; println(acc{u}) }}",
            u = r.next_u64() % 100_000
        ),
        // A library exception (`toInt` on a non-number) plus the success path.
        _ => format!(
            "try {{ println({}.toInt + 1) }} catch {{ case e: NumberFormatException => println(\"NF:\" + e.getMessage) }}",
            pick(r, &["\"12\"", "\"zz\"", "\"-4\"", "\"1x\""])
        ),
    }
}

/// Block-local `def`s: shadowing across nested blocks, capture of an enclosing
/// method's locals, and mutual recursion between two local `def`s. Unlike every
/// other generator these deliberately REUSE one `def` name at different nesting
/// depths — that collision is the whole point, and getting it wrong is a silent
/// wrong answer rather than an error.
fn g_localdef(r: &mut Rng) -> String {
    let u = r.next_u64() % 100_000;
    let a = pick(r, INTS);
    let b = pick(r, &["1", "2", "3", "4"]);
    match r.below(6) {
        // Two sibling methods, each with a same-named local helper.
        0 => format!(
            "{{ def o{u}(): Int = {{ def h(x: Int): Int = x * 2; h({a}) }}; \
               def p{u}(): Int = {{ def h(x: Int): Int = x + 100; h({a}) }}; \
               println(o{u}() + \",\" + p{u}()) }}"
        ),
        // An inner block shadows an outer local `def`, then the outer is back.
        1 => format!(
            "{{ def f(x: Int): Int = x + 1; val r1 = f({a}); \
               val r2 = {{ def f(x: Int): Int = x * 10; f({a}) }}; \
               println(r1 + \"|\" + r2 + \"|\" + f({a})) }}"
        ),
        // Capture of an enclosing method's `val`, through recursion.
        2 => format!(
            "{{ def s{u}(n: Int): Int = {{ val k = {b}; def go(i: Int, acc: Int): Int = \
               if (i > n) acc else go(i + 1, acc + i * k); go(1, 0) }}; println(s{u}(5)) }}"
        ),
        // Mutual recursion where both helpers capture the same enclosing `val`.
        3 => format!(
            "{{ def par{u}(n: Int): String = {{ val t = \"t\"; \
               def ev(k: Int): String = if (k == 0) t + \"E\" else od(k - 1); \
               def od(k: Int): String = if (k == 0) t + \"O\" else ev(k - 1); ev(n) }}; \
               println(par{u}({b})) }}"
        ),
        // A local `def` passed as a function value (eta-expanded with captures).
        4 => format!(
            "{{ def m{u}(xs: List[Int]): List[Int] = {{ val k = {b}; \
               def f(x: Int): Int = x * k; xs.map(f) }}; println(m{u}(List(1, 2, 3))) }}"
        ),
        // A `val` shadowing an outer local `def` name after the block ends.
        _ => format!(
            "{{ val g{u} = {a}; val q = {{ def g{u}(x: Int): Int = x; g{u}(7) }}; \
               println(q + \"/\" + g{u}) }}"
        ),
    }
}

/// Traits, inheritance, overriding, virtual dispatch and `case class` pattern
/// matching. Each probe contributes its own top-level declarations (uniquely
/// suffixed) plus one body statement — see [`TOP_SEP`].
fn g_oop(r: &mut Rng) -> String {
    let sep = TOP_SEP;
    let u = r.next_u64() % 100_000;
    let a = pick(r, &["1", "2", "3", "5"]);
    let b = pick(r, &["2", "4", "7"]);
    match r.below(5) {
        // A trait's concrete method calling an abstract one implemented two ways.
        0 => format!(
            "trait S{u} {{ def area: Int; def name: String = \"s\"; \
               def show: String = name + \"=\" + area }}\n\
             class C{u}(val r: Int) extends S{u} {{ def area: Int = r * r; \
               override def name: String = \"c\" }}\n\
             class D{u}(val w: Int) extends S{u} {{ def area: Int = w + w }}\n\
             {sep}{{ val xs: List[S{u}] = List(new C{u}({a}), new D{u}({b})); \
               xs.foreach(x => println(x.show)) }}"
        ),
        // A three-level class chain with `super` and a forwarded constructor arg.
        1 => format!(
            "class A{u}(val n: String) {{ def speak: String = \"...\"; \
               def intro: String = n + \":\" + speak }}\n\
             class B{u}(m: String) extends A{u}(m) {{ override def speak: String = \"woof\" }}\n\
             class E{u}(m: String) extends B{u}(m) {{ override def speak: String = super.speak + \"!\" }}\n\
             {sep}{{ val xs: List[A{u}] = List(new A{u}(\"g\"), new B{u}(\"r\"), new E{u}(\"p\")); \
               xs.foreach(x => println(x.intro)) }}"
        ),
        // A `case class` with an extra body `val`: toString/equals/unapply all
        // see the primary-constructor prefix only.
        2 => format!(
            "case class P{u}(x: Int, y: Int) {{ val z = x + y }}\n\
             {sep}{{ val p = P{u}({a}, {b}); println(p); println(p.z); \
               println(p == P{u}({a}, {b})); println(p.copy(y = 9)); \
               p match {{ case P{u}(i, j) => println(i * j) }} }}"
        ),
        // A sealed-trait ADT evaluated by pattern match, plus a typed pattern.
        3 => format!(
            "sealed trait X{u}\n\
             case class N{u}(v: Int) extends X{u}\n\
             case class Ad{u}(l: X{u}, rr: X{u}) extends X{u}\n\
             case object Z{u} extends X{u}\n\
             {sep}{{ def ev(e: X{u}): Int = e match {{ case Z{u} => 0; \
               case N{u}(v) => v; case Ad{u}(l, rr) => ev(l) + ev(rr) }}; \
               val t = Ad{u}(N{u}({a}), Ad{u}(N{u}({b}), Z{u})); println(ev(t)); println(t); \
               println(t.isInstanceOf[X{u}]); println(Z{u}) }}"
        ),
        // A mixin whose override calls `super`, over a trait-declared field.
        _ => format!(
            "trait G{u} {{ val pre: String; def greet(n: String): String = pre + n }}\n\
             trait L{u} extends G{u} {{ override def greet(n: String): String = \
               super.greet(n).toUpperCase }}\n\
             class Q{u} extends G{u} {{ val pre = \"hi \" }}\n\
             class R{u} extends G{u} with L{u} {{ val pre = \"yo \" }}\n\
             {sep}{{ println(new Q{u}().greet(\"bob\")); println(new R{u}().greet(\"ann\")); \
               val g: G{u} = new R{u}(); println(g.pre) }}"
        ),
    }
}

/// The pattern-matching forms beyond the simple literal/type/tuple cases:
/// `@` binders, `|` alternations, `::` cons and `Nil`, sequence patterns with a
/// trailing `_*`, nested constructor patterns, and pattern definitions
/// (`val (a, b) = pair`). Every arm set is exhaustive over the values probed, so
/// a `MatchError` is never the expected answer.
fn g_patmatch(r: &mut Rng) -> String {
    let sep = TOP_SEP;
    let u = r.next_u64() % 100_000;
    let n = 3 + r.below(4) as usize;
    let xs = format!("List({})", int_elems(r, n));
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    match r.below(12) {
        // `|` alternation over literals, with a catch-all.
        0 => format!(
            "{{ def f(x: Int): String = x match {{ case 0 | 1 | 2 => \"low\"; \
               case 7 | 9 => \"odd\"; case v if v < 0 => \"neg\"; case v => \"hi\" + v }}; \
             {xs}.foreach(x => println(f(x))); println(f({a})); println(f({b})) }}"
        ),
        // `@` binder around a constructor pattern.
        1 => format!(
            "case class B{u}(x: Int, y: Int)\n{sep}\
             {{ def f(p: B{u}): String = p match {{ case w @ B{u}(0, _) => \"z\" + w; \
               case w @ B{u}(x, y) if x > y => \"gt\" + w.x; case w => \"o\" + w }}; \
             println(f(B{u}(0, {a}))); println(f(B{u}(9, 1))); println(f(B{u}(1, 9))) }}"
        ),
        // `::` cons chains and `Nil`.
        2 => format!(
            "{{ def f(l: List[Int]): String = l match {{ case Nil => \"nil\"; \
               case x :: Nil => \"one\" + x; case x :: y :: Nil => \"two\" + (x + y); \
               case h :: t => \"n\" + h + \"/\" + t.length }}; \
             println(f(Nil)); println(f(List({a}))); println(f(List({a}, {b}))); \
             println(f({xs})) }}"
        ),
        // Sequence patterns, exact and with a trailing `_*`.
        3 => format!(
            "{{ def f(l: List[Int]): String = l match {{ case List() => \"e\"; \
               case List(x) => \"1:\" + x; case List(x, y) => \"2:\" + (x * y); \
               case List(x, rest @ _*) => \"r:\" + x + rest }}; \
             println(f(Nil)); println(f(List({a}))); println(f(List({a}, {b}))); \
             println(f({xs})) }}"
        ),
        // Alternation inside a cons pattern, plus a bound tail.
        4 => format!(
            "{{ def f(l: List[Int]): Int = l match {{ case Nil => -1; \
               case (0 | 1) :: t => t.length; case h :: t => h + t.length }}; \
             println(f(Nil)); println(f(0 :: {xs})); println(f(1 :: Nil)); println(f({xs})) }}"
        ),
        // Nested constructor patterns two levels deep.
        5 => format!(
            "case class In{u}(v: Int)\ncase class Out{u}(k: String, i: In{u})\n{sep}\
             {{ def f(o: Out{u}): String = o match {{ case Out{u}(\"a\", In{u}(0)) => \"a0\"; \
               case Out{u}(\"a\", In{u}(v)) => \"a\" + v; case Out{u}(k, In{u}(v)) => k + v }}; \
             println(f(Out{u}(\"a\", In{u}(0)))); println(f(Out{u}(\"a\", In{u}({a})))); \
             println(f(Out{u}(\"b\", In{u}({b})))) }}"
        ),
        // Pattern definitions: tuple, constructor, cons.
        6 => format!(
            "case class D{u}(x: Int, y: String)\n{sep}\
             {{ val (p, q) = ({a}, \"k\"); println(p + q); \
             val D{u}(dx, dy) = D{u}({b}, \"m\"); println(dx + dy); \
             val hd :: tl = {xs}; println(hd); println(tl); \
             val ((m, n2), o2) = (({a}, {b}), \"t\"); println(m + n2 + o2) }}"
        ),
        // Tuple patterns whose elements are themselves patterns.
        7 => format!(
            "{{ def f(t: (List[Int], Int)): String = t match {{ case (Nil, n) => \"e\" + n; \
               case (h :: _, n) if h > n => \"gt\" + h; case (h :: _, n) => \"le\" + (h + n) }}; \
             println(f((Nil, {a}))); println(f(({xs}, -99))); println(f(({xs}, 99))) }}"
        ),
        // `Option` patterns, including a bound `Some`.
        8 => format!(
            "{{ def f(o: Option[Int]): String = o match {{ case Some(0) => \"z\"; \
               case s @ Some(v) if v < 0 => \"n\" + v + s; case Some(v) => \"p\" + v; \
               case None => \"e\" }}; \
             println(f(Some(0))); println(f(Some({a}))); println(f(Some({b}))); println(f(None)) }}"
        ),
        // String literal alternation.
        9 => format!(
            "{{ def f(s: String): Int = s match {{ case \"a\" | \"b\" => 1; \
               case \"cc\" | \"dd\" | \"z\" => 2; case _ => 3 }}; \
             List({}).foreach(s => println(f(s))) }}",
            str_elems(r, 4)
        ),
        // A pattern-matching anonymous function over a cons pattern.
        10 => format!(
            "{{ val ls: List[List[Int]] = List({xs}, Nil, List({a})); \
             println(ls.map {{ case Nil => 0; case h :: t => h + t.length }}); \
             println(ls.collect {{ case h :: _ if h > 0 => h }}) }}"
        ),
        // A sealed ADT with alternation across two cases. Scala forbids a
        // variable binding inside an alternative, so the alternatives are
        // binding-free (`_`) and the payload is read through a separate arm.
        _ => format!(
            "sealed trait T{u}\ncase class L{u}(v: Int) extends T{u}\n\
             case class M{u}(v: Int) extends T{u}\ncase object N{u} extends T{u}\n{sep}\
             {{ def f(t: T{u}): Int = t match {{ case L{u}(0) | M{u}(0) | N{u} => -1; \
               case L{u}(v) => v * 2; case M{u}(v) => v * 3 }}; \
             println(f(L{u}({a}))); println(f(M{u}({b}))); println(f(N{u})); \
             println(f(L{u}(0))); println(f(M{u}(0))) }}"
        ),
    }
}

/// `scala.Option`'s method surface — the combinators, the conversions, the
/// `Either` results of `toRight`/`toLeft`, and the `Option(x)` factory's `null`
/// case. Both the `Some` and the `None` side of every probe is printed, because
/// the empty case is where a from-scratch implementation drifts.
fn g_option(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let n = 3 + r.below(3) as usize;
    let xs = format!("List({})", int_elems(r, n));
    // Two receivers with the same static type, one empty, so every probe below
    // exercises both branches of the method it names.
    let pre = format!("val s: Option[Int] = Some({a}); val e: Option[Int] = None; ");
    let both = |m: &str| format!("{{ {pre}println(s.{m}); println(e.{m}) }}");
    match r.below(12) {
        0 => both(&format!("getOrElse({b})")),
        1 => both("map(_ * 2)"),
        2 => both("flatMap(x => if (x > 0) Some(x + 1) else None)"),
        3 => both(&format!("filter(_ > {b})")),
        4 => both(&format!("exists(_ > {b})")),
        5 => both(&format!("fold(-1)(_ + {b})")),
        6 => format!(
            "{{ {pre}println(s.isEmpty); println(e.isEmpty); println(s.isDefined); \
             println(e.isDefined); println(s.size); println(e.size); \
             println(s.nonEmpty); println(e.nonEmpty) }}"
        ),
        7 => format!(
            "{{ {pre}println(s.toList); println(e.toList); println(s.toRight(\"z\")); \
             println(e.toRight(\"z\")); println(s.toLeft(\"z\")); println(e.toLeft(\"z\")) }}"
        ),
        8 => format!(
            "{{ {pre}println(s.orElse(Some({b}))); println(e.orElse(Some({b}))); \
             println(s.contains({a})); println(e.contains({a})); \
             println(s.forall(_ > 0)); println(e.forall(_ > 0)) }}"
        ),
        9 => format!(
            "{{ println({xs}.find(_ > {b})); println({xs}.headOption); \
             println(List[Int]().headOption); println({xs}.lastOption); \
             println(List(Some({a}), None, Some({b})).flatten) }}"
        ),
        10 => format!(
            "{{ println(Option({a})); println(Option(null)); \
             println(Option({a}).map(_ + 1)); \
             println({xs}.map(x => Option(x).filter(_ > 0))) }}"
        ),
        _ => format!(
            "{{ {pre}println(for (x <- s) yield x * 3); println(for (x <- e) yield x * 3); \
             println(s.collect {{ case v if v > {b} => v }}); \
             println(e.collect {{ case v if v > {b} => v }}); \
             println(s.count(_ > {b})); println(e.count(_ > {b})) }}"
        ),
    }
}

/// A `case class`'s derived members: `Product` (`productArity`/`productPrefix`/
/// `productElement`/`productIterator`), `copy` with named and positional
/// updates, structural `equals`, and the `Either` cases.
fn g_caseclass(r: &mut Rng) -> String {
    let sep = TOP_SEP;
    let u = r.next_u64() % 100_000;
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let s = pick(r, STRS);
    match r.below(5) {
        0 => format!(
            "case class C{u}(x: Int, y: String)\n{sep}\
             {{ val c = C{u}({a}, {s}); println(c.productArity); println(c.productPrefix); \
             println(c.productElement(0)); println(c.productElement(1)); \
             println(c.productIterator.toList) }}"
        ),
        1 => format!(
            "case class C{u}(x: Int, y: Int, z: Int)\n{sep}\
             {{ val c = C{u}({a}, {b}, 0); println(c); println(c.copy(z = 5)); \
             println(c.copy(x = 1, y = 2)); println(c.copy()); println(c == c.copy()) }}"
        ),
        // A body `val` is NOT a product element — only the constructor prefix is.
        2 => format!(
            "case class C{u}(x: Int) {{ val doubled = x * 2 }}\n{sep}\
             {{ val c = C{u}({a}); println(c); println(c.doubled); println(c.productArity); \
             println(c.productIterator.toList); println(c == C{u}({a})) }}"
        ),
        3 => format!(
            "case object O{u}\ncase class C{u}(x: Int)\n{sep}\
             {{ println(O{u}); println(C{u}({a})); println(C{u}({a}) == C{u}({a})); \
             println(C{u}({a}) == C{u}({b})); println(List(C{u}({a}), C{u}({a})).distinct.length) }}"
        ),
        _ => format!(
            "{{ val rs: List[Either[String, Int]] = List(Right({a}), Left(\"e\"), Right({b})); \
             println(rs); println(rs.collect {{ case Right(v) => v }}); \
             println(rs.collect {{ case Left(m) => m }}); \
             println(rs.map {{ case Right(v) => v * 2; case Left(m) => m.length }}) }}"
        ),
    }
}

/// `StringOps`/`java.lang.String`'s wider surface: the index searches, the
/// total slicing operations, the char-level combinators, and the `Char`
/// predicates. Every receiver is a literal so the expected output is fixed.
fn g_strops(r: &mut Rng) -> String {
    let s = *pick(
        r,
        &[
            "\"Hello, World\"",
            "\"abcabc\"",
            "\"\"",
            "\"x\"",
            "\"Aa Bb\"",
        ],
    );
    let needle = *pick(r, &["\"a\"", "\"o\"", "\"bc\"", "\"z\"", "\" \""]);
    let k = r.below(5) as i64;
    match r.below(10) {
        0 => println_all(&[
            format!("{s}.indexOf({needle})"),
            format!("{s}.lastIndexOf({needle})"),
            format!("{s}.contains({needle})"),
        ]),
        1 => println_all(&[
            format!("{s}.take({k})"),
            format!("{s}.drop({k})"),
            format!("{s}.slice(1, {k})"),
        ]),
        2 => println_all(&[
            format!("{s}.takeRight({k})"),
            format!("{s}.dropRight({k})"),
            format!("{s}.splitAt({k})"),
        ]),
        3 => println_all(&[
            format!("{s}.replace({needle}, \"-\")"),
            format!("{s}.stripPrefix(\"a\")"),
            format!("{s}.stripSuffix(\"c\")"),
        ]),
        4 => println_all(&[
            format!("{s}.capitalize"),
            format!("{s}.distinct"),
            format!("{s}.sorted"),
        ]),
        5 => println_all(&[
            format!("{s}.compareTo(\"abc\")"),
            format!("{s}.equalsIgnoreCase(\"HELLO, WORLD\")"),
            format!("{s}.mkString(\"-\")"),
        ]),
        6 => println_all(&[
            format!("{s}.filter(_ != 'a')"),
            format!("{s}.count(_ == 'a')"),
            format!("{s}.exists(_ == 'b')"),
        ]),
        7 => println_all(&[
            format!("{s}.map(c => c.toUpper)"),
            format!("{s}.takeWhile(_ != 'b')"),
            format!("{s}.dropWhile(_ != 'b')"),
        ]),
        8 => println_all(&[
            format!("{s}.forall(_ != 'q')"),
            format!("{s}.indexWhere(_ == 'b')"),
            format!("{s}.find(_ == 'b')"),
        ]),
        9 => println_all(&[
            format!("{s}.partition(_ < 'c')"),
            format!("{s}.span(_ != 'b')"),
            format!("{s}.zipWithIndex"),
        ]),
        // `StringOps.*` — the infix form and the dotted method form of the same
        // method, plus a count of zero or less (which answers the empty string).
        10 => println_all(&[
            format!("{s} * {k}"),
            format!("{s}.*({k})"),
            format!("({s} * 2).length"),
        ]),
        // `StringOps.map`'s two overloads: a `Char => Char` body rebuilds a
        // `String`, a `Char => B` one answers an `ArraySeq` — including the
        // `_.toString` body, whose one-char results are indistinguishable from
        // `Char`s at run time.
        11 => println_all(&[
            format!("{s}.map(_.toString)"),
            format!("{s}.map(c => c + \"!\")"),
            format!("{s}.toSeq"),
        ]),
        // Every operator is a method: the dotted spelling must answer what the
        // infix one does, including the `/`-truncation and `+`-concatenation
        // dispatch.
        _ => println_all(&[
            format!("{k}.+({k})"),
            format!("7./({})", 1 + r.below(4)),
            format!("{s}.<(\"m\")"),
        ]),
    }
}

/// Type ascription in expression position and the unit literal — the two
/// parenthesized forms that are neither a grouping nor a tuple.
///
/// An ascription is dropped by a dynamically typed runtime except for a numeric
/// widening, which is observable WITHOUT a type checker (`(3: Double)` prints
/// `3.0`, not `3`), so both kinds are generated. `()` is Scala's `Unit` value and
/// prints `()`.
fn g_ascribe(r: &mut Rng) -> String {
    let i = *pick(r, INTS);
    let d = *pick(r, DBLS);
    let s = *pick(r, &["\"ab\"", "\"\"", "\"Hello\"", "\"a b\""]);
    match r.below(10) {
        0 => println_all(&[format!("({i}: Double)"), format!("({i}: Int)")]),
        1 => println_all(&[format!("({i}: Any)"), format!("({d}: Double)")]),
        2 => println_all(&[format!("({i}: Double) + 1"), format!("({i}: Long) + 1")]),
        3 => println_all(&[format!("({s}: String).length"), format!("({s}: Any)")]),
        4 => format!("println((List({}): Seq[Int]).length)", int_elems(r, 3)),
        5 => "{ val u = (); println(u); println(()) }".to_string(),
        6 => "println(() == ())".to_string(),
        7 => format!("{{ val f = (x: Int) => (); println(f({i})) }}"),
        8 => "println((None: Option[Int]).isEmpty)".to_string(),
        _ => format!("println((Some({i}): Option[Int]).getOrElse(0))"),
    }
}

/// The `y = e` enumerator of a `for` comprehension, over both lowerings: inline
/// inside a counted range loop, and Scala's generator-pairing translation over a
/// collection (where a later `if` guard has to see the defined name too).
fn g_forval(r: &mut Rng) -> String {
    let n = 3 + r.below(3) as usize;
    let xs = format!("List({})", int_elems(r, n));
    let k = *pick(r, &["1", "2", "3", "-2"]);
    let lim = *pick(r, &["0", "2", "-3", "10"]);
    match r.below(10) {
        0 => format!("println(for {{ x <- {xs}; y = x * {k} }} yield y)"),
        1 => format!("println(for {{ x <- {xs}; y = x + {k}; if y > {lim} }} yield y)"),
        2 => format!("println(for {{ x <- {xs}; y = x * {k}; z = y + 1 }} yield x + y + z)"),
        3 => format!("println(for {{ i <- 1 to {n}; s = i * i }} yield s)"),
        4 => format!("println(for {{ i <- 1 to {n}; s = i * {k}; if s != 0 }} yield s)"),
        5 => "{ val mv = Map(\"a\" -> 1, \"b\" -> 2); \
              println(for { (mk, v) <- mv; d = v * 10 } yield mk + d) }"
            .to_string(),
        6 => format!("println(for {{ x <- {xs}; y <- List(1, 2); p = x * y }} yield p)"),
        7 => format!("for {{ x <- {xs}; y = x - {k} }} println(y)"),
        8 => format!("for {{ i <- 1 to {n}; s = i + {k}; if s > {lim} }} println(s)"),
        _ => format!("println(for {{ x <- {xs}; if x > {lim}; y = x * {k} }} yield y)"),
    }
}

/// `java.util.regex` through its three Scala doorways: `String.matches`/
/// `replaceAll`/`replaceFirst`/`split`, the `"…".r` `Regex` object, and its
/// `Match` groups. `findAllIn` is always CONSUMED (`.toList`/`.mkString`) — the
/// un-consumed `MatchIterator` is the documented strict-iterator gap.
fn g_regex(r: &mut Rng) -> String {
    let s = *pick(
        r,
        &[
            "\"a1b22c\"",
            "\"2024-01-02\"",
            "\"hello world\"",
            "\"\"",
            "\"abc\"",
            "\"a,b,,c\"",
            "\"xx9\"",
        ],
    );
    // Group-free patterns: safe with any replacement that has no `$N`.
    let plain = *pick(
        r,
        &[
            "\"[0-9]+\"",
            "\"[a-z]\"",
            "\"\\\\d\"",
            "\"\\\\s\"",
            "\"[abc]\"",
            "\",\"",
            "\"x*\"",
        ],
    );
    // Two-group patterns, for the `$1`/`$2` replacement and `Match.group` probes.
    let grouped = *pick(
        r,
        &["\"([a-z])([0-9])\"", "\"(\\\\d)(\\\\d)\"", "\"(a)(b)\""],
    );
    let rep = *pick(r, &["\"#\"", "\"\"", "\"-\"", "\"[]\""]);
    match r.below(12) {
        0 => println_all(&[
            format!("{s}.matches({plain})"),
            format!("{s}.matches(\".*\")"),
        ]),
        1 => println_all(&[
            format!("{s}.replaceAll({plain}, {rep})"),
            format!("{s}.replaceFirst({plain}, {rep})"),
        ]),
        2 => format!("println({s}.split({plain}).toList)"),
        3 => format!("println({s}.replaceAll({grouped}, \"$2$1\"))"),
        4 => format!("println({s}.replaceAll({grouped}, \"<$0>\"))"),
        5 => format!("println({s}.r)"),
        6 => format!("println({plain}.r.findFirstIn({s}))"),
        7 => format!("println({plain}.r.findAllIn({s}).toList)"),
        8 => format!("println({plain}.r.findAllIn({s}).mkString(\"|\"))"),
        9 => println_all(&[
            format!("{plain}.r.replaceAllIn({s}, {rep})"),
            format!("{plain}.r.replaceFirstIn({s}, {rep})"),
        ]),
        10 => println_all(&[
            format!("{plain}.r.matches({s})"),
            format!("{plain}.r.regex"),
        ]),
        _ => format!("println({grouped}.r.findFirstMatchIn({s}).map(_.group(1)))"),
    }
}

/// `Char` as its own type: a value that dispatches as a NUMBER in arithmetic and
/// as TEXT when printed.
///
/// The two faces are what make this worth fuzzing — `'a' + 1` is the `Int` 98
/// while `'a' + "b"` is the `String` `"ab"`, `'5'.toInt` is the code point 53
/// while `"5".toInt` parses to 5, and `"abc".map(_.toUpper)` is a `String` while
/// `"abc".map(_.toInt)` is an `ArraySeq`. Every probe crosses at least one of
/// those boundaries, including the ones a `Char` reaches only through a lambda
/// or a collection, where no static type is left to recover it from.
fn g_char(r: &mut Rng) -> String {
    let c = *pick(r, &["'a'", "'z'", "'A'", "'5'", "'0'", "' '", "'~'"]);
    let d = *pick(r, &["'b'", "'a'", "'Z'", "'9'"]);
    let s = *pick(r, &["\"abc\"", "\"\"", "\"aabbc\"", "\"xyz\"", "\"a1b2\""]);
    let n = *pick(r, &["0", "1", "2", "7", "-1"]);
    match r.below(21) {
        // The text face: printing, interpolation, and as a collection element.
        0 => println_all(&[
            c.to_string(),
            format!("{c}.toString"),
            format!("s\"[${{{c}}}]\""),
        ]),
        1 => println_all(&[
            format!("List({c}, {d})"),
            format!("Some({c})"),
            format!("({c}, {n})"),
        ]),
        // The numeric face.
        2 => println_all(&[
            format!("{c}.toInt"),
            format!("{c}.toLong"),
            format!("{c}.toDouble"),
        ]),
        3 => println_all(&[
            format!("{c} + {n}"),
            format!("{n} + {c}"),
            format!("{c} + {d}"),
        ]),
        4 => println_all(&[format!("{c} - {d}"), format!("{c} * 2"), format!("-{c}")]),
        // `/` and `%` are integer division on the code points (a `Char` divisor
        // is never 0, so this stays outside the documented /0 gap).
        5 => println_all(&[format!("{c} / {d}"), format!("{c} % {d}")]),
        // `+` against a String is concatenation, NOT arithmetic — the one case
        // where the numeric face gives way.
        6 => println_all(&[format!("{c} + {s}"), format!("{s} + {c}")]),
        7 => println_all(&[
            format!("{c} < {d}"),
            format!("{c} == {d}"),
            format!("{c}.compare({d})"),
        ]),
        8 => println_all(&[
            format!("{c}.toUpper"),
            format!("{c}.toLower"),
            format!("{c}.toChar"),
        ]),
        9 => println_all(&[
            format!("{c}.isDigit"),
            format!("{c}.isLetter"),
            format!("{c}.isWhitespace"),
        ]),
        // The round trip through the code point and back.
        10 => println_all(&[
            format!("({c} + {n}).toChar"),
            format!("{c}.toInt.toChar"),
            format!("{c}.hashCode"),
        ]),
        // Char-ness surviving a lambda: `_.toUpper` keeps a String, `_.toInt`
        // does not.
        11 => println_all(&[
            format!("{s}.map(_.toUpper)"),
            format!("{s}.map(_.toInt)"),
            format!("{s}.map(_.toString)"),
        ]),
        // …and surviving a collection.
        12 => println_all(&[
            format!("{s}.toList"),
            format!("{s}.toVector"),
            format!("{s}.toList.map(_.toInt)"),
        ]),
        13 => println_all(&[
            format!("{s}.filter(_ != {d})"),
            format!("{s}.count(_ == {c})"),
            format!("{s}.exists(_ == {d})"),
        ]),
        14 => println_all(&[
            format!("{s}.indexOf({d})"),
            format!("{s}.contains({d})"),
            format!("{s}.headOption"),
        ]),
        15 => println_all(&[
            format!("{s}.foldLeft(0)((a, ch) => a + ch.toInt)"),
            format!("{s}.sortWith(_ > _)"),
        ]),
        // `Char` as a hashed key — its `hashCode` is the code point, which is
        // what puts it in the right CHAMP slot.
        16 => println_all(&[
            format!("{s}.groupBy(ch => ch)"),
            format!("{s}.toSet"),
            format!("Set({c}, {d})"),
        ]),
        17 => println_all(&[
            format!("{s}.flatMap(ch => List(ch, ch))"),
            format!("{s}.collect {{ case ch if ch != {d} => ch }}"),
        ]),
        18 => println_all(&[
            format!("{s}.zipWithIndex"),
            format!("{s}.reverse"),
            format!("{s}.distinct"),
        ]),
        19 => println_all(&[
            format!("{c}.max({d})"),
            format!("{c}.min({d})"),
            format!("{c}.equals({d})"),
        ]),
        // A `val` whose line ENDS with a char literal, so the next line has to
        // be inferred as a new statement. Every other probe buries its literal
        // inside a `println(…)`, which never exercises the newline rule — and a
        // `Char` token missing from it silently glued two statements together.
        _ => {
            let u = r.next_u64() % 100_000;
            format!("val ch{u} = {c}\nprintln(ch{u})\nprintln(ch{u}.toInt)")
        }
    }
}

/// A `Regex` in PATTERN position — `case p(a, b) =>` — plus the `case`
/// generator that filters on a refutable pattern.
///
/// `Regex.unapplySeq` matches the WHOLE input, so `"a1"` does not match
/// `"([0-9]+)".r` even though it contains a digit run; a group that did not
/// participate binds `null`. Scala 3 requires the `case` keyword on a refutable
/// generator (`for (Some(x) <- xs)` does not compile), and that form filters
/// non-matching elements rather than raising.
fn g_patregex(r: &mut Rng) -> String {
    let u = r.next_u64() % 100_000;
    let two = *pick(
        r,
        &[
            "\"([a-z]+)@([a-z]+)\"",
            "\"([0-9]+)-([0-9]+)\"",
            "\"(a+)(b+)\"",
        ],
    );
    let one = *pick(r, &["\"([0-9]+)\"", "\"([a-z]+)\"", "\"(x*)\""]);
    let subj = *pick(
        r,
        &[
            "\"me@host\"",
            "\"12-34\"",
            "\"aabb\"",
            "\"123\"",
            "\"a1\"",
            "\"\"",
            "\"abc\"",
        ],
    );
    match r.below(8) {
        // Two-group extraction, and the `case _` fallback that a whole-input
        // mismatch must reach.
        0 => format!(
            "val p{u} = {two}.r; println({subj} match {{ case p{u}(a, b) => a + \"/\" + b; case _ => \"no\" }})"
        ),
        1 => format!(
            "val p{u} = {one}.r; println({subj} match {{ case p{u}(a) => \"[\" + a + \"]\"; case _ => \"no\" }})"
        ),
        // A group that did not participate binds `null`.
        2 => format!(
            "val p{u} = \"([0-9]+)?-([0-9]+)\".r; println({subj} match {{ case p{u}(a, b) => \"\" + a + \"|\" + b; case _ => \"no\" }})"
        ),
        // The constructor spelling of `"…".r`.
        3 => println_all(&[
            format!("new Regex({one}).findFirstIn({subj})"),
            format!("new Regex({one})"),
        ]),
        // A `case` generator over a refutable extractor: only whole-input
        // matches survive.
        4 => format!(
            "val p{u} = {one}.r; println(for (case p{u}(a) <- List({subj}, \"22\", \"zz\")) yield a)"
        ),
        // …and over a constructor pattern.
        5 => format!(
            "println(for (case Some(x) <- List(Some(1), None, Some({}))) yield x * 2)",
            r.below(9) + 1
        ),
        6 => format!(
            "for (case Some(x) <- List(None, Some({}), None)) println(x)",
            r.below(9) + 1
        ),
        // An irrefutable pattern needs no `case`, and must keep working.
        _ => format!(
            "println(for ((k, v) <- List((1, {}), (2, {}))) yield k + v)",
            r.below(5),
            r.below(5)
        ),
    }
}

/// A closure that ASSIGNS a `var` of the frame that declares it — the shape that
/// only works if the binding is boxed rather than captured by value. The result
/// is always read back from the enclosing frame, so a write that never arrived
/// is a divergence rather than a silently discarded one.
fn g_capture(r: &mut Rng) -> String {
    let u = r.next_u64() % 100_000;
    let n = 3 + r.below(3) as usize;
    let xs = format!("List({})", int_elems(r, n));
    let ws = format!("List({})", str_elems(r, 3));
    let k = *pick(r, &["1", "2", "3", "-2"]);
    match r.below(10) {
        0 => format!(
            "{{ def c{u}(): Int = {{ var t = 0; {xs}.foreach(x => t += x); t }}; println(c{u}()) }}"
        ),
        1 => format!(
            "{{ def c{u}(): Int = {{ var t = {k}; val add = (x: Int) => {{ t = t * x }}; \
               add(3); add(5); t }}; println(c{u}()) }}"
        ),
        2 => format!(
            "{{ def c{u}(): String = {{ var s = \"\"; {ws}.foreach(w => s = s + w); s }}; \
               println(c{u}()) }}"
        ),
        3 => format!(
            "{{ def c{u}(): Int = {{ var t = 0; for (x <- {xs}) {{ t += x * {k} }}; t }}; \
               println(c{u}()) }}"
        ),
        4 => format!(
            "{{ def c{u}(): Int = {{ var t = 0; {xs}.foreach(a => {xs}.foreach(b => t += a + b)); \
               t }}; println(c{u}()) }}"
        ),
        // A closure that OUTLIVES the frame that declared the `var`.
        5 => format!(
            "{{ def mk{u}(): () => Int = {{ var i = {k}; () => {{ i += 1; i }} }}; \
               val f = mk{u}(); println(f() + \",\" + f() + \",\" + f()) }}"
        ),
        6 => format!(
            "{{ def c{u}(): Int = {{ var t = 0; {xs}.foreach(x => if (x > 0) t += x); t }}; \
               println(c{u}()) }}"
        ),
        7 => format!(
            "{{ def c{u}(): Int = {{ var t = 0; var n2 = 0; \
               {xs}.foreach(x => {{ t += x; n2 += 1 }}); t * 100 + n2 }}; println(c{u}()) }}"
        ),
        // A `var` the closure writes but the frame reads only afterwards, with a
        // range comprehension (an inline counted loop) in between.
        8 => format!(
            "{{ def c{u}(): Int = {{ var t = 0; for (i <- 1 to {n}) t += i; \
               {xs}.foreach(x => t += x); t }}; println(c{u}()) }}"
        ),
        _ => format!(
            "{{ def c{u}(): Int = {{ var t = 0; {xs}.map(x => {{ t += 1; x }}); t }}; \
               println(c{u}()) }}"
        ),
    }
}

/// Non-local `return` and `finally` ordering — the two shapes where a `return`
/// does NOT simply pop the frame: one inside a lambda (a `for`/`foreach` body),
/// which must leave the enclosing `def`, and one inside a `try`, which must run
/// every enclosing `finally` on the way out. Both print from the finalizer, so
/// the ORDER of the output is part of the comparison, not just the result.
fn g_nlr(r: &mut Rng) -> String {
    let n = 3 + r.below(3) as usize;
    let xs = format!("List({})", int_elems(r, n));
    let lim = pick(r, &["0", "3", "-5", "10"]);
    // A second, always-positive knob so the `try`/`finally` probes vary their
    // loop bound / returned value instead of repeating one fixed program.
    let stop = 1 + r.below(6);
    match r.below(8) {
        // `return` out of a `for` over a collection (a lambda body).
        0 => format!(
            "{{ def f(l: List[Int]): String = {{ for (x <- l) {{ if (x > {lim}) return \"hit\" + x }}; \
             \"none\" }}; println(f({xs})); println(f(Nil)) }}"
        ),
        // `return` out of an explicit `foreach` lambda.
        1 => format!(
            "{{ def f(l: List[Int]): Int = {{ l.foreach(x => if (x > {lim}) return x); -1 }}; \
             println(f({xs})); println(f(Nil)) }}"
        ),
        // `return` inside a `try` with a `finally` — the finalizer prints first.
        2 => format!(
            "{{ def f(n: Int): Int = {{ try {{ if (n > {lim}) return n * 10; 0 }} \
             finally {{ println(\"fin\") }} }}; println(f(99)); println(f(-99)) }}"
        ),
        // `return` inside a `try` inside a `while` — must not spin.
        3 => format!(
            "{{ def f(n: Int): Int = {{ var i = 0; while (i < {stop}) {{ \
             try {{ if (i == n) return i * 100 }} catch {{ case e: RuntimeException => println(\"c\") }}; \
             i += 1 }}; -1 }}; println(f(2)); println(f(99)) }}"
        ),
        // `return` from the `catch` arm, with a `finally`.
        4 => format!(
            "{{ def f(n: Int): Int = {{ try {{ if (n > 0) throw new RuntimeException(\"x\"); 1 }} \
             catch {{ case e: RuntimeException => return {stop} }} finally {{ println(\"fin\") }} }}; \
             println(f(1)); println(f(-1)) }}"
        ),
        // Nested `try`s: both finalizers run, innermost first.
        5 => format!(
            "{{ def f(n: Int): Int = {{ try {{ try {{ if (n > 0) return n; 0 }} \
             finally {{ println(\"inner\") }} }} finally {{ println(\"outer\") }} }}; \
             println(f({stop})); println(f(-{stop})) }}"
        ),
        // `return` out of a `for` nested in a `try` with a `finally`.
        6 => format!(
            "{{ def f(l: List[Int]): String = {{ try {{ for (x <- l) {{ \
             if (x > {lim}) return \"y\" + x }}; \"n\" }} finally {{ println(\"fin\") }} }}; \
             println(f({xs})); println(f(Nil)) }}"
        ),
        // A `finally` on the NORMAL path still runs, and after the body's value.
        _ => format!(
            "{{ def f(n: Int): Int = {{ try {{ n * 2 }} finally {{ println(\"fin\" + n) }} }}; \
             println(f({stop})); println(f(-{stop})) }}"
        ),
    }
}

/// `Range` as a first-class value: its `toString` (including the `empty `/
/// `inexact ` prefixes), the sequence operations over it, and the `by` step.
fn g_range(r: &mut Rng) -> String {
    let lo = r.below(8) as i64;
    let hi = r.below(12) as i64;
    let step = *pick(r, &["1", "2", "3", "-1", "-2", "4"]);
    let bound = if r.below(2) == 0 { "until" } else { "to" };
    match r.below(6) {
        0 => format!("println({lo} {bound} {hi})"),
        1 => format!("println({lo} {bound} {hi} by {step})"),
        2 => format!("println(({lo} {bound} {hi}).mkString(\",\"))"),
        3 => format!(
            "{{ val r = {lo} {bound} {hi}; println(r.length + \"/\" + r.sum + \"/\" + r.isEmpty) }}"
        ),
        4 => format!("println(({lo} {bound} {hi}).map(_ * 2))"),
        _ => format!(
            "println(({lo} {bound} {hi}).toList); println(({lo} {bound} {hi}).filter(_ % 2 == 0))"
        ),
    }
}

/// `Array`: construction, indexed read/write, and the sequence operations.
/// A bare `println(array)` is never generated — Scala prints the JVM identity
/// form (`[I@1b6d3586`), which no reimplementation can reproduce (`BUGS.md`).
fn g_array(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let c = pick(r, INTS);
    let n = 1 + r.below(4);
    match r.below(5) {
        0 => format!("{{ val a = Array({a}, {b}, {c}); println(a.length + \":\" + a(1)) }}"),
        1 => format!("{{ val a = Array({a}, {b}, {c}); a(1) = {a}; println(a.mkString(\",\")) }}"),
        2 => format!("{{ val a = new Array[Int]({n}); println(a.mkString(\"|\")) }}"),
        3 => format!(
            "{{ val a = Array({a}, {b}, {c}); println(a.map(_ * 2).mkString(\",\") + \
               \" \" + a.sum + \" \" + a.reverse.mkString(\"\")) }}"
        ),
        _ => format!(
            "{{ val a = Array({a}, {b}, {c}); println(a.toList); \
               println(\"c=\" + a.contains({b}) + \",\" + a.isEmpty) }}"
        ),
    }
}

/// `n` distinct small ints in a shuffled order, for a collection literal.
/// Distinctness keeps `Set`/`Map`/`distinct` results a pure function of the
/// list, and the shuffle makes insertion order differ from both sorted and hash
/// order, so a wrong ordering cannot pass by luck.
fn int_elems(r: &mut Rng, n: usize) -> String {
    let mut pool: Vec<i64> = (0..24).map(|i| i * 3 - 11).collect();
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(pool.swap_remove(r.below(pool.len() as u64) as usize));
    }
    out.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `n` distinct short string literals, likewise shuffled.
fn str_elems(r: &mut Rng, n: usize) -> String {
    let mut pool: Vec<&str> = vec![
        "a", "b", "cc", "dd", "eee", "fig", "kiwi", "pear", "plum", "apple", "z", "yy",
    ];
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(pool.swap_remove(r.below(pool.len() as u64) as usize));
    }
    out.iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `println` of several values joined by a space. Scala 3 dropped
/// `any2stringadd`, so a collection may not lead a `+` chain; the empty string
/// literal in front makes the whole chain a `String.+`. Each part is
/// parenthesized because an alphanumeric infix operator binds looser than `+`,
/// so a bare `5 max 12` part would otherwise read as `("" + 5) max 12`.
fn println_all(parts: &[String]) -> String {
    let joined = parts
        .iter()
        .map(|p| format!("({p})"))
        .collect::<Vec<_>>()
        .join(" + \" \" + ");
    format!("println(\"\" + {joined})")
}

/// Sequence combinators over `List`/`Vector`/`Seq`: the transformations, folds,
/// slices, orderings and conversions, each printed as a whole collection so the
/// element order is part of the comparison.
fn g_coll(r: &mut Rng) -> String {
    let ctor = *pick(r, &["List", "Vector", "Seq"]);
    let n = 3 + r.below(4) as usize;
    let xs = format!("{ctor}({})", int_elems(r, n));
    let wn = 3 + r.below(3) as usize;
    let ws = format!("List({})", str_elems(r, wn));
    let k = 1 + r.below(4);
    let one = |e: String| println_all(&[e]);
    let two = |a: String, b: String| println_all(&[a, b]);
    let three = |a: String, b: String, c: String| println_all(&[a, b, c]);
    match r.below(16) {
        0 => one(format!("{xs}.map(_ * 2).filter(_ > 0)")),
        1 => one(format!("{xs}.flatMap(x => List(x, -x))")),
        2 => two(
            format!("{xs}.foldLeft(0)(_ + _)"),
            format!("{xs}.foldRight(\"\")((a, b) => a + b)"),
        ),
        3 => two(format!("{xs}.sorted"), format!("{ws}.sorted")),
        4 => two(
            format!("{xs}.sortBy(x => -x)"),
            format!("{ws}.sortBy(_.length)"),
        ),
        5 => one(format!("{xs}.sortWith((a, b) => a > b)")),
        6 => two(format!("{xs}.zip({ws})"), format!("{xs}.zipWithIndex")),
        7 => three(
            format!("{xs}.take({k})"),
            format!("{xs}.drop({k})"),
            format!("{xs}.slice(1, {k})"),
        ),
        8 => three(
            format!("{xs}.exists(_ > 5)"),
            format!("{xs}.forall(_ > -20)"),
            format!("{xs}.count(_ < 0)"),
        ),
        9 => three(
            format!("{xs}.sum"),
            format!("{xs}.min"),
            format!("{xs}.max + {xs}.length"),
        ),
        10 => two(
            format!("{xs}.mkString(\"[\", \";\", \"]\")"),
            format!("{ws}.mkString(\"-\")"),
        ),
        11 => two(
            format!("{xs}.partition(_ > 0)"),
            format!("{xs}.span(_ > 0)"),
        ),
        12 => three(
            format!("{xs}.takeWhile(_ > 0)"),
            format!("{xs}.dropWhile(_ > 0)"),
            format!("{xs}.init"),
        ),
        13 => three(
            format!("{xs}.reverse"),
            format!("{xs}.tail"),
            format!("{xs}.distinct"),
        ),
        14 => three(
            format!("{xs}.find(_ > 0)"),
            format!("{xs}.headOption"),
            format!("{xs}.indexWhere(_ < 0)"),
        ),
        _ => two(
            format!("{xs}.grouped(2).toList"),
            format!("{xs}.maxBy(x => -x)"),
        ),
    }
}

/// `n` distinct ints chosen to exercise a mutable hash table's bucket layout:
/// the pool spans negatives (whose improved hash flips the high bits), small
/// values, and multiples of 16/32/64 that collide into one bucket at several
/// table lengths. Bigger than [`int_elems`]'s pool, because the interesting
/// element counts here run past thirty.
fn table_ints(r: &mut Rng, n: usize) -> String {
    let mut pool: Vec<i64> = Vec::new();
    for i in 0..12 {
        pool.push(i);
        pool.push(-i - 1);
        pool.push(i * 16);
        pool.push(i * 32 + 3);
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n.min(pool.len()) {
        out.push(pool.swap_remove(r.below(pool.len() as u64) as usize));
    }
    out.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The bitwise / shift operators and their SLS precedence. Scala evaluates
/// these at `Int` width — the shift distance masks to five bits and the result
/// wraps at 32 bits — so the shift distances deliberately run past 31 and the
/// operands past the point where a 64-bit shift would answer differently.
/// Also covers `&`/`|`/`^` on `Boolean` (non-short-circuiting) and on `Set`,
/// and hexadecimal literals.
fn g_bitwise(r: &mut Rng) -> String {
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    let c = pick(r, INTS);
    let sh = pick(r, &["0", "1", "3", "7", "15", "31", "32", "33", "64"]);
    let bop = pick(r, &["&", "|", "^"]);
    let shop = pick(r, &["<<", ">>", ">>>"]);
    let p = pick(r, BOOLS);
    let q = pick(r, BOOLS);
    let hex = pick(r, &["0x0F", "0xFF", "0x10", "0x7FFFFFFF", "0xFFFF", "0x1"]);
    match r.below(10) {
        0 => format!("println({a} {bop} {b})"),
        1 => format!("println({a} {shop} {sh})"),
        2 => format!("println(~({a}))"),
        3 => println_all(&[
            format!("{a} {bop} {b} {bop} {c}"),
            format!("{a} & {b} | {c}"),
            format!("{a} | {b} ^ {c}"),
        ]),
        // Precedence against arithmetic and comparison.
        4 => println_all(&[
            format!("{a} {shop} 1 + 1"),
            format!("({a} & {b}) == {c}"),
            format!("{a} + {b} & {c}"),
        ]),
        5 => format!("println({p} {bop} {q})"),
        6 => format!("println({hex} {bop} {a}); println({hex})"),
        7 => println_all(&[
            format!("(1 {shop} {sh})"),
            format!("(-1 {shop} {sh})"),
            format!("({a} {shop} {sh})"),
        ]),
        8 => format!(
            "println(Set({}) & Set({})); println(Set({}) | Set({}))",
            int_elems(r, 3),
            int_elems(r, 3),
            int_elems(r, 2),
            int_elems(r, 2)
        ),
        _ => println_all(&[
            format!("{a}.abs {bop} {b}"),
            format!("~({a} {bop} {b})"),
            format!("{a} {bop} ~({b})"),
        ]),
    }
}

/// Mutable collections. `ListBuffer`/`ArrayBuffer` are insertion-ordered, so
/// they mostly probe the mutators; a `mutable.Set`/`Map` prints in its **hash
/// table's** order, which is a function of the table length, so element counts
/// straddle every growth threshold (16/24/48 … after the `from` sizing) and the
/// probes mix factory sizing, `+=` growth, `++=` (which size-hints only when the
/// source knows its size), removal (which never shrinks the table) and the
/// derived collections (which start a fresh table at the default capacity).
fn g_mutable(r: &mut Rng) -> String {
    let buf = *pick(r, &["mutable.ListBuffer", "mutable.ArrayBuffer"]);
    let n = 3 + r.below(4) as usize;
    let xs = format!("{buf}({})", int_elems(r, n));
    // Sizes that straddle the table-growth boundaries.
    let sn = 1 + r.below(34) as usize;
    let ints = table_ints(r, sn);
    let wn = 1 + r.below(11) as usize;
    let strs = str_elems(r, wn);
    let pairs = |elems: &str| {
        elems
            .split(", ")
            .enumerate()
            .map(|(i, e)| format!("{e} -> {i}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ipairs = pairs(&ints);
    let a = pick(r, INTS);
    match r.below(16) {
        0 => format!("{{ val b = {xs}; b += {a}; b ++= List({}); println(b) }}", int_elems(r, 3)),
        1 => format!("{{ val b = {xs}; b -= {a}; b += {a}; println(b); println(b.size) }}"),
        2 => format!(
            "{{ val b = {xs}; b.append({a}); b.prepend({a}); println(b); println(b.remove(0)); println(b) }}"
        ),
        3 => format!("{{ val b = {xs}; b.insert(1, {a}); println(b); b(0) = {a}; println(b) }}"),
        4 => format!("{{ val b = {xs}; println(b.map(_ * 2)); println(b.filter(_ > 0)); println(b.sorted) }}"),
        5 => format!("{{ val b = {xs}; println(b.toList); println(b.sum); println(b.mkString(\"|\")); println(b.reverse) }}"),
        6 => format!("{{ val b = {xs}; b.clear(); println(b); println(b.isEmpty); b += {a}; println(b) }}"),
        // The hash-ordered ones. Every print is the whole collection, so the
        // table's iteration order is part of the comparison.
        7 => format!("println(mutable.Set({ints}))"),
        8 => format!("println(mutable.Set({strs}))"),
        9 => format!("println(mutable.Map({ipairs}))"),
        10 => {
            let k = 1 + r.below(20) as usize;
            format!(
                "{{ val s = mutable.Set({}); s += {a}; s += {a}; println(s); s -= {a}; println(s) }}",
                table_ints(r, k)
            )
        }
        11 => format!(
            "{{ val s = mutable.Set({}); s ++= List({}); println(s); println(s.size) }}",
            table_ints(r, 14),
            table_ints(r, 12)
        ),
        12 => format!(
            "{{ val s = mutable.Set({}); s ++= Vector({}); println(s) }}",
            table_ints(r, 12),
            table_ints(r, 10)
        ),
        13 => format!("println(mutable.Set({ints}).map(_ * 2)); println(mutable.Set({ints}).filter(_ > 0))"),
        14 => format!(
            "{{ val m = mutable.Map({ipairs}); m({a}) = 99; println(m); m -= {a}; println(m); println(m.size) }}"
        ),
        _ => println_all(&[
            format!("mutable.Set({ints}).size"),
            format!("mutable.Set({ints}).toList.sorted"),
            format!("mutable.Map({ipairs}).keys.toList.sorted"),
        ]),
    }
}

/// Partial functions: `collect`/`collectFirst` over a `{ case … }` literal
/// (whose `isDefinedAt` decides which elements survive), a `PartialFunction`
/// bound to a `val` and used as a value, `applyOrElse`/`lift`/`orElse`, and the
/// total-function combinators `andThen`/`compose`. Patterns straddle guards,
/// literals, types and destructuring so the derived pattern test is exercised
/// on every arm shape, and an element matching *no* arm is always reachable.
fn g_partial(r: &mut Rng) -> String {
    let n = 4 + r.below(3) as usize;
    let xs = format!("List({})", int_elems(r, n));
    let vs = format!("Vector({})", int_elems(r, n));
    let ws = format!("List({})", str_elems(r, 4));
    let lim = pick(r, &["0", "3", "-5", "10"]);
    match r.below(14) {
        0 => format!("println({xs}.collect {{ case x if x > {lim} => x * 2 }})"),
        1 => format!("println({vs}.collect {{ case x if x % 2 == 0 => x }})"),
        2 => format!("println({xs}.collectFirst {{ case x if x > {lim} => x + 1 }})"),
        3 => format!("println({ws}.collect {{ case s if s.length > 1 => s.toUpperCase }})"),
        4 => println_all(&[
            format!("{xs}.collect {{ case x if x < {lim} => x }}"),
            format!("{xs}.collectFirst {{ case x if x < {lim} => x }}"),
        ]),
        5 => format!(
            "{{ val pf: PartialFunction[Int, Int] = {{ case x if x > {lim} => x * 3 }}; \
             println({xs}.collect(pf)); println(pf.isDefinedAt({lim})); \
             println(pf.applyOrElse({lim}, (i: Int) => -i)) }}"
        ),
        6 => format!(
            "{{ val pf: PartialFunction[Int, String] = {{ case 0 => \"zero\"; case x if x < 0 => \"neg\" }}; \
             println({xs}.map(pf.lift)); println({xs}.collect(pf)) }}"
        ),
        7 => format!(
            "{{ val a: PartialFunction[Int, String] = {{ case x if x > {lim} => \"hi:\" + x }}; \
             val b: PartialFunction[Int, String] = {{ case x => \"lo:\" + x }}; \
             println({xs}.collect(a orElse b)); println((a orElse b)({lim})) }}"
        ),
        8 => format!(
            "{{ val f = (x: Int) => x + {lim}; val g = (y: Int) => y * 2; \
             println({xs}.map(f andThen g)); println({xs}.map(f compose g)) }}"
        ),
        // A heterogeneous list: the arm's type pattern is the whole filter.
        9 => format!(
            "{{ val any: List[Any] = List(1, \"s\", 2.5, true, {}); \
             println(any.collect {{ case i: Int => i * 2 }}); \
             println(any.collect {{ case s: String => s + \"!\" }}); \
             println(any.collectFirst {{ case d: Double => d }}) }}",
            pick(r, INTS)
        ),
        // `Option` and tuple constructor patterns.
        10 => format!(
            "println(List(Some({}), None, Some({})).collect {{ case Some(v) => v }})",
            pick(r, INTS),
            pick(r, INTS)
        ),
        11 => format!(
            "println({xs}.zipWithIndex.collect {{ case (x, i) if i % 2 == 0 => x }})"
        ),
        // A guard reading an enclosing binding, so the derived pattern test has
        // to capture it exactly as the `apply` body does.
        12 => format!(
            "{{ val k = {lim}; val m = 2; println({xs}.collect {{ case x if x > k => x * m }}) }}"
        ),
        _ => format!(
            "{{ val m = Map({}); println(m.collect {{ case (k, v) if v > {lim} => k }}); \
             println(m.collect {{ case (k, v) if v > {lim} => k -> v }}) }}",
            int_elems(r, 4)
                .split(", ")
                .enumerate()
                .map(|(i, e)| format!("\"k{i}\" -> {e}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `Set`/`Map`, whose printed order is the *representation's* order: up to four
/// entries a `Set1`..`Set4` / `Map1`..`Map4` keeps insertion order, and beyond
/// that a CHAMP `HashSet`/`HashMap` prints in trie order. Sizes straddle the
/// boundary deliberately, and derived collections are printed too because a
/// hashed receiver keeps its representation however small the result.
fn g_hashcoll(r: &mut Rng) -> String {
    let n = 1 + r.below(9) as usize;
    let ints = int_elems(r, n);
    let sn = 1 + r.below(6) as usize;
    let strs = str_elems(r, sn);
    let pairs = |elems: &str| {
        elems
            .split(", ")
            .enumerate()
            .map(|(i, e)| format!("{e} -> {i}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let ipairs = pairs(&ints);
    let spairs = pairs(&strs);
    match r.below(12) {
        0 => format!("println(Set({ints}))"),
        1 => format!("println(Set({strs}))"),
        2 => format!("println(List({ints}).toSet)"),
        3 => format!("println(Map({ipairs}))"),
        4 => format!("println(Map({spairs}))"),
        5 => format!("println(List({ints}).groupBy(x => x % 3))"),
        6 => format!("println(List({strs}).groupBy(_.length))"),
        7 => println_all(&[
            format!("Set({ints}).filter(_ > 0)"),
            format!("Set({ints}).map(_ * 2)"),
        ]),
        8 => format!("println(Set({ints}) + 100)"),
        9 => println_all(&[
            format!("Map({ipairs}).keys"),
            format!("Map({ipairs}).values"),
        ]),
        10 => println_all(&[
            format!("Map({ipairs}).size"),
            format!("Map({ipairs}).toList.length"),
        ]),
        12 => println_all(&[
            format!("Set({ints}).sum"),
            format!("Set({ints}).size"),
            format!("Set({ints}).toList.sorted"),
        ]),
        // Collections used as elements and as keys: the trie order then turns
        // on `MurmurHash3`'s seq/set/map hashes, and a `Seq`'s hash has the
        // arithmetic-progression special case (`List(1,2,3)` hashes as the
        // range it is), so the inner element runs deliberately hit both.
        13 => {
            let k = 1 + r.below(7) as usize;
            format!("println(Set({}))", nested_elems(r, k))
        }
        14 => {
            let k = 1 + r.below(6) as usize;
            format!(
                "println(Map({}))",
                nested_elems(r, k)
                    .split("), ")
                    .enumerate()
                    .map(|(i, e)| {
                        let e = if e.ends_with(')') {
                            e.to_string()
                        } else {
                            format!("{e})")
                        };
                        format!("{e} -> {i}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        15 => println_all(&[
            format!("List({ints}).hashCode"),
            format!("Vector({ints}).hashCode"),
            format!("Set({ints}).hashCode"),
            format!("Map({ipairs}).hashCode"),
        ]),
        _ => {
            let (a, b) = (1 + r.below(6) as usize, 1 + r.below(6) as usize);
            format!(
                "println(List({}).groupBy(_.size)); println(List({}).toSet)",
                nested_elems(r, a),
                nested_elems(r, b)
            )
        }
    }
}

/// `n` distinct collection literals, for probing a hash keyed by a collection.
/// Mixes `List`/`Vector`/`Set` and both progression and non-progression element
/// runs, because `MurmurHash3`'s ordered hash treats them differently.
fn nested_elems(r: &mut Rng, n: usize) -> String {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let k = i as i64;
        let body = match r.below(5) {
            0 => format!("{k}"),
            1 => format!("{k}, {}", k + 1),
            2 => format!("{k}, {}, {}", k + 1, k + 2),
            3 => format!("{k}, {}, {k}", k * 3 + 1),
            _ => format!("{}, {k}", -k - 1),
        };
        let ctor = *pick(r, &["List", "Vector", "Set"]);
        out.push(format!("{ctor}({body})"));
    }
    out.join(", ")
}

/// Infix method syntax (`a m b`) and the `for` comprehension forms: guards,
/// several generators, the brace-bracketed enumerator group, and a destructuring
/// generator over a `Map`'s pairs.
fn g_infix(r: &mut Rng) -> String {
    let xn = 3 + r.below(3) as usize;
    let xs = format!("List({})", int_elems(r, xn));
    let ws = format!("List({})", str_elems(r, 3));
    let a = pick(r, INTS);
    let b = pick(r, INTS);
    match r.below(10) {
        0 => format!("println({xs} contains {a})"),
        1 => println_all(&[format!("{a} max {b}"), format!("{a} min {b}")]),
        2 => format!("println({xs} map (_ * 2))"),
        3 => format!("println({xs} mkString \",\")"),
        4 => format!("println({ws} map (_.length) mkString \"|\")"),
        5 => format!("println(for (x <- {xs} if x > 0) yield x * 2)"),
        6 => format!("println(for {{ x <- {xs}; if x < 0 }} yield x - 1)"),
        7 => format!("println(for (x <- {xs}; y <- List(0, 1)) yield x + y)"),
        8 => "{ val m = Map(\"k1\" -> 1, \"k2\" -> 2, \"k3\" -> 3); \
              println(for ((k, v) <- m.toList) yield k + \":\" + v) }"
            .to_string(),
        _ => format!("println({xs}.map {{ x => x + 1 }} ++ List({a}))"),
    }
}

/// `scala.math` members, over both the `Int` and the `Double` overloads (Scala
/// keeps `abs`/`min`/`max`/`signum` integral for integral arguments).
fn g_math(r: &mut Rng) -> String {
    let i = pick(r, INTS);
    let j = pick(r, INTS);
    let d = pick(
        r,
        &[
            "0.5", "1.5", "2.5", "-2.5", "3.0", "16.0", "0.25", "-0.0", "10.0",
        ],
    );
    let e = pick(r, &["2.0", "3.0", "-1.5", "0.5"]);
    let module = *pick(r, &["math", "scala.math", "Math"]);
    match r.below(8) {
        0 => format!("println({module}.abs({i}) + \" \" + {module}.abs({d}))"),
        1 => format!("println({module}.max({i}, {j}) + \" \" + {module}.min({d}, {e}))"),
        2 => format!("println({module}.sqrt({}) + \" \" + {module}.cbrt(27.0))", d.trim_start_matches('-')),
        3 => format!("println({module}.pow({e}, 2.0) + \" \" + {module}.hypot(3.0, 4.0))"),
        4 => format!("println({module}.floor({d}) + \" \" + {module}.ceil({d}) + \" \" + {module}.round({d}))"),
        5 => format!("println({module}.signum({i}) + \" \" + {module}.signum({d}))"),
        6 => "println(math.Pi + \" \" + math.E)".to_string(),
        _ => format!("println({module}.exp(0.0) + \" \" + {module}.log(1.0) + \" \" + {module}.log10(100.0))"),
    }
}

fn g_mixed(r: &mut Rng) -> String {
    let x = pick(r, INTS);
    let y = pick(r, DIVS);
    format!(
        "{{ val x = {x}; val y = {y}; val q = x / y; val rem = x % y; println(\"q=\" + q + \" r=\" + rem) }}"
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    All,
    Arith,
    IntDiv,
    FloatArith,
    FloatFmt,
    Concat,
    Compare,
    Bool,
    Cond,
    Loop,
    Mixed,
    Step,
    Ieee,
    Exc,
    LocalDef,
    Oop,
    Range,
    Array,
    Math,
    Coll,
    HashColl,
    Infix,
    Partial,
    Mutable,
    Bitwise,
    PatMatch,
    Option,
    CaseClass,
    StrOps,
    Nlr,
    Ascribe,
    ForVal,
    Regex,
    Capture,
    Char,
    PatRegex,
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::All => "all",
        Mode::Arith => "arith",
        Mode::IntDiv => "intdiv",
        Mode::FloatArith => "floatarith",
        Mode::FloatFmt => "floatfmt",
        Mode::Concat => "concat",
        Mode::Compare => "compare",
        Mode::Bool => "bool",
        Mode::Cond => "cond",
        Mode::Loop => "loop",
        Mode::Mixed => "mixed",
        Mode::Step => "step",
        Mode::Ieee => "ieee",
        Mode::Exc => "exc",
        Mode::LocalDef => "localdef",
        Mode::Oop => "oop",
        Mode::Range => "range",
        Mode::Array => "array",
        Mode::Math => "math",
        Mode::Coll => "coll",
        Mode::HashColl => "hashcoll",
        Mode::Infix => "infix",
        Mode::Partial => "partial",
        Mode::Mutable => "mutable",
        Mode::Bitwise => "bitwise",
        Mode::PatMatch => "patmatch",
        Mode::Option => "option",
        Mode::CaseClass => "caseclass",
        Mode::StrOps => "strops",
        Mode::Nlr => "nlr",
        Mode::Ascribe => "ascribe",
        Mode::ForVal => "forval",
        Mode::Regex => "regex",
        Mode::Capture => "capture",
        Mode::Char => "char",
        Mode::PatRegex => "patregex",
    }
}

fn parse_mode(s: &str) -> Option<Mode> {
    Some(match s {
        "all" => Mode::All,
        "arith" => Mode::Arith,
        "intdiv" => Mode::IntDiv,
        "floatarith" => Mode::FloatArith,
        "floatfmt" => Mode::FloatFmt,
        "concat" => Mode::Concat,
        "compare" => Mode::Compare,
        "bool" => Mode::Bool,
        "cond" => Mode::Cond,
        "loop" => Mode::Loop,
        "mixed" => Mode::Mixed,
        "step" => Mode::Step,
        "ieee" => Mode::Ieee,
        "exc" => Mode::Exc,
        "localdef" => Mode::LocalDef,
        "oop" => Mode::Oop,
        "range" => Mode::Range,
        "array" => Mode::Array,
        "math" => Mode::Math,
        "coll" => Mode::Coll,
        "hashcoll" => Mode::HashColl,
        "infix" => Mode::Infix,
        "partial" => Mode::Partial,
        "mutable" => Mode::Mutable,
        "bitwise" => Mode::Bitwise,
        "patmatch" => Mode::PatMatch,
        "option" => Mode::Option,
        "caseclass" => Mode::CaseClass,
        "strops" => Mode::StrOps,
        "nlr" => Mode::Nlr,
        "ascribe" => Mode::Ascribe,
        "forval" => Mode::ForVal,
        "regex" => Mode::Regex,
        "capture" => Mode::Capture,
        "char" => Mode::Char,
        "patregex" => Mode::PatRegex,
        _ => return None,
    })
}

const CONCRETE: &[Mode] = &[
    Mode::Arith,
    Mode::IntDiv,
    Mode::FloatArith,
    Mode::FloatFmt,
    Mode::Concat,
    Mode::Compare,
    Mode::Bool,
    Mode::Cond,
    Mode::Loop,
    Mode::Mixed,
    Mode::Step,
    Mode::Ieee,
    Mode::Exc,
    Mode::LocalDef,
    Mode::Oop,
    Mode::Range,
    Mode::Array,
    Mode::Math,
    Mode::Coll,
    Mode::HashColl,
    Mode::Infix,
    Mode::Partial,
    Mode::Mutable,
    Mode::Bitwise,
    Mode::PatMatch,
    Mode::Option,
    Mode::CaseClass,
    Mode::StrOps,
    Mode::Nlr,
    Mode::Ascribe,
    Mode::ForVal,
    Mode::Regex,
    Mode::Capture,
    Mode::Char,
    Mode::PatRegex,
];

fn gen_probe(r: &mut Rng, mode: Mode) -> String {
    let m = if mode == Mode::All {
        *pick(r, CONCRETE)
    } else {
        mode
    };
    match m {
        Mode::Arith => g_arith(r),
        Mode::IntDiv => g_intdiv(r),
        Mode::FloatArith => g_floatarith(r),
        Mode::FloatFmt => g_floatfmt(r),
        Mode::Concat => g_concat(r),
        Mode::Compare => g_compare(r),
        Mode::Bool => g_bool(r),
        Mode::Cond => g_cond(r),
        Mode::Loop => g_loop(r),
        Mode::Mixed => g_mixed(r),
        Mode::Step => g_step(r),
        Mode::Ieee => g_ieee(r),
        Mode::Exc => g_exc(r),
        Mode::LocalDef => g_localdef(r),
        Mode::Oop => g_oop(r),
        Mode::Range => g_range(r),
        Mode::Array => g_array(r),
        Mode::Math => g_math(r),
        Mode::Coll => g_coll(r),
        Mode::HashColl => g_hashcoll(r),
        Mode::Infix => g_infix(r),
        Mode::Partial => g_partial(r),
        Mode::Mutable => g_mutable(r),
        Mode::Bitwise => g_bitwise(r),
        Mode::PatMatch => g_patmatch(r),
        Mode::Option => g_option(r),
        Mode::CaseClass => g_caseclass(r),
        Mode::StrOps => g_strops(r),
        Mode::Nlr => g_nlr(r),
        Mode::Ascribe => g_ascribe(r),
        Mode::ForVal => g_forval(r),
        Mode::Regex => g_regex(r),
        Mode::Capture => g_capture(r),
        Mode::Char => g_char(r),
        Mode::PatRegex => g_patregex(r),
        Mode::All => unreachable!(),
    }
}

/// Generate a program's probe list for a seed (each probe is one line).
fn gen_probes(seed: u64, mode: Mode, n: usize) -> Vec<String> {
    let r = &mut Rng::new(seed);
    (0..n).map(|_| gen_probe(r, mode)).collect()
}

/// Separator inside a probe that carries its own top-level declarations: the
/// text before it is emitted *outside* the entry object (a `trait`/`class` can
/// only be declared at the top level), the text after it is the body statement.
const TOP_SEP: &str = "//@TOP@";

fn build_program(probes: &[String]) -> String {
    let mut top = String::new();
    let mut body = String::new();
    for p in probes {
        let (decls, stmt) = match p.split_once(TOP_SEP) {
            Some((d, s)) => (Some(d), s),
            None => (None, p.as_str()),
        };
        if let Some(d) = decls {
            top.push_str(d.trim_end());
            top.push('\n');
        }
        body.push_str("  ");
        body.push_str(stmt);
        body.push('\n');
    }
    // The `mutable` prefix and the `Regex` type are in scope for every program:
    // they cost an unused import in the modes that never mention them, and let
    // the `mutable`/`patregex` modes write the idiomatic `mutable.ListBuffer(…)`
    // and `new Regex(…)` rather than the fully qualified spellings.
    format!(
        "import scala.collection.mutable\nimport scala.util.matching.Regex\n\
         {top}object T extends App {{\n{body}}}\n"
    )
}

// ───────────────────────── process invocation ──────────────────────────────

struct RunOut {
    stdout: Vec<u8>,
    exit: i32,
    timed_out: bool,
}

static TMP_CTR: AtomicU64 = AtomicU64::new(0);

/// Run `<prog> <tempfile.scala>` under a wall-clock timeout, capturing stdout.
fn run_prog(prog: &Path, src: &str, timeout: Duration) -> RunOut {
    let id = TMP_CTR.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("scalars_fuzz_{}_{}.scala", std::process::id(), id));
    if std::fs::write(&path, src).is_err() {
        return RunOut {
            stdout: Vec::new(),
            exit: -1,
            timed_out: false,
        };
    }

    let mut cmd = Command::new(prog);
    cmd.arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return RunOut {
                stdout: Vec::new(),
                exit: -1,
                timed_out: false,
            };
        }
    };

    let mut out_h = child.stdout.take().map(|mut o| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = o.read_to_end(&mut b);
            b
        })
    });
    // drain stderr so a chatty compiler can't deadlock on a full pipe
    let mut err_h = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = e.read_to_end(&mut b);
            b
        })
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let exit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = status.code().unwrap_or(-1);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let s = child.wait().ok();
                    exit = s.and_then(|s| s.code()).unwrap_or(-1);
                    timed_out = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                exit = -1;
                break;
            }
        }
    }

    let stdout = out_h.take().and_then(|h| h.join().ok()).unwrap_or_default();
    let _ = err_h.take().and_then(|h| h.join().ok());
    let _ = std::fs::remove_file(&path);
    RunOut {
        stdout,
        exit,
        timed_out,
    }
}

/// A parity gap: stdout bytes differ, OR one side accepted the program (exit 0)
/// while the other rejected it. Exact exit codes are not compared — a
/// from-scratch frontend is free to pick its own non-zero code.
fn differs(oracle: &RunOut, ours: &RunOut) -> bool {
    (oracle.exit == 0) != (ours.exit == 0) || oracle.stdout != ours.stdout
}

fn render(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_end_matches('\n');
    if std::str::from_utf8(bytes).is_err() {
        let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
        return format!("{text}\n  (hex) {}", hex.join(" "));
    }
    text.to_string()
}

fn diverges(probes: &[String], ours: &Path, oracle: &Path, timeout: Duration) -> bool {
    let src = build_program(probes);
    let o = run_prog(oracle, &src, timeout);
    if o.timed_out {
        return false; // oracle-side pathology, not a parity gap
    }
    let r = run_prog(ours, &src, timeout);
    differs(&o, &r)
}

/// Shrink a diverging program to the smallest still-diverging probe subset.
/// Probes are independent, so a single offending probe almost always reproduces;
/// fall back to a linear "remove one at a time" pass otherwise.
fn minimize(probes: &[String], ours: &Path, oracle: &Path, timeout: Duration) -> Vec<String> {
    // 1. Try each probe alone.
    for p in probes {
        let one = vec![p.clone()];
        if diverges(&one, ours, oracle, timeout) {
            return one;
        }
    }
    // 2. Interaction case: greedily drop probes that are not needed.
    let mut cur = probes.to_vec();
    let mut i = 0;
    while i < cur.len() && cur.len() > 1 {
        let mut trial = cur.clone();
        trial.remove(i);
        if diverges(&trial, ours, oracle, timeout) {
            cur = trial;
        } else {
            i += 1;
        }
    }
    cur
}

// ───────────────────────── binary resolution ───────────────────────────────

/// Our built `scala` binary — the sibling of this harness binary. NEVER the
/// system `scala` (same file name!), so we always use an absolute path into the
/// build directory rather than a PATH lookup.
fn ours_bin() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_scala") {
        return PathBuf::from(p);
    }
    if let Some(d) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        let cand = d.join("scala");
        if cand.exists() {
            return cand;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join("scala")
}

/// The ORACLE — the reference Scala toolchain. `SCALARS_FUZZ_SCALA` names it
/// explicitly; otherwise probe the usual locations. It must NOT resolve to our
/// own build output, and it must look like a real Scala (`--version` mentions
/// "Scala"). A misconfigured oracle is a HARD error — silently comparing against
/// the wrong thing answers a different question.
fn resolve_oracle(ours: &Path) -> PathBuf {
    let candidates: Vec<String> = if let Ok(p) = std::env::var("SCALARS_FUZZ_SCALA") {
        vec![p]
    } else {
        vec![
            "/opt/homebrew/bin/scala".into(),
            "/usr/local/bin/scala".into(),
            "/usr/bin/scala".into(),
            "scala".into(),
        ]
    };
    for c in &candidates {
        let cp = PathBuf::from(c);
        if cp.canonicalize().ok() == ours.canonicalize().ok() {
            continue; // never the frontend under test
        }
        if scala_version(&cp).is_some() {
            return cp;
        }
    }
    eprintln!("parity-fuzz: no reference scala found; set SCALARS_FUZZ_SCALA to a real `scala`");
    std::process::exit(2);
}

fn scala_version(prog: &Path) -> Option<String> {
    let o = Command::new(prog).arg("-version").output().ok()?;
    let s = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    if s.to_lowercase().contains("scala") {
        Some(s.trim().to_string())
    } else {
        None
    }
}

// ───────────────────────── CLI + driver ────────────────────────────────────

struct Args {
    count: u64,
    probes: usize,
    base_seed: u64,
    jobs: usize,
    mode: Mode,
    once: bool,
    timeout: Duration,
    max_report: usize,
    out_path: PathBuf,
}

fn parse_args() -> Args {
    let mut a = Args {
        count: 200,
        probes: 40,
        base_seed: 1,
        jobs: (std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            / 2)
        .clamp(1, 8),
        mode: Mode::All,
        once: false,
        timeout: Duration::from_secs(40),
        max_report: 40,
        out_path: PathBuf::from("target/parity-divergences.txt"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().expect("missing value for flag");
        match arg.as_str() {
            "--count" => a.count = val().parse().expect("--count"),
            "--probes" => a.probes = val().parse().expect("--probes"),
            "--seed" => a.base_seed = val().parse().expect("--seed"),
            "--jobs" => a.jobs = val().parse::<usize>().expect("--jobs").max(1),
            "--mode" => a.mode = parse_mode(&val()).expect("unknown --mode"),
            "--once" => a.once = true,
            "--timeout" => a.timeout = Duration::from_secs(val().parse().expect("--timeout")),
            "--max-report" => a.max_report = val().parse().expect("--max-report"),
            "--out" => a.out_path = PathBuf::from(val()),
            other => {
                eprintln!("parity-fuzz: unknown flag `{other}`");
                std::process::exit(2);
            }
        }
    }
    a
}

fn main() {
    let args = parse_args();
    let ours = ours_bin();
    if !ours.exists() {
        eprintln!(
            "parity-fuzz: our `scala` not found at {} (run `cargo build` first)",
            ours.display()
        );
        std::process::exit(2);
    }
    let oracle = resolve_oracle(&ours);

    if args.once {
        let probes = gen_probes(args.base_seed, args.mode, args.probes);
        let full = build_program(&probes);
        let o = run_prog(&oracle, &full, args.timeout);
        let r = run_prog(&ours, &full, args.timeout);
        let diverged = !o.timed_out && differs(&o, &r);
        let show = if diverged {
            minimize(&probes, &ours, &oracle, args.timeout)
        } else {
            probes.clone()
        };
        let ms = build_program(&show);
        let mo = run_prog(&oracle, &ms, args.timeout);
        let mr = run_prog(&ours, &ms, args.timeout);
        println!("seed   : {}", args.base_seed);
        println!("mode   : {}", mode_name(args.mode));
        println!("program:\n{ms}");
        println!(
            "--- scala(ref) exit={} timeout={} ---",
            mo.exit, mo.timed_out
        );
        println!("{}", render(&mo.stdout));
        println!(
            "--- scalars    exit={} timeout={} ---",
            mr.exit, mr.timed_out
        );
        println!("{}", render(&mr.stdout));
        println!("--- {} ---", if diverged { "DIVERGE" } else { "match" });
        std::process::exit(if diverged { 1 } else { 0 });
    }

    let next = AtomicU64::new(0);
    let checked = AtomicU64::new(0);
    let timeouts = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let divergences: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());
    let start = Instant::now();

    eprintln!(
        "oracle : {}",
        scala_version(&oracle)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or("")
            .trim()
    );
    eprintln!("ours   : {}", ours.display());
    eprintln!(
        "fuzzing {} programs x {} probes ({}) across {} workers…",
        args.count,
        args.probes,
        mode_name(args.mode),
        args.jobs
    );

    std::thread::scope(|scope| {
        for _ in 0..args.jobs {
            scope.spawn(|| loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= args.count {
                    break;
                }
                let seed = args.base_seed.wrapping_add(idx);
                let probes = gen_probes(seed, args.mode, args.probes);
                let src = build_program(&probes);
                let o = run_prog(&oracle, &src, args.timeout);
                let r = run_prog(&ours, &src, args.timeout);
                let done = checked.fetch_add(1, Ordering::Relaxed) + 1;
                if o.timed_out || r.timed_out {
                    timeouts.fetch_add(1, Ordering::Relaxed);
                }
                if !o.timed_out && differs(&o, &r) {
                    let minimal = minimize(&probes, &ours, &oracle, args.timeout);
                    // re-verify the shrunk case actually reproduces
                    if !diverges(&minimal, &ours, &oracle, args.timeout) {
                        continue;
                    }
                    let ms = build_program(&minimal);
                    let mo = run_prog(&oracle, &ms, args.timeout);
                    let mr = run_prog(&ours, &ms, args.timeout);
                    let rec = format!(
                        "==== seed {seed} ====\n\
                         probe   : {}\n\
                         scala   : exit={} {}\n\
                         scalars : exit={} {}\n",
                        minimal.join(" ; "),
                        mo.exit,
                        render(&mo.stdout).replace('\n', " | "),
                        mr.exit,
                        render(&mr.stdout).replace('\n', " | "),
                    );
                    let mut d = divergences.lock().unwrap();
                    d.push((seed, rec));
                    if d.len() >= args.max_report {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                if done % 20 == 0 {
                    let n = divergences.lock().unwrap().len();
                    eprintln!(
                        "  {done}/{} programs, {n} divergences, {:.1} prog/s",
                        args.count,
                        done as f64 / start.elapsed().as_secs_f64().max(0.001)
                    );
                }
            });
        }
    });

    let checked = checked.load(Ordering::Relaxed);
    let timeouts = timeouts.load(Ordering::Relaxed);
    let mut divergences: Vec<(u64, String)> = divergences.into_inner().unwrap();
    divergences.sort_by_key(|(seed, _)| *seed);
    let divergences: Vec<String> = divergences.into_iter().map(|(_, r)| r).collect();
    let elapsed = start.elapsed();

    println!(
        "\nfuzzed {checked} programs ({} probes) in {:.1}s\n\
         divergences : {}\n\
         timeouts    : {}",
        checked as usize * args.probes,
        elapsed.as_secs_f64(),
        divergences.len(),
        timeouts,
    );

    if !divergences.is_empty() {
        if let Some(parent) = args.out_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::File::create(&args.out_path) {
            for d in &divergences {
                let _ = writeln!(f, "{d}");
            }
        }
        println!(
            "wrote {} divergences to {}",
            divergences.len(),
            args.out_path.display()
        );
        for d in divergences.iter().take(10) {
            println!("\n{d}");
        }
        std::process::exit(1);
    }
    println!("no divergences — scalars matches reference scala across all probes ✓");
}
