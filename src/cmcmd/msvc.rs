use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use super::cocompile::run_single_command;
use crate::{pathresolver, util};

#[derive(Clone, Copy, Default)]
struct FileTime {
    loaded: bool,
    nsec: i128,
}

impl FileTime {
    fn load(path: &str) -> FileTime {
        match fs::metadata(path).and_then(|m| m.modified()) {
            Ok(mtime) => {
                let nsec = match mtime.duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(d) => i128::from(d.as_secs()) * 1_000_000_000 + i128::from(d.subsec_nanos()),
                    Err(e) => {
                        let d = e.duration();
                        -(i128::from(d.as_secs()) * 1_000_000_000 + i128::from(d.subsec_nanos()))
                    }
                };
                FileTime { loaded: true, nsec }
            }
            Err(_) => FileTime::default(),
        }
    }

    fn older(&self, other: &FileTime) -> bool {
        self.nsec < other.nsec
    }
}

fn stream_lines(text: &str, strip_cr: bool) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    if strip_cr {
        lines
            .into_iter()
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .collect()
    } else {
        lines
    }
}

struct BindExpLib {
    symbols: BTreeSet<String>,
    data_symbols: BTreeSet<String>,
    nm_path: String,
}

impl BindExpLib {
    fn new() -> BindExpLib {
        BindExpLib {
            symbols: BTreeSet::new(),
            data_symbols: BTreeSet::new(),
            nm_path: "nm".to_owned(),
        }
    }

    fn add_object_file(&mut self, filename: &str) -> bool {
        let command = vec![
            self.nm_path.clone(),
            "--no-weak".to_owned(),
            "--defined-only".to_owned(),
            "--format=posix".to_owned(),
            filename.to_owned(),
        ];
        let result = run_single_command(&command, true);
        if result.ret != 0 {
            eprintln!("llvm-nm returned an error: {}", result.stdout);
            return false;
        }
        for line in stream_lines(&result.stdout, false) {
            if line.is_empty() {
                continue;
            }
            let Some(sym_end) = line.find(' ') else {
                eprintln!("Couldn't parse llvm-nm output line: {line}");
                return false;
            };
            if line.len() < sym_end + 1 {
                eprintln!("Couldn't parse llvm-nm output line: {line}");
                return false;
            }
            let sym_type = line.as_bytes().get(sym_end + 1).copied().unwrap_or(0);
            let symbol = &line[..sym_end];
            match sym_type {
                b'D' => {
                    self.data_symbols.insert(symbol.to_owned());
                }
                b'T' => {
                    self.symbols.insert(symbol.to_owned());
                }
                _ => {}
            }
        }
        true
    }

