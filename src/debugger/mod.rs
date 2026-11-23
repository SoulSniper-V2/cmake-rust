//! DAP debugger (`Source/cmDebugger*` port).
//!
//! Layout mirrors upstream: `Debugger` is the adapter
//! (`cmDebuggerAdapter`), `Inner::state` the thread/frame/breakpoint/
//! exception state guarded by the adapter `Mutex`, `wire` the spy-wrapped
//! writer, and `read_message` the cppdap `ContentReader` framing. Variable
//! tree construction lives in `vars.rs` (`cmDebuggerVariablesHelper`).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

mod vars;

/// `cmDebuggerStackFrame`.
#[derive(Clone)]
pub struct DebugFrame {
    pub id: i64,
    pub file: String,
    pub line: i64,
    pub name: String,
    pub args: Vec<String>,
    /// The `cmMakefile*` of the running interpreter, as an opaque pointer.
    pub interp: usize,
    /// Makefile index (`Interp::makefiles`).
    pub mf: usize,
}

struct SourceBreakpoint {
    id: i64,
    line: i64,
    valid: bool,
}

struct FunctionLocation {
    start: i64,
    end: i64,
}

/// `cmDebuggerBreakpointManager`.
#[derive(Default)]
struct BreakpointState {
    breakpoints: HashMap<String, Vec<SourceBreakpoint>>,
    function_lines: HashMap<String, Vec<FunctionLocation>>,
    pending_validations: std::collections::HashSet<String>,
}

impl BreakpointState {
    fn calibrate(&self, source_path: &str, line: i64) -> i64 {
        let empty = Vec::new();
        let locations = self.function_lines.get(source_path).unwrap_or(&empty);
        if let Some(location) = locations.iter().find(|loc| loc.start >= line) {
            return location.start;
        }
        if let Some(last) = locations.last()
            && last.end <= line
        {
            return last.start;
        }
        0
    }
}

/// `cmDebuggerExceptionManager` state.
struct ExceptionState {
    /// Filter -> enabled (`RaiseExceptions`). Missing means disabled.
    raise: HashMap<String, bool>,
    /// `(id, description)` of the last exception stop (`TheException`).
    current: Option<(String, String)>,
}

/// (filter, label) pairs from `cmDebuggerExceptionManager`'s `ExceptionMap`,
/// in insertion (`MessageType` value) order.  The map is an
/// `unordered_map` hashed by the enum value: every key lands in its own
/// bucket and libstdc++ links each new bucket's node at the front of the
/// element list, so iteration yields the reverse of this order.
const EXCEPTION_FILTERS: &[(&str, &str)] = &[
    ("AUTHOR_WARNING", "Warning (dev)"),
    ("AUTHOR_ERROR", "Error (dev)"),
    ("FATAL_ERROR", "Fatal error"),
    ("INTERNAL_ERROR", "Internal error"),
    ("MESSAGE", "Other messages"),
    ("WARNING", "Warning"),
    ("LOG", "Debug log"),
    ("DEPRECATION_ERROR", "Deprecation error"),
    ("DEPRECATION_WARNING", "Deprecation warning"),
];

const DEFAULT_RAISE: &[&str] = &[
    "AUTHOR_ERROR",
    "FATAL_ERROR",
    "INTERNAL_ERROR",
    "DEPRECATION_ERROR",
];

/// Map `MessageType` to the upstream `MessageType` filter key.
fn message_filter(kind: crate::messages::MessageType) -> &'static str {
    use crate::messages::MessageType;
    match kind {
        MessageType::AuthorWarning => "AUTHOR_WARNING",
        MessageType::AuthorError => "AUTHOR_ERROR",
        MessageType::FatalError => "FATAL_ERROR",
        MessageType::InternalError => "INTERNAL_ERROR",
        MessageType::Message => "MESSAGE",
        MessageType::Warning => "WARNING",
        MessageType::Log => "LOG",
        MessageType::DeprecationError => "DEPRECATION_ERROR",
        MessageType::DeprecationWarning => "DEPRECATION_WARNING",
    }
}

/// `cmDebuggerVariablesManager` handlers plus `cmDebuggerVariables` state.
pub type VarHandler = Box<dyn Fn() -> Vec<vars::VarEntry> + Send>;

pub struct VarNode {
    pub name: String,
    pub value: String,
    pub ignore_empty: bool,
    pub sort: bool,
    pub handler: Option<VarHandler>,
}

/// Adapter `Mutex` state: the default thread's frames plus breakpoints,
/// exceptions and variable handlers.
pub struct State {
    pub frames: Vec<DebugFrame>,
    pub frame_map: HashMap<i64, usize>,
    breakpoints: BreakpointState,
    exceptions: ExceptionState,
    pub variables: HashMap<i64, VarNode>,
    /// Registered child collection ids per node id.
    pub var_children: HashMap<i64, Vec<i64>>,
    /// Scope var id -> source file of the owning stack frame.
    pub var_frame_source: HashMap<i64, String>,
    pub var_scopes: HashMap<i64, Vec<i64>>,
    /// Whether the default thread exists (`DefaultThread != nullptr`).
    pub thread_started: bool,
}

