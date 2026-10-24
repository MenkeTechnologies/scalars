# Known gaps

An honest list of what scalars does **not** do yet. Unsupported constructs are
reported as parse/compile errors, never silently mis-run.

## Supported

- **All three Scala entry points, and top-level definitions.** `object T extends
  App { … }`, `object T { def main(args: Array[String]) = … }`, and Scala 3's
  `@main def go(…)`. Top-level `def`s and `val`s — the members Scala 3 collects
  into a synthetic `Foo$package` object — are hoisted into the same flat
  function namespace an entry object's helpers use, and a top-level `val`'s
  initializer runs BEFORE the entry body and before the command line is read.

  An `extends App` body's `val`s are FIELDS of the entry object, so a forward
  reference to one reads the JVM field default rather than an absent value:
  `println(later); val later: Int = 7` prints `0`, and `0.0`/`false`/`null` for
  a `Double`/`Boolean`/reference type. The declared type answers that (the width
  analysis could not — `NumTy` separates `Int` from `Long` and nothing else); an
  unannotated binding falls back to the shape of its initializer. A `val` inside
  a `def` is a local, and Scala rejects a forward reference to one outright, so
  no default applies there.

  A `@main` method's parameters come from the command line through the reader
  Scala generates, `CommandLineParser.parseArgument[T]`, for the types that have
  one and are representable here: `Int`, `Long`, `Byte`, `Short`, `Double`,
  `Boolean`, `String`. That reader's failure behaviour is unlike anything else in
  the language and is reproduced exactly — the diagnostic goes to **stdout** and
  the process exits **0**:

  ```
  Illegal command line: more arguments expected
  Illegal command line after first argument: java.lang.NumberFormatException: For input string: "zz"
  Illegal command line after 2 arguments: java.lang.NumberFormatException: Value out of range. Value:"300" Radix:10
  Illegal command line after 4 arguments: java.lang.IllegalArgumentException: For input string: "yes"
  ```

  Extra arguments are ignored, as Scala ignores them. `def main`'s `args` is the
  real argument vector; `extends App`'s `args` is deliberately left null, because
  that is what the reference does — the `DelayedInit` body runs during the
  object's `<clinit>`, ahead of the field, so `args.length` there raises
  `NullPointerException` on both sides.
- **`scala.util.Random` / `java.util.Random`.** `new Random()`, `new Random(seed)`
  (also `new java.util.Random(seed)`) and the `Random` companion's process-wide
  generator. The 48-bit LCG is the JDK's, so a seeded generator reproduces the
  JVM's sequence: `nextInt()`, `nextInt(bound)`, `nextLong()`, `nextLong(n)`,
  `nextDouble()`, `nextBoolean()`, `nextGaussian()`, `setSeed`, `between` and
  `shuffle` (a list, vector or range shuffles to the same collection type,
  a range to a `Vector`). `nextLong(n)` and `between` follow the Scala library,
  not the JDK's newer overloads, and the errors carry the library's messages
  (`bound must be positive`, `requirement failed: Invalid bounds`). `between`
  picks `Double` when either bound is a `Double`, else `Int` when both fit 32
  bits, else `Long`; a `Long` literal that fits an `Int` therefore draws as the
  `Int` form. `nextFloat`, `nextBytes`, `nextString` and `alphanumeric` are not
  modelled.
- **A program-declared implicit `Ordering[C]` is `C`'s default ordering.**
  `implicit val o: Ordering[C] = Ordering.by(…)`, `given Ordering[C] = …` and
  `given Ordering[C] with { def compare … }` make `sorted`, `max`, `min`,
  `maxBy`/`minBy` over `C` itself and a `PriorityQueue[C]` order by it, as
  Scala's implicit search would. `C` may be a library type too: an
  `implicit val ord: Ordering[String]` orders every `String` comparison an
  implicit ordering decides, including the `String` components of a sorted
  tuple (Scala's `Ordering.Tuple2` takes it). The ordering is found by the RUNTIME class of
  the elements being compared, not by static type, so two different `Ordering[C]`
  values in different scopes collapse to the last one registered. An ordering
  whose body compares two instances of `C` again (`Ordering.by(identity)`) falls
  back to the structural order for that inner comparison rather than recursing.
  A `TreeSet`/`TreeMap` of class instances is still not modelled.
- **Varargs spread into a library collection's bulk form.** `buf.append(xs: _*)`,
  `buf.prepend(xs: _*)`, `pq.enqueue(xs: _*)` and `"…".format(xs: _*)` (also
  `printf` and `String.format`) take the sequence as the argument list, unless
  the program declares a method of that name. `append(a, b, …)` / `prepend(a, b,
  …)` with several elements work on the buffers.
- **`empty` and `from` on the mutable collection factories** reached through a
  selector import: `ArrayBuffer.empty[Int]`, `Queue.from(xs)`,
  `PriorityQueue.from(xs)`, and `mutable.Queue.from(xs)` through the package.
  The one-dimensional element factories work on them too — `fill(n)(v)` (by
  name, once per element), `tabulate`, `range`, `iterate` and `unfold` on
  `ArrayBuffer`, `ListBuffer`, `Queue`, `Stack`, `ArrayDeque` and `mutable.Set`
  — as do `empty`/`from` on `mutable.Set`/`mutable.Map` through the package.
- **Scala 3 optional braces.** A pass over the token stream turns each
  indentation region into the `{ … }` block the parser already reads. A region
  opens after a token that ends its line — `=`, `=>`, `match`, `try`, `catch`,
  `finally`, `else`, `for`, `then`, `do`, `yield`, the `:` of an
  `object`/`class`/`trait`/`enum` header, the `with` ending a `given` header,
  and the `)` closing an
  `extension (x: T)` receiver — when the next line is indented deeper and does
  not start with `{`; it closes before the first shallower line, when the
  bracket around it closes, and at end of file. No region opens inside `( )` or
  `[ ]`. `end` markers (`end match`, `end run`) are dropped, the outdent having
  already closed the region. On top of that the parser takes the quiet control
  syntax — `if c then … else …`, `while c do …`, `for x <- xs do …` /
  `yield …` (also with a tuple pattern, `for (k, v) <- m do`), a multi-line
  bracket-less enumerator block, and the inline `catch case e: E => …` — and the
  fewer-braces argument `f: x =>` / `f: (a, b) =>` with the lambda body on the
  deeper lines, or `f:` ending its line with the whole argument (a block, a
  lambda, `_ + 1`, `case …` arms) on the deeper lines. Before this, the body of a brace-less `def` ran its FIRST
  statement and silently dropped the rest.
- **Widening to a declared `Double`.** An `Int`/`Long` value becomes a `Double`
  wherever the declared type is `Double` (or a `List[Double]`/`Vector[Double]`,
  element-wise): a parameter (`def sq(n: Double)` called `sq(3)` is `9.0`), a
  `val`/`var` and an assignment to one, a `def`'s result on every return path
  (`def f(): Double = 3` is `3.0`), a lambda parameter declared `Double` or
  `Float`, and an ascription, including one on a lambda body
  (`xs.map(x => x: Double)`).
- **A `def main` object's members.** Its `val`/`var`s are the object's fields and
  initialize in declaration order before `main` runs (a forward reference reads
  the field default, a `lazy val` waits for its first read), and a `class`,
  `trait` or `object` declared inside it joins the type namespace. Both were
  dropped: every read of such a `val` answered `null`, and such a type was `not
  found`.
- **A `String` holding `null` concatenates.** `s + "y"` with
  `s: String = null` is `nully` (`String.+`); only the literal `null + …`, whose
  type `Null` has no `+`, is refused.
- **`apply` — `receiver(args)` — wherever Scala writes it.** The receiver may be
  a `List`/`Array`/`Vector` (indexing), a `Map` (lookup), a `String` (`s(i)`, i.e.
  `charAt`), or a function value; it may be a literal (`"pear"(1)`), a top-level
  or block `val`, a `def` parameter, a lambda parameter, a captured binding read
  from inside a lambda or a `def` body, or a FIELD of a record (`r.name(0)` is
  `r.name.apply(0)`, not a method named `name`). A selector chains onto the
  result (`"pear"(1).toUpper`), and applications compose (`getFn()(arg)`). The
  placeholder participates on both sides: `_(i)` applies the placeholder
  (`xs.map(_(1))`), and a bare `_` as an argument eta-expands the ENCLOSING call
  (`xs.map(f(_))` is `xs.map(x => f(x))`, per Scala's "smallest expression
  properly containing the underscore" rule) instead of passing the identity
  function. Covered by the parity fuzzer's `apply` mode.
- **The BRACE form of an argument, and `_` inside it.** `xs.map { _ * 2 }`,
  `xs.foldLeft(0) { _ + _ }`, `xs.sortBy { -_ }`, `xs.map { x => … }` and
  `{ case … }` are all the same substitution: a `{ … }` group standing in for a
  parenthesized argument clause. It works on a method (`xs.map { … }`), on a
  plain `def` call (`once { 7 }`), and on the trailing clause of a CURRIED `def`
  (`use(3) { _ + 1 }`, where the braces are a second argument clause and not an
  `apply` on what the first clause returned — the compiler rejoins the clauses
  when the callee's declared arity is exactly what they supply, so
  `def mk(n: Int): Int => Int` called `mk(1)(2)` still dispatches through
  `apply`).

  A brace group is a BLOCK whose value is the argument, so it may hold several
  statements and is evaluated ONCE, to produce the function — `xs.map { c += 1;
  _ + 1 }` increments `c` once, not once per element. The block's value is its
  trailing expression, including a trailing `if` (`xs.map { x => if (p(x)) a else
  b }`).

  A placeholder expands at the smallest expression that PROPERLY contains it, and
  a block statement is one of those boundaries — which is what makes the brace
  and parenthesized spellings mean the same function. So is a `val`'s initializer
  (`val f: Int => Int = _ + 1`). Two consequences follow, both matching Scala
  3.8.4: `(_: Int) + 1` is the TYPED placeholder — the parentheses carry the
  ascription, not the function boundary, so the whole thing is `x => x + 1` — and
  a statement that is nothing but `_` (`xs.map { _ }`) is refused, since there is
  no containing expression to expand at ("Unbound placeholder parameter" in
  Scala). Covered by the parity fuzzer's `braces` mode.

  A call whose argument is a bare `_` (`f(_)`) is expanded at the call only when
  nothing larger contains it; as an OPERAND it belongs to the enclosing
  expression, so `xs.sortWith(_.compareTo(_) < 0)` is `(a, b) => a.compareTo(b)
  < 0` and `xs.map(f(_) + 1)` is `x => f(x) + 1`.
  The right operand of a compound assignment is an operand as well:
  `xs.foreach(t += _)` is `x => t += x`, for a `var`, an indexed place
  (`a(i) += _`) and a buffer (`buf += _`).
- **User-defined methods (`def`), lexically scoped.** Helper `def`s alongside
  `main` (or inside an `extends App` body) are compiled to fusevm's native
  `Op::Call` frame ABI: parameters bind to per-call frame slots, recursion and
  mutual recursion work, the body's last expression (including a tail
  `if`/`else`) is the result, and `return` performs an early exit. A
  zero-parameter `def` is callable paren-less (`def x = …; x`). Given an argument
  list it APPLIES its result — `def xs = List(10, 20); xs(1)` is `xs.apply(1)`,
  `20` — at the top level, in an `object` (`O.xs(1)`) and on an instance. Parameter types
  and return types are parsed but not checked.
- **Block-local `def`s.** A `def` declared inside a block belongs to that block:
  two blocks may each declare `def f`, an inner one shadows an outer one, a
  later `val f` shadows both, and mutually recursive local `def`s (plus forward
  references inside one block) resolve. Implemented in `src/resolve.rs`, which
  uniquely renames each block-local `def` and **lambda-lifts** whatever
  enclosing-frame locals its body reads into extra trailing parameters that
  every call site passes; capture sets propagate through calls to a fixpoint.
- **Type ascription in expression position, and the unit literal.** Both the
  parenthesised `(e: T)` and the bare `e: T` parse wherever an expression does
  (`(None: Option[Int])`, `f(xs: Seq[Int])`, `val n = 3: Long`), which is what
  Scala's `Expr1 ::= PostfixExpr Ascription` allows. In an argument the spread
  `xs: _*` is recognized first, so it stays a spread rather than becoming an
  ascription of the type `_*`.
  The runtime is dynamically typed, so the annotation is dropped — except for a
  numeric widening, which is observable WITHOUT a type checker and so is
  performed: `(3: Double)` answers `3.0`, not `3`. `()` is the `Unit` value; it
  lowers to an empty tuple, which is exactly what Scala prints it as.
- **A `y = e` value definition in a `for` comprehension.**
  `for { x <- xs; y = f(x); if y > 0 } yield y`. Over a *range* generator it
  lowers inline (a store into a loop-body slot, so the counted loop is kept);
  over a *collection* generator it takes Scala's own translation, which pairs the
  value onto the generator (`for ((x, y) <- xs.map(x => (x, f(x))))`), so a later
  guard and the body both see the defined name. A run of definitions nests one
  more pair each.
- **Regular expressions (`java.util.regex` / `scala.util.matching`).**
  `String.matches`/`replaceAll`/`replaceFirst`, and the regex-based
  `String.split`; `"…".r` and `new Regex(…)` building a `Regex` that answers
  `findFirstIn`, `findAllIn`, `findFirstMatchIn`, `findAllMatchIn`,
  `replaceAllIn`, `replaceFirstIn`, `matches`, `split` and `regex`; and
  `Regex.Match` with `group`/`subgroups`/`matched`/`source` and the offsets
  `start`/`end`/`before`/`after` (of the whole match or of group `i`, -1 or
  `null` for a group that did not take part) — `group` by number or by the
  NAME of a `(?<name>…)` group (`null` when it did not take part, Java's
  `IllegalArgumentException: No group with name <x>` when the pattern declares
  none). A `Regex` also works in
  PATTERN position via `unapplySeq` — `case r(a, b) => …`, which matches the
  WHOLE input (so `"a1"` does not match `"([0-9]+)".r`) and binds `null` for a
  group that did not participate. A replacement's `$N` splices are Java's,
  including its "longest group number that exists" rule and its
  `IndexOutOfBoundsException` for a group the pattern does not have, and so are
  its `${name}` splices (`IllegalArgumentException: No group with name {x}` for
  an undeclared name). The match
  scan follows `java.util.regex.Matcher.find` rather than the Rust iterator: Java
  resumes AT the previous match's end (so an empty match is allowed there) and
  only steps forward a character after an empty match — which is what makes
  `"xx9".split("x*")` answer `["", "", "9"]` and `"abc".split("")` answer
  `["a", "b", "c"]`. Backed by `fancy-regex`, whose backtracking engine accepts
  the lookaround and backreferences `java.util.regex` has and the `regex` crate
  rejects.
- **A closure that ASSIGNS an enclosing `var`.** `var t = 0;
  xs.foreach(x => t += x)` inside a `def` reaches the declaring frame. Captures
  are threaded by value, so such a binding is BOXED into a shared heap cell
  (`host::CELL_NEW`/`CELL_GET`/`CELL_SET`) exactly as Scala boxes it into a
  `scala.runtime.IntRef`; the closure captures the cell handle, so the write is
  shared, and it may outlive the frame (`def mk() = { var i = 0; () => { i += 1;
  i } }`). Only the vars a closure actually writes are boxed
  (`compiler::boxed_vars`), so every other local keeps its plain frame slot and a
  program with no mutating closure emits the bytecode it did before.
- **Every operator is a method.** `+ - * / % < > <= >= == !=` dispatch the same
  whether written infix or dotted (`n.+(1)`, `7./(2)`, `"a".*(3)`), including
  `/`'s Int-vs-Double split and its zero-divisor throw. `s * n` is
  `StringOps.*` in both spellings.
- **Compound assignment to a target that is not a plain name.** `a(i) += 1`,
  `m(k) *= 2`, `counts(w) += 1`, `g(0)(1) += 9`, `obj.field += 5` and
  `obj.items += 9`, in all of `+= -= *= /= %=` plus `++=`/`--=`. Scala resolves
  `l op= r` by preferring an `op=` **member** on `l` and falling back to
  `l = l op r`, which for an *application* target expands to
  `l.update(args, l.apply(args) op r)` (SLS 6.12.4). Both halves are taken: an
  `Int` element takes the arithmetic expansion through `update`, while an
  element that is itself growable (`m(k) += 7` over `ListBuffer` values, or a
  selection like `nb.head += 3`) takes the member call and mutates in place, so
  no write-back is emitted and the receiver need not be a record at all. The
  receiver and every index are evaluated exactly ONCE, into temporaries, because
  Scala evaluates them once and they may have side effects
  (`counts(next()) += 1` advances one step). The choice between the two halves is
  the same run-time `IS_GROWABLE` test a plain-name `+=` already took, so a
  program with no mutable collection still emits exactly the arithmetic it did
  before. Covered by the parity fuzzer's `placeassign` mode.
- **Plain `=` to a selection, and to a singleton's `var`.** `obj.field = v`,
  `bs(1).n = 5`, `mk(b).n = 3`, and `Cfg.n = 10` / `Cfg.n += 5` / `Cfg.s += "!"`
  for an `object Cfg { var n = … }`. `recv.field = v` used to be a parse error
  (`unexpected token Assign in expression`), which left a `var` field writable
  only from inside its own class; it takes the same target path the `op=` forms
  take, with `=` as the operator, so the receiver is still evaluated exactly
  once. A singleton's `var` is the one target that is NOT a record field —
  reading `Cfg.n` lowers to `GetVar("Cfg.n")` — so every operator form routes to
  that global rather than to the heap builtins, which is what makes an outside
  write visible to the object's own `def`s (`Cfg.n = 10; Cfg.bump()` answers
  `11`).
- **User classes extending a JDK throwable.** `class E(m: String) extends
  Exception(m)` — directly, or through a user ancestor — keeps the message and
  cause it passed up (`Exception()`, `(msg)`, `(msg, cause)`, and the
  `(cause)` overload whose message is `cause.toString`) in a hidden record
  field, answers `getMessage`/`getLocalizedMessage`/`getCause`, is caught by
  every JDK supertype (`case e: Exception` catches an `E extends
  IllegalStateException`), matches `isInstanceOf`/typed patterns the same way,
  and renders as `Throwable.toString` does — `E: msg`, or `E` for a null
  message — including for a `case class` and through an overridden
  `getMessage`.
- **Any pattern in a `catch` arm.** Beyond `case e: Type` / `case e` / `case _`,
  an arm may be an extractor — `case NonFatal(e)`, `case Code(n)` for a case
  class throwable — with or without a guard. The in-flight exception is taken,
  matched like a `match` arm, and put back for the next arm (and the enclosing
  `try`) when the pattern or guard rejects it. `NonFatal` accepts every
  throwable but `VirtualMachineError`, `ThreadDeath`, `InterruptedException`,
  `LinkageError` and `ControlThrowable`.
- **`try` with neither a `catch` nor a `finally`.** Reference `scala` 3.8.4
  compiles it, warns "A try without catch or finally is equivalent to putting
  its body in a block; no exceptions are handled", and runs the body — so it
  parses to a plain block here. It handles nothing: a raise inside one still
  propagates, and the unwind protocol below is not armed for it.
