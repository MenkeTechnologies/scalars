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
  `Boolean` prints `true`/`false`, `Double` prints `3.0`, `null` prints `null`
  and `Unit` prints `()` — matching `scala`, not the VM's shell-flavoured
  default. `Unit` is its own value rather than an alias for the absent one, so
  `println(xs.foreach(f))` and `println(println("x"))` render `()`.
- **Scala `/` semantics** — a type-dispatching division builtin truncates when
  both operands are `Int` (`7 / 2 == 3`) and floats when either is a `Double`
  (`7 / 2.0 == 3.5`), because fusevm's native divide is always floating.
- **Scala 3 `+` rules** — a strict numeric hook supplies `String` concatenation
  (`"x=" + x`, `1 + "a"`) for the mixed operands the VM's native arithmetic does
  not compute, while rejecting `Boolean`/`null` `+ String` exactly as Scala 3
  does (the universal `any2stringadd` was removed); most numeric arithmetic
  stays on the JIT fast path.
- **Widening past 2^53** — a mixed `Int`/`Double` pair whose integer an `f64`
  cannot hold exactly (`16677181699666569L`) is handed to the same hook rather
  than computed on the rounded value. Scala's answer is the *promoted* one — its
  binary numeric promotion widens to `Double` first, so
  `16677181699666569L == 1.6677181699666568E16` is `true` — and the hook returns
  it for `+`, `-`, `*`, `%`, `/` and all six comparisons in either operand
  order.
- **`Double.toString` fidelity** — whole/decimal values in `[1e-3, 1e7)` print
  plain (`3.0`, `9999999.0`), everything else in Java's computerized scientific
  notation (`1.0E7`, `1.23456789E8`, `1.0E-4`), and exponent literals (`6.022e23`,
  `1E10`) lex — all matching `scala`. Java picks a decimal VALUE and only then
  lays it out, so both of its exceptions to "shortest that round-trips" apply:
  the two-significant-digit floor is a rounding rule, making
  `Double.MinPositiveValue` `4.9E-324` rather than `5E-324` with a zero stuck
  on, and an exact tie goes to the even significand, making `5 * 2^-23`
  `5.960464477539062E-7`. 90,371 values diffed line by line against `scala` —
  the first 60000 subnormals, every power of two in the range with its odd
  multiples, and 16000 pseudo-random draws across the whole exponent span —
  agree everywhere.
- **`Float` at single precision** — `Float` is a distinct type, told apart from
  `Double` by the same static analysis that tells an `Int` from a `Long` (one
  runtime representation, two widths). It rounds where Scala rounds: at the
  LITERAL (`16777217.0f` is `1.6777216E7`, `1.0e-45f` is `1.4E-45`), at every
  OPERATION (`1.0f / 3.0f` is `0.33333334`, and `16777217.0f * 0.2f` is
  `3355443.2` — computing it at 64 bits and narrowing after rounds twice and
  answers `3355443.3`), and at every crossing into a `String`, where
  `Float.toString` picks the shortest decimal that round-trips through 32 bits
  (`0.1f` is `0.1`; the `Double` with those bits is `0.10000000149011612`). The
  constants (`Float.MaxValue` is `3.4028235E38`), `.toFloat`, `getClass`
  (`float`) and `hashCode` follow. It is a distinct RUNTIME value — its 32 bits
  ride in a `Value` variant of exactly that width — so it renders as a `Float`
  wherever it is reached from: out of a `List`, a `case class` field, a `Map`
  value, or an `Any`. What it costs is the JIT, which takes only the VM's two
  native numeric shapes; see `BUGS.md`.
- **`lazy val`** — the initializer runs at the FIRST read and at most once, and
  not at all if the binding is never read. All three are observable when it
  prints or throws, so the binding holds a thunk in a cell until the first read
  replaces it with the value it produced.
- **Scala 3 entry points** — `@main def go(n: Int, s: String)` binds its
  parameters from the command line through the same reader Scala generates,
  including the wording and the exit status of a bad one (`Illegal command line
  after first argument: …` on *stdout*, status 0). Top-level `def`s and `val`s
  are the members of the synthetic `Foo$package` object, and a top-level `val`'s
  initializer runs before the entry body and before the command line is read.
  `def main(args: Array[String])`'s `args` is the real argument vector.
