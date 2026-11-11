//! `scalars::run_program`: stdout capture, stdin feeding, error reporting, and
//! isolation between concurrent runs.

use scalars::run_program;

#[test]
fn captures_every_stdout_spelling() {
    let src = r#"
object Main {
  def main(args: Array[String]): Unit = {
    println("hello")
    print(1 + 2)
    printf(" %s%n", "x")
    Console.println("console")
    System.out.println("system")
    System.out.printf("%d!%n", 7)
  }
}"#;
    assert_eq!(
        run_program(src, "").unwrap(),
        "hello\n3 x\nconsole\nsystem\n7!\n"
    );
}

#[test]
fn input_is_stdin_for_scala_io_std_in() {
    let src = r#"
import scala.io.StdIn
object Main {
  def main(args: Array[String]): Unit = {
    val n = StdIn.readInt()
    var sum = 0L
    for (_ <- 1 to n) sum += StdIn.readLong()
    println(s"sum=$sum")
    println(StdIn.readLine().toUpperCase)
    println(StdIn.readBoolean())
    println(StdIn.readLine() == null)
  }
}"#;
    assert_eq!(
        run_program(src, "2\n40\n2\nshout\nYes\n").unwrap(),
        "sum=42\nSHOUT\ntrue\ntrue\n"
    );

    let echo = r#"
import scala.io.StdIn.readLine
@main def run(): Unit =
  var line = readLine()
  while (line != null) { println("> " + line); line = readLine() }
"#;
    assert_eq!(run_program(echo, "a\r\nb").unwrap(), "> a\n> b\n");
}

#[test]
fn errors_and_exit_status() {
    let throws = r#"
object Main {
  def main(args: Array[String]): Unit = {
    println("before")
    throw new IllegalStateException("boom")
  }
}"#;
    assert!(run_program(throws, "").unwrap_err().contains("boom"));

    // `readInt` is `readLine().toInt`: past the end of input it is an
    // `EOFException`, and a padded number does not parse.
    let read =
        "object Main { def main(a: Array[String]): Unit = println(scala.io.StdIn.readInt()) }";
    assert!(run_program(read, "")
        .unwrap_err()
        .contains("java.io.EOFException"));
    assert!(run_program(read, " 5\n")
        .unwrap_err()
        .contains("NumberFormatException"));

    assert!(run_program("object Main { def main( }", "").is_err());

    let exit = |code: i32| {
        format!(
            "object Main {{ def main(a: Array[String]): Unit = {{ \
             println(\"out\"); sys.exit({code}); println(\"never\") }} }}"
        )
    };
    assert_eq!(run_program(&exit(0), "").unwrap(), "out\n");
    assert!(run_program(&exit(3), "").unwrap_err().contains('3'));
}

#[test]
fn concurrent_runs_keep_their_own_output_and_input() {
    let src = r#"
import scala.io.StdIn
object Main {
  var count = 0
  def main(args: Array[String]): Unit = {
    val tag = StdIn.readLine()
    for (i <- 0 until 2000) { count += 1; println(tag + i) }
    println(count)
  }
}"#;
    let handles: Vec<_> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|tag| std::thread::spawn(move || (tag, run_program(src, tag).unwrap())))
        .collect();
    for h in handles {
        let (tag, out) = h.join().unwrap();
        let want: String = (0..2000).map(|i| format!("{tag}{i}\n")).collect::<String>() + "2000\n";
        assert_eq!(out, want, "run {tag}");
    }
}
