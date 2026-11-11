//! Embedding entry point: run a Scala program from Rust with a given stdin and
//! get its stdout back as a string.

use crate::host;

/// Stack for the interpreter thread each embedded run gets — the same
/// reservation the `scala` binary runs programs on, since closures and user
/// `toString`/`equals` re-enter the VM and nest Rust frames per Scala call.
const INTERPRETER_STACK: usize = 1 << 30;

/// Run `src` as a complete program with `input` as its stdin and return
/// everything it wrote to stdout.
///
/// Each call runs on its own interpreter thread, so heap, type registry and
/// exception state start fresh and concurrent calls never share an output
/// buffer. `scala.io.StdIn` reads `input` line by line; `println` / `print` /
/// `printf` and their `Console` / `System.out` spellings append to the
/// returned string and never reach the process stdout.
///
/// Returns `Err` for a parse or compile error, an uncaught exception, a
/// nonzero `sys.exit` / `System.exit`, or an internal panic. `sys.exit(0)`
/// returns `Ok` with the output written up to that point.
pub fn run_program(src: &str, input: &str) -> Result<String, String> {
    let (src, input) = (src.to_string(), input.to_string());
    std::thread::Builder::new()
        .name("scala".to_string())
        .stack_size(INTERPRETER_STACK)
        .spawn(move || run_captured(&src, &input))
        .map_err(|e| format!("scalars: cannot start the interpreter thread: {e}"))?
        .join()
        .unwrap_or_else(|_| Err("scalars: internal error: the interpreter panicked".to_string()))
}

/// The whole embedded run on the current thread: capture on, run, capture off.
fn run_captured(src: &str, input: &str) -> Result<String, String> {
    host::begin_embedded_io(input);
    let result = crate::run_str(src);
    let output = host::end_embedded_io();
    result?;
    match host::exit_code() {
        None | Some(0) => Ok(output),
        Some(code) => Err(format!("scalars: exit status {code}")),
    }
}