- **Verified against Scala** — the examples and test corpus are diffed
  byte-for-byte against a reference `scala` and frozen (CI needs no Scala
  toolchain), and a `parity-fuzz` binary differentially fuzzes this frontend
  against a live `scala` across its whole generator set — this wave added the
  `--entry` axis, which runs any of them under `@main` / `def main` /
  `extends App` rather than the one shape every probe used to be written in.

The language surface today: programs with an entry point (`@main def`, `def
main`, or an `extends App` body) plus top-level `def`/`val` definitions and
`class`/`object` declarations beside it or inside it,
using `val`/`var`
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
(`ListBuffer`, `ArrayBuffer`, `Queue`, `Stack`, `ArrayDeque`, the raw-heap-order
`PriorityQueue` and `StringBuilder`;
`mutable.Set`/`Map` with their hash table's own iteration order and the
insertion-ordered `LinkedHashSet`/`LinkedHashMap`; the `+=`/`-=`/`++=`
mutators), `Array`, first-class `Range` values, an explicit `Ordering`, the
boxed-primitive statics (`Int.MaxValue`, `Integer.parseInt`, `Character.isDigit`,
`String.valueOf`), `getClass` and `scala.math` are modeled host-side too. The
wider standard library is the next wave (see [`BUGS.md`](BUGS.md)).
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
- **Type declarations, wherever Scala accepts them** — `class`, `trait`,
  `case class`, `case object` and `object` beside the entry object, inside its
  `extends App` body (where they are members, as in Scala), or inside any block
  such as a `def` body. Members are not ordered the way statements are, so a
  member `class` may be declared after the statement that constructs it. A
  `class Q` beside an `object Q` is the companion idiom and compiles; a genuine
  redeclaration is refused rather than silently resolved to one of the two.
- **Bindings** — `val` / `var` with optional type ascription
  (`val x: Int = …`, `var s = …`), type inferred as storage; plain and compound
  assignment to a `var` (`=`, `+=`, `-=`, `*=`, `/=`, `%=`). Reassigning a `val`
  (or a method parameter) is a compile error, as in `scalac`. A compound
  assignment also reaches a target that is not a plain name — `a(i) += 1`,
  `counts(w) += 1`, `g(0)(1) += 9`, `obj.field += 5` — which Scala expands to
  `l.update(args, l.apply(args) op r)` unless the element has an `op=` member of
  its own, in which case that member mutates it in place. The receiver and the
  indices are evaluated exactly once. Plain `=` reaches the same targets:
  `obj.field = v` writes a `var` field through its receiver, and `Cfg.n = 10` /
  `Cfg.n += 5` write a singleton `object`'s `var` — which is a global rather
  than a record field, so both forms reach the same storage the object's own
  `def`s do. A compound assignment is also an
  EXPRESSION, as in Scala — `println(buf += 1)` prints the buffer (the `op=`
  member answers its receiver), `println(n += 1)` prints `()` (the
  `n = n + 1` expansion is an assignment), and `(buf += 2) += 3` chains.
- **User-defined methods** — helper `def f(a: T, b: U): R = body` alongside
  `main` (or in an `App` body) compile to fusevm's native `Op::Call` frames:
  parameters bind to per-call frame slots, so recursion and mutual recursion are
  correct; the body's last expression (including a tail `if`/`else`) is the
  result; `return e` / bare `return` exit early; a zero-parameter `def` is
  callable paren-less. A `def` declared inside a block is scoped to that block:
  two blocks may each declare `def f`, an inner one shadows an outer one, and
  the enclosing-frame locals a local `def` reads are lambda-lifted into extra
  parameters every call site passes (`src/resolve.rs`).
- **Parameter lists** — default values (`def f(x: Int, y: Int = 10)`, evaluated
  at the call site and only when the argument is omitted), named arguments in
  any order (`f(y = 1, x = 2)`), repeated parameters (`def f(xs: Int*)`, which
  arrive as the `ArraySeq` Scala hands a varargs method), and by-name parameters
  (`def f(x: => Int)`, passed as a thunk and re-evaluated at every use — so an
  argument that is never read never runs).