- **Compound assignment in expression position.** `println(buf += 1)`,
  `val r = (n -= 2)`, `xs.map(x => n += x)`, `(buf += 2) += 3`. Scala's
  `l op= r` is an EXPRESSION, and its value is decided by which half of the SLS
  6.12.4 choice ran: the `op=` **member** answers the receiver
  (`println(buf += 1)` prints the buffer), the `l = l op r` expansion is an
  assignment and answers `()` (`println(n += 1)` prints `()`, and `n` still
  moved). That choice is the same run-time `IS_GROWABLE` test the statement
  forms take, so each half stores its own answer into a slot the reader loads
  once both have rejoined — which also means the effect is lowered by exactly
  the statement path, target evaluated once and all
  (`println(a(next()) += 9)` advances one step). Every target shape the
  statement forms accept works, plus one they do not: a target that cannot be
  assigned to at all (`(buf += 2) += 3`, `f() += 1`) has only the member
  reading, since there is nothing to store back into. A `var` written this way
  from inside a closure is boxed exactly as the statement form boxes it.
  Covered by the parity fuzzer's `assignexpr` mode.
- **`Double.toString`, as a choice of DECIMAL rather than of digits.** Java
  picks the decimal `d` for a value `v` and only then lays it out: `d` is the
  shortest that rounds back to `v`, except that when one significant digit
  already suffices it is the closest decimal of length one **or** two, and a tie
  goes to the even significand. Both exceptions are implemented, and both are
  observable only where an ULP is of the same order as the value or its exact
  expansion is one digit longer than its shortest form:
  `Double.MinPositiveValue` is `4.9E-324` (not the one-digit `5E-324` with a
  zero stuck on), and `5 * 2^-23` — exactly `5.9604644775390625E-7` — is
  `5.960464477539062E-7`, the even one. The tie test reads the double's EXACT
  decimal expansion (which always terminates) and requires both candidates to
  round back, which is what makes `2^-24` answer the odd `5.960464477539063E-8`
  off the very same digits: at an exact power of two the gap below is half the
  gap above, so the lower candidate is out of range and there is no tie.
  Layout is then Java's — plain for `1e-3 <= |x| < 1e7`, computerized
  scientific otherwise, always with one fractional digit.
- **Postfix method dispatch on core values.** `s.length`/`.size`,
  `.toUpperCase`/`.toLowerCase`, `.trim`, `.strip`/`.stripLeading`/
  `.stripTrailing`, `.stripMargin`, `.reverse`, `.isEmpty`/`.nonEmpty`,
  `.substring`, `.charAt`, `.contains`/`.startsWith`/`.endsWith`,
  `.toInt`/`.toLong`/`.toDouble` on `String`; `.abs`/`.min`/`.max`/`.signum`/
  `.compareTo`/`.toDouble` on `Int`; `.abs`/`.round`/`.min`/`.max`/`.signum`/
  `.compareTo`/`.isNaN`/`.toInt` on `Double`; and `.toString` on any value.
  Method chaining works (`s.trim.length`). Out-of-range/parse failures throw
  faithfully (`StringIndexOutOfBoundsException`, `NumberFormatException`).
  `trim` and `strip` are not the same cut and neither is Rust's: `trim` removes
  every code point at or below U+0020, `strip` removes what
  `Character.isWhitespace` accepts — which excludes U+00A0.
- **Predef `require`, `assert` and `assume`.** Desugared in the parser to the
  `if`/`throw` they are defined as, so the message parameter keeps its by-name
  semantics and is not evaluated when the condition holds. `require` raises
  `IllegalArgumentException: requirement failed`, `assert` an
  `AssertionError: assertion failed`, `assume` an
  `AssertionError: assumption failed`; a supplied message is appended after
  `": "`. `AssertionError` extends `Error`, so `catch { case e: Exception }`
  does not catch one.
- **Classes, objects, `case class` (host-side object model).** `class C(x: Int)
  { def m = … }` with `new C(…)` or Scala 3's creator application `C(…)`, constructor params + body `val`/`var` fields,
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
- **Cooperative numeric equality inside collections.** An element, tuple
  field, `Option` content or `Set`/`Map` key compares as `BoxesRunTime.equals`
  does: across widths at the wider of the two (`List(1) == List(1.0)`,
  `List(1.0).contains(1)`, `Set[Any](1, 1L, 1.0).size == 1`), a `Float` in single
  precision against an integer (`List(16777217).contains(16777216f)`), and a
  `Char` by its code point (`List('b').contains(98)`). `x.equals(y)` on two
  numbers of different widths is NOT modelled: an `Int` and a `Long` share one
  runtime representation, so `1.equals(1L)` answers `true` where Scala's
  `Integer.equals` answers `false`.
- **`override def equals`, honoured by `==`.** A class that overrides
  `equals(o: Any)` answers `==`/`!=` through it, and so do a `List`/`Vector`/
  tuple/`Option` holding such instances (they compare element by element with
  `==`). The collection lookups run it with the receiver and order the 2.13
  collections use, which an override that prints can observe: `contains` is
  `element == target` front to back; `indexOf` is `target == element`;
  `lastIndexOf` walks a `List` whole from the front but any other kind back
  from the end; `distinct` hashes (running a `hashCode` override) and calls
  `equals` only between elements whose hashes agree. `diff`/`intersect` count the
  argument in a hash table the same way (`occCounts`), comparing a new key
  against its same-hash keys twice, as `updateWith` then `put0` do. An immutable
  `Set`'s elements and a `Map`'s keys are compared through the override too —
  building one (`Set(…)`, `Map(…)`, `toSet`, `toMap`, a `map` that collides two
  elements), `contains`/`apply`/`get`/`getOrElse`, `+`/`-`/`updated`/`++` and
  `Set`/`Map` equality — and a `HashSet`/`HashMap` is laid out in trie order by
  the `hashCode` override. `Set1`..`Set4`/`Map1`..`Map4` test `probe == stored`
  front to back; a trie compares only same-hash keys, alone as `probe == stored`
  for a lookup and `stored == probe` for an update, removal or builder, a
  collision group `stored == probe` (a `HashSet` builder's `probe == stored`,
  a removal's twice), and growing past four replays the switch to a trie. Not
  modelled: two keys of different hashes sharing a trie slot (the real
  `get`/`removed` compare those too), the order a trie takes after a removal
  empties a collision group down to one key, and the mutable `HashSet`/
  `HashMap`, which still compare natively. A program with no override
  keeps the native `Op::NumEq`.
- **By-name arguments of library methods.** `getOrElse(d)` on an
  `Option`/`Either`/`Try`, `orElse(alt)`, `Map.getOrElse(k, d)` and
  `mutable.Map.getOrElseUpdate(k, op)` evaluate the argument only when it is
  needed, so `memo.getOrElseUpdate(n, fib(n - 1) + fib(n - 2))` is a linear
  memo rather than an exponential recursion, and a `throw` or side effect in a
  default that is not taken never happens.
- **A capitalized `val` binder.** `val Pat = "(\\d+)".r` and `val Max: Int = 9`
  bind a value, as Scala reads a lone identifier in a `val` (only a following
  `(`, `@` or `::` makes it a pattern), so `case Pat(a) =>` and
  `val Pat(a) = s` use it as an extractor.
- **`Array.ofDim[T](d1, …)`**, of any rank, each row its own array filled with
  `T`'s zero; **`StringOps.linesIterator`** (terminators `\n`, `\r\n` and a lone
  `\r`); immutable **`Map.transform((k, v) => …)`**; and **`f.tupled`** /
  **`f.curried`** on a function value of two or more parameters.
- **`groupMap(key)(f)` and `groupMapReduce(key)(f)(op)`**, running `key` then
  `f` per element and printing in the order of the `mutable.HashMap` 2.13
  accumulates them in; **`List`/`Vector`/`Array`/`Seq.iterate(start, n)(f)`**;
  **`xs.lift(i)`**; `String.hashCode`; `math`/`Math` **`floorDiv`/`floorMod`**
  (raising `/ by zero`), `sinh`/`cosh`/`tanh`, `log1p`/`expm1`; a case class's
  derived **`canEqual`**; and Scala 3's **`f(xs*)`** spread.
- **`LazyList.iterate(seed)(f)` applies `f` only for elements asked for**:
  `take(3).toList` runs it twice. It used to compute each element's successor
  as soon as the element itself was produced, one application early.
- **Auxiliary constructors.** `def this(n: Int) = this(n, n)` and
  `def this() = { this(7); note = "x" }` — the self-invocation (primary or
  another auxiliary constructor) builds the instance, then the rest of the body
  runs on it as `this`. `new C(…)` picks the constructor by ARGUMENT COUNT, so
  two constructors of the same arity told apart only by parameter types are
  not distinguished: the primary one wins.
- **Applying a field or an object `val` by its bare name.** Inside a method,
  `data(i)` on a field holding an `Array`/`List`/`Map`/function is
  `this.data.apply(i)` (it used to be `not found: data`), and inside an
  object's method its own `val`s apply the same way. An instance of a class
  declaring `apply` is applicable too: `m(0, 1)` calls it.
- **`reduceOption`/`reduceLeftOption`/`reduceRightOption`,
  `minOption`/`maxOption`/`minByOption`/`maxByOption`, `sameElements`,
  `distinctBy`, `patch`, `unzip3`**; **`xs.to(Vector)`** (and the other
  companions) as the conversion rather than a range; **`a.to(b, step)` /
  `a.until(b, step)`**; and **`String.split(regex, limit)`** with Java's
  positive/zero/negative limit rules.
- **`Map.withDefaultValue(v)` / `withDefault(f)`.** `apply` and `default` answer
  the default for an absent key; `get`, `getOrElse` and `contains` ignore it,
  as in Scala. An immutable map's `+`/`updated`/`-`/`removed`/`++` keep the
  default and its other combinators drop it, and a map with a default prints
  as `Map(…)` whatever it wraps (Scala's `WithDefault` wrapper). A MUTABLE
  map's default is attached to the map itself rather than to a separate
  wrapper sharing its entries, so the ORIGINAL handle also answers through the
  default and also prints as `Map(…)`; Scala's original still throws on an
  absent key and prints as `HashMap(…)`.
- **Scala 3 parameter untupling.** A lambda of `n` parameters handed one
  `n`-tuple receives its components: `m.map((k, v) => …)`,
  `pairs.foreach((a, b) => …)`.
- **A `Seq`, `Set` or `Map` where a function is expected** is applied:
  `(0 to n).filter(isPrime)` with `isPrime: Array[Boolean]`, `keys.map(m)`.
- **Deep recursion through a closure or a by-name argument.** The program runs
  on a thread with a 1 GiB stack reservation; Scala-level recursion that passes
  through a lambda, a by-name argument or a user `toString`/`equals` re-enters
  the VM from a builtin, and the default thread stack overflowed at a depth of
  about 100 (`memo.getOrElseUpdate(n, fib(n - 1) + fib(n - 2))` at `n = 90`).
- **Inside a `def` body (a `@main` or `def main` entry), as at the top level.**
  These differed only when the bindings were frame slots rather than program
  globals, which is why the `--entry main` fuzz shape found them:
  - a pattern variable or `for` counter sharing its name with a `var` that a
    closure captures elsewhere in the same body (`case (i, j) => i * j` or
    `for (i <- 2 until 7)` beside a captured `var i`) read a non-existent cell
    and answered `null`; and a lambda's own parameter or a `case` arm's
    pattern variable of that name (`(i: Int) => -i`, `case i: Int => i * 2`)
    no longer counts as a capture of the `var`;
  - a `lazy val` read from inside a closure or another lazy initializer
    (`lazy val q = p + 1`) is forced rather than loaded as its unforced cell;
  - an extractor or stable identifier bound to a LOCAL `val` (`val p =
    "…".r; xs.collect { case p(a) => a }`, `val Lim = 3; case Lim =>`) is
    captured by the closure the pattern sits in;
  - the second clause of a curried by-name call (`rep(i < 3) { t += i }`)
    shares the cells of the `var`s it assigns.
- **`%a`/`%A`, the hexadecimal float conversion**, and `Double.toHexString`.
  `java.util.Formatter.hexDouble` is ported: a precision rounds the significand
  half-even to that many hex digits (`%.0a` is one), a subnormal is normalized
  first, a zero pad goes between `0x` and the digits, and `-0.0` is negative.
  `%(a` and `%,a` raise `FormatFlagsConversionMismatchException`.
- **`%c` of a `Char`, `%h`/`%H`, and `startsWith(prefix, offset)`.**
- **The standard `Ordering[T]` for an implicit parameter.** `def maxOf[T](xs:
  List[T])(implicit ord: Ordering[T])`, its `using` spelling and a `[T:
  Ordering]` context bound, called with no given `Ordering` in the program,
  receive the natural ordering, as Scala finds it in `Ordering`'s companion. A
  given the program declares still wins.
- **The standard `Numeric[T]` / `Integral[T]` / `Fractional[T]`.** An
  implicit parameter or context bound of one, `implicitly[Numeric[Int]]` and
  `Numeric[Double]` (the companion's `apply`) receive the library's instance
  for `Int`, `Long`, `Short`, `Byte`, `Char`, `Double`, `Float`, `BigInt` or
  `BigDecimal` — `Integral` only for the integral types, `Fractional` only for
  `Double`/`Float`/`BigDecimal`, as in the library. Its members answer at the
  instance's type, so an `Int` one's `plus`/`times`/`negate`/`fromInt` wrap at
  32 bits and a `Short` one's at 16: `zero`, `one`, `fromInt`, `plus`,
  `minus`, `times`, `negate`, `abs`, `sign`, `signum`, `toInt`/`toLong`
  (a `Double` saturating), `toDouble`, `toFloat`, `quot`/`rem` (`/ by zero`),
  `div`, `compare`, `lt`/`lteq`/`gt`/`gteq`/`equiv`, `max`/`min`. `n.plus`
  used as a function value eta-expands (`xs.foldLeft(n.zero)(n.plus)`), and
  `xs.sum(n)` / `xs.product(n)` fold through the instance given. A `Char`
  one wraps its arithmetic to 16 bits (`negate('a')` is `\uFF9F`) and enters
  it as the code unit. `parseString` is the library's exception-free parse:
  `StringParsers`' for the primitives (no trimming for an integral type,
  `None` past its range, so `Numeric[Byte].parseString("300")` is `None`),
  `Try(BigInt(s))`/`Try(BigDecimal(s))` for the big ones and
  `Try(s.toInt.toChar)` for `Char`. `mkNumericOps` is not modelled.
- **`implicit class`.** `implicit class Rich(n: Int) { def squared = n * n }`
  registers each method as the Scala 3 extension `extension (n: Int)` it is
  equivalent to, so `4.squared` resolves; the class stays declared for an
  explicit `new Rich(4)`. The class's own type parameters are read, so a
  receiver that IS one (`implicit class Twice[A](a: A)`) accepts any value, and
  one that mentions them (`(A, A)`, `List[A]`, `Seq[A]`) keeps its shape — the
  same as `extension [A](…)`. A receiver whose static type the compiler cannot
  name (`(1, 2).swapped`, a `val xs = List(…)`, the element in `ps.map(_.m)`)
  is selected at run time by a type test against each candidate's declared
  receiver type, which after erasure is all the test can see. A method that calls a SIBLING method of the class
  without a receiver is not rewritten to the extension form.
- **Anonymous classes.** `new T { def m = … }`, `new C(args) { … }` and
  `new C(args) with M1 with M2` declare a fresh class extending the named
  parents and construct it. Like a named class, its body sees top-level and
  entry-object bindings but does NOT capture a local of an enclosing `def`.
  Structural `new { … }` (no parent) is not modelled.
- **`Option.empty`, `Option.when(c)(v)`, `Option.unless(c)(v)`** (the value by
  name), **`opt.zip(other)`**; **`Try { … }`** with a block, qualified or not;
  **`val a, b = e`** (one evaluation of `e` per name);
  **`d.floor`/`d.ceil`/`d.toRadians`/`d.toDegrees`/`d.isWhole`**; and
  **`mutable.ArrayBuffer.empty[T]`** and the other mutable factories' `empty`.
- **`override def toString`, honoured wherever a value is rendered.** Scala
  renders every value through its `toString`, so an override answers for
  `println(p)`, `s"$p"` / `f"$p%s"`, `"x" + p`, `String.valueOf(p)`,
  `xs.mkString(…)`, `"%s".format(p)`, `sb.append(p)`, and at every depth of a
  `List`/`Set`/`Map`/tuple/`Option` — not only for an explicit `p.toString`. An
  override on a `case class` replaces the derived `Class(f0,…)` form, and one
  provided by a `trait` is found through the receiver's linearization. Running
  one re-enters the VM, so it may itself print (rendering happens before stdout
  is locked) and it may raise (the `println` it was rendering for does not run).
  Both the compiler's concat rerouting and the host's per-value lookup are gated
  on the program declaring an override at all: a program with none emits and
  runs exactly the bytecode it did before.

  The `+` half of that is a WHOLE-OPERATION reroute, not an operand rewrite.
  fusevm's `Op::Add` reaches a mixed pair through `NumericHook`, a plain
  `Fn(NumOp, &Value, &Value)` with no VM to re-enter, so a `+` either of whose
  operands could be a class instance is emitted as the `SADD` builtin instead
  (`BuiltinHandler` does get a `&mut VM`). `SADD` answers exactly what the hook
  answers for every other pair — `Set`/`Map` `+`, `Char` arithmetic, `Long`
  wrap, `Int`/`Double` promotion, and Scala 3's rejections, which have no
  universal `any2stringadd` — and differs only in rendering a concatenation
  through the VM-aware path. Deciding it at run time is what reaches the sites
  with no `String` in the source text: `pre + p` for a `val pre = "…"`,
  `s + p` for a `String` parameter, `xs(0) + p`, and `acc += p`, whose far
  operand is the assignment TARGET and so offers no syntax to read. The gate is
  the operand: a literal, and anything the width analysis has typed `Int` or
  `Long`, keep the native `Op::Add`, so `n += 1` in a program that happens to
  declare one override is untouched.
- **Overloaded methods, resolved by argument count.** A `class`/`object` may
  declare one name at several arities (`def g()`, `def g(x: Int)`, `def g(x:
  Int, y: Int)`); each registers its own subroutine and every call site — direct,
  `super.m`, virtual dispatch off a runtime tag, and an unqualified self-call —
  picks the one matching the arity it passes. Overloads that differ only in
  parameter TYPE (`f(Int)` / `f(String)` / `f(Any)`) are renamed apart and
  fronted by a dispatcher that tests the declared parameter types against the
  arguments' run-time types, the most specific first; see *Overloads that
  differ only in parameter type* below for what that cannot tell apart.
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
  Resolution order is Scala's class linearization (SLS 5.1.2), diamonds
  included: `class C extends B with T1 with T2`, both traits extending `B`, is
  `C, T2, T1, B`. `super.m` inside a TRAIT is the next implementation in the
  linearization of the class the trait is mixed into, so stackable traits
  (`abstract override def put(x: Int) = super.put(2 * x)`) compose in mixin
  order. A STATIC `super[T].m` calls `T`'s implementation (found down `T`'s
  own linearization) even inside a trait, and `T` must be a direct parent
  (`A does not name a parent of D` otherwise, as scalac reports).
