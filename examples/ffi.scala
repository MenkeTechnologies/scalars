// Inline Rust FFI: a `rust { ... }` block is compiled to a cdylib and its
// `extern "C"` exports are called from Scala by name. Run: `scala examples/ffi.scala`
object Ffi {
  def main(args: Array[String]): Unit = {
    rust { pub extern "C" fn scala_triple(x: i64) -> i64 { x * 3 } }
    println(scala_triple(14))
  }
}
