# Known gaps

An honest list of what scalars does **not** do yet. Slice 1 is a single-object,
single entry-point subset; unsupported constructs are reported as parse errors,
never silently mis-run.

## Not implemented (parse errors today)

- **User-defined methods.** Only the entry point is compiled. Calling a helper
  `def`, or any method, is rejected. (Next wave: fusevm's native `Op::Call`
  frame ABI.)
- **Classes, traits, `case class`, objects-as-values, `new`.** No instance
  model, no fields, no constructors, no `this`. `new` is lexed but has no
  semantics.
- **Collections and arrays.** `Array`, `List`, `Seq`, `Map`, indexing, `.length`,
  `.size`. The `args` parameter of `main` is parsed and ignored.
- **The standard library.** No `math`, `String` methods, `.toInt`, `scala.io`,
  `scala.collection.*`. Only `println`/`print` exist.
- **Method/field access on values.** `x.foo`, `s.length`, `xs.map(...)`.
- **String interpolation.** `s"$name"`, `f"$x%.2f"`, `raw"..."`.
- **`for … yield` comprehensions and the `by` step** — only side-effecting
  `for (i <- a until|to b)` ranges with step 1 run today.
- **`match`/`case`, pattern matching, `do/while`, `try`/`catch`/`finally`,
  `throw`, `return`.**
- **Functions as values, lambdas (`=>`), by-name params, `given`/`using`,
  generics, `@main` (Scala 3 annotation entry).**

## Modeled with a documented simplification

- **Types are not checked.** Declared types (`Int`, `String`, …) are retained for
  diagnostics but do not gate execution — the runtime is dynamically typed on the
  fusevm value model. Type errors that `scalac` would reject may run.
- **`val` is not enforced immutable.** A `val` reassignment is accepted rather
  than rejected as it would be by `scalac`. Immutability is retained on the AST
  for a later check.
- **`==` on non-numbers compares by value** (structural), which matches Scala's
  `==`/`equals` for the strings and booleans slice 1 handles. Reference identity
  (`eq`) is not modeled.
- **`char` literals are one-character strings**, not an integer `Char` type.
- **Integer arithmetic uses fusevm's 64-bit semantics.** Scala `Int` 32-bit
  overflow wrapping is not modeled (values behave like `Long`).
- **`/` is dispatched at runtime, not by static type.** Scala picks integer vs.
  floating division from the *static* operand types; slice 1 carries no static
  types, so the [`host::b_div`] builtin decides at runtime — both operands `Int`
  truncate, otherwise a double divide. This matches Scala for every monomorphic
  case (`7 / 2 == 3`, `7 / 2.0 == 3.5`), and floating `/0.0` yields `Infinity` as
  Scala does.
- **Integer division by zero yields `null`** instead of throwing
  `ArithmeticException` (slice 1 has no exception machinery). Floating `/0.0` is
  `Infinity`, matching Scala.
- **Uninitialized bindings are unbound** rather than rejected; reading one before
  assignment yields `null` instead of a compile error.
- **`object … extends App` runs the object body directly.** Members other than
  top-level statements inside an `App` object are not supported in slice 1.