- **`break` / `breakable`** — `scala.util.control.Breaks`, lowered the way the
  library implements it: `break()` raises a `BreakControl` that `breakable`
  catches. A `finally` between the two still runs, a `catch { case e: Exception }`
  between them does not swallow it, and a `break` inside a nested `def` unwinds
  out of that frame.
- **Expressions** — integer / floating / string (including the triple-quoted
  `"""…"""` form, taken verbatim) / char / boolean / `null`
  literals and the unit literal `()`; the binary operators `+ - * / %`,
  `== != < > <= >=`, `&& ||` (short-circuiting); unary `-` and `!`;
  parenthesised grouping and type ascription — both the parenthesised `(e: T)`
  and the bare `e: T` Scala's grammar allows wherever an expression is
  expected (an argument, a `val` initializer), with the numeric widening
  `3: Double` actually widening; Scala's `+` string concatenation
  and `*` string repetition; the `java.util.Formatter` conversions behind the
  `f"…"` interpolator, `"…".format(…)`, `String.format(…)` and `x.formatted(…)`
  (`%s %d %f %e %E %x %X %o %b %c`, with the flags, width and precision, rounded
  HALF_UP off the shortest round-tripping decimal exactly as Java rounds);
  `Int`-vs-`Double` division dispatch (integer `/ 0`
  throws `ArithmeticException`, floating `/ 0.0` is `Infinity`); `if`/`else` in
  value position (`val r = if (c) a else b`, including block branches). Every
  operator is a method, so the dotted spelling works too (`n.+(1)`, `"a".*(3)`).
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
  `raw"…"` (escapes stay literal). A `${…}` splice holds a BLOCK, as Scala's
  does, so it may declare and sequence: `s"${ val q = 3; q * 2 }"` is `6`.
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
- **`override def toString`, everywhere a value is rendered** — `println(p)`,
  `s"$p"`, `"x" + p`, `xs.mkString`, `"%s".format(p)` and every depth of a
  nested collection run the user's override, not only an explicit `p.toString`.
  The `+` sites include the ones with no `String` in the source text: `pre + p`
  for a `val pre = "…"`, `s + p` for a `String` parameter, `xs(0) + p`, and
  `acc += p` — whose far operand is the assignment target, so there is no syntax
  to read at all. An override may itself print, and one raising propagates as
  Scala's does.
- **Overloaded methods** — one name at several arities on a `class` or `object`,
  resolved at the call site by argument count through every dispatch route
  (direct, `super.m`, virtual, unqualified self-call). Overloads differing only
  in parameter type are refused rather than silently answered by the first.
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
  form (`_ + 1`, `_ * 2`, `_ + _`, the applied `_(1)`, the typed `(_: Int) + 1`,
  and the bare `_` argument
  that eta-expands its enclosing call, `xs.map(f(_))`). Both spellings work in
  the BRACE form of an argument as well as the parenthesized one —
  `xs.map { _ * 2 }`, `xs.foldLeft(0) { _ + _ }`, `xs.sortBy { -_ }`,
  `once { 7 }`, and the trailing clause of a curried `def`, `use(3) { _ + 1 }` —
  since a brace group is a block whose value is the argument and a block
  statement is one of the boundaries a placeholder expands at. A lambda captures its enclosing frame, so it
  can be stored in a `val`, passed as an argument, returned, and invoked (`f(x)` /
  `f.apply(x)`) — curried closures (`def adder(n: Int): Int => Int = x => x + n`)
  see their upvalues after the defining frame returns. A lambda that ASSIGNS an
  enclosing `var` (`var t = 0; xs.foreach(x => t += x)`) works: such a binding is
  boxed into a shared heap cell exactly as Scala boxes it, so the write reaches
  the declaring frame — and the closure may outlive that frame
  (`def mk() = { var i = 0; () => { i += 1; i } }`). Only the vars a closure
  actually writes are boxed, so every other local keeps its plain frame slot.
  Modeled as a host-heap closure re-entering the VM to run its body — no fusevm
  changes.
