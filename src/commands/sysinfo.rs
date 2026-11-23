//! Host information, variable watches, and miscellaneous configure commands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cache::{Cache, CacheType, parse_entry};
use crate::interp::{CmdResult, Interp, Mode, err};
use crate::messages::MessageType;
use crate::policy::{PolicyStatus, index_of, policy_warning};
use crate::properties::{self, Object};
use crate::util::{expand_list, is_off};

pub fn register(interp: &mut Interp) {
    for (name, function) in [
        ("variable_watch", variable_watch as crate::interp::Builtin),
        ("site_name", site_name),
        ("cmake_host_system_information", host_system_information),
        ("load_cache", load_cache),
        ("include_regular_expression", include_regular_expression),
        ("cmake_file_api", file_api),
        ("exec_program", exec_program),
        ("build_command", build_command),
    ] {
        interp.register(name, function);
    }
}

fn variable_watch(interp: &mut Interp, args: &[String]) -> CmdResult {
    let variable = args
        .first()
        .ok_or("must be called with at least one argument.")?;
    if variable == "CMAKE_CURRENT_LIST_FILE" {
        return err("cannot be set on the variable: CMAKE_CURRENT_LIST_FILE");
    }
    let callback = args.get(1).cloned().unwrap_or_default();
    interp
        .ext
        .variable_watches
        .entry(variable.clone())
        .or_default()
        .push(callback);
    Ok(())
}

/// The interpreter calls this after writing a binding and before completing a
/// watched read. A callback may re-enter the interpreter or add more watches.
pub fn notify_variable(interp: &mut Interp, variable: &str, access: &str, value: &str) {
    let Some(callbacks) = interp.ext.variable_watches.get(variable).cloned() else {
        return;
    };
    for (index, callback) in callbacks.into_iter().enumerate() {
        let key = (variable.to_owned(), index);
        if !interp.ext.active_variable_watches.insert(key.clone()) {
            continue;
        }
        if callback.is_empty() {
            interp.issue(
                MessageType::Log,
                &format!(
                    "Variable \"{variable}\" was accessed using {access} with value \"{value}\"."
                ),
            );
        } else {
            let stack = properties::listfile_stack(interp);
            let file = interp.get_safe("CMAKE_CURRENT_LIST_FILE");
            // `cmVariableWatchCommandVariableAccessed`: the callback runs
            // through `cmMakefile::ExecuteCommand` on a synthetic function
            // whose arguments carry the maximum fake line number.
            let fake_line = i64::MAX as usize;
            let args: Vec<crate::lexer::Arg> = [
                variable.to_owned(),
                access.to_owned(),
                value.to_owned(),
                file.clone(),
                stack,
            ]
            .into_iter()
            .map(|value| crate::lexer::Arg {
                value,
                delim: crate::lexer::Delim::Quoted,
                line: fake_line,
            })
            .collect();
            let command = crate::lexer::Command {
                name: callback.clone(),
                lname: callback.to_ascii_lowercase(),
                line: fake_line,
                line_end: fake_line,
                args,
            };
            let parsed = std::rc::Rc::new(crate::interp::ParsedFile::new(
                std::rc::Rc::from(file.as_str()),
                vec![command],
            ));
            let command = parsed.commands[0].clone();
            let (ok, _) = if interp.fatal {
                (true, crate::interp::Ctl::Normal)
            } else {
                interp.execute(&parsed, &command, None)
            };
            if !ok {
                eprintln!(
                    "CMake Error: Error in cmake code at\nUnknown:0:\nA command failed during the invocation of callback \"{callback}\"."
                );
            }
        }
        interp.ext.active_variable_watches.remove(&key);
    }
}