impl State {
    pub fn frame_source(&self, var_id: i64) -> J {
        let path = self
            .var_frame_source
            .get(&var_id)
            .cloned()
            .unwrap_or_default();
        source(&path)
    }
}

/// `dap::SynchronizedEvent` / `Semaphore` primitives.
struct SyncEvent {
    lock: Mutex<bool>,
    cv: Condvar,
}

impl SyncEvent {
    fn new() -> Self {
        SyncEvent {
            lock: Mutex::new(false),
            cv: Condvar::new(),
        }
    }
    fn fire(&self) {
        *self.lock.lock().unwrap() = true;
        self.cv.notify_all();
    }
    fn wait(&self) {
        let mut fired = self.lock.lock().unwrap();
        while !*fired {
            fired = self.cv.wait(fired).unwrap();
        }
    }
}

struct Semaphore {
    lock: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new() -> Self {
        Semaphore {
            lock: Mutex::new(0),
            cv: Condvar::new(),
        }
    }
    fn notify(&self) {
        *self.lock.lock().unwrap() += 1;
        self.cv.notify_one();
    }
    fn wait(&self) {
        let mut count = self.lock.lock().unwrap();
        while *count == 0 {
            count = self.cv.wait(count).unwrap();
        }
        *count -= 1;
    }
}

/// Minimal ordered JSON value for byte-stable message output.
pub enum J {
    Bool(bool),
    Int(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn str(s: impl Into<String>) -> J {
        J::Str(s.into())
    }
    /// `Json::valueToQuotedStringN` with `emitUTF8 == false` (the
    /// `StreamWriterBuilder` default): control and non-ASCII characters are
    /// written as lowercase `\uXXXX` escapes (UTF-16 surrogate pairs above
    /// the BMP); `/` is not escaped.
    fn quote(s: &str, out: &mut String) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\u{08}' => out.push_str("\\b"),
                '\u{0c}' => out.push_str("\\f"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (' '..'\u{80}').contains(&c) => out.push(c),
                c => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        out.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
        }
        out.push('"');
    }

    /// `Json::BuiltStyledStreamWriter::writeValue` as configured by a default
    /// `Json::StreamWriterBuilder` (what cppdap's `JsonCppSerializer::dump`
    /// uses): tab indentation, `" : "` separators, members in sorted key
    /// order, and every non-empty array written one element per line
    /// (`CommentStyle::All` makes `isMultiLine` unconditional).
    /// `indented` mirrors the writer's `indented_` flag.
    fn write_styled(&self, out: &mut String, indent: &mut String, indented: &mut bool) {
        fn write_with_indent(out: &mut String, indent: &str, indented: &mut bool, text: &str) {
            if !*indented {
                out.push('\n');
                out.push_str(indent);
            }
            out.push_str(text);
            *indented = false;
        }
        match self {
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Int(i) => out.push_str(&i.to_string()),
            J::Str(s) => J::quote(s, out),
            J::Arr(items) if items.is_empty() => out.push_str("[]"),
            J::Arr(items) => {
                write_with_indent(out, indent, indented, "[");
                indent.push('\t');
                for (i, item) in items.iter().enumerate() {
                    if !*indented {
                        out.push('\n');
                        out.push_str(indent);
                    }
                    *indented = true;
                    item.write_styled(out, indent, indented);
                    *indented = false;
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                }
                indent.pop();
                write_with_indent(out, indent, indented, "]");
            }
            J::Obj(fields) if fields.is_empty() => out.push_str("{}"),
            J::Obj(fields) => {
                // `Value::getMemberNames` iterates the std::map: sorted,
                // unique keys.
                let mut sorted: Vec<&(String, J)> = Vec::with_capacity(fields.len());
                for field in fields {
                    match sorted.binary_search_by(|f| f.0.as_str().cmp(&field.0)) {
                        Ok(_) => {}
                        Err(pos) => sorted.insert(pos, field),
                    }
                }
                write_with_indent(out, indent, indented, "{");
                indent.push('\t');
                for (i, (key, value)) in sorted.iter().map(|f| (&f.0, &f.1)).enumerate() {
                    let mut name = String::new();
                    J::quote(key, &mut name);
                    write_with_indent(out, indent, indented, &name);
                    out.push_str(" : ");
                    value.write_styled(out, indent, indented);
                    if i + 1 < sorted.len() {
                        out.push(',');
                    }
                }
                indent.pop();
                write_with_indent(out, indent, indented, "}");
            }
        }
    }
    /// `dap::json::JsonCppSerializer::dump` of the message.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut indent = String::new();
        let mut indented = true;
        self.write_styled(&mut out, &mut indent, &mut indented);
        out
    }
}

/// `dap::Source` with `name` and `path` set to the file path.
fn source(path: &str) -> J {
    J::Obj(vec![
        ("name".into(), J::str(path)),
        ("path".into(), J::str(path)),
    ])
}

/// `dap::Source` with only `path` set (breakpoint events / responses).
fn source_path(path: &str) -> J {
    J::Obj(vec![("path".into(), J::str(path))])
}