- **Collections** — the immutable `List`/`Seq`/`Vector`/`Set`/`Map` family, plus
  `Nil`, `::` cons and `a -> b` tuple pairs. Transformations
  (`map`/`flatMap`/`filter`/`foreach`), folds
  (`foldLeft`/`foldRight`/`fold`/`reduce*`), aggregates
  (`sum`/`product`/`min`/`max`/`minBy`/`maxBy`/`count`), predicates
  (`exists`/`forall`/`find`/`indexOf`/`indexWhere`/`contains`), ordering
  (`sorted` — with or without an explicit `Ordering` — `sortBy`/`sortWith`/
  `reverse`/`distinct`), slicing
  (`take`/`drop`/`slice`/`splitAt`/`span`/`partition`/`takeWhile`/`dropWhile`/
  `init`/`tail`/`headOption`), pairing (`zip`/`zipWithIndex`/`unzip`/`flatten`/
  `grouped`/`sliding`), `groupBy`, `mkString`, the `to*` conversions, the set
  algebra (`union`/`intersect`/`diff`/`subsetOf`, `+`/`-`/`++`/`:+`/`+:`), and
  `Map`'s `apply`/`get`/`getOrElse`/`keys`/`values`/`updated`, and the
  companions' `IterableFactory` members — `List.empty`, `List.fill(n)(v)` (whose
  fill expression is by-name and re-evaluated per element),
  `Vector.tabulate(n)(f)`, `List.range(a, b[, step])`, `List.concat(…)` and
  `List.from(xs)`. `toString` is
  byte-faithful, which for `Set`/`Map` means reproducing Scala's representation
  split: up to four entries the insertion-ordered `Set(…)`/`Map(…)`, beyond that
  a CHAMP `HashSet(…)`/`HashMap(…)` in trie order (the JVM hash codes, the
  `MurmurHash3` product/seq/set/map hashes, the trie's `improve` scramble and its
  iteration order are all ported, so a set or map keyed by another *collection*
  orders correctly too). Equality follows the collection's own contract: a `Set`
  is UNORDERED, so `Set(1, 2) == Set(2, 1)` and a `mutable.HashSet` equals the
  immutable `Set` with the same members, while a `Seq` stays positional
  (`List(1, 2) != List(2, 1)`) and a set is never equal to a non-set. Mutable
  `Array` too — `Array(a, b)`,
  `new Array[T](n)` (zero-filled per `T`), `a(i)` reads and `a(i) = v` writes.
- **The bitwise and shift operators** — `&`, `|`, `^`, `~`, `<<`, `>>`, `>>>`,
  each evaluated at its receiver's width, so `1 << 33` is `2` while `1L << 33` is
  `8589934592`; plus `&`/`|`/`^` on `Boolean`, `&`/`|`/`&~` on `Set`, and
  hexadecimal literals. Precedence follows the SLS's first-character table,
  which is where `&&`/`||` get theirs too.
- **32-bit `Int` overflow** — `2147483647 + 1` is `-2147483648`, as are
  `-Int.MinValue` and `math.abs(Int.MinValue)`, while a `Long` operand promotes
  the expression so it does not wrap (`2147483647 + 1L` is `2147483648`) and
  `Long` itself wraps at 64. The runtime is dynamically typed, so the `Int`/`Long`
  split is decided statically from literals, declared types and the methods whose
  result type is fixed — including the widths Scala supplies from context rather
  than from the expression: a lambda parameter takes the element type of what it
  traverses (`List(2147483647, 2).map(_ * 2)` is `List(-2, 4)`), a class field and
  a `def` return annotation carry their declared widths to a use site, and
  `.sum`/`.product` take the collection's element type, so `(1 to 100000).sum` is
  `705082704`. See `BUGS.md` for the positions where no width can be proven and
  the 64-bit answer is kept instead of a guessed one.
- **Partial functions** — a `{ case … }` literal answers `isDefinedAt` as well
  as `apply`, which is what `collect`/`collectFirst` need to skip a
  non-matching element; `applyOrElse`, `lift`, `orElse`, `andThen` and
  `compose` compose function values.
