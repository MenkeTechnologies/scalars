//! Debug Adapter Protocol over stdio (`scala --dap`).
//!
//! A single-threaded source-line debugger. The program is compiled with
//! per-statement line markers (`Op::CallBuiltin(DBG_LINE, 0)`, emitted only in
//! this mode — normal runs carry zero extra ops) and run on the pure interpreter
//! (the tracing JIT would compile hot loops and skip the markers, so `--dap`
//! runs with `crate::run_chunk_debug`, which registers the marker builtin and
//! never calls `enable_tracing_jit`). The `DBG_LINE` builtin fires synchronously
//! at each marker; when it lands on a breakpoint or a step target it pauses IN
//! PLACE and services DAP requests (`stackTrace`/`scopes`/`variables`/`continue`/
//! `next`/`stepIn`/`stepOut`) from stdin until a resume command, then returns
//! control to the VM.
//!
//! scalars has a single entry frame (no user methods yet), so `stepIn`/`next`/
//! `stepOut` all advance to the next statement marker — the load-bearing
//! features (breakpoints and single-stepping through the `main` body, including
//! `if`/`while`/`for` bodies) work. Program stdout is redirected to a pipe during
//! the run and forwarded as `output` events, so `println`/`print` never corrupt
//! the JSON protocol channel on the saved stdout fd.

use serde_json::{json, Value as J};
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};

use fusevm::{Op, VM};

/// How the debuggee should proceed from a stop.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Continue,
    Step,
}

struct DebugState {
    breakpoints: HashSet<u32>,
    /// Lines that actually carry a marker (so a breakpoint on them can fire).
    verified: HashSet<u32>,
    mode: Mode,
    /// The source line of the current stop (for the stack frame).
    cur_line: u32,
    /// Real stdout, saved before the program's stdout is redirected to a pipe;
    /// all DAP protocol is written here.
    proto_fd: RawFd,
    /// Read end of the program-stdout pipe (non-blocking), drained into `output`
    /// events. `-1` until `launch` sets it up.
    pipe_r: RawFd,
    /// Source path reported in stack frames.
    program: String,
    seq: i64,
    /// True once `launch` has redirected stdout and the debuggee is running.
    active: bool,
}

thread_local! {
    static DBG: RefCell<DebugState> = RefCell::new(DebugState {
        breakpoints: HashSet::new(),
        verified: HashSet::new(),
        mode: Mode::Continue,
        cur_line: 0,
        proto_fd: 1,
        pipe_r: -1,
        program: String::new(),
        seq: 1,
        active: false,
    });
}

/// Entry point for `scala --dap`.
pub fn run() -> Result<(), String> {
    // Save the real stdout up front; all DAP protocol goes here even after the
    // program's stdout is redirected to a pipe during `launch`.
    let proto = unsafe { libc::dup(1) };
    DBG.with(|d| d.borrow_mut().proto_fd = proto);

    let mut input = std::io::stdin();
    while let Some(msg) = read_message(&mut input)? {
        let command = msg.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let req_seq = msg.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
        match command {
            "initialize" => {
                respond(
                    req_seq,
                    command,
                    json!({
                        "supportsConfigurationDoneRequest": true,
                        "supportsEvaluateForHovers": true,
                        "supportsTerminateRequest": true,
                    }),
                );
                event("initialized", json!({}));
            }
            "setBreakpoints" => set_breakpoints(&msg, req_seq),
            "setExceptionBreakpoints" => {
                // Accepted so clients that always send it proceed; the
                // single-threaded adapter does not stop on exceptions.
                respond(req_seq, command, json!({ "breakpoints": [] }));
            }
            "evaluate" => {
                respond(
                    req_seq,
                    command,
                    json!({ "result": "", "variablesReference": 0 }),
                );
            }
            "pause" => respond(req_seq, command, json!({})),
            "configurationDone" => respond(req_seq, command, json!({})),
            "threads" => respond(
                req_seq,
                command,
                json!({ "threads": [{ "id": 1, "name": "main" }] }),
            ),
            "launch" => {
                let program = msg
                    .get("arguments")
                    .and_then(|a| a.get("program"))
                    .and_then(|p| p.as_str())
                    .unwrap_or("")
                    .to_string();
                respond(req_seq, command, json!({}));
                launch(&program);
            }
            "disconnect" | "terminate" => {
                respond(req_seq, command, json!({}));
                break;
            }
            _ => respond(req_seq, command, json!({})),
        }
    }
    unsafe {
        libc::close(proto);
    }
    Ok(())
}

