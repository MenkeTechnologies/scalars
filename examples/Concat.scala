// `object … extends App` — the body runs directly, no explicit main.
object Concat extends App {
  val x = 7
  val y = 6
  println("x * y = " + (x * y))
  val ok = x > 0 && y > 0
  println("both positive: " + ok)
}
