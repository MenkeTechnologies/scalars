# Known gaps

An honest list of what scalars does **not** do yet. Unsupported constructs are
reported as parse/compile errors, never silently mis-run.

## Supported

- **User-defined methods (`def`), lexically scoped.** Helper `def`s alongside
  `main` (or inside an `extends App` body) are compiled to fusevm's native
  `Op::Call` frame ABI: parameters bind to per-call frame slots, recursion and
  mutual recursion work, the body's last expression (including a tail
  `if`/`else`) is the result, and `return` performs an early exit. A
  zero-parameter `def` is callable paren-less (`def x = …; x`). Parameter types
  and return types are parsed but not checked.
- **Block-local `def`s.** A `def` declared inside a block belongs to that block:
  two blocks may each declare `def f`, an inner one shadows an outer one, a
  later `val f` shadows both, and mutually recursive local `def`s (plus forward
  references inside one block) resolve. Implemented in `src/resolve.rs`, which
  uniquely renames each block-local `def` and **lambda-lifts** whatever
  enclosing-frame locals its body reads into extra trailing parameters that
  every call site passes; capture sets propagate through calls to a fixpoint.
- **Postfix method dispatch on core values.** `s.length`/`.size`,
  `.toUpperCase`/`.toLowerCase`, `.trim`, `.reverse`, `.isEmpty`/`.nonEmpty`,
  `.substring`, `.charAt`, `.contains`/`.startsWith`/`.endsWith`,
  `.toInt`/`.toDouble` on `String`; `.abs`/`.min`/`.max`/`.toDouble` on `Int`;
  `.abs`/`.round`/`.isNaN`/`.toInt` on `Double`; and `.toString` on any value.
  Method chaining works (`s.trim.length`). Out-of-range/parse failures throw
  faithfully (`StringIndexOutOfBoundsException`, `NumberFormatException`).
- **Classes, objects, `case class` (host-side object model).** `class C(x: Int)
  { def m = … }` with `new C(…)`, constructor params + body `val`/`var` fields,
  `this`, in-place `var`-field mutation, and instance-method dispatch (runtime
  class-tag dispatch off a `Value::Obj` handle into the frontend heap in
  `src/host.rs`). `object` singletons dispatch `def`s statically and expose
  `val`s as `Name.val`. `case class` derives an ordered-field `toString`
  (`Point(1,2)`), structural `equals`/`hashCode`, `copy(field = …)` (named and
  positional), a companion `apply` (construct without `new`), and `unapply` —
  wired into constructor patterns `case Point(x, y) =>`, nested and guarded.
  All four derived members see the **primary-constructor prefix only**, so a
  `val` declared in the class body is a field but not part of `toString`,
  `equals`, `hashCode` or the extractor's arity, matching Scala.
  Built-in `Option` (`Some(v)`, the `None` case object) rides the same model.
  Plain (non-`case`) classes use reference-identity `equals`/`hashCode` and a
  `Class@hex` `toString`, matching Scala.
- **Traits and inheritance.** `trait T { … }` with abstract members (`def f:
  Int`, `val x: String`) and concrete ones; `class C(x) extends P(x) with T1
  with T2`; `override def`; `super.m(…)`; and virtual dispatch — a method call
  resolves off the receiver's runtime class tag into whichever supertype owns
  the implementation, so a `List[Shape]` of mixed subclasses dispatches
  correctly. Constructor arguments thread up the whole superclass chain
  (`class E(m) extends B(m)`, `class B(m) extends A(m)`), supertype bodies run
  base-most first, and inherited fields join the record. `case class`/`case
  object` ADTs under a `sealed trait` match by constructor pattern and by
  stable identifier. `x.isInstanceOf[T]` and `case x: T =>` consult the
  registered hierarchy, so a subtype instance matches its supertypes.
  Resolution order for `extends P with T1 with T2` is `C, T2, T1, P` (Scala's
  linearization for every non-diamond hierarchy).