- **`Array`.** `Array(a, b, c)`, `new Array[T](n)` (filled with `T`'s zero
  value), `a(i)` reads, `a(i) = v` writes (Scala's `update` sugar), and the
  sequence operations (`length`, `map`, `filter`, `sum`, `mkString`, `toList`,
  `reverse`, `contains`, `foreach`, …).
- **Collections: `List`/`Seq`/`Vector`/`Set`/`Map`.** Literals for all five
  (`Seq` and `Iterable` are `List` and `IndexedSeq` is `Vector`, as in Scala
  3), `Nil`, `::`,
  and the combinator set: `map`/`flatMap`/`filter`/`filterNot`/`foreach`;
  `foldLeft`/`foldRight`/`fold`/`reduce`/`reduceLeft`/`reduceRight`;
  `sum`/`product`/`min`/`max`/`minBy`/`maxBy`/`count`/`length`/`size`;
  `exists`/`forall`/`find`/`indexOf`/`lastIndexOf`/`indexWhere`/`contains`
  (`indexWhere(p, from)`, `lastIndexWhere(p, end)` and `segmentLength(p,
  from)` too);
  `sorted`/`sortBy`/`sortWith`/`reverse`/`distinct`;
  `take`/`drop`/`takeRight`/`dropRight`/`takeWhile`/`dropWhile`/`slice`/
  `splitAt`/`span`/`partition`/`init`/`tail`/`head`/`last`/`headOption`/
  `lastOption`; `zip`/`zipWithIndex`/`unzip`/`flatten`/`grouped`/`sliding`;
  `groupBy`; `mkString` in all three arities; `toList`/`toSeq`/`toVector`/
  `toArray`/`toSet`/`toMap`/`toIndexedSeq`/`toIterable`;
  `permutations`/`combinations`; `updated`; `startsWith`/`endsWith`; the set
  algebra `union`/`intersect`/`diff`/`subsetOf`/`incl`/`excl`; and the
  operators `++`, `:+`, `+:`, `+`, `-`.
  A collection ARGUMENT (`zip`, `zipAll`, `++`/`appendedAll`, `++:`, `diff`,
  `intersect`, `startsWith`, `corresponds`, `++=`/`--=`) is any `IterableOnce`
  Predef converts: a `String` is its `Char`s (`(1 to 3).zip("abc")`), an
  `Option` zero or one element, a `Map` its pairs, and a `LazyList` is forced
  only as far as the operation reads (`xs.zip(LazyList.from(1))`).
  `permutations` and `combinations` enumerate DISTINCT results in Scala's own
  order: the elements are renumbered by the position their value first appeared
  and the walk runs over that, so `List(3,2,1).permutations` leads with
  `List(3, 2, 1)` and a receiver with repeats yields each arrangement once.
  `toIterable` is `this` for every collection (a `Vector` stays a `Vector`, a
  `ListBuffer` a `ListBuffer`), while `toSeq` is `this` only for an immutable
  sequence — a `Set`, a `Map` and every mutable sequence copy out to a `List`,
  and an `Array` to an `ArraySeq`.
  `Map` adds `apply`/`get`/`getOrElse`/`contains`/`keys`/`keySet`/`values`/
  `updated`/`removed`/`+`/`-`/`++`, and its closure-taking methods pass the
  `Tuple2` Scala does, so `m.map { case (k, v) => … }` works.
- **Mutable collections.** `mutable.ListBuffer`, `mutable.ArrayBuffer`
  (also `mutable.Buffer`), `mutable.Queue`, `mutable.Stack`,
  `mutable.ArrayDeque`, `mutable.Set`/`mutable.HashSet`,
  `mutable.Map`/`mutable.HashMap`, `mutable.LinkedHashSet` and
  `mutable.LinkedHashMap`, spelled `mutable.X`,
  `collection.mutable.X`, `scala.collection.mutable.X`, `new …[T]()`, or —
  for the names that cannot also mean an immutable collection in default
  scope — unqualified. They ride the same host heap and answer
  the same read combinators as the immutable ones, plus the mutators
  `+=`/`-=`/`++=`/`--=`, `add`/`addOne`/`addAll`, `append`/`prepend`/
  `appendAll`/`prependAll`, `insert`/`insertAll`, `remove`, `subtractOne`/
  `subtractAll`, `clear`, and — on a `Map` — `put`, `update` (`m(k) = v`),
  `getOrElseUpdate` and `remove`. Each prints with its own prefix
  (`ListBuffer(…)`, `ArrayBuffer(…)`, `Queue(…)`, `Stack(…)`,
  `ArrayDeque(…)`, `LinkedHashSet(…)`/`LinkedHashMap(…)`, and
  `HashSet(…)`/`HashMap(…)` at every size), and a derived collection keeps the
  receiver's kind, so `ListBuffer(1,2).map(_ * 2)` is a `ListBuffer`.
  A **user type of the same name shadows the built-in one**: these constructors
  are recognized by name, so a program that declares its own `class Stack` or
  `case class Queue` constructs that instead.
- **The end-taking buffers, and where each end is.** `Queue.enqueue` appends and
  `dequeue`/`front` read the head; `Stack.push` PREPENDS and `pop`/`top` read the
  head, because a `Stack`'s head is its top; `ArrayDeque` grows at both ends
  (`prepend`/`append`, `removeHead`/`removeLast`). `+=` is `Growable.addOne` on
  all three and therefore always APPENDS — `Stack(1,2,3) += 8` is
  `Stack(1, 2, 3, 8)` where `push(8)` is `Stack(8, 1, 2, 3)`. A removal from an
  empty one raises `java.util.NoSuchElementException: empty collection`; an empty
  `top`/`front` raises `head of empty Stack`/`head of empty Queue`.
- **`scala.collection.immutable.Queue`, the persistent FIFO.** Reached as
  `immutable.Queue(…)`/`scala.collection.immutable.Queue(…)` or, after
  `import scala.collection.immutable.Queue` (or a wildcard over that package), as
  a bare `Queue(…)`/`Queue.empty` — unqualified and unimported, `Queue` is the
  mutable one. `enqueue(x)`/`enqueueAll(xs)` answer a NEW queue, `dequeue` the
  pair of the front and the rest (`dequeue on empty queue` when there is none),
  `dequeueOption` that pair as an `Option`, and `front`/`head`/`last`/`tail`
  raise the class's own `… on empty queue` faults. Every other sequence
  operation keeps the kind and prints `Queue(…)`. Before this the qualified
  spelling silently built the MUTABLE queue, so `q.enqueue(3)` changed `q`.
- **`LinkedHashSet`/`LinkedHashMap` keep INSERTION order.** They are their own
  representation, never an alias for the table-ordered ones: the stored order is
  the linked list Scala threads through the table, so an add appends, a re-added
  element keeps its original position, and one removed and re-added moves to the
  end. They print `LinkedHashSet(…)`/`LinkedHashMap(…)` at every size.
- **`TreeSet`/`TreeMap` (`SortedSet`/`SortedMap`) keep KEY order.** Their own
  representation too: ascending under the natural `Ordering`, which also
  decides that two keys are one (`compare == 0`); `updated` replaces key and
  value. Every derived `Set`/`Map` stays a tree (`map`, `filter`, `+`, `-`,
  `++`, `range`/`rangeFrom`/`rangeUntil`/`rangeTo`), `firstKey`/`lastKey`,
  `minAfter`/`maxBefore` and a map's `keySet`/`keys` (a `TreeSet`) are
  present, and an end access of an empty tree raises `empty tree`. Keys whose
  ordering is the program's own (a class instance under `Ordered` or a given
  `Ordering`) are refused with a diagnostic, not mis-ordered.
- **`StringBuilder`.** `new StringBuilder`, `new StringBuilder(s)` and
  `StringBuilder(s)`. It is a growable `Seq[Char]` whose `toString` is its
  CONTENTS with no wrapper, so `println(b)` prints the text. `append` takes
  `String.valueOf` of any value (`b.append(7)` appends `'7'`), `+=` takes a
  `Char` and `++=` a `String` or a `Char` sequence; `insert`, `setCharAt`,
  `deleteCharAt`, `replace(start, end, s)`, `delete(start, end)`, `setLength`, `clear` and `result()` mutate or freeze it, and
  the `CharSequence` members `substring`/`indexOf`/`lastIndexOf`/`charAt` work
  alongside every sequence one. A SELECTING op (`take`, `filter`, `reverse`)
  answers another `StringBuilder` through `fromSpecific`; `map`, whose element
  type may change, reads `mutable.IndexedSeq`'s factory and so answers an
  `ArrayBuffer` — exactly as Scala does.
- **An explicit `Ordering`.** `Ordering.Int` and its siblings (the natural
  ordering), `.reverse`, `Ordering.by(f)`, `Ordering.fromLessThan(lt)`,
  `Ordering[T]` and `ord.on(f)`, plus the value's own `compare`, `lt`, `gt`,
  `lteq`, `gteq`, `equiv`, `max` and `min`. A `reverse` swaps the operands
  before the ordering it wraps runs (`Ordering.Reverse.compare(x, y)` is
  `outer.compare(y, x)`), so `Ordering.by(f).reverse` applies `f` to the
  right operand first and a reversed `fromLessThan` asks `lt(y, x)` first.
  Scala passes it in a second
  (implicit) parameter list, which this frontend flattens into the same call, so
  `xs.sorted(ord)` arrives with one argument and `xs.sortBy(f)(ord)` with two;
  `sorted`, `sortBy`, `max`, `min`, `maxBy` and `minBy` all take it. A sort asks
  its comparison exactly what the JVM asks it: `sorted`/`sortBy`/`sortWith` (and
  `java.util` `List.sort`, `Collections.sort`, `Arrays.sort(a, cmp)`) run a port
  of `java.util.TimSort` (`src/timsort.rs`) — binary insertion under 32
  elements, galloping run merges above — and an array of primitives under a
  non-default ordering and below 300 elements runs `scala.util.Sorting`'s merge
  sort, as `ArrayOps.sorted` does. `sortBy(f)` is `sorted(ord.on(f))`, so `f`
  runs on both operands of every comparison (left first), and `sortWith(lt)` is
  `fromLessThan(lt)`, which asks `lt(y, x)` when `lt(x, y)` is false. An
  inconsistent comparator raises the JDK's `IllegalArgumentException:
  Comparison method violates its general contract!` exactly where TimSort
  detects it. `ord.on(f)` over a reversed ordering keys both operands before the
  reversal swaps them, where `Ordering.by(f).reverse` swaps first. The NAMED orderings all collapse onto the one natural ordering,
  because there are no static types here to tell `Ordering.Int` from
  `Ordering.String` at a call site and nothing observable distinguishes them
  once applied to values of that type.
- **Boxed-primitive and `String` companion statics.** `scala.Int`'s
  `MaxValue`/`MinValue` family (and `Double`'s `MinPositiveValue`,
  `PositiveInfinity`, `NegativeInfinity`, `NaN`), and the `java.lang` boxes'
  statics: the parses `Integer.parseInt`/`parseLong`/`parseByte`/`parseShort`/
  `parseUnsignedInt`/`decode`/`valueOf` (most taking an optional radix), the
  renderings `toString(i, radix)`, `toBinaryString`/`toHexString`/
  `toOctalString`, `toUnsignedString`, `toUnsignedLong`, the comparisons
  `compare`, `compareUnsigned`, `max`, `min`, `sum`, `signum`, `hashCode`, the
  unsigned arithmetic `divideUnsigned`/`remainderUnsigned`, the bit twiddling
  `bitCount`, `reverse`, `reverseBytes`, `highestOneBit`, `lowestOneBit`,
  `numberOfLeadingZeros`, `numberOfTrailingZeros`, `rotateLeft`, `rotateRight`,
  and `MAX_VALUE`/`MIN_VALUE`;
  `Double.parseDouble`/`isNaN`/`isInfinite`/`toString`; `Boolean.parseBoolean`;
  `Character.isDigit`/`isLetter`/`isLetterOrDigit`/`isWhitespace`/`isUpperCase`/
  `isLowerCase`/`toUpperCase`/`toLowerCase`/`toTitleCase`/`isTitleCase`/
  `getNumericValue`; and
  `String.valueOf`. The two namespaces stay APART, exactly as Scala's do —
  `Double.parseDouble` really is "not a member of object Double" there — and a
  fixed-width rendering follows its box, so `Integer.toHexString(-1)` is
  `ffffffff` and `java.lang.Long.toHexString(-1L)` is `ffffffffffffffff`. The
  same is true of every bit-twiddling static (`Integer.reverse(1)` is
  `-2147483648`, `Long.reverse(1L)` is `-9223372036854775808`), and of the
  parses: which range a string is checked against is the METHOD's, which is why
  `"128".toByte` reports `Value out of range. Value:"128" Radix:10` while
  `"2147483648".toInt` reports `For input string: "2147483648"`. A radix outside
  `2..36` is rejected before any digit is read, with `Character.MIN_RADIX` /
  `MAX_RADIX` naming the bound.
- **The narrowing conversions.** `toByte` and `toShort` are the JVM's `i2b`/
  `i2s` — the low bits of the two's complement, sign-extended — on an `Int`, a
  `Long`, a `Char` and a `Double` receiver, and the result re-enters arithmetic
  at `Int` width (`128.toByte + 1` is `-127`). A `Double` composes the two JVM
  steps in order, saturating to an `Int` first and truncating second, so
  `1e20.toByte` is `-1`. `String` has the parsing side of the same widths
  (`toByte`/`toShort`/`toBoolean` alongside `toInt`/`toLong`/`toDouble`), and
  none of the INTEGER parses trim: `" 42".toInt` raises where `" 42".trim.toInt`
  answers 42, while `toDouble` accepts the padding because
  `Double.parseDouble` does. `toDouble`/`toFloat` (and `Double.parseDouble`/
  `Float.parseFloat`) follow `FloatingDecimal`'s grammar exactly: a trailing
  `f`/`F`/`d`/`D`, hex floats (`0x1.8p1`), case-sensitive `NaN`/`Infinity`
  (`inf` is rejected), `empty String` and `multiple points` as their own
  messages, and `toFloat` rounding once, straight to single precision.
- **`getClass`.** Answers a `java.lang.Class` carrying `getName` and
  `getSimpleName`, and printing as `class <name>` (a reference type) or the bare
  name (`int`, `double`, `boolean`, `char`). Modeled for a `String`, the
  primitives, a user `class`/`case class`/`object` (whose JVM class takes the
  `$` suffix Scala appends to an object) and a throwable — which is the usual
  reason to call it, `e.getClass.getSimpleName`. A bare `getClass` inside a
  class's method is `this.getClass`, so a base class's `toString` can name the
  runtime subclass (`s"${getClass.getSimpleName}(…)"`). A collection's
  `getClass` answers the reference's representation class: `$colon$colon` for a
  non-empty `List`, `Vector1`/`Vector2`/`Vector3` by length, `Map1`..`Map4` then
  `HashMap`, `Set1`..`Set4` then `HashSet`, `Some`, `None$`, `Range$Inclusive`/
  `NumericRange$Exclusive` and the rest, and
  `Tuple2$mc<A><B>$sp` for a pair of `Int`/`Double`/`Boolean`/`Char`
  components (a `Long` component is an `Int` here, so a `(Boolean, Long)` reads
  `Tuple2$mcZI$sp`). A class declared inside an `object` carries the enclosing
  names (`T$O$In`); its `getSimpleName` stays `In`.
  A library member class's `getSimpleName` is its own name too: `Set2`,
  `EmptyMap$`, `Inclusive`.
  There is one `Class` object per class, so `a.getClass == b.getClass` (and
  `eq`) holds for two values of one class. `classOf[T]` answers that same
  object under the JVM erasure: a value type is its primitive (`classOf[Int]`
  is `int`, `classOf[Unit]` `void`), a type constructor drops its arguments
  (`classOf[List[Int]]` is `class scala.collection.immutable.List`), `Any`/
  `AnyRef` are `java.lang.Object`, and a declared type or a throwable is its
  own; an `Array` or function type is refused. `new Object` / `new AnyRef` is
  a fieldless instance equal only to itself.

  A primitive reached through an `Any`-typed position reports the PRIMITIVE
  class where Scala reports the BOXED one: `val a: Any = 1; a.getClass` is
  `int` here and `class java.lang.Integer` in Scala. Boxing is what the JVM
  answer is about, and nothing in this runtime boxes — a value is the same
  value whatever static type it flows through. The divergence is uniform
  across `Int`, `Double`, `Float`, `Boolean` and `Char`, and it applies only to
  the erased position: called directly on the value (`1.getClass`,
  `0.1f.getClass`) all five answer the primitive name, as Scala does. Closing
  it would mean tracking `Any` through the width analysis purely so `getClass`
  could name a box that never exists.
- **`f(xs: _*)` — the varargs spread.** The sequence is handed STRAIGHT to the
  repeated parameter rather than collected into a one-element `ArraySeq`, so
  `def f(xs: Int*) = xs.sum; f(List(1,2,3): _*)` is `6`. Every collection
  FACTORY is varargs too, so `List(xs: _*)`, `mutable.Queue(xs: _*)` and
  `Map(ps: _*)` work and each answers its own kind; the element count is not a
  compile-time constant there, so those go through
  [`crate::host::FROM_SEQ`] rather than the fixed-arity `MAKE_*` builtins. A
  spread aimed at a parameter that is not repeated is a compile error, as it is
  in Scala.
- **Selection and application alternate in a postfix chain.** `"abc"(1).toInt`
  is an apply and then a selection, and so is `List(List(1,2))(0)(1)`. A
  `scala.collection.mutable` FACTORY is not curried either, so the second group
  of `mutable.ArrayBuffer(1,2,3)(1)` is an INDEX (answering `2`) rather than a
  fourth element.
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
  scramble, and the depth-first order its iterator walks. The same hash is a
  method: `x.##` answers it (`1.0.## == 1.##`, unlike `1.0.hashCode`), running a
  user `hashCode` override for anything that is not a number, and `0` for null. A derived collection
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
- **The full `match` pattern grammar.** Beyond literal/wildcard/typed/tuple and
  constructor patterns: `@` binders (`case w @ Pt(0, _) =>`, which bind the whole
  scrutinee alongside its parts), `|` alternations (`case 0 | 1 | 2 =>`, each
  branch tried in turn), the cons pattern `h :: t` and `Nil`, and sequence
  patterns `List(a, b)` / `Seq(…)` / `Vector(…)` / `Array(…)` with an optional
  trailing `_*` (named — `rest @ _*`, or Scala 3's `rest*` — or anonymous). A sequence pattern tests the
  receiver's REPRESENTATION first and its length second, so `case List(a, b)`
  does not match a two-element `Vector`, matching Scala. The same grammar backs
  **pattern definitions**: `val (a, b) = pair`, `val Some(x) = opt`,
  `val h :: t = xs`, `val Pt(x, y) = p` and nested forms all bind into the
  enclosing scope and raise `scala.MatchError` on a non-matching value.
  The `scala.collection` extractors `h +: t` and `init :+ last` match any
  non-empty sequence (a `Range` and an `ArrayBuffer` too) and keep its kind in
  the remainder; `+:` binds tighter than `::`/`:+` and is right-associative,
  `:+` is left-associative (`case rest :+ y :+ z`), and both work in a `val`
  pattern definition.
  A `val` whose binder is a lowercase identifier applied to arguments is an
  extractor pattern too (`val date(y, m) = s` with `val date = "…".r`).
- **`scala.Option`'s method surface.** `get`, `getOrElse`, `orNull`, `isEmpty`,
  `isDefined`, `nonEmpty`, `size`, `contains`, `map`, `flatMap`, `filter`,
  `filterNot`, `exists`, `forall`, `count`, `foreach`, `fold(ifEmpty)(f)`,
  `orElse`, `collect`, `flatten`, `head`, `headOption`, `toList`/`toSeq`/
  `toVector`/`iterator`, and `toRight`/`toLeft`. The `Option(x)` factory answers
  `None` for `null`. `List[Option[A]].flatten` drops the empties.
- **`Either`, right-biased as Scala 2.13 made it.** `Left`/`Right` are built-in
  case classes, so `Right(1)`, `case Left(e) =>` and `List[Either[…]].collect`
  work, and the method surface operates on the `Right` while passing a `Left`
  through: `isLeft`/`isRight`, `getOrElse`, `orElse`, `map`, `flatMap`,
  `foreach`, `exists`/`forall` (vacuously true on a `Left`), `contains`, `fold`,
  `swap`, `filterOrElse`, `toOption` and `toSeq`/`toList`. `Either.left` answers
  the `LeftProjection` that mirrors those onto the `Left` — `get` (which throws
  `Either.left.get on Right`), `getOrElse`, `map`, `flatMap`, `foreach`,
  `exists`/`forall`, `toOption`, `toSeq`/`toList` and `filterToOption`. It is a
  `case class` in Scala and one here, so it renders `LeftProjection(Left(bad))`
  and compares structurally through the ordinary case-record machinery.
- **`scala.util.Try`.** `Try(e)` expands in the parser to
  `try Success(e) catch { case t: Throwable => Failure(t) }`, so `e` is by-name
  and evaluated inside the `try`; `Success`/`Failure` are built-in case classes
  like `Left`/`Right`, which is what makes `case Success(v) =>` and
  `Failure(e).exception` work. The surface is `isSuccess`/`isFailure`, `get`
  (which rethrows), `getOrElse`, `orElse`, `map`, `flatMap`, `foreach`,
  `filter`, `recover`/`recoverWith`, `transform`, `failed`, `toOption`,
  `toEither` and `toSeq`/`toList`. Two deviations: Scala catches only `NonFatal` throwables and
  lets a fatal one (`StackOverflowError`, `OutOfMemoryError`) propagate, where
  this catches every `Throwable`; and a program defining its own `Try` cannot
  shadow the factory, the same way it cannot shadow `List(…)`.
- **`Product` on every `case class`/`case object` and tuple.** `productArity`,
  `productPrefix`, `productElement(i)`, `productElementName(i)`,
  `productIterator` and `productElementNames`, all over the
  primary-constructor prefix (a body `val` is a field but not a product
  element). `Tuple2.swap` and `iterator`/`reverseIterator` on a sequence.
  A `List`'s (and `ListBuffer`'s) iterator advances with `current.head`
  and no `hasNext` test, so `next()` past its end raises `head of empty list`,
  as does a `map`, `zipWithIndex`, or a `drop`/`take`/`slice` that outlasts
  it; a `filter`/`collect` or an exhausted `take` raises `next on empty
  iterator`, as every other kind's iterator does.
  Scala 3's generic tuple operations `size`/`head`/`last`/`tail`/`init`/
  `take`/`drop`/`splitAt`/`reverse`/`++`/`toList`/`toArray` (each positional
  one answering a tuple of the arity left), and `TupleN(…)` (`Tuple1` through
  `Tuple22`) as both a factory and a pattern (`val Tuple2(a, b) = …`); a
  `Tuple1` prints `(a)`.
