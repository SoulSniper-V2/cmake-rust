//! File lock lifetime management and the POSIX lock operation.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crate::interp::{CmdResult, Interp};

use super::{collapse, fatal, mkdir, system_error};

type Locks = HashMap<String, File>;

#[derive(Debug, Default)]
pub struct LockPool {
    process: Locks,
    pub(crate) files: Vec<Locks>,
    pub(crate) functions: Vec<Locks>,
}

impl LockPool {
    fn scopes(&self) -> impl Iterator<Item = &Locks> {
        self.functions
            .iter()
            .chain(&self.files)
            .chain(std::iter::once(&self.process))
    }

    fn release(&mut self, path: &str) {
        for scope in self
            .functions
            .iter_mut()
            .chain(&mut self.files)
            .chain(std::iter::once(&mut self.process))
        {
            scope.remove(path);
        }
    }

    fn acquire(&mut self, path: String, file: File, guard: &str, timeout: Option<u64>) -> String {
        if self.scopes().any(|scope| scope.contains_key(&path)) {
            return "File already locked".into();
        }
        let scope = match guard {
            "FUNCTION" => self.functions.last_mut(),
            "FILE" => self.files.last_mut(),
            _ => Some(&mut self.process),
        };
        let Some(scope) = scope else {
            return if guard == "FUNCTION" {
                "'GUARD FUNCTION' not used in function definition"
            } else {
                "Internal error"
            }
            .into();
        };
        let start = Instant::now();
        let timeout = timeout.map(Duration::from_secs);
        let flags = libc::LOCK_EX | if timeout.is_some() { libc::LOCK_NB } else { 0 };
        loop {
            // The descriptor stays open in the scope until RELEASE or scope exit.
            if unsafe { libc::flock(file.as_raw_fd(), flags) } == 0 {
                scope.insert(path, file);
                return "0".into();
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => {
                    let remaining = timeout.unwrap_or_default().saturating_sub(start.elapsed());
                    if remaining.is_zero() {
                        return "Timeout reached".into();
                    }
                    std::thread::sleep(remaining.min(Duration::from_secs(1)));
                }
                _ => return system_error(&error),
            }
        }
    }
}

pub(super) fn run(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return Err(fatal(
            interp,
            "sub-command LOCK requires at least two arguments.",
        ));
    }
    let mut path = args[1].clone();
    let mut directory = false;
    let mut release = false;
    let mut guard = "PROCESS";
    let mut result_var = "";
    let mut timeout = None;
    let mut args = args[2..].iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "DIRECTORY" => directory = true,
            "RELEASE" => release = true,
            "GUARD" => {
                let help = "expected FUNCTION, FILE or PROCESS after GUARD";
                guard = args.next().ok_or_else(|| fatal(interp, help))?;
                if !matches!(guard, "FUNCTION" | "FILE" | "PROCESS") {
                    return Err(fatal(interp, &format!("{help}, but got:\n  \"{guard}\".")));
                }
            }
            "RESULT_VARIABLE" => {
                result_var = args
                    .next()
                    .ok_or_else(|| fatal(interp, "expected variable name after RESULT_VARIABLE"))?;
            }
            "TIMEOUT" => {
                let value = args
                    .next()
                    .ok_or_else(|| fatal(interp, "expected timeout value after TIMEOUT"))?;
                timeout = value
                    .trim_start()
                    .parse::<i64>()
                    .ok()
                    .filter(|count| *count >= 0)
                    .map(|count| count as u64);
                if timeout.is_none() {
                    return Err(fatal(
                        interp,
                        &format!("TIMEOUT value \"{value}\" is not an unsigned integer."),
                    ));
                }
            }
            _ => {
                return Err(fatal(
                    interp,
                    &format!(
                        "expected DIRECTORY, RELEASE, GUARD, RESULT_VARIABLE or TIMEOUT\nbut got: \"{arg}\"."
                    ),
                ));
            }
        }
    }
    if directory {
        path.push_str("/cmake.lock");
    }
    let path = collapse(&path, &interp.current_source_dir());
    let parent = crate::util::filename_path(&path);
    if mkdir(&parent).is_err() {
        return Err(fatal(
            interp,
            &format!("directory\n  \"{parent}\"\ncreation failed (check permissions)."),
        ));
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|_| {
            fatal(
                interp,
                &format!("file\n  \"{path}\"\ncreation failed (check permissions)."),
            )
        })?;
    let result = if release {
        interp.ext.file_locks.release(&path);
        "0".into()
    } else {
        interp
            .ext
            .file_locks
            .acquire(path.clone(), file, guard, timeout)
    };
    if !result_var.is_empty() {
        interp.set_def(result_var, result);
    } else if result != "0" {
        return Err(fatal(
            interp,
            &format!("error locking file\n  \"{path}\"\n{result}."),
        ));
    }
    Ok(())
}