- **`Array`.** `Array(a, b, c)`, `new Array[T](n)` (filled with `T`'s zero
  value), `a(i)` reads, `a(i) = v` writes (Scala's `update` sugar), and the
  sequence operations (`length`, `map`, `filter`, `sum`, `mkString`, `toList`,
  `reverse`, `contains`, `foreach`, …).
- **Collections: `List`/`Seq`/`Vector`/`Set`/`Map`.** Literals for all five
  (`Seq` is `List` and `IndexedSeq` is `Vector`, as in Scala 3), `Nil`, `::`,
  and the combinator set: `map`/`flatMap`/`filter`/`filterNot`/`foreach`;
  `foldLeft`/`foldRight`/`fold`/`reduce`/`reduceLeft`/`reduceRight`;
  `sum`/`product`/`min`/`max`/`minBy`/`maxBy`/`count`/`length`/`size`;
  `exists`/`forall`/`find`/`indexOf`/`lastIndexOf`/`indexWhere`/`contains`;
  `sorted`/`sortBy`/`sortWith`/`reverse`/`distinct`;
  `take`/`drop`/`takeRight`/`dropRight`/`takeWhile`/`dropWhile`/`slice`/
  `splitAt`/`span`/`partition`/`init`/`tail`/`head`/`last`/`headOption`/
  `lastOption`; `zip`/`zipWithIndex`/`unzip`/`flatten`/`grouped`/`sliding`;
  `groupBy`; `mkString` in all three arities; `toList`/`toSeq`/`toVector`/
  `toArray`/`toSet`/`toMap`; the set algebra `union`/`intersect`/`diff`/
  `subsetOf`/`incl`/`excl`; and the operators `++`, `:+`, `+:`, `+`, `-`.
  `Map` adds `apply`/`get`/`getOrElse`/`contains`/`keys`/`keySet`/`values`/
  `updated`/`removed`/`+`/`-`/`++`, and its closure-taking methods pass the
  `Tuple2` Scala does, so `m.map { case (k, v) => … }` works.
- **Mutable collections.** `mutable.ListBuffer`, `mutable.ArrayBuffer`
  (also `mutable.Buffer`), `mutable.Set`/`mutable.HashSet` and
  `mutable.Map`/`mutable.HashMap`, spelled `mutable.X`,
  `collection.mutable.X`, `scala.collection.mutable.X`, `new …[T]()`, or —
  for `ListBuffer`/`ArrayBuffer`/`Buffer`, whose names cannot also mean an
  immutable collection — unqualified. They ride the same host heap and answer
  the same read combinators as the immutable ones, plus the mutators
  `+=`/`-=`/`++=`/`--=`, `add`/`addOne`/`addAll`, `append`/`prepend`/
  `appendAll`/`prependAll`, `insert`/`insertAll`, `remove`, `subtractOne`/
  `subtractAll`, `clear`, and — on a `Map` — `put`, `update` (`m(k) = v`),
  `getOrElseUpdate` and `remove`. Each prints with its own prefix
  (`ListBuffer(…)`, `ArrayBuffer(…)`, and `HashSet(…)`/`HashMap(…)` at every
  size), and a derived collection keeps the receiver's kind, so
  `ListBuffer(1,2).map(_ * 2)` is a `ListBuffer`.
- **A mutable `Set`/`Map` prints in its hash *table's* order.** Nothing like the
  immutable CHAMP trie: `scala.collection.mutable.HashSet`/`HashMap` are a flat,
  separately chained table, and `src/host.rs` ports it from the 2.13 sources —
  `improveHash(h) = h ^ (h >>> 16)`, the bucket `hash & (len - 1)`, buckets kept
  sorted by improved hash (equal hashes keeping insertion order), the
  `tableSizeFor`/`threshold` sizing that `HashSet.from` starts from and `add`
  doubles at three-quarters full, the `sizeHint` a `++=` applies when its source
  knows its size, and iteration over table indices ascending. The table length
  is therefore part of the collection's state (removal never shrinks it, and a
  derived collection starts a fresh table at the default capacity), which is why
  `mutable.Set(1,2,3)` and `mutable.Set(1,2,3,4,5)` order the same elements
  differently.
- **`Set`/`Map` print in their representation's order.** Up to four entries
  Scala's factories answer `Set1`..`Set4` / `Map1`..`Map4`, which keep insertion
  order and print `Set(…)`/`Map(…)`; beyond that they answer a CHAMP
  `HashSet`/`HashMap`, which prints `HashSet(…)`/`HashMap(…)` in *trie* order.
  Both are reproduced exactly: `src/host.rs` ports the JVM/Scala `##` hash codes
  (`String`, `Int`/`Long`, `Statics.doubleHash`, `Boolean`,
  `MurmurHash3.productHash` for tuples and `case` records, and its
  `orderedHash`/`unorderedHash` for collections), the trie's `improve`
  scramble, and the depth-first order its iterator walks. A derived collection
  keeps the receiver's representation, so `Set(1,2,3,4,5).filter(_ > 1)` is a
  four-element `HashSet` and `groupBy` is always a `HashMap`. A `case class`'s
  `hashCode` is therefore Scala's exact `MurmurHash3` value, not just a
  contract-satisfying one.