- **`search` and `scala.collection.Searching`.** `xs.search(e)` answers
  `Found(i)` or `InsertionPoint(i)` (both also answer `insertionPoint`, and
  both work as patterns). A `List`/`ListBuffer`/immutable `Queue` scans as
  `SeqOps.linearSearch` does and every indexed kind bisects as
  `IndexedSeqOps.binarySearch` does, so an unsorted receiver gives the
  reference's answer for its kind.
- **`partitionMap`, in-place sorting, wildcard type arguments.**
  `partitionMap(f)` on a sequence, `Set` or `Map` splits `f`'s `Left`/`Right`
  payloads. `sortInPlace()`/`sortInPlaceBy(f)`/`sortInPlaceWith(lt)` sort an
  `Array`, `ArrayBuffer` or `ArrayDeque` in place and answer it, and
  `scala.util.Sorting.quickSort(a)`/`stableSort(a)` sort an array in place and
  answer `()`. Scala 3's `?` wildcard type argument (`List[?]`, `case _: (?, ?)`)
  is read wherever a type is. A `Map` also answers `zip`/`zipAll` as the
  `List` of its pairs.
- **Non-local `return`, and `finally` on the way out.** A `return` inside a
  lambda — including the `foreach`/`map` closures a `for` comprehension
  desugars to — leaves the *method* that lexically contains it, not just the
  closure. It is lowered the way Scala lowers it: the value is parked as a
  `scala.runtime.NonLocalReturnControl` and carried out by the same unwind walk
  an exception uses (`NLR_RAISE`/`NLR_TAKE` in `src/host.rs`), so every
  enclosing `finally` runs, innermost first, before the value reaches the
  caller. No `catch` arm can intercept it. `return` is also accepted in
  expression position (`xs.foreach(x => if (p(x)) return x)`), as in Scala,
  where its type is `Nothing`.
- **The wider `String`/`StringOps` surface.** `indexOf`(+`from`), `lastIndexOf`(+`from`),
  `replace`, `stripPrefix`/`stripSuffix`, `capitalize`, `compareTo`(the JDK's
  char-difference result, not a normalized sign)/`compareToIgnoreCase`/
  `equalsIgnoreCase`, `*` (repeat), the total slicing operations `take`/`drop`/
  `takeRight`/`dropRight`/`slice`/`splitAt` (clamped, never throwing),
  `head`/`last`/`init`/`tail`, `apply(i)`, `codePointAt`/`codePointBefore`/`codePointCount`
  (with the JDK's index messages), `distinct`, `sorted`, `mkString`
  (0/1/3-arg), `toCharArray`, `zipWithIndex`, and the closure-taking
  combinators `map`, `flatMap`, `collect`, `filter`/`filterNot`,
  `takeWhile`/`dropWhile`, `count`, `exists`/`forall`, `foreach`, `find`,
  `indexWhere`, `partition`, `span`, `foldLeft`/`foldRight`, `reduce`,
  `scanLeft`, `sortWith`/`sortBy`, `maxBy`/`minBy`, `groupBy`, `groupMap`/`groupMapReduce`,
  `zip`, `sum`/`product` (a `Char`, wrapping at 16 bits, as `Numeric[Char]` does), and the
  conversions `toList`/`toVector`/`toArray`/`toSet`. Every accessor that hands
  out an element hands out a `Char` (`head`, `last`, `apply`/`charAt`,
  `headOption`/`lastOption`, `min`/`max`, and the elements of `toList` and
  friends), which is what makes `"abc".toList.map(_.toInt)` the code points.
- **`Char` as its own type.** `'a'` is a distinct runtime value that dispatches
  as a NUMBER in arithmetic (`'a' + 1 == 98`, `'a' / 2 == 48`, `-'a' == -97`)
  and as TEXT everywhere string conversion applies (`println('a')` is `a`,
  `List('a')` is `List(a)`). Its numeric conversions answer the code point
  (`'a'.toInt == 97`, and `'5'.toInt == 53` where `"5".toInt` parses to `5`),
  `toChar` rounds back from an `Int`, and `asDigit`, `toUpper`/`toLower`,
  `isLetter`/`isDigit`/`isLetterOrDigit`/`isUpper`/`isLower`/`isWhitespace`,
  `compare`, `max`/`min` and `hashCode` (the code point) are all present. `+`
  against a `String` stays concatenation (`'a' + "b" == "ab"`), which is the one
  place the numeric face gives way.
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
  desugars to a pattern-matching anonymous function. A *refutable* generator
  pattern takes Scala 3's `case` spelling — `for (case Some(x) <- opts)`, which
  desugars to a `withFilter` on the pattern and so SKIPS a non-matching element
  rather than raising. Scala 3 requires the keyword (bare `for (Some(x) <- opts)`
  is a compile error there), so it is required here too.
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
  prefixes. `map`/`filter` over one yield a `Vector`; `reverse` and the
  positional slices `take`/`drop`/`takeRight`/`dropRight`/`tail`/`init`/
  `slice`/`splitAt` yield a `Range` with the bounds `Range.scala` computes
  (`(1 to 5).take(2)` is `Range 1 to 2`, an empty slice `empty Range 3 until 3`), and `sum`/`length`/`head`/`last`/`toList`/`mkString`/`contains`/
  `min`/`max` all work. A `dropRight` that removes everything (and so the
  last `inits` window) leaves the empty range at the END, `empty Range 3 until
  3` for `1 to 3`, as the library does. The window iterators answer `Range`s
  too: `grouped` and `sliding` take each window as a `slice` (`(1 to 7).grouped(3)`
  is `Range 1 to 3`, `Range 4 to 6`, `Range 7 to 7`), `tails` is `drop(i)` and
  `inits` is `dropRight(i)` for every `i` up to the length, and `grouped(0)`
  reports Range's own `size=0, but size must be positive`. Used as a `for`
  generator it still compiles to the counted loop (no materialization).
  `sorted` under the natural ordering is the range itself when it ascends and
  its `reverse` when it descends, still a `Range`.
- **A `Long` or `BigInt` range — `1L to 9L`, `BigInt(1) until BigInt(9) by 2`.**
  A `NumericRange`: laid out as the `Range` of its values (a `BigInt` one's
  elements are `BigInt`s; a bound beyond the `Long` range is refused), printing
  `NumericRange 1 to 9` with no `inexact` marker, and its `reverse`/`take`/
  `drop`/`by` stay `NumericRange`s while `sorted` is a `Vector`. A `BigInt`
  range used to treat its bounds as `0` (`List(0)`), and a `Long` one printed
  `Range`. `Range.Long(a, b, s)`/`Range.Long.inclusive` and the `BigInt` forms are
  the same ranges (they were a catchable `NullPointerException`). `getClass`
  names `Range$Inclusive`/`Exclusive` or `NumericRange$Inclusive`/`Exclusive`.
- **A range longer than eight million elements** holds no elements: its
  `length`, `apply`, `contains`, `head`/`last`, `min`/`max`, closed-form `sum`
  (wrapping as `Int`/`Long` arithmetic does), `take`/`drop`/`slice`, `reverse`
  and `by` are answered from its bounds, so `(0 until Int.MaxValue by 2).length`
  is `1073741824`. Its traversals STREAM it — `foreach`, `filter`/`filterNot`,
  `map`, `count`, `exists`/`forall`/`find`/`indexWhere`, `foldLeft`, `takeWhile`
  (a `Range` again) and a lazy `iterator` — each element built as it is visited;
  only a result collection is held — `r.map(f)` over twenty million elements
  is the twenty-million-element `Vector` Scala builds — and one past 64
  million (where a default-sized JVM heap gives out first) raises
  `OutOfMemoryError: Range too large`, as does any other member that would
  hold every element of the range itself. The function is resolved once per
  traversal rather than per element. Measured (debug build, instructions
  retired, `(1 to 9000000).count(_ % 3 == 0)`): 156.2G before, 131.6G after
  (17.4k to 14.6k per element); the rest is the interpreter running the body. Past `Int.MaxValue` elements `length` raises Scala's
  `IllegalArgumentException` (`… seqs cannot contain more than Int.MaxValue
  elements.` for a `Range`, `More than Int.MaxValue elements.` for a
  `NumericRange`, which raises it from `last`/`contains`/`apply`/`sum` too).
- **A `BigDecimal` range** (`BigDecimal(1) to BigDecimal(2) by BigDecimal("0.5")`)
  holds its elements as `start + step * n`, as `NumericRange.locationAfterN`
  computes them, and prints `NumericRange 1 to 2 by 0.5`. Without `by` it is
  `Range.Partial` (`Range requires step`, nothing but `by`). It is held as a
  `Vector` of its elements beside its bounds; `take`/`drop`/`reverse` are
  `NumericRange`s again with the bounds `NumericRange.scala` computes, every
  other derivation the `Vector` Scala's generic `IndexedSeq` builds.
- **A `Char` range — `'a' to 'e'`, `'a' until 'z' by 2`.** Scala's
  `NumericRange[Char]`, a class of its own rather than a `Range`: it prints
  `NumericRange a to e` (`empty NumericRange e to a`, and `by` renders its step
  as the `Char` it is), `take`/`drop`/`tail`/`init`/`splitAt` keep the class
  with the bounds `NumericRange.scala` computes while `slice`/`takeRight`/`span`
  and every transformation build a `Vector` (`grouped` an `ArraySeq` per
  group), `sum`/`product` answer a `Char` that wraps at 16 bits, `contains`/
  `indexOf` miss a non-`Char` probe (`('a' to 'c').contains(98)` is `false`),
  `reverse` of a non-empty one raises `ArithmeticException` because a `Char`
  step cannot be negated, and an empty one's `head`/`last`/`min`/`max` report
  `Nil.head`. A `for` over a `Char`-literal range iterates the `Char`s.
- **`scala.math`.** `abs`, `signum`, `min`, `max`, `round`, `floor`, `ceil`,
  `rint`, `sqrt`, `cbrt`, `exp`, `log`, `log10`, `pow`, `hypot`, the trig and
  inverse-trig functions, `atan2`, `toRadians`/`toDegrees`, `Pi`, `E` — under
  the `math`, `scala.math`, `Math` and `java.lang.Math` spellings. The `Int`
  overloads stay integral (`math.abs(-4)` is `4`, not `4.0`), and the two
  namespaces are kept apart where the JDK lacks an overload
  (`Math.signum(5)` is `1.0`, `math.signum(5)` is `1`). A method named without
  arguments is eta-expanded into a function value, as Scala does:
  `xs.map(math.sqrt)`, `xs.reduce(math.max)`, `math.hypot.tupled`.

  The transcendental functions are bit-exact with the JVM. `src/fdlibm.rs` is a
  port of fdlibm 5.3 as the JDK transliterates it in `java.lang.FdLibm`, which is
  `StrictMath`; measured on JDK 21 (aarch64), `Math.X` equals `StrictMath.X` on
  every one of 200,000 random inputs for `tan`, `asin`, `acos`, `atan`, `atan2`,
  `exp`, `log`, `log10`, `log1p`, `expm1`, `sinh`, `cosh`, `tanh`, `cbrt`,
  `pow` and `hypot`, while the platform libm was a ulp off on 2-40% of them
  (`math.exp(1)` is `2.7182818284590455`, not `2.718281828459045`). `sin` and
  `cos` are the exception — see *Modeled with a documented simplification*.
  `Math.absExact` raises `Overflow to represent absolute value of
  Integer.MIN_VALUE` (`Long.MIN_VALUE` for a `Long`), and `abs`, `floorDiv`,
  `floorMod` and unary `-` at `Long.MinValue` wrap as the JVM does rather than
  faulting; `Math.floorDiv(Int.MinValue, -1)` wraps at 32 bits.

- **`scala.collection.mutable.PriorityQueue`.** `enqueue`/`+=`/`++=`, `dequeue`,
  `dequeueAll`, `head`/`max`, `clone`, `clear`, and the shared read-only
  sequence surface. Its `toString` and its iteration are the raw order of the
  binary HEAP ARRAY behind it — `PriorityQueue(3,1,4,1,5,9,2,6)` prints
  `PriorityQueue(9, 6, 4, 1, 5, 3, 2, 1)`, neither the input order, nor sorted,
  nor the `9, 6, 5, 4, 1, 3, 2, 1` that repeated sift-up insertion leaves — so
  the library's own `fixUp`/`fixDown`/`heapify` are ported rather than a
  max-heap written from scratch. `++=` heapifies from the first new position
  where `+=` sifts one element up, which are different arrays; the factory
  appends raw and runs one bottom-up sweep. `map` answers an `ArrayBuffer` (the
  result element type carries no implied `Ordering`) where `filter` stays a
  `PriorityQueue`. The ordering is the implicit one, including a user
  `Ordered`/`Comparable` class's `compare`; an explicitly passed `Ordering`
  argument is not accepted.
- **`for` comprehensions follow the reference's desugaring, which a user-defined
  collection can observe.** Value definitions after a generator become `val`s
  of its lambda (`for (a <- w; b = f(a); c <- g(b)) yield c` is one `flatMap`
  and one `map`, no tuple), a guard after a definition pairs the value onto the
  generator and filters the pair, a destructuring definition `(p, q) = e` is
  accepted, and `for (c <- w) yield c` does not call a user `map` at all.
  A constructor or typed pattern without `case` (`Some(x) <- w`, `a: Int <- w`)
  parses and does not filter.
- **A member `lazy val` is a memoizing method.** In a class, a trait or a
  singleton object it initializes on its first READ — after the constructor, so
  it sees the fields as they are then — instead of with the instance. (An entry
  object's `lazy val` already did.)
- **Unbounded iterators.** `Iterator.from(n[, step])`, `continually(x)`,
  `iterate(x)(f)` and `unfold(s)(f)` are the `LazyList` machinery marked as an
  iterator: they print `<iterator>`, answer `hasNext`/`next()` and derive
  lazily (`map`/`filter`/`take`/`takeWhile`/`zip`/`drop`). After `next()` every other
  member reads from the cursor on: `it.next(); it.take(2)` used to restart at
  the first element. `Iterator.single`,
  `fill` and `tabulate` work. `LazyList` also has `take`, `slice`, `flatMap`,
  `zipWithIndex`, `scanLeft`, `collect`, `dropWhile`, `++`/`#:::`/`:+`/`+:`,
  `range`, `tabulate`, `fill` and `unfold` lazily; every other member forces
  the list and answers the strict result as the `LazyList` it is on the
  reference. `Stream` is the same list printing `Stream(…)` with its head
  evaluated.
- **`scala.Enumeration`.** See *`Enumeration` beyond `Value`* below for what is
  modelled; a value is a record carrying `id`, `name` and the owning object, a
  `ValueSet` is a sorted set of them printing `Color.ValueSet(…)`.
- **`BigDecimal`**, with `RoundingMode.X` / `BigDecimal.RoundingMode.X` as the
  mode argument — see below.
- **Qualified names through an object.** `new O.In()`, `O.Sub.m(…)`, `case
  O.P(x) =>`, `case O.Const =>` and `import O._` name a type or member declared
  inside an object; the flat type namespace holds it under its own name.
  `` `x` `` in a pattern compares against the value of `x`.
- **A literal with `_` digit separators** (`1_000_000`, `0xFF_FF`).
- **An extractor whose `unapply` parameter is declared `String`, `Int`, … or a
  class** applies only to a scrutinee of that type; any other value falls
  through to the next case.
- **A repeated constructor parameter** (`class C(val xs: Int*)`,
  `case class D(items: String*)`), packed from the trailing arguments or from
  `xs: _*`.
- **The innermost given wins.** A `using` parameter of the enclosing `def` is
  preferred over a same-typed given declared outside it, as Scala's nesting
  rule has it, instead of reporting an ambiguity.
- **`x.asInstanceOf[T]` raises `ClassCastException`** when a `String`, a boxed
  primitive, a collection or a user class is cast to a different one of those
  (numeric-to-numeric casts convert and `null` casts to anything). `sys.error(m)`
  is `throw new RuntimeException(m)`; a tuple has `copy(_1 = …)`.
- **A `MatchError` names the scrutinee's JVM class** the way `getClass` does —
  `B(x) (of class B)`, `List(1, 2) (of class …$colon$colon)`.

- **`scala.util.Using`** (imported by name or under `scala.util._`):
  `Using.resource(res)(f)` is the library's own body — a `null` resource is
  `NullPointerException: null resource`, the resource is closed however `f`
  ends, and when `f` threw, an exception from `close()` is SUPPRESSED into
  `f`'s rather than replacing it (`Using`'s preference order decides which of
  the two wins: `VirtualMachineError` > `LinkageError` >
  `InterruptedException` > a non-fatal throwable > a `ControlThrowable`), so
  a non-local `return` out of `f` still closes and returns. `Using(res)(f)` is
  that inside `Try`, where a resource expression that throws is a `Failure`
  with nothing to close. `Using.Manager { use => … }` hands `op` a manager:
  `use(r)` registers `r` and answers it, `use.acquire(r)` registers it; once
  `op` ends every resource is closed, the last registered first, each close
  running whatever the others did; the first exception (`op`'s, else the
  first close's) is the `Failure`, with the later ones suppressed into it. A
  resource registered after the manager closed is `IllegalStateException:
  Manager has already been closed`. `Throwable.addSuppressed`/`getSuppressed`
  work, with the JDK's self-suppression and `null` checks.
  `Using.resources(r1, …, rn)(f)` (two to four resources) is the library's
  nested `resource` calls: each later resource is evaluated after the earlier
  ones are acquired and released before them. A `Releasable` other than
  `close()` is not modelled.
- **A constructor that throws inside a `try` reaches the handler.** An
  exception from a class body (`require` failing), a supertype argument, or a
  constructor argument returns to the `new` site; it used to stop the run as
  uncaught, because the constructor had no unwind frame of its own.
- **Custom string interpolators.** `id"a${x}b"` for any `id` other than
  `s`/`f`/`raw` is `StringContext("a", "b").id(x)`, the parts kept raw
  (escapes undecoded, `$$` a `$`), so an `extension (sc: StringContext) def
  id(args: Any*)` or an `implicit class` over `StringContext` supplies it.
  `StringContext(parts…)` is a case class of its `parts` (an `ArraySeq`),
  with `s(args…)` — escapes decoded by `processEscapes`, an unknown one
  `StringContext$InvalidEscapeException` — and `raw(args…)`, both also in the
  `xs: _*` form, and an argument count other than one less than the parts' is
  `IllegalArgumentException: wrong number of arguments (n) for interpolated
  string with m parts`. `StringContext(…).f(args…)` on a literal
  `StringContext` is the `f` interpolator — each argument formatted by the
  conversion opening the part after it, `%s` by default — and on any other
  receiver the compile error Scala gives (`Expected statically known
  StringContext`). In a PATTERN, `case id"a${x}b" =>` matches through
  `StringContext("a", "b").id`'s `unapplySeq` (preferred) or `unapply`, the
  splices binding what its `Option` holds.
- **`java.util.Locale`.** The constants (`US`, `UK`, `ROOT`, `GERMANY`,
  `FRANCE`, `ITALY`, `JAPAN`, `KOREA`, `CHINA`, `TAIWAN`, `CANADA`, …), `new
  Locale(language[, country[, variant]])`, `Locale.of`, `forLanguageTag`,
  `getDefault` (`en_US`, the reference's pinned locale, until `setDefault`),
  `getLanguage`/`getScript`/`getCountry`/`getVariant`/`getExtension`/
  `getUnicodeLocaleType`/`toLanguageTag`/`toString`, equality. A locale is its
  language, script, region and variant plus its extensions, ported from the
  JDK's `BaseLocale`/`LanguageTag`/`InternalLocaleBuilder`
  (`src/jlocale.rs`): `forLanguageTag` keeps the script, variants, extensions
  (the `u` extension canonicalized) and a private-use `lvariant` as the
  variant, the obsolete `iw`/`ji`/`in` become `he`/`yi`/`id`, the legacy
  constructor gives `ja_JP_JP` its `u-ca-japanese` and `th_TH_TH` its
  `u-nu-thai` (`ja_JP_JP_#u-ca-japanese`,
  `ja-JP-u-ca-japanese-x-lvariant-JP`), and `toLanguageTag` moves an
  ill-formed variant into private use and spells `no_NO_NY` `nn-NO`.
  `String.format(locale, …)` and `formatLocal(locale, …)` localize the decimal
  conversions (`%d`, `%f`, `%e`, `%g`): the decimal and grouping separators
  and the digits are the locale's CLDR ones, which the JDK formats with —
  `1.234.567,89` for `GERMANY`, a narrow no-break space grouping for
  `FRANCE`, `’` for `de-CH`, Arabic-Indic digits for `ar`. Measured for about
  fifty languages and countries; a locale outside the table formats as
  `en_US`. `toUpperCase(locale)` / `toLowerCase(locale)` pair the dotted and
  dotless `i` for Turkish and Azerbaijani. A `nu` extension picks the digits
  (`th_TH_TH` formats `42` as `๔๒`; `arab`/`arabext` bring their own
  separators too). `Locale.setDefault` changes what `String.format`, `format`,
  `formatted`, the `f` interpolator and the argument-less
  `toUpperCase`/`toLowerCase` localize by. `getDisplayName`/
  `getDisplayLanguage`/`getDisplayScript`/`getDisplayCountry`/
  `getDisplayVariant` answer the JDK's English names (`src/locale_names.rs`,
  measured from its CLDR and COMPAT bundles: `Chinese (Traditional, Taiwan)`,
  `English (United States, Computer)`, `Japanese (Japan, JP, Japanese
  Calendar)`, `xx (YY)` for codes it has no name for); display names in any
  locale but `en`/`en_US` (an explicit `inLocale`, or the default after
  `setDefault`) are refused rather than guessed.
- **`m()(x)` applies `m()`'s result.** An empty argument list ends a member
  call, so `e.getSuppressed()(0)` and `sb.toString()(1)` index the result;
  they used to fold into `m(x)`. A `def` declared with an empty first clause
  (`def m()(x: Int)`) still takes both clauses as one call.
- **`List.range` / `Vector.range` / `Seq.range` elements carry the endpoints'
  width**, so `List.range(0, n).map(x => x - 1).sum` wraps at 32 bits as
  Scala's `Int` sum does. `Seq.range`/`IndexedSeq.range`/`Iterable.range`
  answer the `NumericRange` itself, as Scala's do (`IterableFactory.range` is
  `from(NumericRange(…))`, and those factories hand an `IndexedSeq` back):
  `NumericRange 1 until 4`, class `NumericRange$Exclusive`. A zero step is
  still refused when the range is built, where the library's `NumericRange`
  waits for the first member that counts its elements.
- **`Regex.unanchored`** is an `UnanchoredRegex`: its `matches`, `unapplySeq`
  and its extractor in a pattern find the first match anywhere in the input
  (`case u(n)` binds the first group of it), and `anchored` turns it back.
- **A fold's accumulator has the seed's width.** `xs.foldLeft(0)(_ + _)` over
  `Int`s wraps at 32 bits as Scala's does (`foldRight`'s accumulator is its
  second parameter, and `fold` when seed and elements agree); it used to
  widen silently past `Int.MaxValue`.