fn site_name(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [output] = args else {
        return err("called with incorrect number of arguments");
    };
    if interp.read_definition(output).is_some() {
        return Ok(());
    }
    let command = interp
        .read_definition("HOSTNAME")
        .unwrap_or_else(find_hostname_program);
    let name = if is_off(&command) {
        None
    } else {
        shlex::split(&command).and_then(|args| {
            let (program, args) = args.split_first()?;
            Command::new(program)
                .args(args)
                .stderr(Stdio::null())
                .output()
                .ok()
        })
    };
    let name = name
        .as_ref()
        .and_then(|result| std::str::from_utf8(&result.stdout).ok())
        .and_then(|text| text.split_ascii_whitespace().next())
        .unwrap_or("unknown");
    interp.add_cache_def(
        output,
        Some(name),
        Some("Name of the computer/site where compile is being run"),
        CacheType::String,
        false,
    );
    Ok(())
}

fn find_hostname_program() -> String {
    use std::os::unix::fs::PermissionsExt;

    let search = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&search)
        .chain(
            [
                "/usr/bsd",
                "/usr/sbin",
                "/usr/bin",
                "/bin",
                "/sbin",
                "/usr/local/bin",
            ]
            .into_iter()
            .map(std::path::PathBuf::from),
        )
        .map(|dir| dir.join("hostname"))
        .find(|path| {
            path.metadata()
                .is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
        })
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn load_cache(interp: &mut Interp, args: &[String]) -> CmdResult {
    let dir = args
        .first()
        .ok_or("called with wrong number of arguments.")?;
    if args.get(1).is_some_and(|arg| arg == "READ_WITH_PREFIX") {
        let prefix = args
            .get(2)
            .ok_or("READ_WITH_PREFIX form must specify a prefix.")?;
        let path = format!("{dir}/CMakeCache.txt");
        if !Path::new(&path).exists() {
            return err(format!("Cannot load cache file from {path}"));
        }
        if let Ok(contents) = std::fs::read_to_string(path) {
            for (name, value, _) in contents.lines().filter_map(parse_entry) {
                if args[3..].contains(&name) {
                    let key = format!("{prefix}{name}");
                    if value.is_empty() {
                        interp.unset_def(&key);
                    } else {
                        interp.set_def(key, value);
                    }
                }
            }
        }
        return Ok(());
    }
    if interp.mode == Mode::Script {
        return err("Only load_cache(READ_WITH_PREFIX) may be used in script mode");
    }
    let files = args
        .iter()
        .take_while(|arg| !matches!(arg.as_str(), "EXCLUDE" | "INCLUDE_INTERNALS"));
    let selection = |keyword: &str, end: &str| {
        args.iter()
            .skip_while(|arg| arg.as_str() != keyword)
            .skip(1)
            .take_while(|arg| arg.as_str() != end)
            .collect::<BTreeSet<_>>()
    };
    let excludes = selection("EXCLUDE", "INCLUDE_INTERNALS");
    let includes = selection("INCLUDE_INTERNALS", "EXCLUDE");
    for dir in files {
        let mut cache = Cache::default();
        match cache.load(Path::new(dir)) {
            Ok(true) => {
                for (name, mut entry) in cache.entries {
                    if excludes.contains(&name)
                        || (entry.kind == CacheType::Internal && !includes.contains(&name))
                    {
                        continue;
                    }
                    entry.kind = CacheType::Internal;
                    entry.properties.insert("HELPSTRING".into(), format!("DO NOT EDIT, {name} loaded from external file.  To change this value edit this file: {dir}/CMakeCache.txt"));
                    interp.cache.entries.insert(name, entry);
                }
            }
            Ok(false) => interp.issue(
                MessageType::FatalError,
                &format!("Cannot load cache file from {dir}/CMakeCache.txt"),
            ),
            Err(error) => interp.issue(MessageType::FatalError, &error),
        }
    }
    Ok(())
}

fn include_regular_expression(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == Mode::Script {
        return err("command is not scriptable");
    }
    if !(1..=2).contains(&args.len()) {
        return err("called with incorrect number of arguments");
    }
    let directory = Object::Directory(interp.mf_index());
    properties::set(
        interp,
        directory,
        "INCLUDE_REGULAR_EXPRESSION",
        Some(&args[0]),
    );
    if let Some(complain) = args.get(1) {
        properties::set(
            interp,
            directory,
            "COMPLAIN_REGULAR_EXPRESSION",
            Some(complain),
        );
    }
    Ok(())
}

