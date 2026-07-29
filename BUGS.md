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

- **Generics.** Type parameters are parsed and ignored; nothing is checked or
  specialized. A class's `val`/private access modifiers are parsed but not
  enforced (every field is reachable — a documented simplification).
- **A singleton `object` does not inherit method *bodies*.** `object X extends
  T` registers `T` as a supertype (so `case x: T` and `case X =>` work) and
  dispatches `X`'s own `def`s, but calling a concrete method it inherited from
  `T` fails with "value m is not a member of X": a trait method body needs a
  `this` record, and an `object`'s members are program globals rather than a
  record.
- **Mutable / other collections.** `Seq`/`Vector`/`Set` literals, `mutable.*`,
  and the many collection methods beyond the wired subset. The immutable `List`
  and `Map` are modeled host-side (`List(…)`, `Nil`, `::`,
  `.head`/`.tail`/`.length`/`.isEmpty`/`.reverse`/`.contains`/indexing,
  `.map`/`.filter`/`.flatMap`/`.foreach`/`.foldLeft`/`.foldRight`/`.reduce`/
  `.sum`/`.mkString`/`.exists`/`.forall`/`.count`/`.min`/`.max`/`.toList`/
  `.toArray`/`.toVector`; `Map(k -> v)`, `.apply`/`.get`/`.contains`/`.keys`/
  `.values`/`.size`/`getOrElse`/`+`); a range-led comprehension yields a
  `Vector`. The `args` parameter of `main` is parsed and ignored.
- **The wider standard library.** `scala.io`, `scala.collection.*`, and the many
  `String`/numeric methods beyond the wired subset above.
- **General infix method syntax.** Scala treats `a m b` as `a.m(b)` for any
  method. Only the range operators (`to`, `until`, `by`) and `->` are wired;
  every other infix call must be written `a.m(b)`.
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

- **Types are not checked.** Declared types (`Int`, `String`, …) are retained for
  diagnostics but do not gate execution — the runtime is dynamically typed on the
  fusevm value model. Type errors that `scalac` would reject may run. Related:
  `x.asInstanceOf[T]` is the identity, not a checked cast, so it never throws
  `ClassCastException` and never performs a numeric conversion.
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
- **Integer arithmetic uses fusevm's 64-bit semantics.** Scala `Int` 32-bit
  overflow wrapping is not modeled (values behave like `Long`).
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
  assignment yields `null` instead of a compile error.
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