fn breakpoint_json(id: Option<i64>, line: i64, verified: bool, path: &str) -> J {
    let mut fields: Vec<(String, J)> = Vec::new();
    if let Some(id) = id {
        fields.push(("id".into(), J::Int(id)));
    }
    fields.push(("line".into(), J::Int(line)));
    fields.push(("source".into(), source_path(path)));
    fields.push(("verified".into(), J::Bool(verified)));
    J::Obj(fields)
}

pub struct Inner {
    pub state: Mutex<State>,
    /// The accepted client socket write half (`dap::Writer`), spy-wrapped.
    writer: Mutex<UnixStream>,
    /// `--debugger-dap-log` destination (`dap::file` spy target).
    log: Mutex<Option<std::fs::File>>,
    next_seq: AtomicI64,
    next_frame_id: AtomicI64,
    next_thread_id: AtomicI64,
    next_var_id: AtomicI64,
    next_breakpoint_id: AtomicI64,
    pub session_active: AtomicBool,
    disconnect_event: SyncEvent,
    configuration_done: SyncEvent,
    continue_sem: Semaphore,
    next_step_from: AtomicI64,
    step_in: AtomicBool,
    step_out_depth: AtomicI64,
    pause_request: AtomicBool,
    pub supports_variable_type: AtomicBool,
    /// The default thread id ("CMake script").
    pub default_thread: AtomicI64,
    session_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Inner {
    /// `~cmDebuggerAdapter`: join the session thread and close the log.
    fn drop(&mut self) {
        self.session_active.store(false, Ordering::Relaxed);
        // Wake the blocked reader so the join below cannot hang.
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown(std::net::Shutdown::Read);
        }
        let handle = self.session_thread.lock().unwrap().take();
        if let Some(thread) = handle
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}

impl Inner {
    pub fn next_var_id(&self) -> i64 {
        self.next_var_id.fetch_add(1, Ordering::Relaxed)
    }
    pub fn supports_variable_type(&self) -> bool {
        self.supports_variable_type.load(Ordering::Relaxed)
    }
}

/// The clonable adapter handle stored on `Interp`.
#[derive(Clone)]
pub struct Debugger {
    pub inner: Arc<Inner>,
}

fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(serde_json::Value::as_str)
}

fn json_i64(v: &serde_json::Value, key: &str) -> Option<i64> {
    v.get(key).and_then(serde_json::Value::as_i64)
}

impl Inner {
    /// Writes one JSON message (`ContentWriter`) through the spy logger.
    fn send(&self, value: &J) {
        let text = value.render();
        let header = format!("Content-Length: {}\r\n\r\n", text.len());
        let mut writer = self.writer.lock().unwrap();
        for part in [header, text] {
            // `dap::spy(Writer)`: each write is logged after "\n<-".
            if let Some(log) = self.log.lock().unwrap().as_mut() {
                let _ = write!(log, "\n<-{part}");
                let _ = log.flush();
            }
            if writer.write_all(part.as_bytes()).is_err() {
                break;
            }
        }
    }