fn file_api(interp: &mut Interp, args: &[String]) -> CmdResult {
    if interp.mode == Mode::Script {
        return err("command is not scriptable");
    }
    let subcommand = args.first().ok_or("must be called with arguments.")?;
    if subcommand != "QUERY" {
        return err(format!("does not recognize sub-command {subcommand}"));
    }
    let modes = [
        ("CODEMODEL", 2, 8),
        ("CACHE", 2, 0),
        ("CMAKEFILES", 1, 1),
        ("TOOLCHAINS", 1, 0),
    ];
    let mut values = BTreeMap::<&str, Vec<&str>>::new();
    let mut field = "";
    for arg in &args[1..] {
        if arg == "API_VERSION" || modes.iter().any(|(name, ..)| name == arg) {
            values.entry(arg).or_default();
            field = arg;
        } else if field.is_empty() {
            return err(format!(
                "QUERY subcommand given unknown argument \"{arg}\"."
            ));
        } else {
            values.entry(field).or_default().push(arg);
            if field == "API_VERSION" {
                field = "";
            }
        }
    }
    if let Some((keyword, _)) = values.iter().find(|(_, values)| values.is_empty()) {
        interp.issue(
            MessageType::FatalError,
            &format!("Error after keyword \"{keyword}\":\n  missing required value\n"),
        );
        return Ok(());
    }
    let api = values
        .get("API_VERSION")
        .and_then(|values| values.last())
        .copied()
        .unwrap_or_default();
    if !api.bytes().all(|byte| byte.is_ascii_digit()) {
        return err("QUERY subcommand given a non-integer API_VERSION.");
    }
    if api.parse::<u32>() != Ok(1) {
        return err(format!(
            "QUERY subcommand given an unsupported API_VERSION \"{api}\" (the only currently supported version is 1)."
        ));
    }
    let mut errors = Vec::new();
    for (keyword, major, minor) in modes {
        let Some(versions) = values.get(keyword) else {
            continue;
        };
        let mut error = format!(
            "None of the specified {keyword} versions is supported by this version of CMake."
        );
        for version in versions {
            let atoi = |text: &str| {
                std::ffi::CString::new(text).ok().map_or(0, |text| {
                    // SAFETY: atoi receives a valid C string for the duration of the call.
                    unsafe { libc::atoi(text.as_ptr()) }
                })
            };
            let requested_major = atoi(version);
            let requested_minor = version.split_once('.').map_or(0, |(_, text)| atoi(text));
            if requested_major < 1 || requested_minor < 0 {
                error = format!("Given a malformed version \"{version}\" for {keyword}.");
                break;
            }
            if requested_major == major && requested_minor <= minor {
                interp
                    .ext
                    .file_api_queries
                    .insert(keyword.into(), (major as u32, requested_minor as u32));
                error.clear();
                break;
            }
        }
        if !error.is_empty() {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        err(format!(
            "QUERY subcommand was given invalid arguments:\n  {}",
            errors.join("\n  ")
        ))
    }
}

fn exec_program(interp: &mut Interp, args: &[String]) -> CmdResult {
    match interp.policy_status("CMP0153") {
        PolicyStatus::New => {
            interp.issue(MessageType::FatalError, "The exec_program command should not be called; see CMP0153. Use execute_process() instead.");
            return Ok(());
        }
        PolicyStatus::Warn => {
            let warning = policy_warning(index_of("CMP0153").expect("registered policy"));
            interp.issue(
                MessageType::AuthorWarning,
                &format!("{warning}\nUse execute_process() instead."),
            );
        }
        PolicyStatus::Old => {}
    }
    let Some((program, tail)) = args.split_first() else {
        return err("called with incorrect number of arguments");
    };
    let mut arguments = Vec::new();
    let mut positional = Vec::new();
    let mut field = "";
    let mut output = "";
    let mut result = "";
    for arg in tail {
        if arg == "OUTPUT_VARIABLE" || (field != "OUTPUT_VARIABLE" && arg == "RETURN_VALUE") {
            field = arg;
        } else if field == "OUTPUT_VARIABLE" {
            if !output.is_empty() {
                return err("called with incorrect number of arguments");
            }
            output = arg;
            field = "";
        } else if field == "RETURN_VALUE" {
            if !result.is_empty() {
                return err("called with incorrect number of arguments");
            }
            result = arg;
            field = "";
        } else if arg == "ARGS" {
            field = arg;
        } else if field == "ARGS" {
            arguments.push(arg.as_str());
        } else {
            positional.push(arg);
        }
    }
    let mut command = if arguments.is_empty() {
        program.clone()
    } else {
        format!(
            "{} {} ",
            shlex::try_quote(program).map_or_else(|_| program.clone(), |text| text.into_owned()),
            arguments.join(" ")
        )
    };
    if let [dir] = positional.as_slice() {
        let _ = std::fs::create_dir_all(dir);
        command = format!("cd \"{dir}\" && {command}");
    }
    command.push_str(" 2>&1");
    if output.is_empty() {
        interp.stdout_line(&format!("running {command}"));
    }
    let run = Command::new("/bin/sh").args(["-c", &command]).output();
    let (code, text) = run.map_or_else(
        |error| (-1, format!("\nProcess failed because: {error}")),
        |result| {
            (
                result.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&result.stdout).into_owned(),
            )
        },
    );
    if output.is_empty() {
        print!("{text}");
    } else {
        let trimmed = text.trim_matches([' ', '\n', '\r', '\t']);
        interp.set_def(
            output,
            if trimmed.is_empty() {
                text.as_str()
            } else {
                trimmed
            },
        );
    }
    if !result.is_empty() {
        interp.set_def(result, code.to_string());
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct HostInformation {
    os: BTreeMap<&'static str, String>,
    cpu: BTreeMap<String, String>,
    memory: BTreeMap<String, u64>,
    logical: usize,
    physical: usize,
}

pub(crate) fn instrumentation_static_info() -> serde_json::Value {
    let info = HostInformation::read();
    let os = |name| info.os.get(name).map_or("", String::as_str);
    let cpu = |name| info.cpu.get(name).map_or("", String::as_str);
    let memory = |name| info.memory.get(name).copied().unwrap_or(0) / 1024;
    let vendor = cpu("vendor_id");
    serde_json::json!({
        "familyId": cpu("cpu family"),
        "hostname": os("HOSTNAME"),
        "is64Bits": usize::BITS == 64,
        "modelId": cpu("model"),
        "numberOfLogicalCPU": info.logical,
        "numberOfPhysicalCPU": info.physical,
        "OSName": os("OS_NAME"),
        "OSPlatform": os("OS_PLATFORM"),
        "OSRelease": os("OS_RELEASE"),
        "OSVersion": os("OS_VERSION"),
        "processorAPICID": cpu("apicid").parse::<i32>().unwrap_or(-1),
        "processorCacheSize": cpu("cache size").split_whitespace().next().and_then(|s| s.parse::<i32>().ok()).unwrap_or(-1),
        "processorClockFrequency": cpu("cpu MHz").parse::<f64>().unwrap_or(0.0),
        "processorName": cpu("model name"),
        "totalPhysicalMemory": memory("MemTotal"),
        "totalVirtualMemory": memory("SwapTotal"),
        "vendorString": vendor,
        "vendorID": match vendor {
            "GenuineIntel" => "Intel Corporation",
            "AuthenticAMD" => "Advanced Micro Devices",
            _ => vendor,
        },
    })
}

impl HostInformation {
    fn read() -> Self {
        let mut info = Self::default();
        // SAFETY: uname writes a complete utsname on success. Its fields
        // are NUL-terminated arrays under the POSIX contract.
        unsafe {
            let mut buffer = std::mem::MaybeUninit::<libc::utsname>::uninit();
            if libc::uname(buffer.as_mut_ptr()) == 0 {
                let buffer = buffer.assume_init();
                for (key, value) in [
                    ("OS_NAME", &buffer.sysname),
                    ("OS_RELEASE", &buffer.release),
                    ("OS_VERSION", &buffer.version),
                    ("OS_PLATFORM", &buffer.machine),
                    ("HOSTNAME", &buffer.nodename),
                ] {
                    info.os.insert(
                        key,
                        std::ffi::CStr::from_ptr(value.as_ptr())
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        let cpus = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let mut packages = BTreeMap::new();
        for section in cpus.split("\n\n") {
            let details: BTreeMap<_, _> = section
                .lines()
                .filter_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    Some((key.trim().to_owned(), value.trim().to_owned()))
                })
                .collect();
            if details.contains_key("processor") {
                info.logical += 1;
                if let Some(package) = details.get("physical id") {
                    let cores = details
                        .get("cpu cores")
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(1);
                    packages.insert(package.clone(), cores);
                }
            }
            if info.cpu.is_empty() {
                info.cpu = details;
            }
        }
        info.logical = info.logical.max(1);
        info.physical = if packages.is_empty() {
            info.logical
        } else {
            packages.values().sum()
        };
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        for (key, value) in meminfo.lines().filter_map(|line| line.split_once(':')) {
            if let Some(value) = value
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok())
            {
                info.memory.insert(key.into(), value);
            }
        }
        info
    }

    fn query(&self, key: &str) -> Option<String> {
        let memory = |key: &str| self.memory.get(key).copied().unwrap_or(0);
        let flag = |flag: &str| {
            self.cpu
                .get("flags")
                .is_some_and(|flags| flags.split_whitespace().any(|value| value == flag))
        };
        let boolean = |value: bool| if value { "1" } else { "0" }.to_owned();
        Some(match key {
            "FQDN" => {
                fully_qualified_name(self.os.get("HOSTNAME").map_or("localhost", String::as_str))
            }
            "NUMBER_OF_LOGICAL_CORES" => self.logical.to_string(),
            "NUMBER_OF_PHYSICAL_CORES" => self.physical.to_string(),
            "TOTAL_VIRTUAL_MEMORY" => (memory("SwapTotal") / 1024).to_string(),
            "AVAILABLE_VIRTUAL_MEMORY" => (memory("SwapFree") / 1024).to_string(),
            "TOTAL_PHYSICAL_MEMORY" => (memory("MemTotal") / 1024).to_string(),
            "AVAILABLE_PHYSICAL_MEMORY" => {
                ((memory("MemFree") + memory("Buffers") + memory("Cached")) / 1024).to_string()
            }
            "IS_64BIT" => boolean(usize::BITS == 64),
            "HAS_FPU" => boolean(flag("fpu")),
            "HAS_MMX" => boolean(flag("mmx")),
            "HAS_MMX_PLUS" => boolean(flag("mmxext")),
            "HAS_SSE" => boolean(flag("sse")),
            "HAS_SSE2" => boolean(flag("sse2")),
            "HAS_SSE_FP" | "HAS_SSE_MMX" | "HAS_IA64" => boolean(false),
            "HAS_AMD_3DNOW" => boolean(flag("3dnow")),
            "HAS_AMD_3DNOW_PLUS" => boolean(flag("3dnowext")),
            "HAS_SERIAL_NUMBER" => boolean(flag("pn")),
            "PROCESSOR_SERIAL_NUMBER" => String::new(),
            "PROCESSOR_NAME" => self.processor_name(),
            "PROCESSOR_DESCRIPTION" => {
                let model = self
                    .cpu
                    .get("model name")
                    .cloned()
                    .unwrap_or_else(|| self.processor_name());
                format!(
                    "{} core {}",
                    self.physical,
                    model.split_whitespace().collect::<Vec<_>>().join(" ")
                )
            }
            _ => self.os.get(key)?.clone(),
        })
    }

    fn processor_name(&self) -> String {
        let cpu = |key: &str| self.cpu.get(key).map_or("", String::as_str);
        match (cpu("vendor_id"), cpu("cpu family"), cpu("model")) {
            ("GenuineIntel", "6", "1") => "Pentium Pro",
            ("GenuineIntel", "6", "3" | "5" | "6") => "Pentium II",
            ("GenuineIntel", "6", "7" | "8" | "10" | "11") => "Pentium III",
            ("GenuineIntel", "6", _) => "Unknown P6 family",
            ("GenuineIntel", "15", _) => "Pentium 4",
            _ => return cpu("model name").to_owned(),
        }
        .into()
    }
}

fn fully_qualified_name(hostname: &str) -> String {
    let mut addresses = std::ptr::null_mut();
    // SAFETY: on success getifaddrs returns a linked list owned by this
    // call. The information stays valid until freeifaddrs.
    unsafe {
        if libc::getifaddrs(&mut addresses) != 0 {
            return hostname.to_owned();
        }
        let mut result = hostname.to_owned();
        let mut entry = addresses;
        while let Some(interface) = entry.as_ref() {
            entry = interface.ifa_next;
            let Some(address) = interface.ifa_addr.as_ref() else {
                continue;
            };
            if interface.ifa_flags & libc::IFF_LOOPBACK as libc::c_uint != 0 {
                continue;
            }
            let length = match i32::from(address.sa_family) {
                libc::AF_INET => std::mem::size_of::<libc::sockaddr_in>(),
                libc::AF_INET6 => std::mem::size_of::<libc::sockaddr_in6>(),
                _ => continue,
            };
            let mut host = [0; libc::NI_MAXHOST as usize];
            if libc::getnameinfo(
                address,
                length as libc::socklen_t,
                host.as_mut_ptr(),
                host.len() as libc::socklen_t,
                std::ptr::null_mut(),
                0,
                libc::NI_NAMEREQD,
            ) == 0
            {
                let candidate = std::ffi::CStr::from_ptr(host.as_ptr()).to_string_lossy();
                if candidate.len() > hostname.len() && candidate.contains(hostname) {
                    result = candidate.into_owned();
                    break;
                }
            }
        }
        libc::freeifaddrs(addresses);
        result
    }
}

fn parse_os_release(line: &str) -> Option<(String, String)> {
    let (key, value) = line
        .trim_start_matches(char::is_whitespace)
        .split_once('=')?;
    let mut key_bytes = key.bytes();
    if !key_bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !key_bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return None;
    }
    let mut chars = value.chars();
    let first = chars.next()?;
    let mut text = String::new();
    match first {
        '\'' | '"' => {
            for ch in chars {
                if ch == first && !text.ends_with('\\') {
                    break;
                }
                if ch == first {
                    text.pop();
                }
                text.push(ch);
            }
        }
        '#' | '\\' => return None,
        _ => {
            text.push(first);
            text.extend(chars.take_while(|ch| *ch != '#' && !ch.is_ascii_whitespace()));
        }
    }
    (!text.is_empty()).then(|| (key.to_owned(), text))
}

fn distribution(interp: &mut Interp) -> BTreeMap<String, String> {
    let sysroot = interp.get_safe("CMAKE_SYSROOT");
    for path in ["/etc/os-release", "/usr/lib/os-release"] {
        let path = format!("{sysroot}{path}");
        if Path::new(&path).is_file() {
            let mut data = BTreeMap::new();
            if let Ok(text) = std::fs::read_to_string(path) {
                for (key, value) in text.lines().filter_map(parse_os_release) {
                    data.entry(key).or_insert(value);
                }
            }
            if !data.is_empty() {
                return data;
            }
            break;
        }
    }
    let root = format!("{}/Modules/Internal/OSRelease", interp.cmake_root);
    let mut scripts: Vec<_> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path().to_string_lossy().into_owned())
        .collect();
    scripts.extend(expand_list(
        &interp.get_safe("CMAKE_GET_OS_RELEASE_FALLBACK_SCRIPTS"),
        false,
    ));
    let order = |path: &str| {
        let name = Path::new(path).file_name()?.to_str()?;
        let prefix = name.get(..3)?;
        (name.len() >= 11
            && prefix.bytes().all(|byte| byte.is_ascii_digit())
            && name.as_bytes()[3] == b'-')
            .then(|| prefix.parse::<u32>().ok())
            .flatten()
    };
    scripts.retain(|path| order(path).is_some());
    scripts.sort_by_cached_key(|path| order(path));
    let result_name = "CMAKE_GET_OS_RELEASE_FALLBACK_RESULT";
    let mut data = BTreeMap::new();
    for script in scripts {
        interp.unset_def(result_name);
        if !interp.read_dependent_file(&script, false) || interp.error {
            continue;
        }
        for variable in expand_list(&interp.get_safe(result_name), false) {
            let value = interp.get_safe(&variable);
            interp.unset_def(&variable);
            if let Some(key) = variable.strip_prefix("CMAKE_GET_OS_RELEASE_FALLBACK_RESULT_") {
                data.entry(key.to_owned()).or_insert(value);
            }
        }
        if !data.is_empty() {
            data.insert("USED_FALLBACK_SCRIPT".into(), script);
            break;
        }
    }
    interp.unset_def(result_name);
    data
}

fn host_system_information(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 || args[0] != "RESULT" {
        return err("missing RESULT specification.");
    }
    if args.len() < 4 || args[2] != "QUERY" {
        return err("missing QUERY specification");
    }
    let output = &args[1];
    if args[3] == "WINDOWS_REGISTRY" {
        return registry_query(interp, output, &args[4..]);
    }
    let mut result = String::new();
    for key in &args[3..] {
        if !result.is_empty() {
            result.push(';');
        }
        if let Some(key) = key.strip_prefix("DISTRIB_") {
            if interp.ext.distribution.is_none() {
                interp.ext.distribution = Some(distribution(interp));
            }
            let data = interp
                .ext
                .distribution
                .as_ref()
                .expect("initialized distribution");
            if key == "INFO" {
                let values: Vec<_> = data
                    .iter()
                    .map(|(key, value)| (format!("{output}_{key}"), value.clone()))
                    .collect();
                result.push_str(
                    &values
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(";"),
                );
                for (name, value) in values {
                    interp.set_def(name, value);
                }
            } else if let Some(value) = data.get(key) {
                result.push_str(value);
            }
        } else {
            let info = interp
                .ext
                .host_information
                .get_or_insert_with(HostInformation::read);
            let value = info
                .query(key)
                .ok_or_else(|| format!("does not recognize <key> {key}"))?;
            result.push_str(&value);
        }
    }
    interp.set_def(output, result);
    Ok(())
}

fn registry_query(interp: &mut Interp, result: &str, args: &[String]) -> CmdResult {
    let Some((_, args)) = args.split_first() else {
        return err("missing <key> specification.");
    };
    let mut options = BTreeMap::new();
    let mut unknown = Vec::new();
    let mut missing = BTreeSet::new();
    let keywords = [
        "VALUE",
        "VALUE_NAMES",
        "SUBKEYS",
        "VIEW",
        "SEPARATOR",
        "ERROR_VARIABLE",
    ];
    let mut index = 0;
    while let Some(arg) = args.get(index).map(String::as_str) {
        index += 1;
        match arg {
            "VALUE_NAMES" | "SUBKEYS" => {
                options.insert(arg, "1");
            }
            "VALUE" | "VIEW" | "SEPARATOR" | "ERROR_VARIABLE" => {
                if let Some(value) = args
                    .get(index)
                    .filter(|value| !keywords.contains(&value.as_str()))
                {
                    options.insert(arg, value.as_str());
                    index += 1;
                } else {
                    missing.insert(arg);
                }
            }
            _ => unknown.push(arg),
        }
    }
    if !unknown.is_empty() {
        return err(format!(
            "given invalid argument(s) \"{}\".",
            unknown.join(", ")
        ));
    }
    if !missing.is_empty() {
        let errors: String = missing
            .iter()
            .map(|keyword| {
                format!("Error after keyword \"{keyword}\":\n  missing required value\n")
            })
            .collect();
        interp.issue(MessageType::FatalError, &errors);
        return Ok(());
    }
    let view = options.get("VIEW").copied().unwrap_or_default();
    if !matches!(
        view,
        "" | "64" | "32" | "64_32" | "32_64" | "HOST" | "TARGET" | "BOTH"
    ) {
        return err(format!("given invalid value for VIEW: {view}."));
    }
    let value = options.get("VALUE").is_some_and(|value| !value.is_empty());
    if usize::from(value)
        + usize::from(options.contains_key("VALUE_NAMES"))
        + usize::from(options.contains_key("SUBKEYS"))
        > 1
    {
        return err("given mutually exclusive sub-options VALUE, VALUE_NAMES or SUBKEYS.");
    }
    interp.set_def(result, "");
    if let Some(error) = options
        .get("ERROR_VARIABLE")
        .filter(|name| !name.is_empty())
    {
        interp.set_def(*error, "No Registry on this platform.");
    }
    Ok(())
}

/// `cmBuildCommand`.
fn build_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    // Support the legacy signature of the command.
    if args.len() == 2 {
        let define = &args[0];
        let config = std::env::var("CMAKE_CONFIG_TYPE")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Release".to_owned());
        let command = generate_cmake_build_command(interp, "", &config, "");
        if interp.get_def(define).is_some() {
            return Ok(());
        }
        interp.add_cache_def(
            define,
            Some(&command),
            Some("Command used to build entire project from the command line."),
            CacheType::String,
            false,
        );
        return Ok(());
    }
    let Some(variable) = args.first() else {
        return err("requires at least one argument naming a CMake variable");
    };
    #[derive(PartialEq)]
    enum Doing {
        None,
        Configuration,
        ProjectName,
        Target,
        Parallel,
    }
    let mut doing = Doing::None;
    let (mut configuration, mut project_name, mut target, mut parallel) =
        (String::new(), String::new(), String::new(), String::new());
    for arg in &args[1..] {
        match arg.as_str() {
            "CONFIGURATION" => doing = Doing::Configuration,
            "PROJECT_NAME" => doing = Doing::ProjectName,
            "TARGET" => doing = Doing::Target,
            "PARALLEL_LEVEL" => doing = Doing::Parallel,
            _ => {
                let slot = match doing {
                    Doing::Configuration => &mut configuration,
                    Doing::ProjectName => &mut project_name,
                    Doing::Target => &mut target,
                    Doing::Parallel => &mut parallel,
                    Doing::None => return err(format!("unknown argument \"{arg}\"")),
                };
                *slot = arg.clone();
                doing = Doing::None;
            }
        }
    }
    if configuration.is_empty() {
        configuration = std::env::var("CMAKE_CONFIG_TYPE").unwrap_or_default();
    }
    if configuration.is_empty() {
        configuration = "Release".to_owned();
    }
    if !project_name.is_empty() {
        interp.issue(
            MessageType::AuthorWarning,
            "Ignoring PROJECT_NAME option because it has no effect.",
        );
    }
    let command = generate_cmake_build_command(interp, &target, &configuration, &parallel);
    interp.set_def(variable, command);
    Ok(())
}

