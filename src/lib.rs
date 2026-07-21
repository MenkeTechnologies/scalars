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
pub mod host;
pub mod lexer;
pub mod parser;

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
    match vm.run() {
        VMResult::Ok(v) => Ok(v),
        VMResult::Halted => Ok(vm.stack.last().cloned().unwrap_or(Value::Undef)),
        VMResult::Error(e) => Err(e),
    }
}

/// Compile and run a Scala source string; return the last VM value.
pub fn run_str(src: &str) -> Result<Value, String> {
    run_chunk(compile(src)?)
}

/// Read and run a `.scala` file.
pub fn run_file(path: &str) -> Result<Value, String> {
    let src =
        std::fs::read_to_string(path).map_err(|e| format!("scalars: cannot read {path}: {e}"))?;
    run_str(&src)
}

/// Compile `src` and return a human-readable disassembly of the fusevm chunk
/// (for `scala --disasm`).
pub fn disassemble(src: &str) -> Result<String, String> {
    Ok(compile(src)?.disassemble())
}