    fn send_event(&self, name: &str, body: J) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.send(&J::Obj(vec![
            ("seq".into(), J::Int(seq)),
            ("type".into(), J::str("event")),
            ("event".into(), J::str(name)),
            ("body".into(), body),
        ]));
    }

    fn send_response(&self, request_seq: i64, command: &str, body: J) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.send(&J::Obj(vec![
            ("seq".into(), J::Int(seq)),
            ("type".into(), J::str("response")),
            ("request_seq".into(), J::Int(request_seq)),
            ("success".into(), J::Bool(true)),
            ("command".into(), J::str(command)),
            ("body".into(), body),
        ]));
    }

    fn send_error(&self, request_seq: i64, command: &str, message: &str) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        self.send(&J::Obj(vec![
            ("seq".into(), J::Int(seq)),
            ("type".into(), J::str("response")),
            ("request_seq".into(), J::Int(request_seq)),
            ("success".into(), J::Bool(false)),
            ("command".into(), J::str(command)),
            ("message".into(), J::str(message)),
        ]));
    }

    fn clear_step_requests(&self) {
        self.next_step_from.store(i64::MIN, Ordering::Relaxed);
        self.step_in.store(false, Ordering::Relaxed);
        self.step_out_depth.store(i64::MIN, Ordering::Relaxed);
        self.pause_request.store(false, Ordering::Relaxed);
    }

    fn stack_depth(&self) -> i64 {
        self.state.lock().unwrap().frames.len() as i64
    }

    /// `cmDebuggerExceptionManager::ClearAll` + `ClearAll` + step reset,
    /// notify and disconnect — shared by `disconnect` and session errors.
    fn teardown(&self) {
        {
            let mut state = self.state.lock().unwrap();
            state.breakpoints.breakpoints.clear();
            state.exceptions.raise.clear();
        }
        self.clear_step_requests();
        self.continue_sem.notify();
        self.disconnect_event.fire();
        self.session_active.store(false, Ordering::Relaxed);
    }

    fn session_error(&self, message: &str) {
        if let Some(log) = self.log.lock().unwrap().as_mut() {
            let _ = writeln!(log, "dap::Session error: {message}");
            let _ = log.flush();
        }
        println!("[CMake Debugger] DAP session error: {message}");
        self.teardown();
    }

    // ---- request handlers ---------------------------------------------

    fn handle_initialize(&self, args: &serde_json::Value) -> J {
        self.supports_variable_type.store(
            args.get("supportsVariableType")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            Ordering::Relaxed,
        );
        // `HandleInitializeRequest` fills exceptionBreakpointFilters in
        // `ExceptionMap` iteration order (see EXCEPTION_FILTERS).
        let filters: Vec<J> = {
            let state = self.state.lock().unwrap();
            EXCEPTION_FILTERS
                .iter()
                .rev()
                .map(|(filter, label)| {
                    J::Obj(vec![
                        (
                            "default".into(),
                            J::Bool(
                                state
                                    .exceptions
                                    .raise
                                    .get(&**filter)
                                    .copied()
                                    .unwrap_or(false),
                            ),
                        ),
                        ("filter".into(), J::str(*filter)),
                        ("label".into(), J::str(*label)),
                    ])
                })
                .collect()
        };
        // Field order follows `DAP_IMPLEMENT_STRUCT_TYPEINFO` for
        // `CMakeInitializeResponse` (alphabetical, `cmakeVersion` last).
        J::Obj(vec![
            ("exceptionBreakpointFilters".into(), J::Arr(filters)),
            ("supportsConfigurationDoneRequest".into(), J::Bool(true)),
            ("supportsExceptionInfoRequest".into(), J::Bool(true)),
            ("supportsValueFormattingOptions".into(), J::Bool(true)),
            (
                "cmakeVersion".into(),
                J::Obj(vec![
                    ("major".into(), J::Int(crate::interp::VERSION.0.into())),
                    ("minor".into(), J::Int(crate::interp::VERSION.1.into())),
                    ("patch".into(), J::Int(crate::interp::VERSION.2.into())),
                    ("full".into(), J::str(crate::interp::VERSION_STRING)),
                ]),
            ),
        ])
    }

    fn handle_set_breakpoints(&self, args: &serde_json::Value) -> J {
        let mut state = self.state.lock().unwrap();
        let bp = &mut state.breakpoints;
        // `cmSystemTools::GetActualCaseForPath` resolves to the on-disk
        // casing of a normalized absolute path.
        let source_path = args
            .get("source")
            .and_then(|s| s.get("path"))
            .and_then(serde_json::Value::as_str)
            .map(|p| crate::util::collapse_full_path(p, "/"))
            .unwrap_or_default();
        let empty = Vec::new();
        let requested = args
            .get("breakpoints")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or(empty);
        if let Some(existing) = bp.breakpoints.get_mut(&source_path) {
            existing.clear();
        }
        let loaded = bp.function_lines.contains_key(&source_path);
        let mut out = Vec::with_capacity(requested.len());
        if loaded {
            for item in &requested {
                let line = json_i64(item, "line").unwrap_or(0);
                let corrected = bp.calibrate(&source_path, line);
                let (id, line, verified) = if corrected > 0 {
                    let id = self.next_breakpoint_id.fetch_add(1, Ordering::Relaxed);
                    bp.breakpoints
                        .entry(source_path.clone())
                        .or_default()
                        .push(SourceBreakpoint {
                            id,
                            line: corrected,
                            valid: true,
                        });
                    (Some(id), corrected, true)
                } else {
                    (None, line, false)
                };
                out.push(breakpoint_json(id, line, verified, &source_path));
            }
        } else {
            bp.pending_validations.insert(source_path.clone());
            for item in &requested {
                let line = json_i64(item, "line").unwrap_or(0);
                let id = self.next_breakpoint_id.fetch_add(1, Ordering::Relaxed);
                bp.breakpoints
                    .entry(source_path.clone())
                    .or_default()
                    .push(SourceBreakpoint {
                        id,
                        line,
                        valid: true,
                    });
                out.push(breakpoint_json(Some(id), line, false, &source_path));
            }
        }
        J::Obj(vec![("breakpoints".into(), J::Arr(out))])
    }

    fn handle_set_exception_breakpoints(&self, args: &serde_json::Value) -> J {
        let mut state = self.state.lock().unwrap();
        state.exceptions.raise.clear();
        if let Some(filters) = args.get("filters").and_then(|f| f.as_array()) {
            for filter in filters {
                if let Some(name) = filter.as_str() {
                    // `RaiseExceptions[filter] = true` creates the key.
                    state.exceptions.raise.insert(name.to_owned(), true);
                }
            }
        }
        J::Obj(vec![])
    }

    fn handle_exception_info(&self) -> J {
        let mut state = self.state.lock().unwrap();
        // Field order: breakMode, description, details, exceptionId.
        match state.exceptions.current.take() {
            Some((id, description)) => J::Obj(vec![
                ("breakMode".into(), J::str("always")),
                ("description".into(), J::str(description)),
                ("exceptionId".into(), J::str(id)),
            ]),
            // Defaults of the required fields (`breakMode = "never"`).
            None => J::Obj(vec![
                ("breakMode".into(), J::str("never")),
                ("exceptionId".into(), J::str("")),
            ]),
        }
    }

    fn handle_threads(&self) -> J {
        let state = self.state.lock().unwrap();
        let threads = if state.thread_started {
            vec![J::Obj(vec![
                (
                    "id".into(),
                    J::Int(self.default_thread.load(Ordering::Relaxed)),
                ),
                ("name".into(), J::str("CMake script")),
            ])]
        } else {
            Vec::new()
        };
        J::Obj(vec![("threads".into(), J::Arr(threads))])
    }

    fn handle_stack_trace(&self, args: &serde_json::Value) -> Result<J, String> {
        let thread_id = json_i64(args, "threadId").unwrap_or(0);
        let state = self.state.lock().unwrap();
        if !state.thread_started || thread_id != self.default_thread.load(Ordering::Relaxed) {
            return Err(format!("Unknown threadId '{thread_id}'"));
        }
        let format = args.get("format");
        let flag = |key: &str| {
            format
                .and_then(|f| f.get(key))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        };
        let (show_parameters, show_values, show_line) =
            (flag("parameters"), flag("parameterValues"), flag("line"));
        let mut frames = Vec::with_capacity(state.frames.len());
        for frame in state.frames.iter().rev() {
            let mut name = frame.name.clone();
            if show_parameters {
                name.push('(');
                if show_values && !frame.args.is_empty() {
                    for arg in &frame.args {
                        name.push_str(arg);
                        name.push_str(", ");
                    }
                    name.truncate(name.len() - 2);
                }
                name.push(')');
            }
            if show_line {
                name = format!("{name} Line: {}", frame.line);
            }
            frames.push(J::Obj(vec![
                ("column".into(), J::Int(1)),
                ("id".into(), J::Int(frame.id)),
                ("line".into(), J::Int(frame.line)),
                ("name".into(), J::str(name)),
                ("source".into(), source(&frame.file)),
            ]));
        }
        let total = frames.len() as i64;
        Ok(J::Obj(vec![
            ("stackFrames".into(), J::Arr(frames)),
            ("totalFrames".into(), J::Int(total)),
        ]))
    }

    fn handle_scopes(&self, args: &serde_json::Value) -> J {
        let frame_id = json_i64(args, "frameId").unwrap_or(0);
        let mut state = self.state.lock().unwrap();
        if let Some(ids) = state.var_scopes.get(&frame_id) {
            let scopes: Vec<J> = ids.iter().map(|id| vars::scope_json(&state, *id)).collect();
            return J::Obj(vec![("scopes".into(), J::Arr(scopes))]);
        }
        let Some(index) = state.frame_map.get(&frame_id).copied() else {
            return J::Obj(vec![("scopes".into(), J::Arr(vec![]))]);
        };
        let frame = state.frames[index].clone();
        let supports_type = self.supports_variable_type.load(Ordering::Relaxed);
        let var_id = vars::create_locals(&mut state, self, &frame, supports_type);
        state.var_scopes.insert(frame_id, vec![var_id]);
        let scope = vars::scope_json(&state, var_id);
        J::Obj(vec![("scopes".into(), J::Arr(vec![scope]))])
    }

    fn handle_variables(&self, args: &serde_json::Value) -> J {
        let reference = json_i64(args, "variablesReference").unwrap_or(0);
        let mut state = self.state.lock().unwrap();
        let variables = vars::handle_variables_request(&mut state, self, reference);
        J::Obj(vec![("variables".into(), J::Arr(variables))])
    }

    fn handle_evaluate(&self, args: &serde_json::Value) -> J {
        // `result` is required and always serialized.
        let mut result = String::new();
        let mut found = false;
        if let Some(frame_id) = json_i64(args, "frameId") {
            let state = self.state.lock().unwrap();
            if let Some(index) = state.frame_map.get(&frame_id).copied() {
                let frame = &state.frames[index];
                if let Some(expression) = json_str(args, "expression") {
                    if let Some(value) = vars::get_definition(frame.interp, frame.mf, expression) {
                        result = value;
                        found = true;
                    }
                }
            }
        }
        // `result` and `variablesReference` are required (always
        // serialized); `type` is optional.
        let mut body = vec![("result".into(), J::str(result))];
        if found {
            body.push(("type".into(), J::str("string")));
        }
        body.push(("variablesReference".into(), J::Int(0)));
        J::Obj(body)
    }

    fn dispatch(&self, sequence: i64, command: &str, args: &serde_json::Value) {
        match command {
            "initialize" => {
                let body = self.handle_initialize(args);
                self.send_response(sequence, command, body);
                // `registerSentHandler` fires InitializedEvent once the
                // initialize response is sent.
                self.send_event("initialized", J::Obj(vec![]));
            }
            "launch" => self.send_response(sequence, command, J::Obj(vec![])),
            "attach" => self.send_response(sequence, command, J::Obj(vec![])),
            "setBreakpoints" => {
                let body = self.handle_set_breakpoints(args);
                self.send_response(sequence, command, body);
            }
            "setExceptionBreakpoints" => {
                let body = self.handle_set_exception_breakpoints(args);
                self.send_response(sequence, command, body);
            }
            "exceptionInfo" => {
                let body = self.handle_exception_info();
                self.send_response(sequence, command, body);
            }
            "configurationDone" => {
                self.send_response(sequence, command, J::Obj(vec![]));
                self.configuration_done.fire();
            }
            "threads" => {
                let body = self.handle_threads();
                self.send_response(sequence, command, body);
            }
            "stackTrace" => match self.handle_stack_trace(args) {
                Ok(body) => self.send_response(sequence, command, body),
                Err(message) => self.send_error(sequence, command, &message),
            },
            "scopes" => {
                let body = self.handle_scopes(args);
                self.send_response(sequence, command, body);
            }
            "variables" => {
                let body = self.handle_variables(args);
                self.send_response(sequence, command, body);
            }
            "pause" => {
                self.pause_request.store(true, Ordering::Relaxed);
                self.send_response(sequence, command, J::Obj(vec![]));
            }
            "continue" => {
                self.continue_sem.notify();
                self.send_response(sequence, command, J::Obj(vec![]));
            }
            "next" => {
                let depth = self.stack_depth();
                self.next_step_from.store(depth, Ordering::Relaxed);
                self.continue_sem.notify();
                self.send_response(sequence, command, J::Obj(vec![]));
            }
            "stepIn" => {
                self.step_in.store(true, Ordering::Relaxed);
                self.continue_sem.notify();
                self.send_response(sequence, command, J::Obj(vec![]));
            }
            "stepOut" => {
                let depth = self.stack_depth();
                self.step_out_depth.store(depth - 1, Ordering::Relaxed);
                self.continue_sem.notify();
                self.send_response(sequence, command, J::Obj(vec![]));
            }
            "disconnect" => {
                self.teardown();
                self.send_response(sequence, command, J::Obj(vec![]));
            }
            "evaluate" => {
                let body = self.handle_evaluate(args);
                self.send_response(sequence, command, body);
            }
            other => {
                self.session_error(&format!(
                    "No request handler registered for command '{other}'"
                ));
            }
        }
    }
}