/// `cmGlobalGenerator::GenerateCMakeBuildCommand` (no native options).
fn generate_cmake_build_command(
    interp: &Interp,
    target: &str,
    config: &str,
    parallel: &str,
) -> String {
    let mut command = format!(
        "{} --build .",
        convert_to_unix_output_path(&interp.cmake_command)
    );
    if !config.is_empty() {
        command.push_str(&format!(" --config \"{config}\""));
    }
    if !parallel.is_empty() {
        command.push_str(&format!(" --parallel \"{parallel}\""));
    }
    if !target.is_empty() {
        command.push_str(&format!(" --target \"{target}\""));
    }
    command
}

/// `kwsys::SystemTools::ConvertToUnixOutputPath`.
pub(crate) fn convert_to_unix_output_path(path: &str) -> String {
    // Remove "//" except at the beginning.
    let mut ret = path.to_owned();
    let mut pos = 1;
    while let Some(found) = ret.get(pos..).and_then(|rest| rest.find("//")) {
        ret.remove(pos + found);
        pos += found;
    }
    if !ret.contains(' ') {
        return ret;
    }
    let mut result = String::with_capacity(ret.len());
    let mut last = '\u{1}';
    for ch in ret.chars() {
        if ch == ' ' && last != '\\' {
            result.push('\\');
        }
        result.push(ch);
        last = ch;
    }
    result
}
