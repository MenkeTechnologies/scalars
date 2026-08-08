```
███████╗ ██████╗ █████╗ ██╗      █████╗ ██████╗ ███████╗
██╔════╝██╔════╝██╔══██╗██║     ██╔══██╗██╔══██╗██╔════╝
███████╗██║     ███████║██║     ███████║██████╔╝███████╗
╚════██║██║     ██╔══██║██║     ██╔══██║██╔══██╗╚════██║
███████║╚██████╗██║  ██║███████╗██║  ██║██║  ██║███████║
╚══════╝ ╚═════╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝╚══════╝
```

[![CI](https://github.com/MenkeTechnologies/scalars/actions/workflows/ci.yml/badge.svg)](https://github.com/MenkeTechnologies/scalars/actions/workflows/ci.yml)
![Rust](https://img.shields.io/badge/Rust-2021-05d9e8?style=flat-square)
![license](https://img.shields.io/badge/license-MIT-ff2a6d?style=flat-square)
![status](https://img.shields.io/badge/status-active%20%C2%B7%20in%20development-9b5de5?style=flat-square)

### `[SCALA, COMPILED TO BYTECODE — JIT-COMPILED, NOT WALKED — NO JVM]`

> *"The JVM runs Scala on the JVM. scalars runs Scala on fusevm."*

**Scala in Rust** — a Scala frontend that lexes and parses Scala source, lowers
it to [`fusevm`](https://github.com/MenkeTechnologies/fusevm) bytecode, and runs
it on the shared three-tier Cranelift JIT — the same engine behind `zshrs`,
`stryke`, `awkrs`, `elisp`, and `ruby`. No bespoke VM. No JVM. No `.class`
files.

---

## Table of Contents

- [\[0x00\] Overview](#0x00-overview)
- [\[0x01\] Install](#0x01-install)
- [\[0x02\] Usage](#0x02-usage)
- [\[0x03\] Language Features](#0x03-language-features)
- [\[0x04\] Command-Line Flags](#0x04-command-line-flags)
- [\[0x05\] Architecture](#0x05-architecture)
- [\[0x06\] Status & Roadmap](#0x06-status--roadmap)
- [\[0xFF\] License](#0xff-license)

---

## [0x00] OVERVIEW

Every Scala runtime in existence targets a host VM: `scalac` emits JVM `.class`
bytecode (or Scala.js JavaScript, or Scala Native machine code), and a JVM
interprets and JIT-compiles it. `scalars` takes a different path — it lexes and
parses Scala to an AST, lowers that AST **directly to fusevm bytecode**, and runs
it on fusevm's compiled VM with a Cranelift tracing JIT. scalars carries no VM or
JIT of its own; it is a pure frontend over the shared engine. Highlights:

- **Compiled, not tree-walked** — arithmetic, comparisons, and control flow
  lower to native fusevm ops (`LoadInt`, `Add`, `NumLt`, `JumpIfFalse`, …), so
  the tracing JIT compiles hot loops to native code.
- **fusevm-hosted, no JVM** — no local `vm.rs` / `jit.rs`, no `.class` files, no
  `libjvm`. The same three-tier Cranelift engine that hosts zshrs, stryke,
  awkrs, elisp, and ruby runs Scala too. `jit-disk-cache` persists native code
  across runs.
- **Scala print semantics** — `println`/`print` lower to a formatting builtin so
  `Boolean` prints `true`/`false`, `Double` prints `3.0`, and `null` prints
  `null` — matching `scala`, not the VM's shell-flavoured default.
- **Scala `/` semantics** — a type-dispatching division builtin truncates when
  both operands are `Int` (`7 / 2 == 3`) and floats when either is a `Double`
  (`7 / 2.0 == 3.5`), because fusevm's native divide is always floating.
- **Scala 3 `+` rules** — a strict numeric hook supplies `String` concatenation
  (`"x=" + x`, `1 + "a"`) for the mixed operands the VM's native arithmetic does
  not compute, while rejecting `Boolean`/`null` `+ String` exactly as Scala 3
  does (the universal `any2stringadd` was removed); all-numeric arithmetic stays
  on the JIT fast path.
- **`Double.toString` fidelity** — whole/decimal values in `[1e-3, 1e7)` print
  plain (`3.0`, `9999999.0`), everything else in Java's computerized scientific
  notation (`1.0E7`, `1.23456789E8`, `1.0E-4`), and exponent literals (`6.022e23`,
  `1E10`) lex — all matching `scala`.
- **Verified against Scala** — the examples and test corpus are diffed
  byte-for-byte against a reference `scala` and frozen (CI needs no Scala
  toolchain), and a `parity-fuzz` binary differentially fuzzes this frontend
  against a live `scala` across twenty-nine generators — every probe in this
  wave's `patmatch`, `option`, `caseclass`, `strops` and `nlr` modes ran clean,
  on top of the 61,000+ that ran clean before it.

The language surface today: programs with an entry point (`def main`, or an
`extends App` body) plus sibling `class`/`object` declarations, using `val`/`var`
bindings (with `val` immutability enforced), arithmetic, `if`/`while`, the Scala
range `for` (with a `by` step), `try`/`catch`/`finally`/`throw`, and
`println`/`print`. User-defined `def`s — parameters, recursion,
mutual recursion, `return`, a tail `if`/`else` result, and true block-local
scoping (an inner `def` shadows an outer one; enclosing locals are lambda-lifted
into parameters) — compile to fusevm's native call frames, and postfix `.`
dispatch wires a core `String`/`Int`/`Double` method slice. A host-side object
model (`class`/`object`/`case class`, `trait`, `extends`/`with`, `override`,
`super`, virtual dispatch, `new`, fields, `this`, structural
`equals`/`hashCode`/`toString`, `copy`, `apply`/`unapply`, constructor patterns,
`isInstanceOf`, built-in `Option`) rides fusevm's `Value::Obj` handle.
First-class functions (`x => …`, `(a, b) => …`, block bodies, `_` placeholders,
capture of the enclosing frame, `{ case … }` partial-function literals with a
real `isDefinedAt` behind `collect`/`collectFirst`/`lift`/`orElse`), the
immutable collections `List`/`Seq`/`Vector`/`Set`/`Map` (the full combinator set,
`::`, indexing, `k -> v`, `for … yield` comprehensions, and Scala's own
`HashSet`/`HashMap` trie ordering past four entries — including for a set or map
keyed by another collection, through the ported `MurmurHash3` seq/set/map
hashes), the mutable collections
(`ListBuffer`, `ArrayBuffer`, `mutable.Set`/`Map` with their hash table's own
iteration order, and the `+=`/`-=`/`++=` mutators), `Array`, first-class `Range`
values and `scala.math` are modeled host-side too. The wider standard library is
the next wave (see [`BUGS.md`](BUGS.md)).
Nothing is faked — an unsupported construct is a parse error or an honest runtime
throw, not a silent mis-run.

---

## [0x01] INSTALL

```sh
git clone https://github.com/MenkeTechnologies/scalars
cd scalars
cargo build

# run a .scala file
./target/debug/scala examples/FizzBuzz.scala
```

`scalars` is a standalone Rust crate (an explicit empty `[workspace]` keeps it
independent of the meta repo). `fusevm` is pulled from crates.io with the `jit`,
`jit-disk-cache`, and `aot` features. Run the tests with `cargo test` (no Scala
toolchain required).

#### Zsh tab completion

```sh
cp completions/_scala /usr/local/share/zsh/site-functions/_scala
# or: fpath=(/path/to/scalars/completions $fpath) in .zshrc
autoload -Uz compinit && compinit
```

---

## [0x02] USAGE

```scala
object FizzBuzz {
  def main(args: Array[String]): Unit = {
    for (i <- 1 to 15) {
      if (i % 15 == 0) println("FizzBuzz")
      else if (i % 3 == 0) println("Fizz")
      else if (i % 5 == 0) println("Buzz")
      else println(i)
    }
  }
}
```

```sh
$ scala FizzBuzz.scala
1
2
Fizz
4
Buzz
...
```

The shorter `object … extends App` form runs its body directly, no explicit
`main`:

```scala
object Hello extends App {
  val who = "fusevm"
  println("Hello from scalars — Scala on " + who)
}
```

---

## [0x03] LANGUAGE FEATURES

Implemented and checked against the reference `scala`:

- **Entry point** — `object Name { def main(args: Array[String]): Unit = { … } }`
  or `object Name extends App { … }` (the body runs directly). An object that
  also declares other members still finds and runs its `main`.
- **Bindings** — `val` / `var` with optional type ascription
  (`val x: Int = …`, `var s = …`), type inferred as storage; plain and compound
  assignment to a `var` (`=`, `+=`, `-=`, `*=`, `/=`, `%=`). Reassigning a `val`
  (or a method parameter) is a compile error, as in `scalac`.
- **User-defined methods** — helper `def f(a: T, b: U): R = body` alongside
  `main` (or in an `App` body) compile to fusevm's native `Op::Call` frames:
  parameters bind to per-call frame slots, so recursion and mutual recursion are
  correct; the body's last expression (including a tail `if`/`else`) is the
  result; `return e` / bare `return` exit early; a zero-parameter `def` is
  callable paren-less. A `def` declared inside a block is scoped to that block:
  two blocks may each declare `def f`, an inner one shadows an outer one, and
  the enclosing-frame locals a local `def` reads are lambda-lifted into extra
  parameters every call site passes (`src/resolve.rs`).
- **Expressions** — integer / floating / string / char / boolean / `null`
  literals; the binary operators `+ - * / %`, `== != < > <= >=`, `&& ||`
  (short-circuiting); unary `-` and `!`; parenthesised grouping; Scala's `+`
  string concatenation; `Int`-vs-`Double` division dispatch (integer `/ 0`
  throws `ArithmeticException`, floating `/ 0.0` is `Infinity`); `if`/`else` in
  value position (`val r = if (c) a else b`, including block branches).
- **Exceptions** — `try { … } catch { case e: T [if guard] => … } finally { … }`
  and `throw e`, both value-producing expressions. Handler arms match the JVM
  throwable hierarchy (`case e: Exception` catches an
  `IllegalArgumentException`), an unmatched arm keeps unwinding, and `finally`
  runs on the normal *and* the exceptional exit. Runtime faults the host already
  raised are catchable with their JDK messages (`ArithmeticException: / by
  zero`, `NumberFormatException: For input string: "zz"`, `scala.MatchError`);
  `new RuntimeException("…")` and the other built-in throwables construct
  without a user `class`, and expose `getMessage`/`toString`.
- **Ranges** — `a to b`, `a until b`, `… by s` as first-class values with
  Scala's `Range.toString` (`Range 1 to 10 by 3`, and the `empty `/`inexact `
  prefixes), `sum`/`length`/`map`/`filter`/`toList`/`reverse`/`mkString`/…; used
  as a `for` generator they still compile to a counted loop, with `s` a literal
  or a runtime value of either sign, and a zero step throwing
  `IllegalArgumentException` as Scala's `Range` does.
- **String interpolation** — `s"…"` with `$id` and `${expr}` splices, `f"…"`
  with Java-`Formatter` specs (`%d`, `%.2f`, `%-5s`, `%05d`, `%x`, `%b`, …), and
  `raw"…"` (escapes stay literal).
- **Pattern matching** — `expr match { case … }` over literal, `_` wildcard,
  variable-binding, typed (`case s: String`), guarded (`case x if x > 0`), and
  constructor / case-class patterns (`case Point(x, y)`, `case Some(v)`,
  `case None`) — nested and guarded; a non-exhaustive match throws
  `scala.MatchError`.
- **Object model** — `class C(x: Int) { def m = … }` with `new C(…)`, fields,
  `this`, in-place `var`-field mutation, and instance-method dispatch; `object`
  singletons (static `def`s, `Name.val` members); and `case class` with an
  ordered-field `toString` (`Point(1,2)`), structural `equals`/`hashCode`,
  `copy(field = …)`, companion `apply` (no `new`) and `unapply` — all four over
  the primary-constructor parameters only, as Scala derives them. Built-in
  `Option` (`Some(v)` / `None`). All of it rides a host-side object heap behind
  fusevm's `Value::Obj` handle (`src/host.rs`) — no fusevm changes, no JVM.
- **Traits and inheritance** — `trait T { def f: Int; def g = … }` with abstract
  and concrete members, `class C(x) extends P(x) with T1 with T2`, `override
  def`, `super.m(…)`, and virtual dispatch off the receiver's runtime class tag
  (a `List[Shape]` of mixed subclasses dispatches correctly). Constructor
  arguments thread up the whole superclass chain, supertype bodies run base-most
  first, `sealed trait` + `case class`/`case object` ADTs match by constructor
  pattern, and `x.isInstanceOf[T]` / `case x: T =>` consult the registered
  hierarchy.
- **First-class functions** — lambdas (`x => e`, `(a, b) => e`, block bodies
  `x => { … }`, `Int => Int` function-type annotations) and the `_`-placeholder
  form (`_ + 1`, `_ * 2`, `_ + _`). A lambda captures its enclosing frame, so it
  can be stored in a `val`, passed as an argument, returned, and invoked (`f(x)` /
  `f.apply(x)`) — curried closures (`def adder(n: Int): Int => Int = x => x + n`)
  see their upvalues after the defining frame returns. Modeled as a host-heap
  closure re-entering the VM to run its body — no fusevm changes.
- **Collections** — the immutable `List`/`Seq`/`Vector`/`Set`/`Map` family, plus
  `Nil`, `::` cons and `a -> b` tuple pairs. Transformations
  (`map`/`flatMap`/`filter`/`foreach`), folds
  (`foldLeft`/`foldRight`/`fold`/`reduce*`), aggregates
  (`sum`/`product`/`min`/`max`/`minBy`/`maxBy`/`count`), predicates
  (`exists`/`forall`/`find`/`indexOf`/`indexWhere`/`contains`), ordering
  (`sorted`/`sortBy`/`sortWith`/`reverse`/`distinct`), slicing
  (`take`/`drop`/`slice`/`splitAt`/`span`/`partition`/`takeWhile`/`dropWhile`/
  `init`/`tail`/`headOption`), pairing (`zip`/`zipWithIndex`/`unzip`/`flatten`/
  `grouped`/`sliding`), `groupBy`, `mkString`, the `to*` conversions, the set
  algebra (`union`/`intersect`/`diff`/`subsetOf`, `+`/`-`/`++`/`:+`/`+:`), and
  `Map`'s `apply`/`get`/`getOrElse`/`keys`/`values`/`updated`. `toString` is
  byte-faithful, which for `Set`/`Map` means reproducing Scala's representation
  split: up to four entries the insertion-ordered `Set(…)`/`Map(…)`, beyond that
  a CHAMP `HashSet(…)`/`HashMap(…)` in trie order (the JVM hash codes, the
  `MurmurHash3` product/seq/set/map hashes, the trie's `improve` scramble and its
  iteration order are all ported, so a set or map keyed by another *collection*
  orders correctly too). Mutable `Array` too — `Array(a, b)`,
  `new Array[T](n)` (zero-filled per `T`), `a(i)` reads and `a(i) = v` writes.
- **The bitwise and shift operators** — `&`, `|`, `^`, `~`, `<<`, `>>`, `>>>` on
  `Int` (evaluated at 32-bit `Int` width, so `1 << 33` is `2`), `&`/`|`/`^` on
  `Boolean`, `&`/`|`/`&~` on `Set`, and hexadecimal literals. Precedence follows
  the SLS's first-character table, which is where `&&`/`||` get theirs too.
- **Partial functions** — a `{ case … }` literal answers `isDefinedAt` as well
  as `apply`, which is what `collect`/`collectFirst` need to skip a
  non-matching element; `applyOrElse`, `lift`, `orElse`, `andThen` and
  `compose` compose function values.
- **`scala.collection.mutable`** — `ListBuffer`, `ArrayBuffer`, `mutable.Set`
  and `mutable.Map`, with `+=`/`-=`/`++=`/`--=`, `append`/`prepend`/`insert`/
  `remove`/`clear`, `put`/`update`/`getOrElseUpdate`, and the same combinator
  set. A mutable `Set`/`Map` prints in its **hash table's** order, which is a
  different algorithm from the immutable trie and is ported from the 2.13
  sources down to the table sizing that decides it.
- **`scala.math`** — `abs`, `signum`, `min`/`max`, `round`/`floor`/`ceil`/`rint`,
  `sqrt`/`cbrt`/`exp`/`log`/`log10`/`pow`/`hypot`, the trig family, `atan2`,
  `toRadians`/`toDegrees`, `Pi`, `E`, under the `math`, `scala.math`, `Math` and
  `java.lang.Math` spellings. Integral overloads stay integral, and the two
  namespaces are kept apart where the JDK lacks an overload (`Math.signum(5)` is
  `1.0`, `math.signum(5)` is `1`).
- **Method dispatch** — postfix `.` on core values: `String` (`length`,
  `toUpperCase`/`toLowerCase`, `trim`, `reverse`, `substring`, `charAt`,
  `contains`/`startsWith`/`endsWith`, `toInt`/`toDouble`, …), `Int`/`Double`
  (`abs`, `min`/`max`, `round`, `toDouble`/`toInt`, …), and `toString` on any
  value; chains left-to-right (`s.trim.length`).
- **Control flow** — `if` / `else if` / `else` (statement *and* expression
  position), `while`, and `for` comprehensions over both integer ranges
  (`for (i <- a until b) …`, `for (i <- a to b) yield …` collecting a `Vector`)
  and collections (`for (x <- List(1,2,3)) yield x*2`, desugared to
  `.map`/`.flatMap`/`.withFilter`), with multiple generators, `if` guards, both
  the `for (…)` and `for { … }` enumerator groups, and destructuring generators
  (`for ((k, v) <- m)`).
- **Infix method syntax** — `a m b` is `a.m(b)` for any single-argument method
  (`xs contains 2`, `1 to n`, `xs map f mkString ","`), with Scala's precedence
  (alphanumeric operators bind loosest) and associativity (a name ending in `:`
  dispatches on its right operand, so `0 +: xs` is `xs.+:(0)`).
- **Generics, type-erased** — type parameters on `class`/`case class`/`trait`/
  `def` and type arguments at use sites parse and run; nothing is checked or
  specialized.
- **Statement separators** — inferred line breaks *or* explicit `;` (the lexer
  applies Scala's can-end / can-begin newline rule, so no semicolons are needed).
- **Output** — `println(x)` / `print(x)` with Scala value formatting.
- **Comments** — `//` line, `/* … */` block.

---

## [0x04] COMMAND-LINE FLAGS

| Flag | Effect |
| --- | --- |
| `FILE [args…]` | Run a `.scala` file. |
| `-version` / `--version` | Print the version banner and exit. |
| `-h` / `--help` | Print usage and exit. |
| `--dump-tokens FILE` | Print the lexer token stream and exit. |
| `--dump-ast FILE` | Print the parsed AST and exit. |
| `--disasm FILE` | Print the lowered fusevm bytecode and exit. |
| `--tiers FILE` | Run it, then report which fusevm execution tier took each of its chunks. |

`scala --version` reports the targeted language level (`3.3`) followed by the
real engine (`scalars <crate-version>`) and the host triple, so nothing is
misrepresented as the JVM Scala.

---

## [0x05] ARCHITECTURE

scalars contains no virtual machine or JIT of its own. The execution path
mirrors how `zshrs` hosts zsh and `ruby` hosts Ruby:

```
Scala source → lexer → parser (AST) → lower to fusevm bytecode → fusevm VM + Cranelift JIT
                            │
              strict numeric hook (Scala `+` concat)
              division builtin (Int-vs-Double dispatch)
              print builtins (Scala value formatting)
```

| Piece | How |
| --- | --- |
| **fusevm-hosted** | No local `vm.rs` / `jit.rs`, no JVM. Scala lowers to fusevm bytecode and runs on the shared three-tier Cranelift JIT; `jit-disk-cache` persists native code across runs. |
| **Newline inference** | The lexer emits a statement separator for a source line break only where one token can end a statement and the next can begin one (Scala's rule), so idiomatic semicolon-free source parses. |
| **Native arithmetic** | Operators lower to native fusevm ops; the JIT traces hot integer loops. A strict numeric hook supplies Scala's `+` string concatenation for non-numeric operands; a division builtin restores `Int` truncation. |
| **Scala print semantics** | `println`/`print` lower to a registered builtin that formats values Scala-style (`true`/`false`, `3.0`, `null`), rather than the VM's shell-flavoured `PrintLn`. |

---

## [0x06] STATUS & ROADMAP

This release: single-object programs, `def main` / `extends App`, `val`/`var`
(with `val` immutability enforced), arithmetic / comparison / logic, `if` /
`while` / range `for`, `println`/`print`, Scala 3 `+` rules, `Int`-vs-`Double`
division (integer `/ 0` throws `ArithmeticException`), and
`Double.toString`-accurate float formatting — all verified byte-for-byte against
a reference `scala` and continuously fuzzed against it (see below). Added since
slice 1: user-defined `def`s (parameters, recursion, mutual recursion, `return`,
tail `if`/`else` result) over fusevm's native `Op::Call` frame ABI; postfix `.`
dispatch wiring a core `String`/`Int`/`Double` method slice; `s`/`f`/`raw` string
interpolation; `if`/`else` in expression position; `match`/`case` over
literal / wildcard / variable / typed / guarded / **constructor** patterns; a
`for … yield` range comprehension (multi-generator + `if` guards) collecting a
`Vector`; a **host-side object model** — `class`/`object`/`case class`,
`new`, fields, `this`, method dispatch, structural `equals`/`hashCode`,
`toString`, `copy`, companion `apply`/`unapply`, and built-in `Option`; `by`
steps on range comprehensions; **exceptions** —
`try`/`catch`/`finally`/`throw` with JVM-hierarchy handler matching, over a
statement-granular unwind protocol that needs no fusevm changes;
**block-local `def` scoping** with lambda-lifted captures (`src/resolve.rs`);
**traits and inheritance** — `trait`, `extends`/`with`, `override`, `super`,
virtual dispatch, inherited fields and constructor-argument threading, and
`isInstanceOf`/typed patterns over the registered hierarchy; **`Array`,
first-class `Range` values and `scala.math`**; the **full pattern grammar** —
`@` binders, `|` alternations, `h :: t` and `Nil`, sequence patterns with a
trailing `_*`, and pattern definitions (`val (a, b) = pair`); **`scala.Option`'s
method surface** plus the `Option(x)` factory and `Either`'s `Left`/`Right`;
`Product` on every case class and tuple; a wider `String`/`StringOps` surface
including the closure-taking combinators; and **non-local `return`** — a
`return` inside a lambda (or inside the closures a `for` desugars to) leaves the
enclosing method, running every `finally` on the way out, lowered exactly as
Scala lowers it.

### Differential parity fuzzer

`cargo run --bin parity-fuzz -- --count 300 --probes 40` generates
deterministic-output Scala programs — each packing many probes to amortize the
reference toolchain's JVM startup — biased toward the historically weak areas of
a from-scratch frontend (`Int`-vs-`Double` division, `+` concatenation rules,
structural `==`, `Double.toString` notation, range `for`, `by` steps, IEEE
division by zero, `try`/`catch`, block-local `def` scoping, traits and virtual
dispatch, `Range` values, `Array`, and the `scala.math` overload split) and diffs
`scala <file>` against this frontend, shrinking any divergence to the offending
probe. Individual generators run with `--mode <name>` (`step`, `ieee`, `exc`,
`localdef`, `oop`, `range`, `array`, `math`, `coll`, `hashcoll`, `infix`,
`partial`, `mutable`, `bitwise`, `patmatch`, `option`, `caseclass`, `strops`,
`nlr`, …). It needs a real `scala` on
`PATH` (or `SCALARS_FUZZ_SCALA`), so CI never runs it; `tests/parity.rs` replays
a frozen, scala-verified corpus instead. The fuzzer found the float-notation and
`Boolean/null + String` gaps, the `catch`-guard binding bug, and — in this
release — the block-expression-in-value-position gap, the
`Math.signum` vs. `math.signum` overload split, the builder an empty
`Map.collect` picks, and `-3.abs` reading as `-(3.abs)` instead of `(-3).abs`.
The `nlr` mode found two control-flow bugs in this release: a `return` inside a
`for`/`foreach` body silently ended only that iteration instead of the method,
and a `return` out of a `try` skipped the `finally` entirely.

Next waves, in priority order:

1. **Lazy views** — `.view` and `LazyList` (`.iterator` is wired, but strictly).
   Alongside them the three parse-level gaps the new modes surfaced: type
   ascription in expression position (`(e: T)`), the unit literal `()`, and a
   `y = …` value definition inside a `for` comprehension.
2. **The broader standard library** — `scala.io`, `scala.collection.*` as a
   namespace, and user `Ordering`s.
3. **The rest of `scala.collection.mutable`** — the insertion-ordered
   `LinkedHashSet`/`LinkedHashMap`, `Queue`, `Stack` and `ArrayDeque`.

See [`BUGS.md`](BUGS.md) for the honest known-gaps list.

---

## [0xFF] LICENSE

MIT — free and open source. See [`LICENSE`](LICENSE).