/// `dap::Session` request processing: one framed message -> dispatch.
fn process_message(inner: &Arc<Inner>, text: &str) {
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(text);
    let Ok(value) = parsed else {
        inner.session_error("Failed to deserialize message");
        return;
    };
    let Some(kind) = json_str(&value, "type") else {
        inner.session_error("Message missing string 'type' field");
        return;
    };
    let _sequence = json_i64(&value, "seq").unwrap_or(0);
    if json_i64(&value, "seq").is_none() {
        inner.session_error("Message missing number 'seq' field");
        return;
    }
    match kind {
        "request" => {
            let Some(command) = json_str(&value, "command") else {
                inner.session_error("Request missing string 'command' field");
                return;
            };
            let args = value
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            inner.dispatch(_sequence, command, &args);
        }
        "event" => {
            let Some(event) = json_str(&value, "event") else {
                inner.session_error("Event missing string 'event' field");
                return;
            };
            inner.session_error(&format!("No event handler registered for event '{event}'"));
        }
        "response" => {
            inner.session_error("Unknown response");
        }
        other => {
            inner.session_error(&format!("Unknown message type '{other}'"));
        }
    }
}

/// `dap::ContentReader` (kScan mode): scan to `Content-Length:`, read the
/// length, expect `\r\n\r\n`, then read the payload. `read` logs each
/// socket chunk to the spy log (`"\n->"`), matching `dap::spy`.
fn read_message(stream: &mut UnixStream, log: &Mutex<Option<std::fs::File>>) -> Option<String> {
    let mut buf: Vec<u8> = Vec::new();
    let mut fill = |buf: &mut Vec<u8>, need: usize| -> bool {
        let mut chunk = [0u8; 256];
        while buf.len() < need {
            let want = std::cmp::min(256, need - buf.len());
            match stream.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => return false,
                Ok(got) => {
                    // `dap::spy(Reader)`: each read is logged after "\n->".
                    if let Some(log) = log.lock().unwrap().as_mut() {
                        let _ = log.write_all(b"\n->");
                        let _ = log.write_all(&chunk[..got]);
                        let _ = log.flush();
                    }
                    buf.extend_from_slice(&chunk[..got]);
                }
            }
        }
        true
    };
    // `ContentReader::scan`: buffer the prefix length, compare, and on a
    // mismatch drop one byte and buffer again.
    let prefix = b"Content-Length:";
    loop {
        if !fill(&mut buf, prefix.len()) {
            return None;
        }
        if buf.starts_with(prefix) {
            break;
        }
        buf.remove(0);
    }
    buf.drain(..prefix.len());
    loop {
        if buf.is_empty() && !fill(&mut buf, 1) {
            return None;
        }
        match buf.first().copied() {
            Some(b' ') | Some(b'\t') => {
                buf.remove(0);
            }
            _ => break,
        }
    }
    let mut len: usize = 0;
    loop {
        if buf.is_empty() && !fill(&mut buf, 1) {
            return None;
        }
        let c = buf[0];
        if c.is_ascii_digit() {
            len = len * 10 + usize::from(c - b'0');
            buf.remove(0);
        } else {
            break;
        }
    }
    if len == 0 {
        return None;
    }
    if !fill(&mut buf, 4) {
        return None;
    }
    if !buf.starts_with(b"\r\n\r\n") {
        return None;
    }
    buf.drain(..4);
    if !fill(&mut buf, len) {
        return None;
    }
    let payload: Vec<u8> = buf.drain(..len).collect();
    String::from_utf8(payload).ok()
}