- **Partial functions and `collect`.** A `{ case … }` literal is Scala's
  `PartialFunction` literal, so besides `apply` it answers `isDefinedAt`: the
  compiler emits a **second** subroutine per literal — the same patterns and
  guards with every arm body replaced by `true` and a trailing `case _ => false`
  — and the closure handle carries both (`src/compiler.rs`, `defined_at_body`;
  `src/host.rs`, `MAKE_PARTIAL`). That is Scala's `applyOrElse` split: an arm
  body never runs for an element the function is not defined at. It powers
  `collect`/`collectFirst` on every sequence, `Set`, `Range` and `Map`, and the
  value-level surface `isDefinedAt`, `applyOrElse`, `lift`, `orElse`, `andThen`
  and `compose` (the last four answer a composed function value, so
  `(a orElse b).isDefinedAt` consults both operands and never runs an arm body).
  A literal used as a *total* function still raises `MatchError` where no arm
  matches, matching Scala's `Function1` reading of the same literal. A
  `PartialFunction[A, B]` annotation is a type ascription, so binding one to a
  `val` and passing it around works.
- **The bitwise and shift operators.** `&`, `|`, `^`, `~`, `<<`, `>>` and `>>>`
  on `Int`; `&`, `|` and `^` on `Boolean` (Scala's non-short-circuiting forms);
  and `&`/`|`/`&~` on `Set` (intersection, union, difference). Hexadecimal
  literals (`0x1F`) lex, read at `Int` width. Precedence follows the SLS table
  keyed by an operator's first character — `| < ^ < & < = ! < < > < : < + - <
  * / %` — which is also where `&&`/`||` get theirs, so `a | b && c` is
  `a | (b && c)` and `1 << 2 + 1` is `1 << 3`.
- **`for` comprehensions.** `yield` and the side-effecting form; range and
  collection generators; several generators (nesting through `flatMap`);
  `if` guards, trailing or standing alone; both the `for (…)` and the `for { … }`
  enumerator groups; and a destructuring generator (`for ((k, v) <- m)`), which
  desugars to a pattern-matching anonymous function.
- **General infix method syntax.** `a m b` is `a.m(b)` for any single-argument
  method — `xs contains 2`, `s startsWith "a"`, `1 to n`. An alphanumeric
  operator binds looser than every symbolic one (`1 to n - 1` is `1 to (n - 1)`)
  and chains left-associatively. The symbolic collection operators `++`, `:+`
  and `+:` take their precedence from the first character and, for a name ending
  in `:`, dispatch on the right operand (`0 +: xs` is `xs.+:(0)`). A line break
  between the receiver and the name rules the infix reading out, so a bare
  expression statement is never absorbed into the next line.
- **Pattern-matching anonymous functions.** `{ case p => e }` in argument
  position is a function that matches on its argument, including tuple patterns
  (`case (k, v) =>`, nested and wildcarded). A caller that passes more arguments
  than the literal declares — `foldLeft(z) { case (acc, (k, v)) => … }` — has
  them tupled, as Scala's `FunctionN` conversion does. A brace group also stands
  in for a parenthesized argument generally (`xs.map { x => x + 1 }`).
- **Singleton `object`s inherit concrete members.** `object X extends T` gets
  T's concrete `def`s and `val`s. A singleton has no `this` record, so the
  members are spliced into the object as if declared there (`src/compiler.rs`,
  `inherit_into_objects`) and compile in the object's own scope; a member X
  declares itself wins, supertypes are consulted in linearization order, and
  inherited `val`s initialize base-most first. The inherited body is reachable
  both by name (`X.m`) and by virtual dispatch through a supertype-typed
  binding.
- **Generics, type-erased.** Type parameters on `class`/`case class`/`trait`/
  `def`, type arguments at use sites (`new Box[Int](3)`, `id[String]("hi")`,
  `xs.map[Int](f)`), and parameterized annotations (`val m: Map[String,
  List[Int]]`) all parse and run. Nothing is checked or specialized — the
  runtime is dynamically typed, exactly as the other fusevm frontends are.
- **`Range` as a first-class value.** `1 to 5`, `1 until 5`, `1 to 10 by 3` are
  values, with Scala's `Range.toString` including the `empty `/`inexact `
  prefixes. `map`/`filter` over one yield a `Vector`, `reverse` yields a
  `Range`, and `sum`/`length`/`head`/`last`/`toList`/`mkString`/`contains`/
  `min`/`max` all work. Used as a `for` generator it still compiles to the
  counted loop (no materialization).
- **`scala.math`.** `abs`, `signum`, `min`, `max`, `round`, `floor`, `ceil`,
  `rint`, `sqrt`, `cbrt`, `exp`, `log`, `log10`, `pow`, `hypot`, the trig and
  inverse-trig functions, `atan2`, `toRadians`/`toDegrees`, `Pi`, `E` — under
  the `math`, `scala.math`, `Math` and `java.lang.Math` spellings. The `Int`
  overloads stay integral (`math.abs(-4)` is `4`, not `4.0`), and the two
  namespaces are kept apart where the JDK lacks an overload
  (`Math.signum(5)` is `1.0`, `math.signum(5)` is `1`).

## Not implemented (parse errors / unresolved today)

- **`LinkedHashSet`/`LinkedHashMap`, and the rest of
  `scala.collection.mutable`.** The linked forms keep insertion order rather
  than the table's, so they are deliberately *not* aliased onto the hash-table
  ones — they are an unresolved name, not a silent mis-run. `Queue`, `Stack`,
  `PriorityQueue`, `ArrayDeque` and `StringBuilder` are likewise absent.
- **An unqualified `Set`/`Map` always means the immutable one.** `import` lines
  are skipped, not tracked, so `import scala.collection.mutable.Set` followed by
  a bare `Set(1, 2)` builds the immutable set. Write `mutable.Set(1, 2)`.
- **`x += e` in expression position.** `println(buf += 1)` — Scala's `+=` is an
  expression whose value is the buffer. Here `+=` is a statement, so it is a
  parse error rather than a mis-run. `buf += 1` on its own line works.
- **Lazy views and `Iterator`.** `.view`, `.iterator`, and `LazyList`. The
  strict methods above materialize; `grouped`/`sliding` answer the `List` of
  windows rather than an `Iterator`, so `.toList`/`.foreach` on one matches but
  printing it bare does not.
- **Sorting by a user `Ordering`.** `sorted(ord)`, `sortBy` with an explicit
  `Ordering`, and `Ordered`/`Comparable` on a user class. The built-in ordering
  covers numbers, `String`, `Boolean` and tuples of those; anything else
  compares equal, which a stable sort leaves in input order.
- **Symbolic operators beyond the wired set.** `/:`, `:\` and user-defined
  symbolic method names.
- **The wider standard library.** `scala.io`, `scala.collection.*` as a
  namespace, and the many `String`/numeric methods beyond the wired subset
  above. The `args` parameter of `main` is parsed and ignored.
- **`case NonFatal(e)` and other extractor patterns in `catch`.** Only
  `case e: Type`, `case _: Type`, `case e` and `case _` arms are modeled.
- **User exception classes inside the hierarchy.** `class MyErr(m: String)
  extends Exception` can be thrown and caught *by its own name*, but the JDK
  throwables are not part of the registered class hierarchy, so `case e:
  Exception` will not catch it.
- **By-name params, `given`/`using`, `@main` (Scala 3 annotation entry).**
- **`do/while` is not a gap.** Scala 3 removed it from the language and the
  reference compiler rejects it, so scalars does not implement it either.

## Modeled with a documented simplification

- **Types are not checked.** Declared types (`Int`, `String`, …) and type
  parameters (`class Box[A]`, `def id[A]`) are retained for diagnostics but do
  not gate execution — the runtime is dynamically typed on the fusevm value
  model. Type errors that `scalac` would reject may run. Related:
  `x.asInstanceOf[T]` is the identity, not a checked cast, so it never throws
  `ClassCastException` and never performs a numeric conversion. A class's
  `val`/private access modifiers are parsed but not enforced (every field is
  reachable).
- **`Array.toString` renders `Array(1, 2, 3)`.** Scala prints the JVM identity
  form (`[I@1b6d3586`), which no reimplementation can reproduce byte-for-byte,
  so the readable form is emitted instead. This is the one place a supported
  construct deliberately diverges; the parity fuzzer therefore never prints a
  bare `Array`.
- **A local `def` may not *assign* to a binding it captures.** Captures are
  threaded by value, so a write inside the lifted body could not reach the
  enclosing frame. It is rejected at compile time ("a captured binding is
  read-only here") rather than silently lost. Reads see the value at call time,
  which matches Scala for the `val`s and parameters that make up the common case.
- **A subclass constructor parameter that renames a superclass field.** Our
  parser makes every constructor parameter a field, so in `class D(n: String)
  extends A(f(n))` the superclass argument is stored under `A`'s parameter name
  and `D`'s body then sees that value under it. When the subclass forwards the
  parameter unchanged — `class D(n: String) extends A(n)`, the idiom — this is
  exactly Scala; when it transforms it, the subclass body reads the transformed
  value.
- **Linearization is "self, then parents right-to-left".** This is Scala's order
  for every hierarchy without a diamond; the full C3 rule differs only when one
  supertype is reachable by two paths.
- **`==` on non-numbers compares by value** (structural), which matches Scala's
  `==`/`equals` for the strings and booleans this frontend handles. Reference
  identity (`eq`) is not modeled.
- **`char` literals are one-character strings**, not an integer `Char` type.
- **`~-1` parses here and does not in Scala.** Scala lexes a run of symbolic
  characters as one operator name, so `~-1` is the undefined prefix operator
  `~-`; this lexer takes `~` and `-1`. Write `~(-1)`. This is a case of
  accepting more than Scala, like the unchecked types above, never of running
  something differently.
- **An *empty* `Map.map`/`Map.collect` picks its builder from the function's
  syntax.** Scala reads the result builder off the function's static result
  type: a pair rebuilds a `Map`, anything else falls back to
  `immutable.Iterable`'s builder, which is `List`. With at least one result the
  runtime reads that off the results themselves, exactly; with none — an empty
  map, or a `collect` no entry matched — it consults a compile-time flag that is
  true when every value the function body can answer is a *pair literal*
  (`compiler::yields_pairs`). A function that returns a pair some other way (a
  call, a variable) and matches nothing therefore answers `List()` where Scala
  answers `Map()`.
- **`m.keys`/`m.keySet` answer the map's own order.** Scala's key view is a
  `HashMap.HashKeySet`, which prints `Set(…)` however large the map and iterates
  in the map's order; that is what is built, so a five-entry map's `keys` prints
  `Set(…)` rather than `HashSet(…)`. Mapping or filtering the view renormalizes
  it into a real `Set`/`HashSet`.
- **`grouped`/`sliding` answer a `List` of windows**, not an `Iterator`. Every
  consumption Scala programs use (`.toList`, `.foreach`, `.map`) matches;
  printing the un-consumed result does not (Scala prints `<iterator>`).
- **Integer arithmetic uses fusevm's 64-bit semantics.** Scala `Int` 32-bit
  overflow wrapping is not modeled for `+`/`-`/`*` (values behave like `Long`).
  The **bitwise** operators are the exception: `~`, `<<`, `>>` and `>>>` are
  evaluated at `Int` width, because that is observable at ordinary magnitudes
  (`1 << 33` is `2`, not `8589934592`) rather than only on overflow. A genuinely
  `Long` receiver therefore shifts as an `Int` would.
- **`/` is dispatched at runtime, not by static type.** Scala picks integer vs.
  floating division from the *static* operand types; there are no static types
  here, so the [`host::b_div`] builtin decides at runtime — both operands `Int`
  truncate, otherwise a double divide. This matches Scala for every monomorphic
  case (`7 / 2 == 3`, `7 / 2.0 == 3.5`), and floating `/0.0` yields `Infinity` as
  Scala does.
- **Integer division by zero throws** `java.lang.ArithmeticException: / by zero`,
  matching Scala/the JVM `idiv` trap. It is catchable (see the exception section
  below); an *uncaught* one aborts the program with
  `scalars: java.lang.ArithmeticException: / by zero` on stderr and exit status
  1, where `scala` prints a JVM stack trace — the message is faithful, the trace
  is not modeled. Floating `/0.0` is `Infinity` (not an exception), also matching
  Scala.
- **A `Range` value is materialized, not lazy.** Scala's `Range` computes
  elements on demand; here they are built when the range is created, capped at
  8M elements (beyond which an `OutOfMemoryError` is raised rather than the
  process hanging). Every observable result matches for the sizes real programs
  use; the memory profile does not.
- **Uninitialized bindings are unbound** rather than rejected; reading one before
  assignment yields `null` instead of a compile error. The same value stands in
  for `Unit`, so *printing* a `Unit` result — `println(xs.clear())`,
  `println(println("x"))` — renders `null` where Scala renders `()`. Every other
  use of a `Unit` value agrees.
- **`x += e` chooses the growable method at run time, not from a static type.**
  Scala calls `x.+=(e)` when the receiver's type has that method and falls back
  to `x = x + e` otherwise. There are no static types here, so a program that
  builds a mutable collection *anywhere* emits a one-op test
  ([`crate::host::IS_GROWABLE`]) that picks the branch on the receiver's runtime
  kind. A program with no mutable collection in it emits exactly the arithmetic
  it did before, so a counted `while` loop stays JIT-trace-eligible.
- **`object … extends App` runs the object body directly.** Statements run in
  order and any `def` members are hoisted and callable; other member kinds
  (fields, nested types) are skipped.
- **Singleton `object` `val`s initialize eagerly**, before `main`, rather than
  lazily on first access. Observably identical for pure initializers.

## Object model: how it works

`case class` needs an **ordered, type-tagged record** so `toString` renders
`Point(1,2)` in declared field order and structural `equals`/`hashCode` compare
field-by-field. fusevm's `Value::Obj(u32)` is an opaque handle into a
*frontend-owned* object heap; scalars owns that heap (`src/host.rs`): a
`Value::Obj` indexes a per-run arena of records (`class name`, `is_case`,
`is_object`, ordered `(field, value)` list). Construction, field access,
`toString`/`equals`/`hashCode`, `copy`, and constructor-pattern binding are host
extension builtins (`OBJ_NEW`/`OBJ_CLASS`/`OBJ_COPY`/`OBJ_SET`, plus the `Obj`
arm of `SMETHOD` and the `==` numeric hook) — no fusevm changes. Every
construction site bakes the class name + ordered field names into the bytecode,
so no runtime class registry is needed for construction.

Inheritance adds the one thing a flat record cannot carry: **which supertypes an
instance conforms to**, and **how many of its fields came from the primary
constructor**. Both are published once before `main` by a `TYPE_REG` call per
declared type, and read back by `isInstanceOf`/typed patterns and by the derived
`case class` members. Method calls stay a compile-time-emitted class-tag chain:
for each concrete class that responds to the name, compare the receiver's tag and
call the `Owner$method` subroutine of whichever supertype implements it. When a
name has exactly one implementation and the receiver is a known `this`, the chain
collapses to a direct call, so a hierarchy-free program emits the bytecode it did
before traits existed.

A singleton `object` is the one type with no record to dispatch off: its `val`s
are program globals and its `def`s are statically-called subroutines. It
therefore inherits by *copy* — `inherit_into_objects` in `src/compiler.rs`
splices each supertype's concrete `def`s and `val`s into the object before
anything is indexed, so they compile in the object's own name scope and every
later stage (the method table, the `Name.val` globals, the class-tag chain) sees
one flat singleton. Members the object declares itself win, supertypes are
consulted in linearization order, and inherited `val`s are prepended base-most
first so a supertype's initializer runs before a body that reads it.

## Collections: how the `HashSet`/`HashMap` order is reproduced

Scala's immutable `Set`/`Map` factories switch representation at five entries:
`Set1`..`Set4` / `Map1`..`Map4` keep insertion order, and a `HashSet`/`HashMap`
is a CHAMP hash trie whose iteration order is a function of the elements' hash
codes alone. Printing one therefore cannot be faked, and three pieces are ported
into `src/host.rs` to get it byte-for-byte:

1. **`##`** — `String.hashCode` over UTF-16 code units, `Long.hashCode`,
   `Statics.doubleHash` (which agrees with the `Int`/`Long`/`Float` hash wherever
   the value is exactly representable as one), `Boolean`'s 1231/1237, and
   `MurmurHash3.productHash` — seed, `productPrefix` hash, each element's `##`,
   finalized with the arity — for tuples and `case` records. That last one also
   became the `case class` `hashCode`, which is now Scala's exact value.
   A collection hashes through the rest of the `MurmurHash3` family:
   `orderedHash` with the `Seq` seed for every sequence — which is why
   `List(1,2,3)`, `Vector(1,2,3)`, `1 to 3` and `ListBuffer(1,2,3)` all hash
   alike, including the arithmetic-progression case that makes a `Range` agree
   with its elements — and `unorderedHash` with the `Set`/`Map` seed for a
   `Set` and for a `Map`'s `Tuple2` entries. So a `HashSet` of `List`s, or a
   `HashMap` keyed by one, prints in trie order too, at any nesting depth.
2. **`improve`** — the trie's hash scramble.
3. **The traversal** — five bits per level, low bits first; within a node the
   slots holding one element are emitted first in ascending slot order, then the
   sub-tries in ascending slot order. CHAMP compacts on removal, so this shape
   depends only on the set of hashes and can be computed from the elements
   directly.

A collection's representation is stored alongside it (`HashRep`) because a
derived collection keeps the receiver's: `Set(1,2,3,4,5).filter(_ > 1)` is a
four-element `HashSet`, while `Set(1,2,3).map(_ * 2)` is a `Set3`. `groupBy`
builds through a `HashMap` builder and so is always hashed.

## Block-local `def`s: how they work

The compiler consumes a flat function namespace, so `src/resolve.rs` sits between
the parser and the compiler:

1. The AST is walked with a scope stack. Each block pre-binds its own `def`s
   (giving forward references and mutual recursion inside one block), and every
   `Var`/`Call` is rewritten to the unique global name of whichever `def` it
   actually resolves to. `val`s and parameters shadow an outer `def` from their
   declaration onward.
2. Each hoisted `def` gets the enclosing-frame locals its body reads appended as
   extra parameters, and every call site passes them; capture sets propagate
   through calls to a fixpoint, so a local `def` that calls a sibling capturing
   `k` threads `k` too.

Final names are chosen only after the whole program is walked, and the first
claimant of a name keeps it verbatim — a program with no collisions compiles to
exactly the bytecode it did before this pass existed.

## Exception unwinding: how it works, and what it costs

fusevm has no unwind opcode, and scalars lowers `def`s to fusevm's **native**
`Op::Call` frames — so a raise cannot longjmp out of a frame the way a
sub-chunk-interpreting frontend can. `try` is instead a cooperative protocol
split across the host and the compiler:

* **Runtime half (`src/host.rs`).** A raise parks the exception in a
  thread-local `PENDING` slot instead of halting, provided a `try` is
  dynamically active (`TRY_DEPTH > 0`). Every builtin with an observable side
  effect — `println`/`print`, `/`, method dispatch, closure application, the
  `MatchError` fall-through, the numeric hook — short-circuits while an
  exception is in flight, so nothing is printed and nothing faults a second time
  between the raise and its handler.
* **Compile-time half (`src/compiler.rs`).** When the program contains a `try`
  anywhere, an `EXC_PENDING` test is emitted after every statement. The
  innermost enclosing construct decides where a `true` answer jumps: out of a
  loop, out of a `def` frame (`ReturnValue Unit`), into a `catch` dispatch, or —
  at top level — into the terminal abort. Binding stores (`val`/`var` init,
  assignment, and object-`val` update) get their check *before* the store and
  drop the computed value, so a raise part-way through an initializer never
  commits garbage to a binding a handler could read.

Two consequences worth stating plainly:

- **Unwinding is statement-granular.** A raise part-way through one statement
  finishes evaluating that statement's remaining operands before control reaches
  the handler. Those evaluations run on garbage values with the side-effecting
  builtins suppressed, so they are unobservable in the cases the test suite and
  the `exc` fuzzer mode cover — but it is a real approximation, not exact JVM
  semantics.
- **A program with no `try` pays nothing.** The checks are not emitted at all,
  and a fault halts the run exactly as it did before exceptions existed.

`case NonFatal(e)`, exception chaining (`getCause`, `initCause`,
`addSuppressed`), stack traces, and `Try`/`Success`/`Failure` are not modeled.