## Not implemented (parse errors / unresolved today)

- **`Enumeration` beyond `Value`.** `object Color extends Enumeration { val Red,
  Green = Value }`, `Value("n")`, `Value(id)`, `Value(id, "n")`, `values`,
  `withName`, `apply(id)`, `maxId`, `id`, `compare` and the orderings are
  modelled. A `case class V extends super.Val` (extra members on a value), the
  `nextId`/`ValueSet` constructors and `Enumeration(initial)` are not.
- **`enum` cases share one type namespace.** The cases share the one flat type namespace, so two
  enums with a case of the same name are a redeclaration.
- **What the optional-braces pass does not cover.** An indentation region
  inside `( )` is not a region, so a fewer-braces argument written inside an
  argument list (`println(wrap("b"):` with the block below) does not parse. (A singleton object's `lazy val` is a memoizing method, initialized on
  its first read.)
- **Widening gaps.** An `Int` constant in range narrows to a declared `Char`
  (`val c: Char = 66` is `B`; it printed `66`), and a `Char` argument to an
  `Integer`/`Long` static (`Integer.toBinaryString('a')`) is its code. A `Char` widens to `Double`/`Float` but not to a declared
  `Int`/`Long` (`val i: Int = 'c'` is `c`, not `99`), and a function type's
  result does not widen (`val f: Int => Double = x => x * 2` answers `6` for
  `f(3)`, not `6.0`).
- **A wildcard `import` binds only the members this frontend actually provides
  for that package.** `import scala.math._` binds `sqrt`, `Pi` and the rest of
  `scala.math`; `import scala.collection.mutable._` binds the collection
  factories, including making a bare `Set(1, 2)` the MUTABLE one. A wildcard
  over any other package binds nothing, and an unresolved name under any
  wildcard is still `not found` rather than a rewritten guess.

  That gate is deliberate. A wildcard names no member, so binding one means
  deciding that some opened package provides a name — and a blind rewrite would
  turn every unresolved name into a member access on whichever package was
  imported last, reporting a failure further downstream and in worse terms than
  the honest one. The two package member lists mirror the runtime's own tables
  (`host::math_member`, the mutable factories), so a wildcard cannot bind a name
  the runtime then refuses.

  An unqualified `mutable` package path is also accepted without the import
  Scala requires for it: `mutable.Set(1, 2)` compiles here and is `Not found:
  mutable` in Scala. That is a leniency, not a wrong answer — every program the
  reference accepts behaves identically.

  A QUALIFIED spelling of a name this frontend answers unqualified resolves to
  the same thing: `scala.util.Try(e)` is the `Try(e)` expansion,
  `scala.collection.immutable.List(1)` and `immutable.List(1)` are the `List`
  literal, and `scala.Some(1)` is `Some(1)`. The stripped prefixes are only the
  packages that hold nothing of their own — `scala`, `scala.util`,
  `scala.util.control`, `scala.collection.immutable`, `scala.collection` (whose
  `Seq`/`Map`/`Set`/`SortedMap` companions delegate to the immutable ones, so
  `scala.collection.Seq(1)` is `List(1)`) and their shorter spellings. `scala.math` and `scala.collection.mutable` are NOT stripped,
  because their members are not their bare names (`math.signum(5)` is `1` and
  there is no bare `signum`; `mutable.Set(1)` builds a different collection
  than `Set(1)`), and a capitalized selection is required, so a value that
  happens to be named `util` still answers its own methods.

- **`xs: _*` outside an argument position.** The spread is parsed where Scala
  allows it — as an argument — and rejected everywhere else, which is also what
  Scala does. A spread handed to a parameter that is not repeated is a compile
  error on both sides.
- **Building an IMMUTABLE collection one element at a time is quadratic**, and
  it is the REPRESENTATION that is quadratic rather than anything specific to
  `Map`. Measured against a frozen binary, taking the MINIMUM of repeated runs —
  this machine runs many builds at once, and under load a single timing is not
  a measurement (one run of an unchanged case came back 145x its own minimum):

  | n | `m = m + (k -> v)` | `v = v :+ i` |
  | --- | --- | --- |
  | 1000 | 0.21s | 0.03s |
  | 2000 | 1.03s (4.9x) | 0.13s (4.3x) |
  | 4000 | 4.22s (4.1x) | 0.44s (3.4x) |
  | 8000 | | 1.97s (4.5x) |
  | 16000 | | 8.86s (4.5x) |
  | 32000 | | 35.91s (4.1x) |

  Both are ~4x per doubling. The `Vector` case has no hashing, no ordering and
  no key lookup — only the copy — so it is the FLOOR for a copying
  representation, and it is quadratic too. The `Map` sits roughly an order of
  magnitude above it (7x at n=1000 rising to 10x at n=4000, so the gap grows:
  the trie-order rebuild carries a log factor the copy does not).

  Scala is cheap here for one reason, and it is not an index: its immutable
  `Map`/`Set`/`Vector` are PERSISTENT — a CHAMP trie and a radix-balanced
  vector — so `+` and `:+` share all but one path with the collection they came
  from and cost `O(log n)`. A lookup accelerator does not help, because the cost
  is the COPY that `+` is defined to make.

  Implementing CHAMP would close it, and the obstruction is not the trie itself
  but what the trie would have to replace. The entry vector IS the interface
  here: it is what `toString`, every combinator, every iteration and 17 separate
  `HeapVal::Map` sites read, and materializing one per read would reintroduce
  the `O(n)` it removed. The order is also NOT incrementally maintainable the
  way the mutable table's is — that one is sorted by `(bucket, hash)`, so an
  insert is a binary search, whereas CHAMP order depends on which other keys
  share a hash prefix, and adding one key can move another from a node's data
  section into a new sub-trie.

  What IS available without the trie is the constant. Replacing `champ_order`'s
  per-node `BTreeMap` with a fixed 32-slot array — a node has exactly 32 slots —
  is **1.77x**, measured by running the two binaries INTERLEAVED so both arms
  see the same load, and identical at n=1000 and n=2000 and on both the median
  and the minimum (1.76x-1.78x). It leaves the growth class alone.

  MUTABLE `Map`/`Set` do not have this shape: their inserts are amortized
  `O(1)` (see the entry above), and so are the growable sequences.

- **`Future`/`ExecutionContext`.** `Future {
  … }` with `Await.result` is not modeled — there is no scheduler here, and a
  synchronous stand-in would answer correctly for the programs that only await
  and wrongly for every program that observes concurrency, which is the whole
  reason to reach for one. An `s"…"` PATTERN binds names or `_` only
  (`case s"$k=$v"`); a nested pattern inside `${…}` is a parse error.

  Not gaps: `unapply` and `unapplySeq` on a user `object` both bind, including
  the length check that keeps `case P(x, y)` from matching three elements;
  union and intersection types (`Int | String`, `A & B`) parse and are erased,
  which is what the `match` a program writes against them already checks;
  `opaque type`, `type` aliases and `inline def` behave as the reference does.

  A `type` alias is substituted textually where a type is written, scoped to the
  declaring block, class or `def`. A parameterized alias (`type Pair[A] = (A, A)`)
  is not expanded, and a class type parameter named like an in-scope alias is not
  shadowed — both fall back to the unexpanded name, which is erased as before.