impl Debugger {
    /// `cmake::StartDebuggerIfEnabled`: bind, listen, print the banner,
    /// accept, connect the session and block until `configurationDone`.
    /// Returns the adapter or an error string (upstream throws -> fatal).
    pub fn start(pipe_path: &str, dap_log: Option<&str>) -> Result<Debugger, String> {
        let log = match dap_log {
            Some(path) => match std::fs::File::create(path) {
                Ok(file) => Some(file),
                Err(error) => {
                    return Err(format!("Failed to open DAP log file '{path}': {error}"));
                }
            },
            None => None,
        };
        // `cmDebuggerPipeConnection_POSIX::StartListening`.
        let _ = std::fs::remove_file(pipe_path);
        let listener = UnixListener::bind(pipe_path)
            .map_err(|error| format!("Failed to bind name '{pipe_path}' to socket: {error}"))?;
        println!("Waiting for debugger client to connect...");
        let _ = std::io::stdout().flush();
        let (stream, _) = listener.accept().map_err(|error| error.to_string())?;
        drop(listener);
        // Upstream unlinks the socket path after accept.
        let _ = std::fs::remove_file(pipe_path);
        println!("Debugger client connected.");
        let _ = std::io::stdout().flush();
        let reader = stream.try_clone().map_err(|e| e.to_string())?;
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                frames: Vec::new(),
                frame_map: HashMap::new(),
                breakpoints: BreakpointState::default(),
                exceptions: ExceptionState {
                    raise: DEFAULT_RAISE
                        .iter()
                        .map(|f| ((*f).to_owned(), true))
                        .collect(),
                    current: None,
                },
                variables: HashMap::new(),
                var_children: HashMap::new(),
                var_frame_source: HashMap::new(),
                var_scopes: HashMap::new(),
                thread_started: false,
            }),
            writer: Mutex::new(stream),
            log: Mutex::new(log),
            next_seq: AtomicI64::new(1),
            next_frame_id: AtomicI64::new(1),
            next_thread_id: AtomicI64::new(1),
            next_var_id: AtomicI64::new(1),
            next_breakpoint_id: AtomicI64::new(0),
            session_active: AtomicBool::new(true),
            disconnect_event: SyncEvent::new(),
            configuration_done: SyncEvent::new(),
            continue_sem: Semaphore::new(),
            next_step_from: AtomicI64::new(i64::MIN),
            step_in: AtomicBool::new(false),
            step_out_depth: AtomicI64::new(i64::MIN),
            pause_request: AtomicBool::new(false),
            supports_variable_type: AtomicBool::new(false),
            default_thread: AtomicI64::new(0),
            session_thread: Mutex::new(None),
        });
        let session_inner = inner.clone();
        let session_thread = std::thread::spawn(move || {
            let mut reader = reader;
            let log = &session_inner.log;
            while session_inner.session_active.load(Ordering::Relaxed) {
                match read_message(&mut reader, log) {
                    Some(text) => process_message(&session_inner, &text),
                    // EOF/closed: cppdap's read() returns empty and the
                    // adapter keeps polling; here the thread exits quietly
                    // while the session stays up until disconnect.
                    None => break,
                }
            }
        });
        *inner.session_thread.lock().unwrap() = Some(session_thread);
        let debugger = Debugger { inner };
        debugger.inner.configuration_done.wait();
        let thread_id = debugger
            .inner
            .next_thread_id
            .fetch_add(1, Ordering::Relaxed);
        debugger
            .inner
            .default_thread
            .store(thread_id, Ordering::Relaxed);
        debugger.inner.state.lock().unwrap().thread_started = true;
        // `threadEvent` "started".
        debugger.inner.send_event(
            "thread",
            J::Obj(vec![
                ("reason".into(), J::str("started")),
                ("threadId".into(), J::Int(thread_id)),
            ]),
        );
        Ok(debugger)
    }
}