/// `setBreakpoints`: store the requested lines and report each verified only if
/// the program actually emits a marker on that line (a blank/comment line with
/// no compiled statement is reported unverified — a breakpoint there would never
/// fire).
fn set_breakpoints(msg: &J, req_seq: i64) {
    let path = msg
        .get("arguments")
        .and_then(|a| a.get("source"))
        .and_then(|s| s.get("path"))
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    let lines: Vec<u32> = msg
        .get("arguments")
        .and_then(|a| a.get("breakpoints"))
        .and_then(|b| b.as_array())
        .map(|bps| {
            bps.iter()
                .filter_map(|b| b.get("line").and_then(|l| l.as_u64()).map(|l| l as u32))
                .collect()
        })
        .unwrap_or_default();

    let markers = marker_lines(&path);
    DBG.with(|d| {
        let mut s = d.borrow_mut();
        if !path.is_empty() {
            s.program = path;
        }
        s.breakpoints = lines.iter().copied().collect();
        s.verified = markers;
    });
    let bps: Vec<J> = DBG.with(|d| {
        let s = d.borrow();
        lines
            .iter()
            .map(|l| json!({ "verified": s.verified.contains(l), "line": l }))
            .collect()
    });
    respond(req_seq, "setBreakpoints", json!({ "breakpoints": bps }));
}

/// The set of source lines that carry a `DBG_LINE` marker in the compiled
/// program — the lines on which a breakpoint can actually stop.
fn marker_lines(path: &str) -> HashSet<u32> {
    let mut set = HashSet::new();
    let Ok(src) = std::fs::read_to_string(path) else {
        return set;
    };
    let Ok(chunk) = crate::compile_debug(&src) else {
        return set;
    };
    for (i, op) in chunk.ops.iter().enumerate() {
        if let Op::CallBuiltin(id, _) = op {
            if *id == crate::host::DBG_LINE {
                if let Some(l) = chunk.lines.get(i) {
                    set.insert(*l);
                }
            }
        }
    }
    set
}

/// Run the program under the debugger: redirect its stdout to a pipe, run with
/// the debug marker builtin (which pauses at breakpoints/steps), then restore
/// stdout, flush remaining output, and emit `terminated`.
fn launch(program: &str) {
    if program.is_empty() {
        return;
    }
    DBG.with(|d| {
        let mut s = d.borrow_mut();
        if s.program.is_empty() {
            s.program = program.to_string();
        }
    });
    // SAFETY: standard pipe + dup2 on the process's own stdout fd; the read end
    // is set non-blocking so `drain_output` never stalls the debugger.
    let pipe_r = unsafe {
        let mut fds = [0i32; 2];
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            -1
        } else {
            libc::dup2(fds[1], 1);
            libc::close(fds[1]);
            let flags = libc::fcntl(fds[0], libc::F_GETFL);
            libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
            fds[0]
        }
    };
    DBG.with(|d| {
        let mut s = d.borrow_mut();
        s.pipe_r = pipe_r;
        s.mode = Mode::Continue;
        s.active = true;
    });

    if let Err(e) = crate::eval_file_debug(program) {
        eprintln!("scala: {e}");
    }

    // Restore stdout, drain any trailing program output, then close the pipe.
    let _ = std::io::stdout().flush();
    DBG.with(|d| d.borrow_mut().active = false);
    drain_output();
    let saved = DBG.with(|d| d.borrow().proto_fd);
    unsafe {
        if saved >= 0 {
            libc::dup2(saved, 1);
        }
        if pipe_r >= 0 {
            libc::close(pipe_r);
        }
    }
    DBG.with(|d| d.borrow_mut().pipe_r = -1);
    event("terminated", json!({}));
}

/// Called by the VM at each statement marker (via the `DBG_LINE` builtin). Reads
/// the marker's source line; if it is a breakpoint or a step target, pauses and
/// services DAP requests until a resume command.
pub fn on_debug_line(vm: &mut VM) {
    let line = *vm.chunk.lines.get(vm.ip.saturating_sub(1)).unwrap_or(&0);
    if line == 0 {
        return;
    }
    let (stop, reason) = DBG.with(|d| {
        let mut s = d.borrow_mut();
        s.cur_line = line;
        if !s.active {
            return (false, "");
        }
        let bp = s.breakpoints.contains(&line) && s.verified.contains(&line);
        let step = s.mode == Mode::Step;
        let reason = if bp { "breakpoint" } else { "step" };
        (bp || step, reason)
    });
    if !stop {
        return;
    }
    drain_output();
    event(
        "stopped",
        json!({
            "reason": reason,
            "threadId": 1,
            "allThreadsStopped": true,
        }),
    );

    // Service requests until a resume command returns control to the VM.
    let mut stdin = std::io::stdin();
    loop {
        match read_message(&mut stdin) {
            Ok(Some(msg)) => {
                if handle_stopped(vm, &msg) {
                    break;
                }
            }
            _ => {
                // EOF / read error: let the program run to completion.
                DBG.with(|d| d.borrow_mut().mode = Mode::Continue);
                break;
            }
        }
    }
}

/// Snapshot the debuggee's locals as (name, Scala-formatted value) pairs. Names
/// are the chunk's name pool; synthetic `for` upper-bound locals (a leading
/// space, impossible to lex) and never-assigned slots are hidden.
fn locals(vm: &VM) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, name) in vm.chunk.names.iter().enumerate() {
        if name.starts_with(' ') {
            continue;
        }
        let val = vm.globals.get(i).cloned().unwrap_or(fusevm::Value::Undef);
        out.push((name.clone(), crate::host::scala_str(&val)));
    }
    out
}

