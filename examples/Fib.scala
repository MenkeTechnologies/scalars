// Iterative Fibonacci — compiled to fusevm bytecode, hot loop trace-JITed.
object Fib {
  def main(args: Array[String]): Unit = {
    var a = 0
    var b = 1
    for (i <- 0 until 10) {
      println(a)
      val next = a + b
      a = b
      b = next
    }
  }
}