impl Inner {
    /// `cmDebuggerAdapter::ReportExitCode`.
    pub fn report_exit_code(&self, exit_code: i64) {
        let inner = &self;
        let thread_id = inner.default_thread.load(Ordering::Relaxed);
        {
            let mut state = inner.state.lock().unwrap();
            state.thread_started = false;
            state.frames.clear();
            state.frame_map.clear();
        }
        if inner.session_active.load(Ordering::Relaxed) {
            inner.send_event(
                "thread",
                J::Obj(vec![
                    ("reason".into(), J::str("exited")),
                    ("threadId".into(), J::Int(thread_id)),
                ]),
            );
            inner.send_event(
                "exited",
                J::Obj(vec![("exitCode".into(), J::Int(exit_code))]),
            );
            inner.send_event("terminated", J::Obj(vec![]));
        }
        inner.disconnect_event.wait();
    }

    /// `OnBeginFunctionCall`.
    pub fn begin_function_call(
        &self,
        interp: usize,
        mf: usize,
        source_path: &str,
        line: i64,
        name: &str,
        args: Vec<String>,
    ) {
        let inner = &self;
        let hits = {
            let mut state = inner.state.lock().unwrap();
            let id = inner.next_frame_id.fetch_add(1, Ordering::Relaxed);
            let index = state.frames.len();
            state.frames.push(DebugFrame {
                id,
                file: source_path.to_owned(),
                line,
                name: name.to_owned(),
                args,
                interp,
                mf,
            });
            state.frame_map.insert(id, index);
            if line == 0 {
                return;
            }
            state
                .breakpoints
                .breakpoints
                .get(source_path)
                .map(|all| {
                    all.iter()
                        .filter(|bp| bp.valid && bp.line == line)
                        .map(|bp| bp.id)
                        .collect::<Vec<i64>>()
                })
                .unwrap_or_default()
        };
        let mut wait_sem = false;
        let mut reason: Option<&str> = None;
        let mut hit_ids: Vec<i64> = Vec::new();
        if !hits.is_empty() {
            inner.clear_step_requests();
            wait_sem = true;
            reason = Some("breakpoint");
            hit_ids = hits;
        }
        let depth = inner.stack_depth();
        if depth <= inner.next_step_from.load(Ordering::Relaxed)
            || inner.step_in.load(Ordering::Relaxed)
            || depth <= inner.step_out_depth.load(Ordering::Relaxed)
        {
            inner.clear_step_requests();
            wait_sem = true;
            reason = Some("step");
        }
        if inner.pause_request.load(Ordering::Relaxed) {
            inner.clear_step_requests();
            wait_sem = true;
            reason = Some("pause");
        }
        if wait_sem {
            let mut body = vec![("allThreadsStopped".into(), J::Bool(true))];
            if let Some(reason) = reason {
                body.push(("reason".into(), J::str(reason)));
            }
            if !hit_ids.is_empty() {
                body.push((
                    "hitBreakpointIds".into(),
                    J::Arr(hit_ids.into_iter().map(J::Int).collect()),
                ));
            }
            body.push((
                "threadId".into(),
                J::Int(inner.default_thread.load(Ordering::Relaxed)),
            ));
            inner.send_event("stopped", J::Obj(body));
            inner.continue_sem.wait();
        }
    }