/// Handle one request while stopped. Returns true when a resume command
/// (`continue`/`next`/`stepIn`/`stepOut`) was processed and the VM should run on.
fn handle_stopped(vm: &VM, msg: &J) -> bool {
    let command = msg.get("command").and_then(|c| c.as_str()).unwrap_or("");
    let req_seq = msg.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
    match command {
        "threads" => {
            respond(
                req_seq,
                command,
                json!({ "threads": [{ "id": 1, "name": "main" }] }),
            );
            false
        }
        "stackTrace" => {
            let (program, line) = DBG.with(|d| {
                let s = d.borrow();
                (s.program.clone(), s.cur_line)
            });
            let frames = json!([{
                "id": 0,
                "name": "main",
                "line": line,
                "column": 1,
                "source": { "path": program },
            }]);
            respond(
                req_seq,
                command,
                json!({ "stackFrames": frames, "totalFrames": 1 }),
            );
            false
        }
        "scopes" => {
            respond(
                req_seq,
                command,
                json!({ "scopes": [{ "name": "Locals", "variablesReference": 1, "expensive": false }] }),
            );
            false
        }
        "variables" => {
            let vars: Vec<J> = locals(vm)
                .into_iter()
                .map(|(n, v)| json!({ "name": n, "value": v, "variablesReference": 0 }))
                .collect();
            respond(req_seq, command, json!({ "variables": vars }));
            false
        }
        "setBreakpoints" => {
            set_breakpoints(msg, req_seq);
            false
        }
        "setExceptionBreakpoints" => {
            respond(req_seq, command, json!({ "breakpoints": [] }));
            false
        }
        "evaluate" => {
            let expr = msg
                .get("arguments")
                .and_then(|a| a.get("expression"))
                .and_then(|e| e.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let result = locals(vm)
                .into_iter()
                .find(|(n, _)| *n == expr)
                .map(|(_, v)| v)
                .unwrap_or_else(|| if expr.is_empty() { String::new() } else { format!("<cannot evaluate `{expr}`>") });
            respond(
                req_seq,
                command,
                json!({ "result": result, "variablesReference": 0 }),
            );
            false
        }
        "pause" => {
            respond(req_seq, command, json!({}));
            false
        }
        "continue" => {
            DBG.with(|d| d.borrow_mut().mode = Mode::Continue);
            respond(req_seq, command, json!({ "allThreadsContinued": true }));
            true
        }
        "next" | "stepIn" | "stepOut" => {
            DBG.with(|d| d.borrow_mut().mode = Mode::Step);
            respond(req_seq, command, json!({}));
            true
        }
        "disconnect" | "terminate" => {
            DBG.with(|d| d.borrow_mut().mode = Mode::Continue);
            respond(req_seq, command, json!({}));
            true
        }
        _ => {
            respond(req_seq, command, json!({}));
            false
        }
    }
}

/// Read whatever the program has written to its stdout pipe so far (non-blocking)
/// and forward it as an `output` event.
fn drain_output() {
    let fd = DBG.with(|d| d.borrow().pipe_r);
    if fd < 0 {
        return;
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n > 0 {
            out.extend_from_slice(&buf[..n as usize]);
        } else {
            break;
        }
    }
    if !out.is_empty() {
        let text = String::from_utf8_lossy(&out).to_string();
        event("output", json!({ "category": "stdout", "output": text }));
    }
}

// ---- wire protocol --------------------------------------------------------

/// Read one `Content-Length`-framed JSON message; `None` at EOF.
fn read_message(input: &mut std::io::Stdin) -> Result<Option<J>, String> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                header.push(byte[0]);
                if header.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(e) => return Err(format!("dap read: {e}")),
        }
    }
    let header = String::from_utf8_lossy(&header);
    let len: usize = header
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length:"))
        .and_then(|v| v.trim().parse().ok())
        .ok_or("dap: missing Content-Length")?;
    let mut body = vec![0u8; len];
    input
        .read_exact(&mut body)
        .map_err(|e| format!("dap body: {e}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("dap json: {e}"))
}

/// Write a framed JSON message to the saved protocol fd (never to fd 1, which is
/// the program's redirected stdout during a run).
fn send(msg: &J) {
    let body = msg.to_string();
    let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    let fd = DBG.with(|d| d.borrow().proto_fd);
    // SAFETY: `fd` is a valid duplicated stdout fd owned by this process; wrapped
    // in ManuallyDrop so the File does not close it on drop.
    unsafe {
        let mut f = std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(fd));
        let _ = f.write_all(frame.as_bytes());
        let _ = f.flush();
    }
}

fn next_seq() -> i64 {
    DBG.with(|d| {
        let mut s = d.borrow_mut();
        let n = s.seq;
        s.seq += 1;
        n
    })
}

fn respond(req_seq: i64, command: &str, body: J) {
    send(&json!({
        "seq": next_seq(),
        "type": "response",
        "request_seq": req_seq,
        "success": true,
        "command": command,
        "body": body,
    }));
}

fn event(ev: &str, body: J) {
    send(&json!({ "seq": next_seq(), "type": "event", "event": ev, "body": body }));
}
