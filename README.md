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
  against a live `scala` across ten generators (32,000+ probes clean).

This is an early slice: single-object programs whose entry point (`def main`, or
an `extends App` body) uses `val`/`var` bindings (with `val` immutability
enforced), arithmetic, `if`/`while`, the Scala range `for`, and `println`/`print`.
User-defined `def`s — parameters, recursion, mutual recursion, `return`, and a
tail `if`/`else` result — compile to fusevm's native call frames, and postfix
`.` dispatch wires a core `String`/`Int`/`Double` method slice. Classes,
collections, and the wider standard library are the next waves (see
[`BUGS.md`](BUGS.md)). Nothing is faked — an unsupported construct is a parse
error or an honest runtime throw, not a silent mis-run.

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
  callable paren-less.
- **Expressions** — integer / floating / string / char / boolean / `null`
  literals; the binary operators `+ - * / %`, `== != < > <= >=`, `&& ||`
  (short-circuiting); unary `-` and `!`; parenthesised grouping; Scala's `+`
  string concatenation; `Int`-vs-`Double` division dispatch (integer `/ 0`
  throws `ArithmeticException`, floating `/ 0.0` is `Infinity`); `if`/`else` in
  value position (`val r = if (c) a else b`, including block branches).
- **String interpolation** — `s"…"` with `$id` and `${expr}` splices, `f"…"`
  with Java-`Formatter` specs (`%d`, `%.2f`, `%-5s`, `%05d`, `%x`, `%b`, …), and
  `raw"…"` (escapes stay literal).
- **Pattern matching** — `expr match { case … }` over literal, `_` wildcard,
  variable-binding, typed (`case s: String`), and guarded (`case x if x > 0`)
  patterns; a non-exhaustive match throws `scala.MatchError`. Constructor /
  case-class patterns are not modeled (fusevm-blocked — see [`BUGS.md`](BUGS.md)).
- **Method dispatch** — postfix `.` on core values: `String` (`length`,
  `toUpperCase`/`toLowerCase`, `trim`, `reverse`, `substring`, `charAt`,
  `contains`/`startsWith`/`endsWith`, `toInt`/`toDouble`, …), `Int`/`Double`
  (`abs`, `min`/`max`, `round`, `toDouble`/`toInt`, …), and `toString` on any
  value; chains left-to-right (`s.trim.length`).
- **Control flow** — `if` / `else if` / `else` (statement *and* expression
  position), `while`, and Scala range `for` comprehensions: side-effecting
  `for (i <- a until b) …` and `for (i <- a to b) yield …` (collecting a
  `Vector`), with multiple `;`-separated generators and `if` guards.
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
literal / wildcard / variable / typed / guarded patterns; and `for … yield`
range comprehensions (multi-generator + `if` guards) collecting a `Vector`.

### Differential parity fuzzer

`cargo run --bin parity-fuzz -- --count 300 --probes 40` generates
deterministic-output Scala programs — each packing many probes to amortize the
reference toolchain's JVM startup — biased toward the historically weak areas of
a from-scratch frontend (`Int`-vs-`Double` division, `+` concatenation rules,
structural `==`, `Double.toString` notation, range `for`) and diffs `scala
<file>` against this frontend, shrinking any divergence to the offending probe.
It needs a real `scala` on `PATH` (or `SCALARS_FUZZ_SCALA`), so CI never runs it;
`tests/parity.rs` replays a frozen, scala-verified corpus instead. The fuzzer
found the float-notation and `Boolean/null + String` gaps that this release
fixes; 32,000+ probes now run clean.

Next waves, in priority order:

1. **Reference types** — the wider `String`/numeric method surface, collections
   (`List`, `Array`, `Map`) as first-class values with `map`/`flatMap`/`filter`,
   and a class/`case class`/trait object model. The last is blocked on an
   ordered, type-tagged record value (fusevm's `Value` has only an unordered
   `Hash` and an opaque `Obj` handle) — see [`BUGS.md`](BUGS.md). Collection
   generators in `for` (`for (x <- List(1,2,3))`) and constructor patterns in
   `match` (`case Some(x)`) unlock with it.
2. **Scala idioms** — lambdas / function values (needed for real
   `map`/`flatMap`/`withFilter` desugaring), tuples, and `Option`.

See [`BUGS.md`](BUGS.md) for the honest known-gaps list.

---

## [0xFF] LICENSE

MIT — free and open source. See [`LICENSE`](LICENSE).