- **`scala.collection.mutable`** — `ListBuffer`, `ArrayBuffer`, `Queue`,
  `PriorityQueue` (a binary max-heap whose `toString` and iteration expose the
  raw heap array, ported from the library's own `fixUp`/`fixDown`/`heapify`),
  `Stack`, `ArrayDeque`, `StringBuilder`, `mutable.Set`/`Map` and the
  insertion-ordered `LinkedHashSet`/`LinkedHashMap`, with `+=`/`-=`/`++=`/`--=`,
  `append`/`prepend`/`insert`/`remove`/`clear`,
  `put`/`update`/`getOrElseUpdate`, `enqueue`/`dequeue`, `push`/`pop`/`top`,
  `removeHead`/`removeLast`, and the same combinator set. A mutable `Set`/`Map`
  prints in its **hash table's** order, which is a different algorithm from the
  immutable trie and is ported from the 2.13 sources down to the table sizing
  that decides it; the linked forms print in insertion order instead. `+=` is
  `Growable.addOne` everywhere, so it appends even on a `Stack` (whose `push`
  prepends, because a `Stack`'s head is its top).
- **An explicit `Ordering`** — `Ordering.Int` and its siblings, `.reverse`,
  `Ordering.by(f)`, `Ordering.fromLessThan(lt)`, `Ordering[T]` and `ord.on(f)`,
  driving `sorted`, `sortBy`, `max`, `min`, `maxBy` and `minBy`, plus the
  value's own `compare`/`lt`/`gt`/`lteq`/`gteq`/`equiv`/`max`/`min`.
- **`Ordered`/`Comparable` on a user class** — a class that defines its own
  `compare` (or `compareTo`) drives the IMPLICIT ordering too, so `sorted`,
  `min`, `max`, `sortBy`, `maxBy` and `minBy` all run it, including through
  tuples and nested sequences. `Ordered` also derives `<`, `>`, `<=`, `>=` and
  `compareTo` from it — `compareTo` answering the user's value verbatim, the
  operators its sign. It does NOT derive `min`/`max` on the instance, matching
  Scala 3, where those need
  `import scala.math.Ordering.Implicits.infixOrderingOps`.
- **The boxed-primitive and `String` statics** — `scala.Int`'s
  `MaxValue`/`MinValue` family and `java.lang.Integer`/`Long`/`Double`/
  `Boolean`/`Character`'s `parseInt`, `toHexString`, `bitCount`, `isDigit`,
  `String.valueOf` and the rest. The two namespaces stay apart exactly as
  Scala's do, and a fixed-width rendering follows its box.
- **`getClass`** — a `java.lang.Class` answering `getName`/`getSimpleName` for a
  `String`, a primitive, a user type or a throwable (`e.getClass.getSimpleName`).
- **`scala.math`** — `abs`, `signum`, `min`/`max`, `round`/`floor`/`ceil`/`rint`,
  `sqrt`/`cbrt`/`exp`/`log`/`log10`/`pow`/`hypot`, the trig family, `atan2`,
  `toRadians`/`toDegrees`, `Pi`, `E`, under the `math`, `scala.math`, `Math` and
  `java.lang.Math` spellings. Integral overloads stay integral, and the two
  namespaces are kept apart where the JDK lacks an overload (`Math.signum(5)` is
  `1.0`, `math.signum(5)` is `1`).
- **Method dispatch** — postfix `.` on core values: `String` (`length`,
  `toUpperCase`/`toLowerCase`, `trim`, `strip`, `stripMargin`, `reverse`,
  `substring`, `charAt`, `contains`/`startsWith`/`endsWith`,
  `toInt`/`toLong`/`toDouble`, …), `Int`/`Double` (`abs`, `min`/`max`, `round`,
  `signum`, `compareTo`, `toDouble`/`toInt`, …), and `toString` on any value;
  chains left-to-right (`s.trim.length`). `Math.round` is the JDK algorithm, so
  `(-2.5).round` is `-2`; `min`/`max` propagate a NaN operand; and the implicit
  `Ordering[Double]` is `TotalOrdering`, so `sorted` puts NaN last and `-0.0`
  before `0.0`.
- **Predef `require`/`assert`/`assume`** — with their by-name messages and their
  exact prefixes (`requirement failed`, `assertion failed`, `assumption
  failed`).
- **Regular expressions** — `java.util.regex` through its three Scala doorways:
  `String.matches`/`replaceAll`/`replaceFirst` (with `$N` group splices in the
  replacement) and the regex-based `String.split`; `"…".r` building a
  `scala.util.matching.Regex` with `findFirstIn`/`findAllIn`/`findFirstMatchIn`/
  `findAllMatchIn`/`replaceAllIn`/`replaceFirstIn`/`matches`/`split`/`regex`;
  and `Regex.Match` with `group`/`subgroups`/`matched`. The match scan follows
  `java.util.regex.Matcher.find`'s rule rather than the Rust iterator's, which is
  what makes `"xx9".split("x*")` answer `["", "", "9"]` and `"abc".split("")`
  answer `["a", "b", "c"]`.
- **Control flow** — `if` / `else if` / `else` (statement *and* expression
  position), `while`, and `for` comprehensions over both integer ranges
  (`for (i <- a until b) …`, `for (i <- a to b) yield …` collecting a `Vector`)
  and collections (`for (x <- List(1,2,3)) yield x*2`, desugared to
  `.map`/`.flatMap`/`.withFilter`), with multiple generators, `if` guards, both
  the `for (…)` and `for { … }` enumerator groups, destructuring generators
  (`for ((k, v) <- m)`), and `y = e` value definitions
  (`for { x <- xs; y = f(x); if y > 0 } yield y` — lowered inline inside a
  counted range loop, and by Scala's own generator-pairing translation over a
  collection, so a later guard sees the defined name).
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
| **Native arithmetic** | Operators lower to native fusevm ops; the JIT traces hot integer loops. A strict numeric hook supplies Scala's `+` string concatenation for non-numeric operands, `Long` wrapping on integer overflow, and `Double` promotion for a mixed pair past 2^53; a division builtin restores `Int` truncation. `Float` is the one arithmetic that leaves the native ops: single precision has to round once, at 32 bits, so it costs a builtin. |
| **Rotated loops** | `while` and the counted `for` are lowered body-first, with the test at the bottom, so the loop closes on a CONDITIONAL backward branch — the one shape fusevm's tracer compiles. Emitted the other way (test at the top, unconditional `Jump` back), every loop this frontend produced was recorded and then declined. `scala --tiers` reports which tier each loop actually reached. |
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
method surface** plus the `Option(x)` factory, `Either`'s `Left`/`Right` and
its right-biased method surface, and `scala.util.Try` (`Try(e)` expanding to the
`try Success(e) catch Failure(t)` it is defined as);
`Product` on every case class and tuple; a wider `String`/`StringOps` surface
including the closure-taking combinators; **non-local `return`** — a
`return` inside a lambda (or inside the closures a `for` desugars to) leaves the
enclosing method, running every `finally` on the way out, lowered exactly as
Scala lowers it; and **`Char` as its own type** — a value that dispatches as a
number in arithmetic (`'a' + 1 == 98`) and as text when printed
(`println('a')` is `a`), carrying its type through lambdas and collections so
`"abc".toList.map(_.toInt)` is the code points while `"abc".map(_.toUpper)` is
still a `String`.

Writing an element is amortized O(1) — appending to a growable collection
(`buf += x`, `sb.append(x)`, `q.enqueue(x)`) and the indexed `a(i) = v`. Both
write the receiver in place instead of copying its elements, which is what the
general method path does and what had made either one quadratic in a loop:
40,000 `StringBuilder.append`s took 45s (and the 200,000 a real program does
never finished), and filling a 16,000-element `Array` by index took 6.8s. They
are 0.16s, 0.79s and 0.04s.

A `mutable.Map`/`Set` insert is amortized O(1) for the same reason plus one
more. Its entries are stored in the hash table's own iteration order, which is
what makes printing one byte-identical to `scala`; an add used to copy the
collection, re-sort it into that order, and scan it for the key. It now
binary-searches the order it is already sorted by and splices in place, and
`apply`/`get`/`contains` answer from the same search without copying. The
re-sort still runs when the table GROWS and re-buckets everything — the
doubling amortizes it. Filling a `mutable.Map` by key across n = 4k / 8k / 16k
went from 4.13s / 5.30s / 24.76s to 0.03s / 0.06s / 0.11s, and stays at 2x per
doubling out to n = 64k; a `mutable.Set` went from 1.24s / 5.42s / 19.57s to
0.02s / 0.04s / 0.09s. Building an IMMUTABLE map one `+` at a time is still
quadratic — see `BUGS.md`.

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
`nlr`, `ascribe`, `forval`, `regex`, `capture`, `char`, `patregex`, `breaks`,
`params`, `fmt`, `apply`, `narrow`, `braces`, …). It needs a real `scala` on
`PATH` (or `SCALARS_FUZZ_SCALA`), so CI never runs it; `tests/parity.rs` replays
a frozen, scala-verified corpus instead. The fuzzer found the float-notation and
`Boolean/null + String` gaps, the `catch`-guard binding bug, and — in this
release — the block-expression-in-value-position gap, the
`Math.signum` vs. `math.signum` overload split, the builder an empty
`Map.collect` picks, and `-3.abs` reading as `-(3.abs)` instead of `(-3).abs`.
The `nlr` mode found two control-flow bugs in this release: a `return` inside a
`for`/`foreach` body silently ended only that iteration instead of the method,
and a `return` out of a `try` skipped the `finally` entirely. The new `regex`
mode found `String.split` iterating matches by the Rust rule instead of
`java.util.regex.Matcher.find`'s, which dropped a field from
`"xx9".split("x*")`. The new `char` mode found `"".sortWith(_ > _)` answering a
`Vector` instead of `""` — a comparator's result type says nothing about the
elements — and `"abc".toList` still handing out one-character `String`s, which
silently made `"abc".toList.map(_.toInt)` a parse instead of the code points.
The `params` mode exists because by-name parameters parsed but compiled
call-by-value, which is the quietest possible failure: `f(x)` with `x + x` still
answered a plausible number, just one evaluation short. The `fmt` mode found
`%f` rounding half-to-even where `java.util.Formatter` rounds HALF_UP off the
shortest round-tripping decimal (`f"${0.125}%.2f"` was `0.12` and
`f"${1.005}%.2f"` was `1.00`), `-0.0` losing its sign, and `%x`/`%o` on a
negative `Int` rendering 64 bits instead of 32. The `breaks` mode is a reminder
that a clean score can be an artifact of the generator: it scored zero on its
first run only because the emitted program omitted `import
scala.util.control.Breaks._`, so the reference rejected `breakable` too and the
two agreed on the failure. Every mode above then applied its receivers the same
way — a name in the scope being compiled, applied from that same scope — so none
of them ever wrote `xs(i)` from inside a lambda, indexed a `String` held in a
binding, applied a field, or wrote `_(i)`/`f(_)`. The `apply` mode does, and each
of those was broken: a top-level binding applied from a nested body was rejected
as an undefined function, `s(i)` on any non-literal string was "cannot be applied
to arguments", a selector after an apply on a literal was a parse error, and a
bare `_` argument became the identity function instead of eta-expanding the
enclosing call — which for `xs.map(m(_))` looked the *function* up as a map key.
The same run added a **no-signal** count to the report: a program the oracle
rejects prints nothing and exits non-zero, a frontend that rejects it too then
"agrees", and the run scores clean having compared nothing. A run where no
program carried signal — including `--count 0` and `--probes 0`, which both used
to report a clean score — now exits 2 instead of green.

The `narrow` mode covers the one axis the arithmetic modes cannot see: a value's
WIDTH. Every probe in it reads an integer at a width the runtime does not carry —
the low eight bits for `toByte`, the low sixteen for `toShort`, 32 versus 64 for
the radix renderings and every `java.lang` bit-twiddling static, an `Int`'s range
for `parseInt` versus a `Byte`'s for `parseByte` — so a frontend that treats them
all as "the number" scores clean on `overflow` and wrong on all of these. It
found the whole family missing (`toByte`/`toShort` on every receiver,
`String.toByte`/`toShort`/`toBoolean`, the `RichInt` radix renderings,
`Integer.decode` and the bit statics), `String.toInt` TRIMMING where
`Integer.parseInt` does not and skipping the 32-bit range check, and the one
divergence that was not a wrong answer at all: an out-of-range radix reached
Rust's `from_str_radix`, which panics rather than returning an error, so
`Integer.parseInt("12", 40)` aborted the process with a Rust backtrace instead of
throwing a catchable `NumberFormatException`. Its `Char` operands then found
`'\uXXXX'` escapes unlexed in every literal form.

The `braces` mode covers the shape a corpus is likeliest to miss precisely
because it is everywhere: the BRACE form of an argument. Every brace group the
other modes had ever emitted was a `{ case … }` pattern-matching literal
(`collect`, `collectFirst`, `PartialFunction`), and every `_` placeholder they
emitted sat inside PARENTHESES — two different boundaries in Scala's grammar,
since a brace group is a block whose value is the argument and the placeholder
then expands at a block-statement boundary rather than at an argument one. With
no probe crossing the second, a clean score over the first said nothing, and
`xs.map { _ * 2 }` — the single most common line in written Scala — did not
parse at all (`` `_` placeholder outside an argument ``). Neither did
`once { 7 }` or `use(3) { _ + 1 }`, where the brace stands in for a whole
argument clause on a plain `def` (the curried case had a second failure behind
it: `use(3)(g)` applied `g` to whatever the FIRST clause returned rather than
completing the call, so it reported `value 3 cannot be applied to arguments`).
Two more scoping bugs sat under the same mode: Scala's typed placeholder
`(_: Int) + 1` read the parentheses as the function boundary, making the group
the identity function and then adding `1` to a function; and the trailing `if`
of a block was compiled as a statement run for effect, so every block whose
value was a conditional answered `()` — silently, and in exactly the shape a
brace-argument lambda body most often takes
(`xs.map { x => if (p(x)) a else b }` answered `List((), ())`).

Two things the harness could not report at all, and now can. **The entry point
was a constant, not an axis:** every probe ever generated was placed in one
`object T extends App { … }`, so anything that differs by entry shape was
invisible however many probes ran. `--entry main|mainsig|app` runs the same
generators under the other two, and its first pass found a by-name argument
whose thunk wrote an enclosing `var` losing the write — correct at the top level
where the `var` is a global, wrong inside a `def` where it needed a boxed cell,
which is why no `app` run had ever seen it. **The oracle's locale was
unchecked:** `resolve_oracle` gated the reference's JVM version but not its
default locale, and the frozen corpus pins `%f`/`%e` conversions whose decimal
separator and `toUpperCase` results come from it. A re-capture under `de_DE`
would freeze `0,13` for `0.13` with nothing about it looking wrong, so the
locale is now a hard gate beside the version.

Next waves, in priority order:

1. **Lazy views** — `.view` and `LazyList`. (`Iterator` itself is done: it is a
   real consumable iterator, not a strict `Iterable`.)
2. **The broader standard library** — `scala.io`, `scala.util.Random`, `BigInt`
   and `BigDecimal`, and `scala.collection.*` as a namespace. (`scala.util.Try` is
   done, and `Either`'s right-biased surface with `Either.left`'s
   `LeftProjection` beside it.)
3. **Named regex groups** — `(?<name>…)` and `${name}` in a replacement. Both
   are refused rather than approximated: reading one by name used to answer the
   whole match and `${name}` in a replacement used to be copied through
   verbatim.
4. **Overloading a block-level `def`** — a class member's overload resolves by
   argument count, but the flat `def` namespace has no such split, so two
   same-name `def`s in one block are refused.
5. **`@main` beyond the plain parameter list** — a repeated parameter
   (`rest: String*`), and choosing between two `@main` methods the way
   `--main-class` does.

See [`BUGS.md`](BUGS.md) for the honest known-gaps list.

---

## [0xFF] LICENSE

MIT — free and open source. See [`LICENSE`](LICENSE).
