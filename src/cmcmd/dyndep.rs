//! cmGlobalNinjaGenerator's build-time Fortran scanner and dyndep collator.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use super::fortran::Parser;
use crate::commands::string::json::stream_value;
use crate::commands::sysinfo::convert_to_unix_output_path;
use crate::fsutil::{write_generated, write_if_different};
use crate::generator::ninja::{Build, write_build};
use crate::util;

fn read_json(command: &str, path: &str) -> Result<Value, String> {
    let data = std::fs::read(path)
        .map_err(|err| format!("{path}:1: -E {command} failed to open: {err}"))?;
    serde_json::from_slice(&data)
        .map_err(|err| format!("{path}:{}: -E {command} failed to parse: {err}", err.line()))
}

fn string<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or_default()
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map_or(&[], Vec::as_slice)
}

fn write_file(path: &str, text: &str, if_different: bool) -> Result<(), String> {
    let writer = if if_different {
        write_if_different
    } else {
        write_generated
    };
    writer(Path::new(path), text.as_bytes())
        .map_err(|err| format!("{path}:1: failed to write: {err}"))
}

pub(super) fn run(command: &str, args: &[String]) -> Result<(), String> {
    let scanning = command == "cmake_ninja_depends";
    let required: &[&str] = if scanning {
        &["tdi", "src", "out", "dep", "obj", "ddi", "lang"]
    } else {
        &["tdi", "lang", "dd"]
    };
    let mut full = Vec::new();
    for arg in args {
        if !scanning && let Some(path) = arg.strip_prefix('@') {
            let response = std::fs::read_to_string(path).map_err(|err| {
                format!(
                    "failed to open for reading ({}):\n  {path}",
                    super::os_error(&err)
                )
            })?;
            full.extend(crate::generator::flags::unix_command_line(
                response.lines().next().unwrap_or_default(),
            ));
        } else {
            full.push(arg.clone());
        }
    }
    let mut opts = BTreeMap::new();
    let mut ddis = Vec::new();
    for arg in &full {
        if let Some((key, value)) = arg.strip_prefix("--").and_then(|s| s.split_once('=')) {
            if scanning && key == "pp" {
                opts.insert("src", value);
                opts.insert("out", value);
                continue;
            }
            if required.contains(&key) || key == if scanning { "src-orig" } else { "modmapfmt" } {
                opts.insert(key, value);
                continue;
            }
        } else if !scanning && !arg.starts_with("--") && arg.ends_with(".ddi") {
            ddis.push(arg);
            continue;
        }
        return Err(format!("-E {command} unknown argument: {arg}"));
    }
    for &key in required {
        if opts.get(key).is_none_or(|s| s.is_empty()) {
            return Err(format!("-E {command} requires value for --{key}="));
        }
    }
    if opts["lang"] != "Fortran" {
        return Err(format!(
            "-E {command} does not understand the {} language",
            opts["lang"]
        ));
    }
    let tdi = read_json(command, opts["tdi"])?;
    if scanning {
        scan(&opts, &tdi)
    } else if let Some(fmt) = opts.get("modmapfmt").filter(|fmt| !fmt.is_empty()) {
        Err(format!(
            "{}:1: unsupported Fortran module map format {fmt}",
            opts["tdi"]
        ))
    } else {
        collate(&tdi, opts["dd"], &ddis)
    }
}

fn encode_filename(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_control() {
            use std::fmt::Write;
            let _ = write!(out, "\\u{:04x}", ch as u32);
        } else {
            if matches!(ch, '\\' | '"') {
                out.push('\\');
            }
            out.push(ch);
        }
    }
    out
}

fn scan(opts: &BTreeMap<&str, &str>, tdi: &Value) -> Result<(), String> {
    let mut search = Vec::new();
    if let Some(src) = opts.get("src-orig") {
        let parent = util::filename_path(src);
        if !parent.is_empty() {
            search.push(parent);
        }
    }
    search.extend(
        array(tdi, "include-dirs")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned),
    );
    let mut parser = Parser::new(
        search,
        string(tdi, "submodule-sep"),
        string(tdi, "submodule-ext"),
    );
    parser.scan(opts["src"])?;
    let mut deps = format!("{}:", convert_to_unix_output_path(opts["out"]));
    for inc in &parser.includes {
        deps.push_str(" \\\n ");
        deps.push_str(&convert_to_unix_output_path(inc));
    }
    deps.push('\n');
    write_file(opts["dep"], &deps, false)?;
    let dir = string(tdi, "module-dir");
    let top = string(tdi, "dir-top-bld");
    let provides: Vec<_> = parser
        .provides
        .iter()
        .map(|name| {
            let mut m = json!({"logical-name":encode_filename(name),"is-interface":true});
            if !dir.is_empty() {
                let path = format!("{}/{name}", dir.trim_end_matches('/'));
                let path = if top.is_empty() {
                    path
                } else {
                    util::relative_if_under(top, &path)
                };
                m["compiled-module-path"] = json!(encode_filename(&path));
            }
            m
        })
        .collect();
    let requires: Vec<_> = parser
        .requires
        .difference(&parser.provides)
        .map(|name| json!({"logical-name":encode_filename(name)}))
        .collect();
    let ddi = json!({"version":0,"revision":0,"rules":[{"primary-output":encode_filename(opts["obj"]),"outputs":[],"provides":provides,"requires":requires}]});
    write_file(opts["ddi"], &stream_value(&ddi), false)
}

