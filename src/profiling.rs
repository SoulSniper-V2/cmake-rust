//! `--profiling-format=google-trace` output (`cmMakefileProfilingData`):
//! a JSON array of Chrome trace "B"/"E" events written as the run goes.
//!
//! The "project" and "script" entries match CMake's event for event. The
//! genex_compile/genex_eval/genex_exec entries come from cmake-rust's own
//! generator-expression engine, which compiles and evaluates expressions a
//! different number of times than cmGeneratorExpression does, so their
//! counts differ from CMake's.
//!
//! The cmake instance owns one profiler; cmake-rust runs one instance per
//! process (try_compile projects do not profile, like the nested cmake
//! instance in the C++), so it lives in a thread-local.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};

use crate::commands::string::json::{Json, write_json_ostream};

struct Profiler {
    out: BufWriter<File>,
    /// `ProfileStream.tellp()`.
    written: u64,
    good: bool,
}

thread_local! {
    static PROFILER: RefCell<Option<Profiler>> = const { RefCell::new(None) };
    /// Nesting depth of in-process try_compile projects, which stand for
    /// the C++'s separate (unprofiled) cmake instances.
    static SUSPENDED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Suspends profiling while a try_compile project runs.
pub struct Suspended(());

pub fn suspend() -> Suspended {
    SUSPENDED.with(|depth| depth.set(depth.get() + 1));
    Suspended(())
}

impl Drop for Suspended {
    fn drop(&mut self) {
        SUSPENDED.with(|depth| depth.set(depth.get() - 1));
    }
}

/// The `cmMakefileProfilingData` constructor: opens (truncates) the output
/// and writes "[".
pub fn start(path: &str) -> Result<(), String> {
    let file = File::create(path).map_err(|_| format!("Unable to open: {path}"))?;
    let mut profiler = Profiler {
        out: BufWriter::new(file),
        written: 0,
        good: true,
    };
    profiler.write(b"[");
    PROFILER.with(|cell| *cell.borrow_mut() = Some(profiler));
    Ok(())
}

/// `cmake::IsProfilingEnabled`.
pub fn enabled() -> bool {
    SUSPENDED.with(std::cell::Cell::get) == 0 && PROFILER.with(|cell| cell.borrow().is_some())
}

impl Profiler {
    fn write(&mut self, bytes: &[u8]) {
        if self.out.write_all(bytes).is_err() {
            self.good = false;
            crate::cmcmd::error("Error writing profiling output!");
        }
        self.written += bytes.len() as u64;
    }
}

fn timestamp() -> u64 {
    // std::chrono::steady_clock: CLOCK_MONOTONIC.
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    (now.tv_sec as u64) * 1_000_000 + (now.tv_nsec as u64) / 1_000
}

fn common(object: &mut BTreeMap<Vec<u8>, Json>) {
    object.insert(b"ts".to_vec(), Json::UInt(timestamp()));
    object.insert(b"pid".to_vec(), Json::Int(i64::from(std::process::id())));
    object.insert(b"tid".to_vec(), Json::Int(0));
}

/// `cmMakefileProfilingData::StartEntry`. `args` is only built when
/// profiling is enabled (`CreateProfilingEntry`'s argument callback).
pub fn start_entry(category: &str, name: &str, args: impl FnOnce() -> Option<Json>) {
    PROFILER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(profiler) = cell.as_mut() else {
            return;
        };
        if !profiler.good {
            return;
        }
        if profiler.written > 1 {
            profiler.write(b",");
        }
        let mut object = BTreeMap::new();
        object.insert(b"ph".to_vec(), Json::Str(b"B".to_vec()));
        object.insert(
            b"name".to_vec(),
            Json::Str(crate::bytes::encode(name).into_owned()),
        );
        object.insert(
            b"cat".to_vec(),
            Json::Str(crate::bytes::encode(category).into_owned()),
        );
        common(&mut object);
        if let Some(args) = args() {
            object.insert(b"args".to_vec(), args);
        }
        let text = write_json_ostream(&Json::Object(object));
        profiler.write(text.as_bytes());
    });
}

/// `cmMakefileProfilingData::StopEntry`.
pub fn stop_entry() {
    PROFILER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let Some(profiler) = cell.as_mut() else {
            return;
        };
        if !profiler.good {
            return;
        }
        profiler.write(b",");
        let mut object = BTreeMap::new();
        object.insert(b"ph".to_vec(), Json::Str(b"E".to_vec()));
        common(&mut object);
        let text = write_json_ostream(&Json::Object(object));
        profiler.write(text.as_bytes());
    });
}

/// `cmMakefileProfilingData::RAII`: an entry that stops when dropped.
pub struct Entry(bool);

impl Entry {
    /// An entry that records nothing.
    pub fn none() -> Self {
        Self(false)
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if self.0 {
            stop_entry();
        }
    }
}

/// `cmake::CreateProfilingEntry`.
pub fn entry(category: &str, name: &str, args: impl FnOnce() -> Option<Json>) -> Entry {
    if !enabled() {
        return Entry(false);
    }
    start_entry(category, name, args);
    Entry(true)
}

/// The `cmMakefileProfilingData` destructor: "]" and close.
pub fn finish() {
    PROFILER.with(|cell| {
        if let Some(mut profiler) = cell.borrow_mut().take()
            && profiler.good
        {
            profiler.write(b"]");
            if profiler.out.flush().is_err() {
                crate::cmcmd::error("Error writing profiling output!");
            }
        }
    });
}
