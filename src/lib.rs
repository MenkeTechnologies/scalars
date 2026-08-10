//! scalars — Scala as a fusevm frontend.
//!
//! Pipeline: `lexer` → `parser` builds a Scala AST → `compiler` lowers it to a
//! `fusevm::Chunk` → fusevm executes it on the shared three-tier Cranelift JIT,
//! calling back into `host` (the strict numeric hook) for Scala's `String` `+`
//! overload. There is no bespoke VM or JVM here — execution and codegen live in
//! fusevm, the same engine behind zshrs, stryke, awkrs, elisp, and ruby.

pub mod ast;
pub mod banner;
pub mod cli;
pub mod compiler;
pub mod corpus;
pub mod dap;
pub mod host;
pub mod lexer;
pub mod lsp;
pub mod parser;
pub mod resolve;
pub mod rust_ffi;
pub mod tiers;

pub use banner::version_banner;
use fusevm::{VMResult, Value, VM};

/// Parse Scala `src` to an AST.
pub fn parse(src: &str) -> Result<ast::Program, String> {
    parser::parse(src)
}

/// Parse and lower Scala `src` to a runnable fusevm chunk.
pub fn compile(src: &str) -> Result<fusevm::Chunk, String> {
    let prog = parser::parse(src)?;
    compiler::compile(&prog)
}

/// Register the scalars builtins + strict numeric hook on a fresh VM, enable the
/// tracing JIT, and run the chunk. Returns the last top-of-stack value.
fn run_chunk(chunk: fusevm::Chunk) -> Result<Value, String> {
    let mut vm = VM::new(chunk);
    host::install(&mut vm);
    vm.set_numeric_hook(std::sync::Arc::new(host::numeric_hook));
    vm.enable_tracing_jit();
    host::reset_heap(); // start each run with an empty object arena
    host::reset_types(); // and with an empty declared-type registry
    host::reset_exceptions(); // and with no exception left in flight
    let _ = host::take_error(); // clear any stale FFI fault from a prior run
    let res = vm.run();
    // An FFI compile/dispatch fault halts the VM and parks its message; surface
    // it as an error rather than the (Undef) top of stack.
    if let Some(err) = host::take_error() {
        return Err(err);
    }
    match res {
        VMResult::Ok(v) => Ok(v),
        VMResult::Halted => Ok(vm.stack.last().cloned().unwrap_or(Value::Undef)),
        VMResult::Error(e) => Err(e),
    }
}

/// Compile and run a Scala source string; return the last VM value.
pub fn run_str(src: &str) -> Result<Value, String> {
    run_chunk(compile(src)?)
}

/// Parse and lower Scala `src` with per-statement `DBG_LINE` markers (for
/// `scala --dap`).
pub fn compile_debug(src: &str) -> Result<fusevm::Chunk, String> {
    let prog = parser::parse(src)?;
    compiler::compile_debug(&prog)
}

/// The `DBG_LINE` marker builtin: hand control to the DAP adapter, which pauses
/// on breakpoints/steps, then return `Unit`.
fn dbg_line_builtin(vm: &mut VM, _argc: u8) -> Value {
    dap::on_debug_line(vm);
    Value::Undef
}

/// Run a debug-compiled chunk: install the host builtins and the `DBG_LINE`
/// marker builtin, but do NOT enable the tracing JIT (it would compile hot loops
/// and skip the markers). Used only by `scala --dap`.
fn run_chunk_debug(chunk: fusevm::Chunk) -> Result<Value, String> {
    let mut vm = VM::new(chunk);
    host::install(&mut vm);
    vm.register_builtin(host::DBG_LINE, dbg_line_builtin);
    vm.set_numeric_hook(std::sync::Arc::new(host::numeric_hook));
    host::reset_heap(); // start each run with an empty object arena
    host::reset_types(); // and with an empty declared-type registry
    host::reset_exceptions(); // and with no exception left in flight
    match vm.run() {
        VMResult::Ok(v) => Ok(v),
        VMResult::Halted => Ok(vm.stack.last().cloned().unwrap_or(Value::Undef)),
        VMResult::Error(e) => Err(e),
    }
}

/// Read and run a `.scala` file under the debug marker path (for `scala --dap`).
pub fn eval_file_debug(path: &str) -> Result<(), String> {
    let src =
        std::fs::read_to_string(path).map_err(|e| format!("scalars: cannot read {path}: {e}"))?;
    run_chunk_debug(compile_debug(&src)?)?;
    Ok(())
}

/// Read and run a `.scala` file.
pub fn run_file(path: &str) -> Result<Value, String> {
    let src =
        std::fs::read_to_string(path).map_err(|e| format!("scalars: cannot read {path}: {e}"))?;
    run_str(&src)
}

/// Run a Scala source string with the program arguments a `def main`'s `args`
/// parameter and a `@main` method's parameters read from.
pub fn run_str_with_args(src: &str, argv: Vec<String>) -> Result<Value, String> {
    host::set_argv(argv);
    run_str(src)
}

/// Compile `src` and return a human-readable disassembly of the fusevm chunk
/// (for `scala --disasm`).
pub fn disassemble(src: &str) -> Result<String, String> {
    Ok(compile(src)?.disassemble())
}
