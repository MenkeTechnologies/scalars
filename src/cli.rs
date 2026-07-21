//! Command-line parsing for the `scala` binary.
//!
//! Slice 1 accepts a single-file invocation plus a small set of introspection
//! flags. The `scala` runner's full option grammar (`-classpath`, `-cp`,
//! `-D…`, REPL) grows in later waves; unknown options error rather than being
//! silently ignored.

/// Parsed command line.
#[derive(Debug, Default)]
pub struct Cli {
    /// The `.scala` file to run, if any.
    pub file: Option<String>,
    /// Program arguments after the file (become `args` — unused in slice 1).
    pub argv: Vec<String>,
    pub show_version: bool,
    pub show_help: bool,
    /// `--dump-tokens FILE` — print the lexer token stream and exit.
    pub dump_tokens: bool,
    /// `--dump-ast FILE` — print the parsed AST and exit.
    pub dump_ast: bool,
    /// `--disasm FILE` — print the lowered fusevm bytecode and exit.
    pub disasm: bool,
    /// `--lsp` — speak the Language Server Protocol over stdio.
    pub lsp: bool,
    /// `--dap` — speak the Debug Adapter Protocol over stdio.
    pub dap: bool,
}

/// Parse process args (excluding argv[0]).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
    let mut cli = Cli::default();
    for a in args {
        match a.as_str() {
            "--version" | "-version" => cli.show_version = true,
            "--help" | "-h" | "-help" | "-?" => cli.show_help = true,
            "--dump-tokens" => cli.dump_tokens = true,
            "--dump-ast" => cli.dump_ast = true,
            "--disasm" => cli.disasm = true,
            "--lsp" => cli.lsp = true,
            "--dap" => cli.dap = true,
            _ if a.starts_with('-') && cli.file.is_none() => {
                return Err(format!("scalars: unrecognized option `{a}`"))
            }
            _ => {
                if cli.file.is_none() {
                    cli.file = Some(a);
                } else {
                    cli.argv.push(a);
                }
            }
        }
    }
    Ok(cli)
}

/// `scala --help` text.
pub const USAGE: &str = "\
usage: scala [options] <file.scala> [args...]

options:
  -version, --version   print the version banner and exit
  -h, --help            print this help and exit
  --dump-tokens FILE    print the lexer token stream and exit
  --dump-ast FILE       print the parsed AST and exit
  --disasm FILE         print the lowered fusevm bytecode and exit
  --lsp                 speak the Language Server Protocol over stdio
  --dap                 speak the Debug Adapter Protocol over stdio
";