    fn add_definition_file(&mut self, filename: &str) -> bool {
        let Ok(bytes) = fs::read(filename) else {
            eprintln!("Couldn't open definition file '{filename}'");
            return false;
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in stream_lines(&text, false) {
            if line.starts_with("LIBRARY") || line.starts_with("EXPORTS") {
                continue;
            }
            let mut symbol = line.trim_start_matches([' ', '\t']).to_owned();
            if let Some(found) = symbol.find(" \t DATA") {
                symbol.truncate(found);
                self.data_symbols.insert(symbol);
            } else {
                self.symbols.insert(symbol);
            }
        }
        true
    }

    fn write_file(&self, file: &mut File) {
        let _ = file.write_all(b"EXPORTS \n");
        for symbol in &self.data_symbols {
            let _ = writeln!(file, "\t{symbol} \t DATA");
        }
        for symbol in &self.symbols {
            let _ = writeln!(file, "\t{symbol}");
        }
    }
}

pub(crate) fn create_def(args: &[String]) -> i32 {
    if args.len() < 2 {
        eprintln!("__create_def Usage: -E __create_def outfile.def objlistfile [--nm=nm-path]");
        return 1;
    }
    let out_file = &args[0];
    let obj_list_file = &args[1];
    let Ok(mut input) = File::open(obj_list_file) else {
        eprintln!("could not open object list file: {obj_list_file}");
        return 1;
    };
    let mut bytes = Vec::new();
    if input.read_to_end(&mut bytes).is_err() {
        bytes.clear();
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut files = Vec::new();
    let out_time = FileTime::load(out_file);
    let mut out_valid = out_time.loaded;
    for file in stream_lines(&text, true) {
        files.push(file.to_owned());
        if out_valid {
            let in_time = FileTime::load(file);
            out_valid = in_time.loaded && in_time.older(&out_time);
        }
    }
    if out_valid {
        return 0;
    }
    let Ok(mut output) = File::create(out_file) else {
        eprintln!("could not open output .def file: {out_file}");
        return 1;
    };
    let mut def_file = BindExpLib::new();
    if let Some(arg) = args.get(2) {
        if let Some(nm) = arg.strip_prefix("--nm=") {
            def_file.nm_path = nm.to_owned();
        } else {
            eprintln!("unknown argument: {arg}");
        }
    }
    for file in &files {
        if util::filename_last_extension(file).eq_ignore_ascii_case(".def") {
            if !def_file.add_definition_file(file) {
                return 1;
            }
        } else if !def_file.add_object_file(file) {
            return 1;
        }
    }
    def_file.write_file(&mut output);
    0
}

fn is_c_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

fn file_exists(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let Ok(path) = CString::new(path) else {
        return false;
    };
    unsafe { libc::access(path.as_ptr(), libc::R_OK) == 0 }
}

fn parse_windows_command_line(command: &str, args: &mut Vec<String>) {
    let mut in_argument = false;
    let mut in_quotes = false;
    let mut backslashes = 0usize;
    let mut arg = String::new();
    for c in command.chars() {
        if c == '\\' {
            backslashes += 1;
            in_argument = true;
        } else if c == '"' {
            let backslash_pairs = backslashes / 2;
            let backslash_escaped = backslashes % 2;
            for _ in 0..backslash_pairs {
                arg.push('\\');
            }
            backslashes = 0;
            if backslash_escaped != 0 {
                arg.push('"');
            } else {
                in_quotes = !in_quotes;
            }
            in_argument = true;
        } else {
            for _ in 0..backslashes {
                arg.push('\\');
            }
            backslashes = 0;
            if is_c_space(c) {
                if in_quotes {
                    arg.push(c);
                } else if in_argument {
                    args.push(std::mem::take(&mut arg));
                    in_argument = false;
                }
            } else {
                in_argument = true;
                arg.push(c);
            }
        }
    }
    for _ in 0..backslashes {
        arg.push('\\');
    }
    if in_argument {
        args.push(arg);
    }
}

fn ascii_strtoul(value: &str) -> Option<u64> {
    let value = value.trim_start_matches(is_c_space);
    if value.starts_with('-') || value.is_empty() {
        return None;
    }
    value.parse::<u64>().ok()
}

struct VsLink {
    link_type: i32,
    verbose: bool,
    incremental: bool,
    link_embeds_manifest: bool,
    link_generates_manifest: bool,
    link_command: Vec<String>,
    user_manifests: Vec<String>,
    linker_manifest_file: String,
    manifest_file: String,
    manifest_file_rc: String,
    manifest_file_res: String,
    target_file: String,
    mt_path: String,
    rc_path: String,
}

impl VsLink {
    fn new(link_type: i32, verbose: bool) -> VsLink {
        VsLink {
            link_type,
            verbose,
            incremental: false,
            link_embeds_manifest: true,
            link_generates_manifest: true,
            link_command: Vec::new(),
            user_manifests: Vec::new(),
            linker_manifest_file: String::new(),
            manifest_file: String::new(),
            manifest_file_rc: String::new(),
            manifest_file_res: String::new(),
            target_file: String::new(),
            mt_path: String::new(),
            rc_path: String::new(),
        }
    }

    fn parse(&mut self, args: &[String]) -> bool {
        let mut int_dir = String::new();
        let mut index = 0usize;
        while index < args.len() && args[index].starts_with('-') {
            let arg = &args[index];
            if arg == "--" {
                index += 1;
                break;
            }
            if arg == "--manifests" {
                index += 1;
                while index < args.len() && !args[index].starts_with('-') {
                    self.user_manifests.push(args[index].clone());
                    index += 1;
                }
            } else if let Some(value) = arg.strip_prefix("--intdir=") {
                int_dir = value.to_owned();
                index += 1;
            } else if let Some(value) = arg.strip_prefix("--rc=") {
                self.rc_path = value.to_owned();
                index += 1;
            } else if let Some(value) = arg.strip_prefix("--mt=") {
                self.mt_path = value.to_owned();
                index += 1;
            } else if let Some(value) = arg.strip_prefix("--msvc-ver=") {
                if let Some(msvc_ver) = ascii_strtoul(value) {
                    self.link_embeds_manifest = msvc_ver > 1600;
                }
                index += 1;
            } else {
                eprintln!("unknown argument '{arg}'");
                return false;
            }
        }
        if int_dir.is_empty() || index == args.len() {
            return false;
        }
        self.link_command.extend_from_slice(&args[index..]);
        for arg in &self.link_command {
            if arg.eq_ignore_ascii_case("/INCREMENTAL:YES")
                || arg.eq_ignore_ascii_case("-INCREMENTAL:YES")
                || arg.eq_ignore_ascii_case("/INCREMENTAL")
                || arg.eq_ignore_ascii_case("-INCREMENTAL")
            {
                self.incremental = true;
            } else if arg.eq_ignore_ascii_case("/INCREMENTAL:NO")
                || arg.eq_ignore_ascii_case("-INCREMENTAL:NO")
            {
                self.incremental = false;
            } else if arg.eq_ignore_ascii_case("/MANIFEST:NO")
                || arg.eq_ignore_ascii_case("-MANIFEST:NO")
            {
                self.link_generates_manifest = false;
            } else if arg.starts_with("/Fe") || arg.starts_with("-Fe") {
                self.target_file = arg[3..].to_owned();
            } else if arg.starts_with("/out:") || arg.starts_with("-out:") {
                self.target_file = arg[5..].to_owned();
            }
        }
        if self.target_file.is_empty() {
            return false;
        }
        self.manifest_file = format!("{int_dir}/embed.manifest");
        self.linker_manifest_file = format!("{int_dir}/intermediate.manifest");
        if self.incremental {
            self.manifest_file_rc = format!("{int_dir}/manifest.rc");
            self.manifest_file_res = format!("{int_dir}/manifest.res");
        }
        if self.link_generates_manifest && (self.incremental || !self.link_embeds_manifest) {
            self.link_command.push("/MANIFEST".to_owned());
            self.link_command
                .push(format!("/MANIFESTFILE:{}", self.linker_manifest_file));
        }
        true
    }

    fn link(mut self) -> i32 {
        if self.incremental && (self.link_generates_manifest || !self.user_manifests.is_empty()) {
            if self.verbose {
                println!("Visual Studio Incremental Link with embedded manifests");
            }
            return self.link_incremental();
        }
        if self.verbose {
            if !self.incremental {
                println!("Visual Studio Non-Incremental Link");
            } else {
                println!("Visual Studio Incremental Link without manifests");
            }
        }
        self.link_non_incremental()
    }

    fn link_incremental(&mut self) -> i32 {
        let abs_manifest_file = pathresolver::to_normalized_path_on_disk(&self.manifest_file);
        if self.verbose {
            println!("Create {}", self.manifest_file_rc);
        }
        {
            let Ok(mut output) = File::create(&self.manifest_file_rc) else {
                return -1;
            };
            let _ = write!(
                output,
                "#pragma code_page(65001)\n{} /* CREATEPROCESS_MANIFEST_RESOURCE_ID */ 24 /* RT_MANIFEST */ \"{}\"",
                self.link_type, abs_manifest_file
            );
        }
        if !file_exists(&self.manifest_file) {
            if self.verbose {
                println!("Create empty: {}", self.manifest_file);
            }
            if self.user_manifests.is_empty() {
                let _ = File::create(&self.manifest_file);
            } else {
                self.run_mt(&format!("/out:{}", self.manifest_file), false);
            }
        }
        let rc_command = vec![
            if self.rc_path.is_empty() {
                "rc".to_owned()
            } else {
                self.rc_path.clone()
            },
            "/fo".to_owned(),
            self.manifest_file_res.clone(),
            self.manifest_file_rc.clone(),
        ];
        if !run_command("RC Pass 1", &rc_command, self.verbose, false, None, None) {
            return -1;
        }
        self.link_command.push(self.manifest_file_res.clone());
        if !run_command(
            "LINK Pass 1",
            &self.link_command,
            self.verbose,
            false,
            None,
            None,
        ) {
            return -1;
        }
        let mt_ret = self.run_mt(&format!("/out:{}", self.manifest_file), true);
        if !mt_ret_is_update(mt_ret) {
            return mt_ret;
        }
        if !run_command("RC Pass 2", &rc_command, self.verbose, false, None, None) {
            return -1;
        }
        if !run_command(
            "FINAL LINK",
            &self.link_command,
            self.verbose,
            false,
            None,
            None,
        ) {
            return -1;
        }
        0
    }

    fn link_non_incremental(&mut self) -> i32 {
        if !self.link_embeds_manifest {
            if !run_command("LINK", &self.link_command, self.verbose, false, None, None) {
                return -1;
            }
            if !self.link_generates_manifest && self.user_manifests.is_empty() {
                return 0;
            }
            let mt_out = format!(
                "/outputresource:{};#{}",
                self.target_file,
                if self.link_type == 1 { 1 } else { 2 }
            );
            return self.run_mt(&mt_out, false);
        }
        if util::is_full_path(&self.rc_path) {
            let rc_dir = util::filename_path(&self.rc_path);
            let path = match std::env::var("PATH") {
                Ok(path) => format!("{path}:{rc_dir}"),
                Err(_) => rc_dir,
            };
            unsafe { std::env::set_var("PATH", path) };
        }
        if self.link_generates_manifest || !self.user_manifests.is_empty() {
            self.link_command.push(format!(
                "/MANIFEST:EMBED,ID={}",
                if self.link_type == 1 { '1' } else { '2' }
            ));
            for manifest in &self.user_manifests {
                self.link_command.push(format!("/MANIFESTINPUT:{manifest}"));
            }
        }
        if !run_command("LINK", &self.link_command, self.verbose, false, None, None) {
            return -1;
        }
        0
    }

    fn run_mt(&self, out: &str, notify: bool) -> i32 {
        let mut mt_command = vec![
            if self.mt_path.is_empty() {
                "mt".to_owned()
            } else {
                self.mt_path.clone()
            },
            "/nologo".to_owned(),
        ];
        if self.link_generates_manifest && file_exists(&self.linker_manifest_file) {
            mt_command.push("/manifest".to_owned());
            mt_command.push(self.linker_manifest_file.clone());
        }
        for manifest in &self.user_manifests {
            mt_command.push("/manifest".to_owned());
            mt_command.push(manifest.clone());
        }
        mt_command.push(out.to_owned());
        if notify {
            mt_command.push("/notify_update".to_owned());
        }
        let mut mt_ret = 0;
        if !run_command(
            "MT",
            &mt_command,
            self.verbose,
            true,
            Some(&mut mt_ret),
            Some(mt_ret_is_update),
        ) {
            return -1;
        }
        mt_ret
    }
}

fn mt_ret_is_update(mt_ret: i32) -> bool {
    mt_ret == 0x41020001 || mt_ret == 0xbb
}

fn run_command(
    comment: &str,
    command: &[String],
    verbose: bool,
    hex_exit: bool,
    ret_code_out: Option<&mut i32>,
    ret_code_okay: Option<fn(i32) -> bool>,
) -> bool {
    if verbose {
        println!("{comment}:");
        println!("{}", command.join(" "));
    }
    let result = run_single_command(command, true);
    let ret_code_success = result.ret == 0 || ret_code_okay.is_some_and(|okay| okay(result.ret));
    let success = result.ok && ret_code_success;
    if let Some(out) = ret_code_out {
        if result.ok || !ret_code_success {
            *out = result.ret;
        } else {
            *out = -1;
        }
    }
    if !success {
        let exit_code = if hex_exit {
            format!("0x{:x}", result.ret)
        } else {
            result.ret.to_string()
        };
        print!(
            "{comment}: command \"{}\" failed (exit code {exit_code}) with the following output:\n{}",
            command.join(" "),
            result.stdout
        );
    } else if verbose && !result.stdout.contains("Resource Compiler Version") {
        print!("{}", result.stdout);
    }
    success
}

pub(crate) fn visual_studio_link(args: &[String], link_type: i32) -> i32 {
    let verbose = std::env::var_os("VERBOSE").is_some();
    let mut expanded_args = Vec::new();
    for arg in args {
        if arg.starts_with('@') && !arg.starts_with("@CMakeFiles") {
            if let Ok(bytes) = fs::read(&arg[1..]) {
                let text = String::from_utf8_lossy(&bytes);
                for line in stream_lines(&text, true) {
                    parse_windows_command_line(line, &mut expanded_args);
                }
            }
        } else {
            expanded_args.push(arg.clone());
        }
    }
    let mut link = VsLink::new(link_type, verbose);
    if !link.parse(&expanded_args) {
        return -1;
    }
    link.link()
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let mut start = 0;
    while start < bytes.len() && is_space(bytes[start]) {
        start += 1;
    }
    let mut end = bytes.len();
    while end > start && is_space(bytes[end - 1]) {
        end -= 1;
    }
    &bytes[start..end]
}

fn parse_lines(data: &[u8], mut process: impl FnMut(&[u8])) {
    let mut line = Vec::new();
    for &byte in data {
        if byte == b'\n' || byte == 0 {
            process(&line);
            line.clear();
        } else if byte != b'\r' {
            line.push(byte);
        }
    }
}

pub(crate) fn cl_compile_depends(args: &[String]) -> i32 {
    let mut dep_file = String::new();
    let mut current_binary_dir = String::new();
    let mut filter_prefix = String::new();
    let mut command = Vec::new();
    let mut index = 0usize;
    while index < args.len() {
        let arg = &args[index];
        if let Some(value) = arg.strip_prefix("--dep-file=") {
            dep_file = value.to_owned();
        } else if let Some(value) = arg.strip_prefix("--working-dir=") {
            current_binary_dir = value.to_owned();
        } else if let Some(value) = arg.strip_prefix("--filter-prefix=") {
            filter_prefix = value.to_owned();
        } else if arg == "--" {
            command.extend_from_slice(&args[index + 1..]);
            break;
        } else {
            return 1;
        }
        index += 1;
    }
    let Ok(mut dep_out) = File::create(&dep_file) else {
        return 3;
    };
    if command.is_empty() {
        unsafe {
            libc::signal(libc::SIGSEGV, libc::SIG_DFL);
            libc::raise(libc::SIGSEGV);
        }
        return 1;
    }
    let mut child = Command::new(&command[0]);
    child
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !current_binary_dir.is_empty() {
        child.current_dir(&current_binary_dir);
    }
    let result = child.output();
    let (status, output) = match result {
        Ok(output) => {
            let status = if output.status.code().is_none() {
                1
            } else {
                output.status.code().unwrap_or(0)
            };
            (status, Some(output))
        }
        Err(_) => (2, None),
    };
    if let Some(output) = output {
        let mut stdout = io::stdout().lock();
        parse_lines(&output.stdout, |line| {
            if line.starts_with(filter_prefix.as_bytes()) {
                let path = trim_ascii_whitespace(&line[filter_prefix.len()..]);
                let _ = dep_out.write_all(path);
                let _ = dep_out.write_all(b"\n");
            } else {
                let _ = stdout.write_all(line);
                let _ = stdout.write_all(b"\n");
            }
        });
        let mut stderr = io::stderr().lock();
        parse_lines(&output.stderr, |line| {
            let _ = stderr.write_all(line);
            let _ = stderr.write_all(b"\n");
        });
    }
    if status != 0 {
        drop(dep_out);
        let _ = fs::remove_file(&dep_file);
    }
    status
}