fn collate(tdi: &Value, dd: &str, paths: &[&String]) -> Result<(), String> {
    let top = string(tdi, "dir-top-bld");
    let load_modules =
        |dir: &str| read_json("cmake_ninja_dyndep", &format!("{dir}/FortranModules.json"));
    let mut available = BTreeMap::new();
    let mut metadata = json!({"modules":{},"references":{},"usages":{}});
    for dir in array(tdi, "linked-target-dirs")
        .iter()
        .filter_map(Value::as_str)
    {
        let linked = load_modules(dir)?;
        if let Some(map) = linked["modules"].as_object() {
            available.extend(
                map.iter()
                    .map(|(k, v)| (k.clone(), string(v, "bmi").to_owned())),
            );
        }
        for key in ["references", "usages"] {
            if let Some(map) = linked[key].as_object() {
                metadata[key].as_object_mut().unwrap().extend(map.clone());
            }
        }
    }
    let mut objects = Vec::new();
    for path in paths {
        let ddi = read_json("cmake_ninja_dyndep", path)?;
        let invalid =
            |msg: &str| format!("{path}:1: -E cmake_ninja_dyndep failed to parse ddi file: {msg}");
        if ddi["version"].as_u64().unwrap_or(0) > 1 {
            return Err(invalid("unsupported version"));
        }
        let [rule] = array(&ddi, "rules") else {
            return Err(invalid("expected 1 source entry"));
        };
        let workdir = string(rule, "work-directory");
        let resolve = |p: &str| {
            if workdir.is_empty() || util::is_full_path(p) {
                p.to_owned()
            } else {
                format!("{workdir}/{p}")
            }
        };
        let obj = resolve(
            rule["primary-output"]
                .as_str()
                .ok_or_else(|| invalid("invalid primary-output"))?,
        );
        let mut provides = Vec::new();
        let mut requires = Vec::new();
        for (key, dest) in [("provides", &mut provides), ("requires", &mut requires)] {
            if rule.get(key).is_some() && !rule[key].is_array() {
                return Err(invalid(&format!("{key} is not an array")));
            }
            for module in array(rule, key) {
                let name = module["logical-name"]
                    .as_str()
                    .ok_or_else(|| invalid("invalid logical-name"))?;
                dest.push(name.to_owned());
                if key == "provides" {
                    let bmi = if let Some(path) = module["compiled-module-path"]
                        .as_str()
                        .filter(|p| !p.is_empty())
                    {
                        let path = resolve(path);
                        if util::is_full_path(&path) {
                            path
                        } else {
                            util::collapse_full_path(&path, top)
                        }
                    } else {
                        let dir = string(tdi, "module-dir");
                        let name = format!("{}.bmi", name.replace(':', "-"));
                        if dir.is_empty() {
                            name
                        } else {
                            format!("{}/{name}", dir.trim_end_matches('/'))
                        }
                    };
                    metadata["modules"][name] = json!({"bmi":bmi,"is-private":false});
                    available.insert(name.to_owned(), bmi);
                }
            }
        }
        objects.push((obj, provides, requires));
    }
    let ninja = |path: &str| {
        if top.is_empty() {
            path.to_owned()
        } else {
            util::relative_if_under(top, path)
        }
    };
    let mut text = String::from("ninja_dyndep_version = 1.0\n");
    for (obj, provides, requires) in objects {
        let mut build = Build::new("dyndep");
        let obj = ninja(&obj);
        if !provides.is_empty() {
            build.variables.insert("restat".into(), "1".into());
        }
        build.implicit_outs.extend(
            provides
                .iter()
                .map(|p| ninja(&available[p]))
                .filter(|p| p != &obj),
        );
        build.outputs.push(obj);
        build.implicit_deps.extend(
            requires
                .iter()
                .filter_map(|r| available.get(r))
                .map(|p| ninja(p)),
        );
        write_build(&mut text, &build, 0);
    }
    write_file(dd, &text, false)?;
    for dir in array(tdi, "forward-modules-from-target-dirs")
        .iter()
        .filter_map(Value::as_str)
    {
        let linked = load_modules(dir)?;
        for key in ["modules", "references", "usages"] {
            if let Some(map) = linked[key].as_object() {
                for (name, val) in map {
                    metadata[key]
                        .as_object_mut()
                        .unwrap()
                        .entry(name)
                        .or_insert_with(|| val.clone());
                }
            }
        }
    }
    write_file(
        &format!("{}/FortranModules.json", util::filename_path(dd)),
        &stream_value(&metadata),
        true,
    )
}