    /// `OnEndFunctionCall`.
    pub fn end_function_call(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(frame) = state.frames.pop() {
            state.frame_map.remove(&frame.id);
            state.var_scopes.remove(&frame.id);
        }
    }

    /// `OnBeginFileParse`: push a synthetic line-0 frame.
    pub fn begin_file_parse(&self, interp: usize, mf: usize, source_path: &str) {
        self.begin_function_call(interp, mf, source_path, 0, source_path, Vec::new());
    }

    /// `OnEndFileParse`.
    pub fn end_file_parse(&self) {
        self.end_function_call();
    }

    /// `OnFileParsedSuccessfully`: register the parsed function line
    /// ranges and validate pending breakpoints.
    pub fn file_parsed(&self, source_path: &str, functions: &[(i64, i64)]) {
        let mut events = Vec::new();
        {
            let mut state = self.state.lock().unwrap();
            let bp = &mut state.breakpoints;
            if bp.function_lines.contains_key(source_path) {
                return;
            }
            let locations: Vec<FunctionLocation> = functions
                .iter()
                .map(|(start, end)| FunctionLocation {
                    start: *start,
                    end: *end,
                })
                .collect();
            bp.function_lines.insert(source_path.to_owned(), locations);
            if !bp.pending_validations.remove(source_path) {
                return;
            }
            let mut list = bp.breakpoints.remove(source_path).unwrap_or_default();
            for entry in list.iter_mut() {
                let corrected = bp.calibrate(source_path, entry.line);
                if corrected != entry.line {
                    entry.line = corrected;
                }
                let verified = corrected > 0;
                if !verified {
                    entry.valid = false;
                }
                events.push(breakpoint_json(
                    Some(entry.id),
                    if verified { corrected } else { entry.line },
                    verified,
                    source_path,
                ));
            }
            bp.breakpoints.insert(source_path.to_owned(), list);
        }
        for breakpoint in events {
            self.send_event(
                "breakpoint",
                J::Obj(vec![
                    ("breakpoint".into(), breakpoint),
                    ("reason".into(), J::str("changed")),
                ]),
            );
        }
    }

    /// `OnMessageOutput`: raise an exception stop when the filter is on.
    pub fn message_output(&self, kind: crate::messages::MessageType, text: &str) {
        let inner = &self;
        let raise = {
            let mut state = inner.state.lock().unwrap();
            let filter = message_filter(kind);
            if state.exceptions.raise.get(filter).copied().unwrap_or(false) {
                state.exceptions.current = Some((filter.to_owned(), text.to_owned()));
                true
            } else {
                false
            }
        };
        if raise {
            inner.send_event(
                "stopped",
                J::Obj(vec![
                    ("allThreadsStopped".into(), J::Bool(true)),
                    ("description".into(), J::str("Pause on exception")),
                    ("reason".into(), J::str("exception")),
                    ("text".into(), J::str(text)),
                    (
                        "threadId".into(),
                        J::Int(inner.default_thread.load(Ordering::Relaxed)),
                    ),
                ]),
            );
            inner.continue_sem.wait();
        }
    }
}

impl Debugger {
    /// `cmDebuggerAdapter::ReportExitCode`.
    pub fn report_exit_code(&self, exit_code: i64) {
        self.inner.report_exit_code(exit_code);
    }
}