- **`BigDecimal`.** An unscaled `BigInt` and a scale, under
  `MathContext.DECIMAL128` (34 significant digits, half-even) for `+ - * /` and
  `pow`, with `java.math.BigDecimal`'s `toString` (plain unless the scale is
  negative or the adjusted exponent is below -6) and its scale-insensitive
  `==`/`compare`/`hashCode`. `setScale(n[, RoundingMode.X])`, `scale`,
  `precision`, `abs`, `signum`, `max`/`min`, `toInt`/`toLong`/`toDouble`/
  `toBigInt`, `floor`/`ceil`, `sum`/`product` and `Int`/`Double`/`BigInt`
  operands on either side are modelled. A value carries its `MathContext`, as
  Scala's does: `BigDecimal(x, mc)` rounds to `mc` and every `a op b` rounds in
  `a`'s, by its mode; `new MathContext(n[, mode])` (`HALF_UP` by default),
  `MathContext.DECIMAL32`/`64`/`128`/`UNLIMITED` (an UNLIMITED `/` must
  terminate, else the JDK's `ArithmeticException`), `round(mc)`, `mc`, and
  `bigDecimal`/`underlying`'s `stripTrailingZeros`/`toPlainString` are
  modelled; `sqrt` is not.

  `BigInt` is a host-heap value (`Value::Obj` carrying a `num_bigint::BigInt`),
  the same shape a `Char` has, so every operator on one reaches the numeric hook
  and never the native fast path. Arithmetic, comparison, `==` against an
  integral value, hashing (`BigInt(3).## == 3`), `sorted`/`max`/`sum`/`product`,
  `pow`/`abs`/`signum`/`gcd`/`mod`/`modPow`/`/%`/`isProbablePrime`, the bitwise
  operators and shifts, `toInt`/`toLong` (low-order bits), `toDouble`,
  `toString(radix)`, `BigInt("…", radix)` and the typed pattern `case b:
  BigInt` all answer as the reference does. A declared `BigInt` type widens an
  integral value assigned to it (`val z: BigInt = 7`), as Scala's `int2bigInt`
  does. Division by zero raises `/ by zero` while the dividend fits a `Long`
  and `BigInteger divide by zero` past it, because Scala's `BigInt` keeps a
  `Long`-range value as a bare `Long` and divides it with `ldiv`.

- **A `LazyList` combinator that must see every element terminates only on a
  finite list**, as in Scala — `toList`, `sum`, `length`, `mkString`,
  `foreach`. An infinite one is BOUNDED rather than left to hang: forcing gives
  up after a million steps and reports, because an infinite source with no
  element matching a `filter` cannot be told from a slow one.

  The structure itself is complete. Elements are produced on demand and
  MEMOISED, so the infinite factories work (`LazyList.from(1)`, `.from(n, step)`,
  `.iterate(seed)(f)`, `.continually(v)`), `#::` conses with a BY-NAME tail —
  which is what makes a list definable in terms of itself
  (`val fibs: LazyList[Int] = 0 #:: 1 #:: fibs.zip(fibs.tail).map(…)`) —
  `map`/`filter`/`zip`/`tail`/`drop`/`takeWhile` force nothing, a literal starts unforced,
  and printing shows only what has been computed
  (`LazyList(1, 2, <not computed>)`). Traversing twice recomputes nothing,
  which a program can count.

- **A counted `for` over a `Char` range whose endpoints are both variables.**
  `val a = 'a'; val z = 'z'; for (c <- a to z)` is recognized as a range
  generator by shape and lowers to the inline integer loop, so `c` is the code
  point rather than the `Char`; the parser can see a `Char` literal endpoint
  but not a binding's type. `(a to z)` as a VALUE is a `NumericRange[Char]`, so
  `(a to z).foreach(…)` is right.
- **A `Char` range stepped by zero.** `'a' to 'c' by 0` raises
  `IllegalArgumentException: step cannot be 0.` where it is built; the
  reference builds it (it prints) and raises on the first element access.
- **Overloads that differ only in parameter type** are dispatched on the
  arguments' RUN-TIME types, not the static ones Scala resolves on. The
  declared parameter types are tested most specific first (`Any` and type
  parameters last), so `f(Int)`/`f(String)`/`f(Any)` pick as Scala does for a
  value of known type. What a run-time test cannot tell apart is two overloads
  that differ only by `Int` vs `Long` (one representation here), or a static
  type narrower than the value's class — `val a: Animal = new Dog; f(a)` picks
  the `f(Dog)` overload here and `f(Animal)` in Scala.
- **Symbolic operators beyond the wired set.** `/:` and `:\`. A user class's
  own symbolic methods (`def +(o: V)`, `def <(o: V)`, `def unary_-`) are
  supported: the infix use dispatches on the left operand's runtime class, so
  `a + b` calls `V$+` for a `V` and stays plain arithmetic for a number, and
  `p += q` expands to `p = p + q` as SLS 6.12.4 specifies.
- **Anonymous classes.** `new Greeter { def greet(n: String) = … }` is
  refused (`trait Greeter is abstract; it cannot be instantiated`). The body
  would be a class declared inside an expression, and a class here cannot
  capture the locals of the frame that declares it (see below), which is what
  an anonymous class body reads most often.
- **A companion `apply` overloaded by parameter TYPE on a `case class`.**
  `case class Pt(x: Int); object Pt { def apply(s: String) = … }`: `Pt("41")`
  has the constructor's arity, so it constructs, where Scala resolves the
  `String` overload. A companion `apply` of a different arity — or any companion
  `apply` of a plain class — is called, and a companion `apply` overloaded among
  itself by type (`apply(Int)`/`apply(String)`) is dispatched on the argument's
  run-time type.
- **The wider standard library.** `scala.io`, `scala.collection.*` as a
  namespace, and the many `String`/numeric methods beyond the wired subset
  above. Members a differential sweep reached and found missing (each an
  honest `not a member` error, never a wrong answer) were `prefixLength` on a
  sequence instance, `Ordering.Tuple2(o1, o2)` and the JDK `String` member
  `lines`; all three are now modelled. `Map.foreachEntry`, `Map.applyOrElse`, `Map.updatedWith`
  (`updateWith` on the mutable one), `Map --`/`removedAll`, `sizeIs`/`lengthIs`,
  an instance's `empty` and `toBooleanOption` were found the same way and are
  now modelled.
- **`getClass` on a function, an `Array`, an `Iterator` or a `Range`.** The
  representation classes of those (`Range$Inclusive`, `[I`, the lambda's
  synthetic name) are not modelled, and `getClass` on them answers `value
  getClass is not a member of value`. The collection, option, either and tuple
  classes are (see above).
- **A `Float` is not observable through a JIT-compiled loop.** `Float` is a real
  type — single-precision literals, arithmetic, conversions, constants,
  `toString`, `getClass` and `hashCode` all match Scala, and they match it
  wherever the value is reached from, including out of a collection, a record
  field, a `Map` value or an `Any`. It is a distinct RUNTIME value:
  `Value::Status`, whose payload is exactly the 32 bits an `f32` needs (see
  `scala.Float` in `src/host.rs`).

  What that costs is the JIT. fusevm's `is_native_num` admits only `Int` and
  `Double`, which is precisely what routes every operation on a `Float` through
  the numeric hook instead of letting native arithmetic read the bit pattern as
  a number — and the same test keeps a `Float` out of the JIT's typed
  registers. So a hot `Float` loop stays interpreted where the identical loop
  over `Int` or `Double` reaches native code, and `scala --tiers` reports that
  honestly. The statically-typed path costs a `CallBuiltin` (`SF32_ARITH`) for
  the same reason: rounding an `f64` result afterwards rounds twice and can
  land a ulp from the value Scala computes (`16777217.0f * 0.2f` is
  `3355443.2`; the double-rounded product is `3355443.3`).

  This is the same trade the Java frontend makes for the identical rule, and it
  is a floor rather than a bug: single precision cannot be both exact and
  native on a VM whose native floats are 64-bit.

- **Implicit resolution is a match on the declared type's TEXT.** `given`,
  `using`, `summon`, context bounds, `extension` methods and implicit
  conversions all work (see `README.md`), and what bounds them is how a
  candidate is selected: `Sh[Int]` finds `given Sh[Int]` by comparing type
  names, with a call site's type parameters substituted first.

  Substitution is driven by a value parameter declared to BE the type
  parameter, so `def show[A](x: A)(using Sh[A])` called `show(1)` binds
  `A = Int`. Inference through a CONSTRUCTED type (`xs: List[A]`) or from a
  return position is not attempted: that needs a real type system, and a guess
  there resolves to the wrong given rather than reporting that it could not. An
  implicit that cannot be resolved stays an ordinary arity error, and an
  AMBIGUOUS one is an error, as in Scala.

  A CONDITIONAL given — `given listShow[A](using s: Show[A]): Show[List[A]]
  with { … }`, named or anonymous, with any number of `using` instances — is a
  class whose constructor takes those instances. When no plain given matches,
  its result type is unified with the one wanted (`Show[List[A]]` against
  `Show[List[Int]]`, tuple types `(A, B)` included) and each `using` instance is
  resolved in turn, recursively. A type argument is read off an argument whose
  static type is nameable: a literal, a binding, a sequence literal of one
  element type (`List(1, 2)` is `List[Int]`), a tuple literal, `Option(x)`, and
  a library method whose result follows from its receiver's type (`xs.head`,
  `xs.headOption`/`lastOption`/`find`, `filter`/`take`/`sorted`/…, an
  `Option`'s `get`/`getOrElse`) on a receiver of known type. A
  type argument that is the whole of an expression's type is read too —
  `List.empty[Int]`, `Option.empty[String]`, `List[Int]()` and the
  `Vector`/`Seq`/`IndexedSeq`/`Set`/`Map` forms — so `show(List.empty[Int].headOption)`
  resolves. A `map` whose function is a one-parameter lambda is typed by the
  lambda's body, read with the parameter standing as a value of the element
  type (`List.empty[Int].map(_ + 1)` is `List[Int]`, `.map(_.toString + "!")`
  `List[String]`): operators with numeric promotion and `String` `+`, and the
  fixed-result members `toString`/`toInt`/`toLong`/`toDouble`/`length` of the
  value types. A lambda body of any other shape still names no type.
  An `extension [A](xs: List[A])(using s: Show[A])` clause is a `using`
  parameter of every method the extension declares, resolved at the call from
  the receiver's type. An `opaque type`/`type` alias declared in an `object`
  body is in force for the object's members and its importers.

  What follows from text-matching: no subtyping (a `given Animal` does not
  satisfy a `using Dog`, nor the reverse), no variance, no implicit priority
  ordering beyond "exactly one candidate or it is ambiguous", and no
  companion-object search — a given must be in the program's own scope.

  A real MEMBER of the receiver wins over an extension of the same name, as in
  Scala: a user class's member, a library collection's (`xs.size` under
  `extension (xs: List[Int]) def size`), and the own members of `String`
  (`java.lang.String`'s), the numeric value classes, `Boolean`, `Char` and
  `Array`. Their implicit ENRICHMENTS — `StringOps`, `RichInt`, `ArrayOps` — are
  not members, and an extension shadows them (`s.reverse` under `extension (s:
  String) def reverse` is the extension), which is also Scala's rule. A
  receiver selected by its run-time type tries the member first and falls back
  to the extension only when the receiver has none; where the extension takes
  a parameter BY NAME and the argument has side effects, member existence is
  decided from the receiver's class alone (a table of the standard-library
  classes' members), so the argument is evaluated only by the call taken. One
  residual: Scala keeps a member only when it TYPE-CHECKS against the
  arguments, and falls back to the extension otherwise (`xs.exists(b)` with a
  `Boolean` `b` under `extension (xs: List[A]) def exists(f: => Boolean)` is the
  extension); this frontend has no types to check with and takes the member.

  A `given`, an `extension`, an `implicit val`/`def` and an `implicit object`
  declared as a member of a `def main` entry object enter the implicit scope as
  they do in an `extends App` body; they used to be skipped there, so a `using`
  clause found nothing (`value show is not a member of Int`). An `implicit
  object` is a given of each parent type it declares.

  `given` as an implicit CLASS or with an `inline` body is not modeled.

- **A repeated `@main` parameter, and two `@main` methods in one file.**
  `@main def go(first: Int, rest: String*)` is refused rather than collecting
  the remainder, and a file declaring two `@main` methods is refused rather than
  picking one — Scala asks for `--main-class` there, which this frontend has no
  flag for.
- **Overloading a block-level `def`.** Two `def`s of one name in the SAME block
  are a Scala overload; the resolver's inner-shadows-outer rule read them as a
  redefinition and pointed every call at the second body, including the calls
  written before it. `def g(x: Int) = "int"` beside `def g(x: String) = "str"`
  answered `str` twice, and even the statically decidable `def h()` beside `def
  h(x: Int)` answered `h1` twice — no diagnostic either time. They are now
  refused. A CLASS or OBJECT member's overload still resolves by argument count
  (`Owner$method$arity`); only the flat `def` namespace lacks that split.
- **A forward reference inside a BLOCK is accepted, where Scala rejects it.**
  Scala's forward-reference rule applies to a block's own definitions, not to a
  template's members: `def main(…) = { val c = new C(3); …; class C(…) }` and
  `def main(…) = { println(later); val later: Int = 7 }` are both compile errors
  ("forward reference to C … extends over the definition of c"), and this
  frontend runs them — answering `8` and `null` respectively. The `extends App`
  spelling of the same two programs is LEGAL Scala, because there the `val`s and
  `class`es are object members; that half is modeled (an object member is
  visible in any order, and a forward-referenced field reads its type's default).
  Only the block rule is unchecked. Found by running the `appmember` mode under
  `--entry mainsig`, which no `app` run could have reached.
- **Found missing by a differential sweep, each an honest error rather than a
  wrong answer:** (`Ordering.Tuple2(o1, o2)` … `Tuple5` with explicit
  component orderings, the argument-less `Ordering.Tuple2[A, B]` … `Tuple5`, as are `orElse`/`orElseBy` chains, `Ordering[T].on(f)` and the
  `None`-first `Option` ordering — the last of which compared every `Option`
  equal, so `List(Some(3), None).sorted` kept its input order), and an explicit
  ordering for an immutable `TreeSet`/`TreeMap`/`SortedSet`/`SortedMap` or a
  `PriorityQueue` — `TreeSet(3, 1)(using ord)`, which parsed as `apply` and
  answered `contains`, `false` — are modelled: the elements are held in that
  order, membership is the ordering's `compare == 0`, `min`/`max` stay the
  implicit natural ones, the members answering the same sorted kind keep it
  while `map` builds under the result type's own, and a `PriorityQueue` sifts
  by it. The MUTABLE `TreeSet`/`TreeMap` take one too:
  `+=`/`put`/`update` re-lay the collection out in that order, and
  `-=`/`remove`/`contains`/`get` find the element it calls equal. `ts + x`/
  `ts - x` (and `ts += x` on a `var`) is the collection's `+`/`-` MEMBER
  wherever the receiver's static type is a collection or cannot be named — a
  parameter, a field, a method's result, a chain — so an `Ordering.by(f)`
  runs `f`. The choice is the receiver's type: a proven `Int`/`Long`/`Float`,
  `Double`, `String`, `Char`, `Boolean`, big number or program class keeps the
  native arithmetic op, and a program that builds no explicitly ordered
  collection (the only source of one) routes nothing. Measured (debug build,
  instructions retired): a three-million-iteration `Int` `while` loop of
  `s = s + i - 1` is 140.5M before and 139.7M after in a program that builds
  an ordered `TreeSet` (the loop stays `Add`/`Sub`); a fold's lambda
  parameters take the seed's and the elements' widths, so
  `(1 to 2000000).foldLeft(0L)(_ + _)` stays native too: 24.75G in a program
  with an explicit ordering against 24.66G without one. An element the ordering
  calls equal to one already held leaves that one in place, as the red-black
  tree does — `+`/`incl`/`++`/`concat`/`union` and the mutable
  `+=`/`add`/`++=` keep the existing element, `+`/`updated`/`++` and
  `put`/`update` on a map keep the existing KEY and take the new value — and a
  literal inserts its elements in source order, so `TreeSet("xyz", "abc")(using
  Ordering.by(_.length))` is `TreeSet(xyz)` and `TreeMap("aa" -> 1, "zz" ->
  2)(…)` is `TreeMap(aa -> 2)`. `--`/`removedAll`/`diff` remove by the
  ordering too. An IMMUTABLE `TreeSet`/`TreeMap` under an explicit ordering is
  stored in a port of `scala.collection.immutable.RedBlackTree`
  (`src/rbtree.rs`), so the ordering is asked exactly the comparisons the
  library asks — `compare` for insert, delete, lookup and `minAfter`/
  `maxBefore`, ONE `lt`/`lteq` for the range views (one closure call for a
  `fromLessThan`), `equiv` for `==` between two such trees on keys that are not
  the same JVM box — and a member that changes nothing answers the receiver
  itself (`(s + x) eq s` for an `x` held). The tree-to-tree `union`/
  `intersect`/`diff`/`--` (against a tree under an `==` ordering), `filter`,
  `partition`, `take`/`drop`/`slice` and `tail`/`init` are the library's
  join-based algorithms; `filterNot` and a `diff` against any other set build
  through the factory's insertion, as theirs do. The JVM box identity the
  library tests before comparing (`k eq tree.key`) is modelled as the
  `Integer`/`Character` caches decide it (a value in [-128, 127] shares one
  box, a `Double` never does), and a `String` is taken to be an interned
  literal. Measured (debug build, instructions retired) on 4000 inserts, 8000
  lookups, 4000 `updated`s, two traversals and 1000 removals: 242.6G-242.8G on
  the sorted vector, 41.4G on the tree. The MUTABLE `TreeSet`/`TreeMap` still
  keep a sorted vector, so how many times their key function runs is not the
  library's), and
  SAM conversion of a lambda to a user trait (`val s: Show[Double] = (t:
  Double) => …` then `s.show(x)`).
- **An integer's box is kept only where it is WIDENED to `Any` with a static
  type the compiler knows.** Every integer is one `i64`, so the box
  (`java.lang.Integer`/`Long`/`Short`/`Byte`) is recorded at the widening:
  a `val`/argument declared `Any` (or `AnyVal`/`Matchable`/`AnyRef`), an
  ascription `e: Any`, an argument to a parameter typed by a type parameter
  (`def f[T](x: T)`), the elements of a literal declared `C[Any]` and the values
  of one declared `Map[K, Any]` (after Scala 3's literal harmonization:
  `Vector(1, 2L)` is all `Long`), the elements of an immutable `List`/`Vector`
  of PROVEN element width reaching `C[Any]` (`val xs: List[Any] = ys` with
  `ys: List[Long]`), and both sides of `equals`. Type tests,
  `asInstanceOf` (a `ClassCastException` across boxes), `getClass`, `equals`
  and format argument checks read the box; printing, `==`, `##`, hashing and
  arithmetic read the number, and a typed pattern binds the number. A value
  whose box was never recorded — one that reached `Any` through a path the
  compiler could not type, such as a `map` whose body calls a `def` with no
  declared result — still passes every integral type test. An `if` WIDENED to
  `Any` (`val v: Any = if (c) 1 else 2L`, or a block ending in one) boxes each
  branch on its own, and so does a `map` reaching `C[Any]`: its function body
  is widened, so `xs.map(i => if (even) i else i.toLong)` yields `Integer`s and
  `Long`s, as Scala's per-branch boxing does. A box is an INLINE
  value (a `Value::Ref` whose payload variant names the box), not an arena
  entry, so it is freed with its last copy. Measured (debug build, instructions
  retired, peak RSS), against the build before boxes existed: a numeric loop
  with no widening 127.38G → 127.41G; a loop widening two integers to `Any` and
  type-testing both per iteration 13.57G / 14 MB → 17.59G / 14 MB (+30%: two
  `BOX_ANY` builtin calls and the `Long` test the old permissive `Int` test used
  to skip). The first, arena-interned representation cost 20.83G / 140 MB.
- **`String.chars`/`codePoints` are a lazy `IntStream` of the members a
  program reaches for** — `map`/`filter`/`distinct`/`sorted`/`skip`/`limit`,
  and `sum`/`count`/`toArray`/`forEach`/`anyMatch`/`allMatch`/`noneMatch`/
  `reduce(identity, op)`, with `max`/`min`/`findFirst`/`findAny`/`average` as
  `OptionalInt`/`OptionalDouble`. Each element runs the whole pipeline before
  the next is pulled, `limit` stops the pull, and a size-preserving pipeline
  answers `count` without running, as on JDK 21. `boxed`/`mapToObj` turn it
  into an object `Stream` (with `map`/`filter`/`sorted`/`distinct`/`limit`/
  `skip`, `toList`, `findFirst` as an `Optional`, `reduce` in both forms, and
  `collect` with `Collectors.toList`/`toSet`/`joining`/`counting`, a
  collected list printing `[a, b]`), and a stream is single-use: an op on one
  already linked or consumed is the JDK's `IllegalStateException`. Also
  `flatMap` (with `java.util.stream.Stream.of`), `max`/`min` with a
  `Comparator` (`Comparator.naturalOrder`/`reverseOrder` or a two-argument
  function), `IntStream.summaryStatistics`, and `Collectors.groupingBy(f[,
  downstream])`/`toMap(kf, vf)` into a `HashMap` printed `{k=v, …}` (with
  `get`/`getOrDefault`/`containsKey`/`size`/`isEmpty`). A Java set or map
  iterates in its table order of the JDK `hashCode` (`Long` and `Double` fold
  their bits). `LongStream`/`DoubleStream` (and their `IntStream`/`Stream`
  factories `range`/`rangeClosed`/`of`/`iterate`/`empty`) carry their own
  width: `LongStream.sum` wraps at 64 bits, `DoubleStream.sum`/`average` use the
  JDK's compensated (Kahan) summation, `mapToInt`/`mapToLong`/`mapToDouble`/
  `asLongStream`/`asDoubleStream` convert. `Collectors.partitioningBy`/
  `mapping`/`reducing` and a `toMap` merge function are modelled, as are a
  collected list's `lastIndexOf`/`subList`/`containsAll`, a collected map's
  `keySet`/`values`/`entrySet` (entries with `getKey`/`getValue`)/
  `containsValue`/`forEach`, and `java.util.List.of`. A collected `ArrayList`
  or `HashMap` is mutable (`add`/`add(i, x)`/`set`/`remove(int)`/
  `remove(Integer.valueOf(x))`/`addAll`/`sort`/`clear`; `put`/`remove`/
  `merge`/`computeIfAbsent`/`putIfAbsent`/`clear`, re-laid-out in table
  order), and an unmodifiable one (`List.of`, `Stream.toList`) answers those with
  `UnsupportedOperationException`; a collected `HashSet` (`Collectors.toSet`)
  is mutable too (`add`/`remove`/`addAll`/`removeAll`/`retainAll`/`clear`), its
  table only ever growing, as the JDK's does. `Long`/`DoubleSummaryStatistics`,
  `peek`, and `Stream`/`IntStream` `.generate`/`.concat` are modelled; `concat`
  and `flatMap` pull their inner streams element by element at the terminal
  op, so a short-circuiting terminal stops them, and `count` over a pipeline
  of known size (`concat` of sized streams, `peek` included) runs nothing, as
  on JDK 21. `new java.util.ArrayList`/`LinkedList`/`HashMap`/`HashSet`/
  `ArrayDeque`/`TreeMap`/`TreeSet` (named in full or imported) build the JDK
  classes, with the `Deque` operations, the `NavigableSet`/`NavigableMap`
  queries, `iterator()`, and `java.util.Arrays` (`asList` — fixed size, `set`
  and `sort` allowed —, `sort`, `fill`, `binarySearch`, `toString`, `equals`)
  and `java.util.Collections` (`sort`, `reverse`, `shuffle(l, rnd)`, `swap`,
  `max`/`min`, `frequency`, `nCopies`, `emptyList`, `singletonList`). A
  `catch` arm may name a built-in throwable by its qualified name (`case e:
  java.util.NoSuchElementException`), which used to match nothing. An
  `Arrays.sort`/`toString`/`binarySearch`/`fill`/`equals` call whose array
  argument's element type is a reference type other than `AnyRef`/`Any` (or
  `Boolean` for `sort`/`binarySearch`) is the compile error scalac reports —
  no JDK overload takes it — read off the argument's static type, an explicit
  `Array[AnyRef](…)` type argument included; an element type that cannot be
  named is left to run. `Ordering.on(f)` on the companion is `value on is not
  a member of object Ordering`, since `Ordering[T]` now keeps its type argument.
  `PriorityQueue` is the JDK binary heap (printed and iterated in its array
  order, `siftUp`/`siftDown`/`heapify` ported), with a `Comparator` argument
  as for `TreeSet`/`TreeMap`, whose comparator decides membership
  (`compare == 0` is the same element); under a comparator those two answer
  `add`/`put`/`get`/`contains`/`containsKey`/`remove`/`first`/`last`/
  `firstKey`/`lastKey`, the `NavigableSet`/`NavigableMap` queries
  (`floor`/`ceiling`/`higher`/`lower` and their `Key`/`Entry` forms,
  `pollFirst`/`pollLast`, the first/last entries, the descending forms) and the
  read-only views. `headSet`/`tailSet`/`subSet`/`headMap`/`tailMap`/`subMap`
  (every arity, with the JDK's inclusive defaults) are LIVE views, natural
  order or comparator: a write to the backing collection shows in them, a
  write through one must fall in its range (`IllegalArgumentException: key
  out of range`) and reaches the backing collection, and its `clear`/`poll…`
  empty or take from the range there. A view of a view is the JDK's
  `NavigableSubMap` one: each bound the inner call names must lie in the outer
  view (an exclusive bound may sit on an outer bound) —
  `IllegalArgumentException: fromKey out of range` / `toKey out of range`
  otherwise — and a bound it leaves open is the outer one, so the result is
  the intersection and its writes are range-checked against it; a
  `subSet`/`subMap` with its low bound above its high one is `fromKey >
  toKey`. The one-argument `headSet(e)`/`tailSet(e)`/`subSet(a, b)` answer a
  `java.util.SortedSet` (and `headMap(k)`/`tailMap(k)`/`subMap(a, b)` a
  `SortedMap`), statically typed so: the inclusive-flag forms and the
  navigation members (`floor`, `ceiling`, `pollFirst`, `firstEntry`, …) are
  refused on one, as scalac refuses them. `Collections.unmodifiableList`/`Set`/`Map`/`Collection` are VIEWS: a
  write to the backing collection shows through, and their own mutators raise
  `UnsupportedOperationException`. `new java.lang.StringBuilder` is Scala's
  builder except that `reverse()` reverses in place. Not modelled: the
  remaining `java.util` classes. `toUnmodifiableSet` and `Set.of` iterate in
  an order salted per JVM run (`ImmutableCollections.SALT32L`; three reference
  runs printed three orders), which no frozen record can pin. A stream's `toString` is the JVM identity form,
  which is not reproducible.
- **Found by the second October 2026 differential round.** Each raised a
  catchable `java.lang.NullPointerException` where Scala runs, and is now
  modelled: a bare `getClass` inside an `object`/`case object` body (a trait
  method inherited into `case object Unit1` answers `Unit1$`), `this` and
  `getClass` inside the ENTRY object (`this.h`, `this.v`, `getClass.getName`
  answering `Main$`), `Function.tupled(f)`, `mutable.TreeMap`/`TreeSet`/
  `SortedMap`/`SortedSet` and `mutable.BitSet`, and `scala.util.boundary` with
  `break(v)` (bare through `import scala.util.boundary.break` or the relative
  `import scala.util.boundary, boundary.break`, or as `boundary.break(v)`).
  A bare `getClass` inside a TOP-LEVEL `def`, an `@main` body or a lambda in
  either now answers `scala.Predef$` (Scala 3.10.0 resolves it to
  `Predef.getClass`, with a warning), and one inside a lambda in a class
  method captures `this` and answers the class; `Function.untupled` (at any
  arity it is applied), `Function.const` (its argument evaluated once) and
  `Function.chain` are modelled, and so is a trailing application folded into
  any of them or `Function.tupled` (`Function.tupled(f)((1, 2))`).
  Still open: a `break` whose `Label` arrives through a `using` parameter
  rather than lexically is refused (`break` outside any `boundary`), and so is
  `boundary[T]` with an explicit type argument; `println(this)` in the entry
  object prints `Main@0` where Scala prints the unreproducible identity form
  `Main$@<hash>`.
- **Found by the October 2026 Scala 3 feature sweep.** Each failed to compile
  or ran wrong where Scala 3.10.0 runs, and is now modelled: Scala 3's
  wildcard `import a.b.*` (it recorded nothing, so `import Units.*` left the
  object's members reading `null`, and it ended no line, so a following
  `@main def` was swallowed into the import and the file had `no entry
  point`), `{x, *}` and the `as` rename (`import U.{v as vv}`, `import a.b.c
  as d`); an `import` BELOW the prologue (`import Units.*` after `object
  Units`), which the top-level loop stepped over token by token, losing the
  import and any `@main` after it; a `derives CanEqual` clause on a class,
  which consumed the following `@main` the same way; `extends A, B` (Scala 3's
  comma between parents); the creator application `C(args)` for a plain class
  (`new` was required); `lazy val fib: LazyList[Int] = 0 #:: 1 #:: fib.zip(…)`
  inside a `def` (its thunk captured the cell before it existed) and a lazy
  `val` holding a function applied as `g(x)` (it was not forced); the
  `h #:: t` extractor and `case _: LazyList[?]`; a `MatchError` on a
  `LazyList` naming `scala.collection.immutable.LazyList`; `@main` with a
  repeated last parameter (`rest: String*`, read by
  `CommandLineParser.parseRemainingArguments`); `inline` parameters, `inline
  if` and `inline x match`; the JDK `String.lines` stream and the deprecated
  `prefixLength`. Still open from the same sweep: `export` clauses (`export
  in.{hello}` in a class body, `export A.*` in an object — the forwarders are
  not generated, so the member reports `not a member`), and `scala.io.Source`
  (no reader at all; `Using(Source.fromString(s))(…)` answers a `Failure`).
  (`Regex.unapplySeq`, `super.m` inside a singleton `object`, the immutable
  `ListMap`, a `case class` with a SECOND parameter list — whose product is the
  first list only —, `BigDecimal.underlying` (a `java.math.BigDecimal` with
  `toPlainString`) and `MatchIterator.matchData` are now modelled, as are the buffer/`Map` `…InPlace` transformers, `keysIterator`/`valuesIterator`, and the `String` members
  `indexOfSlice`/`lastIndexOfSlice`/`containsSlice`/`inits`/`tails`/
  `reverseIterator`/`sameElements`/`distinctBy`/`union`/`regionMatches`/
  `indent`/`getBytes(charset)` this round listed;
  `getBytes` knows UTF-8, ISO-8859-1, US-ASCII and the UTF-16 forms, by name or
  `StandardCharsets` constant.)
- **`do/while` is not a gap.** Scala 3 removed it from the language and the
  reference compiler rejects it, so scalars does not implement it either.

## Modeled with a documented simplification

- **`inline if` / `inline x match` choose their branch at run time.** The
  reference reduces them at compile time, a type pattern against the
  scrutinee's STATIC type. They agree whenever the condition is a constant or
  the static type is the runtime class (`kind(3)` with `case _: Int`); they
  differ for a scrutinee widened before the call (`val a: Any = 3; kind(a)`
  takes `case _` in Scala and `case _: Int` here). `inline` parameters are
  ordinary by-value parameters.
- **`Function.untupled(f)` takes any number of arguments.** Scala overloads it
  for `Tuple2` to `Tuple5`; with no static arity for `f`, the function built
  here tuples however many arguments it is applied to.

- **A member access on `null` raises `NullPointerException` without the
  JDK's message.** `s.length` on a `null` `String` is the catchable
  `java.lang.NullPointerException`, but `getMessage` answers `null` where the
  JDK's helpful message (`Cannot invoke "String.length()" because "<local1>" is
  null`) names bytecode slots and accessors this frontend does not have.
- **`String.valueOf` of an EMPTY `Array[Char]`** answers `""` when the array was
  BUILT as one (`Array[Char]()`, `Array.empty[Char]`, `new Array[Char](0)`,
  `Array.ofDim[Char](0)`) or DERIVED from an `Array[Char]` by an
  element-preserving op (`filter`, `take`, `drop`, `slice`, `tail`, …). One
  that is empty for any other reason (`Array[Any]()` cast, a `map` whose result
  happens to be empty) has no element to tell it from any other empty array and
  still renders `Array()`.
- **`math.sin` and `math.cos` round as the platform libm does, not as the
  JVM.** On JDK 21 aarch64 they are HotSpot intrinsics that disagree with
  `StrictMath` on about 7% of inputs (measured: 14,323 of 200,000), and on
  x86-64 they are a different intrinsic again, so there is no portable answer
  to port; a ulp-level difference remains on roughly 8% of arguments.
  `src/fdlibm.rs` carries the `StrictMath` versions (`strict_sin`,
  `strict_cos`), unused.
- **`Char` classification follows a newer Unicode than JDK 21.** `isLetter`,
  `isDigit`, `isSpaceChar`, `isWhitespace`, `isControl`, `isTitleCase`,
  `asDigit`, `Character.digit` and the simple case mappings `toUpper`/`toLower`
  are Java's general-category rules (a sweep of the whole BMP agrees with the
  reference on all but 18 chars). Those 18 are characters Unicode 16 added or
  re-cased (`U+A7CB`..`U+A7DC`, `U+1C89`, `U+1C8A`, `U+019B`, `U+0264`,
  `U+0295`); JDK 21 implements Unicode 15, and `toTitleCase`, which is
  `toUpper` outside the Latin digraphs and the Georgian Mkhedruli letters,
  inherits the same 18. `Character.getNumericValue` is exact over the whole
  BMP: its non-digit values (`Ⅻ` is 12, `½` is -2) are a table extracted from
  JDK 21.
- **`sizeIs`/`lengthIs` answer the size itself.** Scala's `xs.sizeIs > n`
  builds a `SizeCompareOps` wrapper whose six comparison operators each call
  `sizeCompare(n)` — a short-circuit that lets a `LazyList` or a `List` stop
  counting at `n + 1`. Here the receiver's elements are already materialized
  (see *A `Range` value is materialized* below), so there is nothing to
  short-circuit, and the wrapper is replaced by the `Int` it would compare: the
  six comparisons answer as Scala's do because they are the same comparisons.
  What differs is unobservable except by printing the wrapper itself
  (`println(xs.sizeIs)` is `3` here and a `SizeCompareOps` identity in Scala)
  or by handing it to a method expecting one, neither of which a program does.
  `lengthIs` is `Seq`'s and is refused on a `Set`, as the reference refuses it.
  `Map.updatedWith` keeps the other identity Scala documents: an absent key
  whose function answers `None` hands back the receiver itself.

- **`eq`/`ne` answer reference identity for class instances, singleton
  `object`s, throwables and collections.** Those are heap records whose handle
  IS their identity (every mention of an `object` answers the one record, so
  `O eq O`). A collection member that hands back its receiver (an immutable
  `TreeSet`/`TreeMap` member that changes nothing, `Map.updatedWith` answering
  `None` for an absent key) answers the same handle; one this frontend rebuilds
  where the library would share answers `false`.
  A `String` has no identity in the value model — `"abc" eq "abc"` is true in
  Scala because literals are interned, while a concatenated copy is not `eq` —
  so `eq` on a `String` is refused (`not a member`) rather than guessed.
  `equals` on a primitive follows the boxed class (`1.equals(1.0)` and
  `"a".equals('a')` are false), but an `Int` and a `Long` of equal value share
  one representation, so `1L.equals(1)` answers `true` where Scala says `false`.

- **A `String` is indexed by CODE POINT, where the JVM indexes by UTF-16 code
  unit.** `Char` here is a Rust `char` — a whole scalar value — so a character
  outside the Basic Multilingual Plane occupies one position instead of the two
  a JVM `String` gives it. Every index-shaped operation therefore disagrees on a
  string containing one, and only on such a string. Measured against Scala 3.8.4
  on JDK 26.0.2 with `"𝕏a"` (`U+1D54F` followed by `a`):

  | expression | Scala | here |
  | --- | --- | --- |
  | `.length` | `3` | `2` |
  | `.indexOf("a")` | `2` | `1` |
  | `.charAt(0).toInt` | `55349` (the high surrogate) | `120143` (the scalar) |
  | `.head.toInt` | `55349` | `120143` |
  | `.substring(0, 2)` | `𝕏` | `𝕏a` |
  | `.reverse.length` | `3` | `2` |

  This is a REPRESENTATION difference, not a wrong constant: closing it means a
  `Char` that can hold an unpaired surrogate, which `char` cannot. Every string
  whose characters are all in the BMP — which is every string in the frozen
  corpora and every string in the test suite — indexes identically. Noted rather
  than half-fixed, because a partial conversion (say, `length` counting code
  units while `charAt` still counts scalars) would be worse than either
  consistent model.
- **A function value prints `<function1>`, not the JVM lambda's identity.**
  Scala 3.8.4 renders a function through `Object.toString`, which is a class name
  plus an identity hash (`T$$$Lambda/0x0000180001099c0@64c64813`) — a fresh
  number on every run, so it is unreproducible by construction, the same category
  as `Array.toString`. `<function1>` is Scala 2's rendering and at least names the
  arity. It becomes observable wherever a function reaches `println` directly,
  which now includes the eta-expansion of a bare argument placeholder
  (`println(xs.map(_))` prints the function `x => xs.map(x)`, not a list — see the
  placeholder rule below). Nothing DOWNSTREAM of a function value differs: only
  printing the function itself does. For the same reason no frozen corpus record
  prints one.
- **Types are not checked.** Declared types (`Int`, `String`, …) and type
  parameters (`class Box[A]`, `def id[A]`) are retained for diagnostics but do
  not gate execution — the runtime is dynamically typed on the fusevm value
  model. Type errors that `scalac` would reject may run. Related:
  `x.asInstanceOf[T]` is the identity, not a checked cast, so it never throws
  `ClassCastException` and never performs a numeric conversion. A class's
  `val`/private access modifiers are parsed but not enforced (every field is
  reachable).
- **A `String` combinator's result type is decided from the results, except on an
  empty receiver.** Scala reads `map`/`collect`'s `Char => Char` overload (which
  answers a `String`) apart from `Char => B` (which answers an `IndexedSeq`) off
  the function's static result type. `Char` is its own runtime type here, so the
  results themselves answer it exactly — `s.map(_.toUpper)` is a `String`,
  `s.map(_.toString)` and `s.map(c => c + "!")` are sequences. The one case with
  no results to read is an EMPTY receiver, where the decision falls back to the
  syntax of the body (`compiler::yields_chars` / `yields_strings`, carried on the
  closure next to `yields_pairs`): the element itself, a `Char` literal, or
  `toUpper`/`toLower`/`toChar` counts as `Char`-valued, and `flatMap` asks the
  `String` side instead. An empty receiver with a body outside those shapes —
  a call, a variable — takes the sequence branch, which is right for every `B`
  but `Char`. `filter`/`takeWhile`/`dropWhile`/`partition`/`span`/`sortWith`/
  `sortBy` are unaffected: they only select or reorder, so their result is
  always a `String`.
- **A non-local return is a `scala.runtime.NonLocalReturnControl`.** `case e:
  Throwable` catches it and `case e: Exception` does not, as in Scala, and
  rethrowing the caught value (`throw e`, which `Using` does) carries it on to
  its `def`; a rethrow used to stop the run as an uncaught exception.
- **`Array.toString` renders `Array(1, 2, 3)`.** Scala prints the JVM identity
  form (`[I@1b6d3586`), which no reimplementation can reproduce byte-for-byte,
  so the readable form is emitted instead. This is the one place a supported
  construct deliberately diverges; the parity fuzzer therefore never prints a
  bare `Array`.
- **A local `def` that assigns a binding it captures, inside a `def` body.** A
  block-local `def` is lambda-LIFTED (its captures become extra parameters), so
  a capture it ASSIGNS — directly, or by passing it on to another local `def`
  that does — is passed by REFERENCE: the call site hands over the enclosing
  `var`'s heap cell (marked `$ref(n)` by `crate::resolve`) and the lifted body
  reads and writes through it, as a lambda's capture already did. This covers
  a parameter DEFAULT that calls such a `def` (`def f(y: Int = d())`), which is
  spliced in at the call outside the body the boxing scan walks. Before, every
  one of these was refused at compile time ("a captured binding is read-only
  here") inside a `@main` or `def main` body, where the binding is a frame
  slot; under `extends App` the binding is a program global and always worked.
  A position the boxing scan still cannot see is refused with that message
  rather than losing the write.
- **`"abc".toSeq` is a `WrappedString`** (as are `toIndexedSeq` and a `String`
  passed where a `Seq[Char]` is declared): it prints as `abc`, a selecting op
  (`filter`, `reverse`, `take`) keeps it a `WrappedString`, and `map`/`zip`/
  `zipWithIndex` build a `Vector`. Comparing one with `==` against a `String`
  is a compile error in Scala 3.
- **A subclass constructor parameter that renames a superclass field.** Our
  parser makes every constructor parameter a field, so in `class D(n: String)
  extends A(f(n))` the superclass argument is stored under `A`'s parameter name
  and `D`'s body then sees that value under it. When the subclass forwards the
  parameter unchanged — `class D(n: String) extends A(n)`, the idiom — this is
  exactly Scala; when it transforms it, the subclass body reads the transformed
  value.
- **`==` on non-numbers compares by value** (structural), which matches Scala's
  `==`/`equals` for the strings and booleans this frontend handles. Reference
  identity (`eq`) is not modeled.
- **A `Char` is an interned heap value**, not a width-tagged integer. It compares
  and hashes by code point (so `'a' == 97` and a `Char` key lands in the same
  CHAMP slot its `Int` code point would), and arithmetic on one leaves the native
  integer fast path for the host's numeric hook — `Int` arithmetic is untouched.
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
- **A `Long` shift inside a `def` body truncates to 32 bits.** `<<`, `>>` and
  `>>>` are evaluated at the RECEIVER's width, and the host cannot tell an `Int`
  from a `Long` by value, so `compiler::method_inner` selects the 64-bit
  spelling only when `num_ty(recv)` PROVES `NumTy::Long` and otherwise emits the
  32-bit one. Everywhere else an unproven width suppresses narrowing; here it
  causes it. A `def` body starts from an empty width map
  (`std::mem::take(&mut self.widths)`), so a top-level `val` reachable by name
  from that body arrives unproven and its shift truncates:

  ```scala
  val big = 6679246017915255L
  def g(): Long = big >>> 11        // scala 3261350594685, scalars 313469
  def h(): Long = big << 2          // scala 26716984071661020,
                                    // scalars -1727027748
  ```

  The same expressions at the top level are correct, and so is a shift on a
  `def` PARAMETER (`def g(x: Long): Long = x >>> 11`), whose annotation Scala
  requires and which is therefore always proven. A silent wrong answer, and the
  only known place an unproven width narrows rather than declining to.

  Closing it means carrying the widths of program globals into a `def` frame
  rather than clearing the whole map, which widens what narrowing applies to
  across every `def` body — worth doing as its own change, not as a rider.
- **Integer arithmetic narrows to 32 bits only where the width is PROVEN.**
  `Int` overflow now wraps — `2147483647 + 1` is `-2147483648` — and a `Long`
  anywhere in an expression promotes it so it does not
  (`2147483647 + 1L` is `2147483648`). The runtime is still dynamically typed, so
  the decision is made statically by `compiler::NumTy`, which proves a width from
  a literal (`1` vs `1L`), a declared type (`val n: Long`, and a `def`
  parameter's mandatory annotation), an inferred `val` initializer, and the
  methods whose result type is fixed (`toInt`, `toLong`, `length`, `indexOf`, …).
  What it CANNOT prove stays 64-bit, because the wrap is emitted as a shift pair
  that would destroy a `Double` or a `String`. An enclosing scope's widths travel
  into a lambda body, so a captured accumulator still wraps
  (`var t = 0; xs.foreach(t += _)`). The positions Scala types from CONTEXT are
  modelled too: a **lambda parameter** takes the element width of what it
  traverses (`xs.map(x => x * 2147483647)` over a `List[Int]` wraps), a **class
  field** carries its declared width to a use site and to a bare reference inside
  a method, a `def`'s **return annotation** types its call site, and
  `.sum`/`.product` take the collection's element width — which is why
  `(1 to 100000).sum` answers `705082704`. A collection's element width comes
  from a literal's elements, a declared `List[Int]`, a range's endpoints, or an
  element-preserving combinator (`filter`, `take`, `sorted`, …).
  A `map`/`flatMap` result is typed by its function's BODY, read with the
  parameter bound to the receiver's element width (`(1 to 100000).map(_ *
  2).sum` wraps at 32 bits, `xs.map(_.toLong)` is a `List[Long]`); a body this
  cannot type (a block, an `if` of mixed widths) leaves the result unproven, so
  a later `sum`/`product` over it stays 64-bit. Measured cost: compiling the
  first 120 corpus programs, 3.75G instructions before and 3.70G–3.76G after
  (within run-to-run noise).
  A `def` with no return annotation is also unproven — Scala infers one from the
  body and this frontend does not.
- **The `%x`/`%X`/`%o` format conversions take their width from the static type
  only where the compiler can prove it.** Java writes the two's-complement bit
  pattern at the width of the operand's boxed type — `Byte` 8 bits, `Short` 16,
  `Int` 32 (`"%x".format(-1)` is `ffffffff`), `Long` 64 (`ffffffffffffffff`) —
  and that box also names the class in `format`'s argument check
  (`"%.2f".format(2L)` is `f != java.lang.Long`, `"%c".format(65L)` an
  `IllegalFormatConversionException` because `%c` takes no `Long`). The type
  travels with the argument for `"…".format(…)` and the `f` interpolator when
  it is a binding declared `Long`/`Short`/`Byte`, a `Long` literal or
  arithmetic, a `toLong`/`toShort`/`toByte` result, or a traversal element of a
  proven `List[Long]`. Anywhere else the value alone decides: the narrowest
  width that holds it, which is right for every `Int` and for any `Long` outside
  `Int` range, and `Integer` in an argument-check message. A collection
  or function argument to a numeric conversion is refused outright rather than
  named, since its runtime class (`scala.collection.immutable.$colon$colon`) is
  the private name `getClass` also refuses.
- **Widening to a declared `Double` covers the sites that carry a declared
  type, not a type the checker would infer.** `val`/`var`, parameters,
  constructor fields, a `def`'s result, field assignments and `copy` arguments
  convert an `Int`
  (or `Long`/`Char`) to `Double` as Scala does. Two expected types are not
  seen: a function type's result (`val f: Int => Double = x => x; f(2)` is `2`
  here, `2.0` in Scala), and a `return` from inside a LAMBDA in a `def` declared
  `Double` (the lambda is compiled apart from the `def`, so the value is returned
  unconverted: `def g(xs: List[Int]): Double = { xs.foreach(x => if (x > 1)
  return x); 0 }` answers `5` for `List(1, 5)` where Scala answers `5.0`). A
  `copy` on a receiver whose class is not known statically converts only when
  every `case class` declaring that parameter name agrees on its type.
- **`Map.view`, `mapValues` and `filterKeys` answer a strict `Map`, not a
  `MapView`.** Every member a program follows them with (`toMap`, `get`,
  `foreach`) answers what the forced view would, but PRINTING one directly
  diverges: `Map(1 -> 2).mapValues(_ + 1)` prints `Map(1 -> 3)` where Scala 3.9
  prints `MapView(<not computed>)`. A `MapView` needs a lazy map representation
  the heap model does not have.
- **`break`/`breakable` are recognized without their import.** Scala requires
  `import scala.util.control.Breaks._` (or the qualified spelling) before either
  name resolves; this frontend recognizes them in the parser, so a program that
  omits the import still runs here and is rejected by `scalac`. That is a
  superset, not a wrong answer — but it means the parity fuzzer's generated
  programs must emit the import, or the reference rejects the probe and the two
  sides agree on a failure that tests nothing.
- **A default value in a later parameter group cannot see the earlier group's
  parameters.** The groups are flattened into one list, and a default is
  spliced at the call site from that flat list, so `def f(a: Int)(b: Int = a)`
  does not resolve `a`. Scala allows it across groups; it forbids it *within*
  one group, which is what makes call-site splicing exact everywhere else.

  Partial application across the groups is NOT a gap: the flattened list keeps
  each group's boundary, so `def add(a: Int)(b: Int)` applied to one argument
  answers `b => add(a, b)` and `def add3(a: Int)(b: Int)(c: Int)` applied to one
  answers a function that answers another. A call that stops INSIDE a group is
  still a missing argument, as Scala has it.
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
  assignment yields `null` instead of a compile error. `Unit` no longer shares
  that value — it is the empty tuple the `()` literal already lowered to, so
  `println(println("x"))` and `println(xs.foreach(f))` render `()` while `null`
  keeps rendering `null`.
- **`x += e` chooses the growable method at run time, not from a static type.**
  Scala calls `x.+=(e)` when the receiver's type has that method and falls back
  to `x = x + e` otherwise. There are no static types here, so a program that
  builds a mutable collection *anywhere* emits a one-op test
  ([`crate::host::IS_GROWABLE`]) that picks the branch on the receiver's runtime
  kind. A program with no mutable collection in it emits exactly the arithmetic
  it did before, so a counted `while` loop stays JIT-trace-eligible.
- **`object … extends App` runs the object body directly.** Statements run in
  order, any `def` members are hoisted and callable, and a **nested type** —
  `class`, `trait`, `case class`, `case object`, `object` — is a member, exactly
  as it is in Scala: `object T extends App { class C(var n: Int); … }` compiles.
  Declaring the type beside the object is a placement, not a requirement, and
  the same declarations are accepted in any block (a `def` body, a brace group).
  Members are not ordered the way statements are, so a member `class` may be
  declared after the statement that constructs it.
- **A type declaration lands in ONE flat namespace, so it cannot be shadowed.**
  Scala scopes same-named types — a member `class Q` shadows a top-level `Q`,
  and `def a() = { case class Q(v: Int); … }` / `def b() = { case class Q(v:
  Int, w: Int); … }` declare two different `Q`s. There is one table here, so the
  second declaration would silently replace the first and every `Q` in the
  program would mean whichever won. That is refused
  (``type `Q` is already declared``) rather than run. A `class Q` beside an
  `object Q` is the companion idiom, not a redeclaration, and still compiles:
  the object is kept under the JVM's name for it, `Q$`, so its members and the
  class's never share a subroutine, `Q.m` reaches the object, and a bare
  `Q(…)` calls the companion's `apply` when it declares one.
- **A type declared inside a `def` body cannot capture that frame's locals.**
  `def f(k: Int) = { class C(val n: Int) { def g = n + k }; new C(1).g }` is
  legal Scala and aborts here: the class is modelled as a top-level one, so `k`
  is simply an unresolved name and fails the way any unresolved name in a class
  body fails (``+` is not defined between `1` and `null``). The `App` body's own
  `val`s are program globals, so a member class DOES read those
  (`val k = 10; class C(val n: Int) { def g = n + k }` answers 11) — it is the
  `def`/lambda frames that a class body cannot reach into. Closing it means
  lambda-lifting captures into constructor fields, the way [`crate::resolve`]
  already lifts them for a nested `def`.
- **A nested `object` that is itself an entry point is refused.**
  `object T extends App { object U extends App { … } }` and the `def main`
  spelling of the same both exit with ``nested entry object `U```, where Scala
  compiles them (and never runs `U`'s body unless something touches `U`). A
  refusal, not a wrong answer.
- **A singleton `object` whose body holds only literal `val`s initializes
  before `main`**; every other one initializes on first access, as in Scala
  (`Obj$clinit`, guarded by an `Obj$init` flag at each access from outside the
  object). The two are indistinguishable: a literal initializer has no effect
  and reads nothing. Its body's non-`val` statements — which the parser used to
  skip, so `object O { println("init") }` printed nothing — run with it; a
  forward or cyclic reference reads the field's type default (`0`, `0.0`,
  `false`, NUL, `null`); a throwing initializer raises
  `ExceptionInInitializerError` (an `Error` passes through unwrapped) and every
  later access `NoClassDefFoundError: Could not initialize class O$`. A
  statement the parser has no statement form for is still skipped.
- **A METHOD VALUE needs a receiver whose class can be named, and a method that
  declares one arity.** `xs.map(o.f)` eta-expands to `($eta0) => o.f($eta0)`,
  with the receiver evaluated once where the value is written. Three receivers
  do not reach it, all refusals rather than wrong answers. A BUILT-IN receiver's
  method arities live in the host's dispatch rather than in a table, so
  `xs.map(s.charAt)` reports `String.charAt: wrong number of arguments`. A LAMBDA
  PARAMETER has no static class here — the width analysis carries an element's
  numeric type but not its class — so `ts.map(t => xs.map(t.f))` over a
  `List[Tr]` reports that the receiver is not a number. And an OVERLOADED name
  is declined outright: which of its arities a method value denotes is decided
  by the expected function type, which this frontend does not model. The
  explicit `xs.map(t.f(_))` spelling works in every one of those cases.
- **A given is resolved by the TEXT of its declared type, so inference reaches
  one level.** A type parameter is bound from a value parameter declared to be
  exactly that parameter (`def show[A](x: A)(using Sh[A])`), or from one declared
  `C[A]` whose argument is a collection LITERAL, whose first element names the
  type (`def chain[A](xs: List[A])(using Sh[A])` at `chain(List(1, 2, 3))`).
  Nothing else: not a two-parameter constructor, not a nested one, not a return
  position, and not a `C[A]` argument that is a variable rather than a literal.
  An unresolved clause is left to the arity path.

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

## Block-local `def`s and block-scoped `val`s: how they work

The compiler consumes a flat namespace — one function table, and one storage slot
per name per frame — so `src/resolve.rs` sits between the parser and the compiler
and gives both halves of that namespace their unique names.

1. The AST is walked with a scope stack. Each block pre-binds its own `def`s
   (giving forward references and mutual recursion inside one block), and every
   `Var`/`Call` is rewritten to the unique global name of whichever `def` it
   actually resolves to. `val`s and parameters shadow an outer `def` from their
   declaration onward.
2. **Value bindings are renamed by the same walk, for the same reason.** A `val`,
   `var`, pattern binder, `for` generator or destructured name that re-uses a name
   an enclosing scope already bound is given a fresh one, and every read of it in
   its scope is rewritten to match. Without that step the compiler's
   `declare_place` handed the inner declaration the OUTER binding's slot back —
   the two spellings were one storage location — so the inner binding did not
   shadow the outer one, it overwrote it, and the outer value was gone for the
   rest of the frame. That was wrong for every scope that introduces a binding:
   a brace block, an `if` branch, a `for` or `while` body, a `match` or `catch`
   arm's binder, a for-comprehension's `val` clause, a destructuring `val (a, b)`.
   The `catch` case did not merely answer a stale value — it leaked the THROWABLE
   into the outer name.
3. Each hoisted `def` gets the enclosing-frame locals its body reads appended as
   extra parameters, and every call site passes them; capture sets propagate
   through calls to a fixpoint, so a local `def` that calls a sibling capturing
   `k` threads `k` too.

Renaming is what makes a *shadow* distinguishable from a *write*, so it is also
what decides which name a later write reaches: `var a = 5; { val a = 100 }; a = 7`
assigns the outer binding, and before the rename the frontend rejected that
program outright as a "reassignment to val".

The same walk records which of the renamed bindings were declared `val`, because
that is what decides whether a compound assignment through one is a write at all.
`buf += x` on a `val` is not a rebinding — Scala rejects a write to a `val`, so
the only reading that compiles is the growable **member** call (SLS 6.12.4), and
that is what the compiler emits. Counting it as an assignment made a local `def`
that merely appended to a captured `ListBuffer` fail with "a captured binding is
read-only here", which refused a program Scala runs. A compound assignment
through a `var` is still a write: it makes the capture travel by reference.

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

Stack traces (`getStackTrace`, `printStackTrace`) and `initCause` are not
modelled; `getCause`, `addSuppressed`/`getSuppressed`, `case NonFatal(e)` and
`Try`/`Success`/`Failure` are.

## What a clean parity-fuzz run does not prove

The `parity-fuzz` binary diffs generated programs against a real `scala`. It is
the main evidence that this frontend is faithful, so it is worth being precise
about the ways a green run has been wrong.

### What each harness is structurally unable to report

Below each is what it CANNOT see — not a gap in how many probes have run, but a
class of divergence the harness has no way to express. The first column of every
entry is the constant that hides the axis.

| Harness | Blind to |
| --- | --- |
| `parity-fuzz` generators | Non-ASCII source. Every literal any generator emits is ASCII, so the lexer's byte-offset operator lookahead splitting a multi-byte character (`List("a", "é")` PANICKED) was unreachable from any seed. |
| `parity-fuzz` `differs` | The exact exit code (only zero-vs-nonzero), all of stderr, and therefore *which* exception went uncaught: two runs that print the same bytes and both die compare equal whatever killed them. |
| `parity-fuzz` `diverges` | An oracle that hangs. A timeout on the reference side is excused as pathology, so a construct that makes real Scala loop forever can never be reported. |
| `parity-fuzz` `build_program` | Anything that differs by ENTRY SHAPE, while the shape was a constant. `--entry` makes it an axis; before it, the by-name-thunk write below could not be seen. |
| `tests/parity.rs` | Any program that must FAIL — every record asserts success — plus stderr, the exit code, and program arguments. A record is one line, so no program containing a TAB or a real newline is expressible. |
| `tests/mixed_numeric.rs` | Whether the compiler ever ROUTES to `numeric_hook`; it calls the hook directly. It was also blind to `Div`'s value until this round: `Div` appeared only in the never-rejected test, which asserts the answer is a `Float` and not which one. |
| `tests/eval.rs` | The same entry-shape constant — `wrap()` is `object T extends App` — and, for the 350-odd tests that use `run` rather than `run_full`, the whole of stderr. |
| oracle gates | Whether an expectation was ever CAPTURED. Gating the reference's JVM version and locale says which Scala answered; it says nothing about a frozen string no Scala ever printed. Only re-running the program answers that. |

The `Div` gap and the entry-shape gap are closed (twelve `div` records; `--entry
app|main|mainsig`). The non-ASCII gap is closed in the frontend and pinned by a
test, though the generators are still ASCII-only. The rest stand as stated.

**A debug-only fault can wrap into agreement under `--release`.** The host used
to fold an integral `sum`/`product` with `Iterator::sum`, whose `+` is the
checked one. `List(Long.MaxValue, 1L).sum` therefore *raised* in a debug build,
where Scala answers `-9223372036854775808`. The same expression under
`--release` wraps silently and agrees — so the bug was visible only in the
profile nobody ships, and a release-mode fuzz run would have scored it clean
forever. Any arithmetic the host performs in Rust must say which overflow
behaviour it means (`wrapping_add`, `checked_*`) rather than inherit it from the
profile; where it does not, a green run is evidence about the profile, not about
the frontend.

**A mode only covers the shapes it generates.** The `overflow` mode exercises
widths, not name resolution, so it cannot see a regression in
`could_be_user_instance` — that needs a program declaring a member whose name
collides with a stdlib one (`class Thing { def abs: Int }`) *and* a receiver
that is neither a literal nor a plain variable. `(-2147483648 + 0).abs`
answers `-2147483648` in Scala; a build that drops the proven-width exemption
answers `2147483648` and still scores a clean `overflow` run across thousands of
programs. Do not read a clean run in one mode as coverage of another.

**A run the machine broke is not a run.** `run_prog` reports `exit == -1` for
its own failures — it could not write the temp file, or could not spawn the
binary — which a full disk causes in bulk. Those are counted and reported
separately (`harness-err`), excused from the divergence count, and once they
exceed a tenth of the run the harness exits 2 rather than green, because those
programs were never compared. An *oracle* exiting non-zero is deliberately NOT
excused: "scala rejects a program we accept" is a real divergence.

**An oracle that disagrees with itself is not an oracle.** The reference `scala`
is a launcher script, and two things outside the repository decide what it
answers. `JAVA_HOME` picks the JVM, and `Double.toString` was reimplemented in
JDK 19 (JDK-4511638) — Corretto 17.0.4.1 prints `1.0e23` as `9.999999999999999E22`
where OpenJDK 21.0.12 and 26.0.2 print `1.0E23`, which is one of the axes this
fuzzer is most biased toward. `LANG`/`LC_ALL` pick the default locale, which
decides the decimal separator and the grouping separator of every `%f`/`%e`/`%,d`
conversion and the case-mapping rules of `toUpperCase`. `resolve_oracle` probes
both and exits 2 rather than comparing against a reference that would make a
whole class of "divergences" spurious.

Neither gate answers the question one layer above them: **whether a frozen
expectation was ever captured at all.** A pin that no version and no locale of
the reference ever printed passes every gate, because the gates check WHICH
Scala answered rather than WHETHER one did. The only test is re-running the
program. Every record in `tests/data/mixed_numeric_expected.txt` was
regenerated from Scala 3.8.4 on JDK 26.0.2 this round and reproduced
byte-for-byte, and every exception message pinned in `tests/eval.rs` was re-run
against the same pair.

## A user member whose name collides with a stdlib one

`method_width` refuses to narrow a receiver's width when any user class
declares a member sharing the name (`class Thing { def abs: Int = 5 }` makes
every `abs` suspect), because a stdlib width rule must not outrank a user
declaration. Receivers that are literals, or whose numeric width is already
proven, are exempt. Two costs of that rule are unfixed:

**A receiver of unproven width answers the unnarrowed result.** The exemption
is driven by `num_ty`, so a receiver it cannot prove — an `if`, for instance —
stays poisoned and the result is not narrowed to 32 bits:

    class Thing { def abs: Int = 5 }
    println((if (true) -2147483648 else 0).abs)   // scala: -2147483648
                                                  // scalars: 2147483648

The instinct here is that skipping a narrow is the conservative choice. It is
not. **Declining to narrow is not the safe direction, because the `Int`
boundary is exactly where the widths disagree** — `abs` is the operation whose
answer differs between 32 and 64 bits, so refusing to commit to a width does
not abstain, it silently picks the 64-bit one. `(-2147483648).abs` and `x.abs`
for a proven `x` are both correct; only the unproven receiver is wrong.

**Compile time is exponential in the length of the method chain.** Deciding the
exemption calls `num_ty` on the receiver, which re-enters `method_width` for a
method-call receiver, which asks about *its* receiver in turn — so an `n`-link
chain is walked about `2^n` times. Measured on a chain of `x.abs.abs…` with
`Thing` declared:

    depth 16   0.15s
    depth 18   0.35s
    depth 20   2.22s
    depth 22   7.76s
    depth 26   136s

Roughly a doubling per link. Programs without a colliding member name are
unaffected — the name is tested first and answers `false` before any receiver
is walked — so this needs both a collision and a deep chain. The fix is to
memoise the width walk or to prove the common receiver shapes without
re-entering `num_ty`; simply treating every non-literal receiver as unprovable
is NOT a fix, because it produces the wrong answer above for `(-2147483648 + 0).abs`
as well.

## Operational: a fuzz number from a shared checkout is not trustworthy

Several agent sessions work this repo at once, against ONE working copy and one
`target/`. `parity-fuzz` spawns `target/debug/scala` fresh for every program, so
it reads that path thousands of times over a run lasting tens of minutes. Any
`cargo build` in any other session — including one that only asks for a
different binary, since the library is shared — relinks `target/debug/scala`
underneath a run already in progress.

The failure is silent, which is what makes it worse than a crash. The peer's
run does not error; it simply compares its first N programs against one binary
and its remaining programs against another, then prints a single clean score
for both halves. Nothing in the output says which binary produced which result,
and a divergence introduced by the newer code reads as a divergence in the
older. This session did it: a 4000-program run was measuring a build that was
replaced mid-run, and its partial number had to be discarded rather than
reported.

So: **a clean fuzz number is evidence only if no other session built during it.**
Before quoting one, confirm the binary's mtime predates the run's start. Before
building, check whether a peer has a run in flight (`pgrep -f parity-fuzz`) —
and if one does, either wait or accept that you are invalidating their result.

An untaken mitigation, recorded rather than implemented: have the harness copy
the frontend to a pid-suffixed path once at startup and spawn that copy, the way
scratch files are already namespaced per pid. The run then holds its own
immutable binary and a peer's rebuild cannot reach it. It costs one file copy
per run and would make the number self-contained; it has NOT been done, so today
the discipline above is the only protection.
