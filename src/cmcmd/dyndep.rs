//! cmGlobalNinjaGenerator's build-time Fortran scanner and dyndep collator.

use std::collections::{BTreeMap, BTreeSet};
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
    if opts["lang"] != "Fortran" && opts["lang"] != "CXX" {
        return Err(format!(
            "-E {command} does not understand the {} language",
            opts["lang"]
        ));
    }
    let tdi = read_json(command, opts["tdi"])?;
    if scanning {
        if opts["lang"] != "Fortran" {
            return Err(format!(
                "-E {command} does not support scanning the {} language",
                opts["lang"]
            ));
        }
        scan(&opts, &tdi)
    } else if opts["lang"] == "CXX" {
        collate_cxx(
            &tdi,
            opts["dd"],
            &ddis,
            opts.get("modmapfmt").copied().unwrap_or(""),
        )
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

// ---------- C++ module collation (cmDyndepCollation + cmCxxModuleMapper) ----------

#[derive(Clone)]
struct CxxReq {
    logical: String,
    compiled_module_path: String,
    is_interface: bool,
    method: &'static str,
}

struct CxxObject {
    primary: String,
    provides: Vec<CxxReq>,
    requires: Vec<CxxReq>,
}

/// `cmScanDepFormat_P1689_Parse` for a `.ddi` file.
fn parse_p1689(path: &str) -> Result<CxxObject, String> {
    let ddi = read_json("cmake_ninja_dyndep", path)
        .map_err(|_| format!("-E cmake_ninja_dyndep failed to parse ddi file {path}"))?;
    let invalid = |msg: &str| format!("-E cmake_ninja_dyndep failed to parse {path}: {msg}");
    let version = ddi["version"].as_u64().unwrap_or(0);
    if version > 1 {
        return Err(format!(
            "-E cmake_ninja_dyndep failed to parse {path}: version {version}"
        ));
    }
    let rules = array(&ddi, "rules");
    if !rules.is_empty() && rules.len() != 1 {
        return Err(invalid("expected 1 source entry"));
    }
    let mut obj = CxxObject {
        primary: String::new(),
        provides: vec![],
        requires: vec![],
    };
    for rule in rules {
        let workdir = if rule["work-directory"].is_string() {
            Some(rule["work-directory"].as_str().unwrap().to_owned())
        } else if !rule["work-directory"].is_null() {
            return Err(invalid("work-directory is not a string"));
        } else {
            None
        };
        let filename = |v: &Value| -> Result<String, String> {
            let mut res = v
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("invalid filename"))?;
            if let Some(wd) = &workdir {
                if !wd.is_empty() && !util::is_full_path(&res) {
                    res = format!("{wd}/{res}");
                }
            }
            Ok(res)
        };
        let blob = |v: &Value| -> Result<String, String> {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("invalid blob"))
        };
        if rule.get("primary-output").is_some() {
            obj.primary = filename(&rule["primary-output"])?;
        }
        if let Some(provides) = rule.get("provides") {
            if !provides.is_array() {
                return Err(invalid("provides is not an array"));
            }
            for p in provides.as_array().unwrap() {
                let use_source_path = match p.get("unique-on-source-path") {
                    Some(v) => v
                        .as_bool()
                        .ok_or_else(|| invalid("unique-on-source-path is not a boolean"))?,
                    None => false,
                };
                let compiled = if p.get("compiled-module-path").is_some() {
                    filename(&p["compiled-module-path"])?
                } else {
                    String::new()
                };
                if p.get("source-path").is_some() {
                    filename(&p["source-path"])?;
                } else if use_source_path {
                    return Err(invalid("source-path is missing"));
                }
                let is_interface = match p.get("is-interface") {
                    Some(v) => v
                        .as_bool()
                        .ok_or_else(|| invalid("is-interface is not a boolean"))?,
                    None => true,
                };
                obj.provides.push(CxxReq {
                    logical: blob(&p["logical-name"])?,
                    compiled_module_path: compiled,
                    is_interface,
                    method: "by-name",
                });
            }
        }
        if let Some(reqs) = rule.get("requires") {
            if !reqs.is_array() {
                return Err(invalid("requires is not an array"));
            }
            for q in reqs.as_array().unwrap() {
                let use_source_path = match q.get("unique-on-source-path") {
                    Some(v) => v
                        .as_bool()
                        .ok_or_else(|| invalid("unique-on-source-path is not a boolean"))?,
                    None => false,
                };
                let compiled = if q.get("compiled-module-path").is_some() {
                    filename(&q["compiled-module-path"])?
                } else {
                    String::new()
                };
                if q.get("source-path").is_some() {
                    filename(&q["source-path"])?;
                } else if use_source_path {
                    return Err(invalid("source-path is missing"));
                }
                let method = match q.get("lookup-method") {
                    Some(v) => {
                        let m = v
                            .as_str()
                            .ok_or_else(|| invalid("lookup-method is not a string"))?;
                        match m {
                            "by-name" => "by-name",
                            "include-angle" => "include-angle",
                            "include-quote" => "include-quote",
                            _ => {
                                return Err(format!(
                                    "-E cmake_ninja_dyndep failed to parse \
                                     {path}: lookup-method is not a valid: {m}"
                                ));
                            }
                        }
                    }
                    None => "by-name",
                };
                obj.requires.push(CxxReq {
                    logical: blob(&q["logical-name"])?,
                    compiled_module_path: compiled,
                    is_interface: true,
                    method,
                });
            }
        }
    }
    Ok(obj)
}

struct CxxExport<'a> {
    install: bool,
    name: &'a str,
    filesystem_name: &'a str,
    prefix: &'a str,
    cxx_module_info_dir: &'a str,
    namespace: &'a str,
}

struct CxxBmiInstall<'a> {
    component: &'a str,
    destination: &'a str,
    exclude_from_all: bool,
    optional: bool,
    permissions: &'a str,
    message_level: &'a str,
    script_location: &'a str,
}

#[derive(Clone, Default)]
struct CxxFileSet<'a> {
    bmi_only: bool,
    relative_directory: String,
    source: &'a str,
    type_name: &'a str,
    visibility: &'a str,
    destination: Option<&'a str>,
}

/// `cmCxxModuleExportInfo` parsed from the TDI.
struct CxxExportInfo<'a> {
    config: String,
    exports: Vec<CxxExport<'a>>,
    database_info: Option<(&'a str, &'a str)>,
    bmi_installation: Option<CxxBmiInstall<'a>>,
    object_to_file_set: BTreeMap<String, CxxFileSet<'a>>,
    object_to_source: BTreeMap<String, &'a str>,
}

fn parse_export_info<'a>(tdi: &'a Value) -> CxxExportInfo<'a> {
    let mut config = string(tdi, "config").to_owned();
    if config.is_empty() {
        config = "noconfig".to_owned();
    }
    let exports = array(tdi, "exports")
        .iter()
        .map(|e| CxxExport {
            install: e["install"].as_bool().unwrap_or(false),
            name: string(e, "export-name"),
            filesystem_name: string(e, "filesystem-export-name"),
            prefix: string(e, "export-prefix"),
            cxx_module_info_dir: string(e, "cxx-module-info-dir"),
            namespace: string(e, "namespace"),
        })
        .collect();
    let database_info = if tdi["database-info"].is_object() {
        Some((
            string(&tdi["database-info"], "template-path"),
            string(&tdi["database-info"], "output"),
        ))
    } else {
        None
    };
    let bmi_installation = if tdi["bmi-installation"].is_object() {
        let b = &tdi["bmi-installation"];
        Some(CxxBmiInstall {
            component: string(b, "component"),
            destination: string(b, "destination"),
            exclude_from_all: b["exclude-from-all"].as_bool().unwrap_or(false),
            optional: b["optional"].as_bool().unwrap_or(false),
            permissions: string(b, "permissions"),
            message_level: string(b, "message-level"),
            script_location: string(b, "script-location"),
        })
    } else {
        None
    };
    let mut object_to_file_set = BTreeMap::new();
    if let Some(mods) = tdi["cxx-modules"].as_object() {
        for (obj, info) in mods {
            let mut rel = string(info, "relative-directory").to_owned();
            if !rel.is_empty() && !rel.ends_with('/') {
                rel.push('/');
            }
            object_to_file_set.insert(
                obj.clone(),
                CxxFileSet {
                    bmi_only: info["bmi-only"].as_bool().unwrap_or(false),
                    relative_directory: rel,
                    source: string(info, "source"),
                    type_name: string(info, "type"),
                    visibility: string(info, "visibility"),
                    destination: info["destination"].as_str(),
                },
            );
        }
    }
    let mut object_to_source = BTreeMap::new();
    if let Some(srcs) = tdi["sources"].as_object() {
        for (obj, info) in srcs {
            object_to_source.insert(obj.clone(), string(info, "source"));
        }
    }
    CxxExportInfo {
        config,
        exports,
        database_info,
        bmi_installation,
        object_to_file_set,
        object_to_source,
    }
}

fn cxx_is_private(fs: Option<&CxxFileSet>) -> bool {
    matches!(fs, Some(f) if f.visibility != "PUBLIC" && f.visibility != "INTERFACE")
}

/// `CxxModuleUsage` bookkeeping.
#[derive(Default)]
struct CxxUsage {
    usage: BTreeMap<String, BTreeSet<String>>,
    reference: BTreeMap<String, (String, String)>,
}

fn cxx_add_reference(
    usages: &mut CxxUsage,
    logical: &str,
    loc: &str,
    method: &str,
) -> Result<(), String> {
    if let Some((path, m)) = usages.reference.get(logical) {
        if path == loc && m == method {
            return Ok(());
        }
        return Err(format!(
            "Disagreement of the location of the '{logical}' module. \
             Location A: '{path}' via {m}; Location B: '{loc}' via {method}."
        ));
    }
    usages
        .reference
        .insert(logical.to_owned(), (loc.to_owned(), method.to_owned()));
    Ok(())
}

/// `CxxModuleUsageSeed`: seed internal usages and detect import cycles.
fn cxx_usage_seed(
    mod_files: &BTreeMap<String, (String, bool)>,
    objects: &[CxxObject],
    usages: &mut CxxUsage,
    private_usage_found: &mut bool,
    top: &str,
) -> Result<Vec<String>, String> {
    let mut internal_usages: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for object in objects {
        for p in &object.provides {
            if let Some((bmi, false)) = mod_files.get(&p.logical) {
                // `CxxModuleLocations::BmiGeneratorPathForModule` stores the
                // path as ninja sees it (relative to the build tree top).
                cxx_add_reference(usages, &p.logical, &ninja_path(top, bmi), "by-name")?;
            }
        }
        for r in &object.requires {
            if let Some((_, true)) = mod_files.get(&r.logical) {
                *private_usage_found = true;
                return Err(format!(
                    "Unable to use module '{}' as it is 'PRIVATE' and \
                     therefore not accessible outside of its owning target.",
                    r.logical
                ));
            }
            let known = mod_files.get(&r.logical);
            let transitive = usages.usage.get(&r.logical).cloned();
            for p in &object.provides {
                let this_usages = usages.usage.entry(p.logical.clone()).or_default();
                this_usages.insert(r.logical.clone());
                if transitive.is_none() || internal_usages.contains_key(&r.logical) {
                    if known.is_some() {
                        internal_usages
                            .entry(p.logical.clone())
                            .or_default()
                            .insert(r.logical.clone());
                    }
                } else if let Some(t) = &transitive {
                    this_usages.extend(t.iter().cloned());
                }
            }
            if let Some((bmi, _)) = known {
                cxx_add_reference(usages, &r.logical, &ninja_path(top, bmi), r.method)?;
            }
        }
    }
    while !internal_usages.is_empty() {
        let starting = internal_usages.len();
        let names: Vec<String> = internal_usages.keys().cloned().collect();
        for name in names {
            let Some(pending) = internal_usages.remove(&name) else {
                continue;
            };
            let mut remaining = BTreeSet::new();
            let mut resolved: Vec<String> = vec![];
            for u in pending {
                if internal_usages.contains_key(&u) {
                    remaining.insert(u);
                } else if let Some(t) = usages.usage.get(&u) {
                    resolved.extend(t.iter().cloned());
                }
            }
            usages
                .usage
                .entry(name.clone())
                .or_default()
                .extend(resolved);
            if !remaining.is_empty() {
                internal_usages.insert(name, remaining);
            }
        }
        if starting == internal_usages.len() {
            return Ok(internal_usages.keys().cloned().collect());
        }
    }
    Ok(vec![])
}

struct TransUsage {
    logical: String,
    location: String,
    method: String,
}

/// `GetTransitiveUsages` from `cmCxxModuleMapper.cxx`.
fn cxx_transitive_usages(
    mod_files: &BTreeMap<String, (String, bool)>,
    top: &str,
    required: &[CxxReq],
    usages: &CxxUsage,
) -> Vec<TransUsage> {
    let mut directs = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut all = vec![];
    for r in required {
        if let Some((bmi, false)) = mod_files.get(&r.logical) {
            all.push(TransUsage {
                logical: r.logical.clone(),
                location: ninja_path(top, bmi),
                method: r.method.to_owned(),
            });
            directs.insert(r.logical.clone());
            if let Some(t) = usages.usage.get(&r.logical) {
                names.extend(t.iter().cloned());
            }
        }
    }
    for name in names {
        if directs.contains(&name) {
            continue;
        }
        if let Some((path, method)) = usages.reference.get(&name) {
            all.push(TransUsage {
                logical: name,
                location: path.clone(),
                method: method.clone(),
            });
        }
    }
    all
}

fn ninja_path(top: &str, path: &str) -> String {
    if top.is_empty() {
        path.to_owned()
    } else {
        util::relative_if_under(top, path)
    }
}

/// `CxxModuleMapContent` for the clang/gcc/msvc formats.
fn cxx_modmap_content(
    fmt: &str,
    mod_files: &BTreeMap<String, (String, bool)>,
    top: &str,
    obj: &CxxObject,
    usages: &CxxUsage,
) -> String {
    let mut mm = String::new();
    match fmt {
        "clang" => {
            if obj.provides.len() > 1 {
                return String::new();
            }
            for p in &obj.provides {
                if mod_files.contains_key(&p.logical) {
                    mm.push_str("-x c++-module\n");
                    mm.push_str(&format!(
                        "-fmodule-output={}\n",
                        ninja_path(top, &mod_files[&p.logical].0)
                    ));
                    break;
                }
            }
            for u in cxx_transitive_usages(mod_files, top, &obj.requires, usages) {
                mm.push_str(&format!("-fmodule-file={}={}\n", u.logical, u.location));
            }
        }
        "gcc" => {
            mm.push_str("$root .\n");
            for p in &obj.provides {
                if let Some((bmi, _)) = mod_files.get(&p.logical) {
                    mm.push_str(&format!("{} {}\n", p.logical, ninja_path(top, bmi)));
                }
            }
            for r in &obj.requires {
                if let Some((bmi, _)) = mod_files.get(&r.logical) {
                    mm.push_str(&format!("{} {}\n", r.logical, ninja_path(top, bmi)));
                }
            }
        }
        "msvc" => {
            if obj.provides.len() > 1 {
                return String::new();
            }
            for p in &obj.provides {
                mm.push_str(if p.is_interface {
                    "-interface\n"
                } else {
                    "-internalPartition\n"
                });
                if let Some((bmi, _)) = mod_files.get(&p.logical) {
                    mm.push_str(&format!("-ifcOutput {}\n", ninja_path(top, bmi)));
                }
            }
            for u in cxx_transitive_usages(mod_files, top, &obj.requires, usages) {
                let flag = match u.method.as_str() {
                    "include-angle" => "-headerUnit:angle",
                    "include-quote" => "-headerUnit:quote",
                    _ => "-reference",
                };
                mm.push_str(&format!("{flag} {}={}\n", u.logical, u.location));
            }
        }
        _ => {}
    }
    mm
}

fn escape_nowrap(value: &str) -> String {
    let inner = util::escape_for_cmake(value);
    inner[1..inner.len() - 1].to_owned()
}

/// `cmDyndepCollation::WriteDyndepMetadata` (exports, BMI install script,
/// private/public checks, build database).
fn write_dyndep_metadata(
    objects: &[CxxObject],
    export: &CxxExportInfo,
    mod_files: &BTreeMap<String, (String, bool)>,
) -> Result<(), String> {
    let mut errors = String::new();
    let config_upper = export.config.to_uppercase();

    let mut exports: Vec<(String, &CxxExport)> = vec![];
    for e in &export.exports {
        let dir = format!("{}/{}/", e.prefix, e.cxx_module_info_dir);
        let path = format!("{dir}target-{}-{}.cmake", e.filesystem_name, export.config);
        let head = format!(
            "set_property(TARGET \"{}{}\"\n  PROPERTY IMPORTED_CXX_MODULES_{}\n",
            e.namespace, e.name, config_upper
        );
        if let Some(parent) = Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        write_file(&path, &head, false)?;
        exports.push((path, e));
    }

    let mut build_db: Option<(Value, BTreeMap<String, usize>, String)> =
        if let Some((tmpl, out)) = export.database_info {
            match std::fs::read_to_string(tmpl) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(db) => {
                        let mut lookup = BTreeMap::new();
                        if let Some(tus) = db["translation-units"].as_array() {
                            for (i, tu) in tus.iter().enumerate() {
                                if let Some(src) = tu["source"].as_str() {
                                    lookup.insert(src.to_owned(), i);
                                }
                            }
                        }
                        Some((db, lookup, out.to_owned()))
                    }
                    Err(e) => {
                        errors.push_str(&format!(
                            "Failed to read the template build database {tmpl}: {e}\n"
                        ));
                        None
                    }
                },
                Err(e) => {
                    errors.push_str(&format!(
                        "Failed to read the template build database {tmpl}: {e}\n"
                    ));
                    None
                }
            }
        } else {
            None
        };

    let mut bmi_script = String::new();
    let install_destination = |dest: &str| -> (bool, String) {
        if util::is_full_path(dest) {
            (true, escape_nowrap(dest))
        } else {
            (
                false,
                format!("${{_IMPORT_PREFIX}}/{}", escape_nowrap(dest)),
            )
        }
    };

    let mut private_modules = BTreeSet::new();
    let mut public_source_requires: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for object in objects {
        let output_path = &object.primary;
        if let (Some(src), Some((db, lookup, _))) =
            (export.object_to_source.get(output_path), build_db.as_mut())
        {
            if let Some(&i) = lookup.get(*src) {
                if let Some(tu) = db["translation-units"][i].as_object_mut() {
                    let reqs: Vec<Value> =
                        object.requires.iter().map(|r| json!(r.logical)).collect();
                    tu.insert("requires".to_owned(), Value::Array(reqs));
                }
            } else {
                errors.push_str(&format!("Failed to find module database entry for {src}\n"));
            }
        }

        let Some(file_set) = export.object_to_file_set.get(output_path).cloned() else {
            if let Some(p) = object.provides.first() {
                errors.push_str(&format!(
                    "Output {} provides the `{}` module but it is not found in \
                     a `FILE_SET` of type `CXX_MODULES`\n",
                    object.primary, p.logical
                ));
            }
            continue;
        };
        let has_provides = !object.provides.is_empty();

        if let Some((db, lookup, _)) = build_db.as_mut() {
            if let Some(&i) = lookup.get(file_set.source) {
                if let Some(tu) = db["translation-units"][i].as_object_mut() {
                    if has_provides {
                        let mut provides_map = serde_json::Map::new();
                        for p in &object.provides {
                            if let Some((bmi, _)) = mod_files.get(&p.logical) {
                                provides_map.insert(p.logical.clone(), json!(bmi));
                            } else {
                                errors.push_str(&format!(
                                    "Failed to find BMI location for {}\n",
                                    p.logical
                                ));
                            }
                        }
                        tu.insert("provides".to_owned(), Value::Object(provides_map));
                    }
                    let reqs: Vec<Value> =
                        object.requires.iter().map(|r| json!(r.logical)).collect();
                    tu.insert("requires".to_owned(), Value::Array(reqs));
                }
            } else {
                errors.push_str(&format!(
                    "Failed to find module database entry for {}\n",
                    file_set.source
                ));
            }
        }

        if file_set.type_name == "CXX_MODULES" {
            if !has_provides {
                errors.push_str(&format!(
                    "Output {} is of type `CXX_MODULES` but does not provide a \
                     module interface unit or partition\n",
                    object.primary
                ));
                continue;
            }
        } else if file_set.type_name == "CXX_MODULE_HEADERS" {
            // TODO (matches upstream).
        } else {
            if let Some(p) = object.provides.first() {
                errors.push_str(&format!(
                    "Source {} provides the `{}` C++ module but is of type `{}` \
                     module but must be of type `CXX_MODULES`\n",
                    file_set.source, p.logical, file_set.type_name
                ));
            }
            continue;
        }

        if cxx_is_private(Some(&file_set)) {
            for p in &object.provides {
                private_modules.insert(p.logical.clone());
            }
            continue;
        }

        {
            let reqs = public_source_requires
                .entry(file_set.source.to_owned())
                .or_default();
            for r in &object.requires {
                reqs.insert(r.logical.clone());
            }
        }

        for p in &object.provides {
            let (bmi_dest_is_abs, bmi_destination) = match &export.bmi_installation {
                Some(b) => {
                    let (abs, dest) = install_destination(b.destination);
                    (abs, format!("{dest}/"))
                }
                None => (false, String::new()),
            };
            let (install_bmi_path, build_bmi_path) = match mod_files.get(&p.logical) {
                Some((bmi, _)) => (
                    format!(
                        "{bmi_destination}{}",
                        escape_nowrap(&util::filename_name(bmi))
                    ),
                    escape_nowrap(bmi),
                ),
                None => (String::new(), String::new()),
            };

            for (exp_path, e) in &exports {
                let iface_source =
                    if let Some(destination) = file_set.destination.filter(|_| e.install) {
                        let (_, dest) = install_destination(destination);
                        format!(
                            "{dest}/{}{}",
                            escape_nowrap(&file_set.relative_directory),
                            escape_nowrap(&util::filename_name(file_set.source))
                        )
                    } else {
                        escape_nowrap(file_set.source)
                    };
                let bmi_path = if e.install && export.bmi_installation.is_some() {
                    install_bmi_path.clone()
                } else if !e.install {
                    build_bmi_path.clone()
                } else {
                    String::new()
                };
                if iface_source.is_empty() {
                    continue;
                }
                let mut line = format!("    \"{}={}", escape_nowrap(&p.logical), iface_source);
                if !bmi_path.is_empty() {
                    line.push(',');
                    line.push_str(&bmi_path);
                }
                line.push_str("\"\n");
                let mut text = std::fs::read_to_string(exp_path).unwrap_or_default();
                text.push_str(&line);
                write_file(exp_path, &text, false)?;
            }

            if let Some(bmi) = &export.bmi_installation {
                if let Some((bmi_path, _)) = mod_files.get(&p.logical) {
                    bmi_script.push_str(&format!(
                        "if (CMAKE_INSTALL_COMPONENT STREQUAL \"{}\"",
                        escape_nowrap(bmi.component)
                    ));
                    if !bmi.exclude_from_all {
                        bmi_script.push_str(" OR NOT CMAKE_INSTALL_COMPONENT");
                    }
                    bmi_script.push_str(")\n  file(INSTALL\n    DESTINATION \"");
                    if !bmi_dest_is_abs {
                        bmi_script.push_str("${CMAKE_INSTALL_PREFIX}/");
                    }
                    bmi_script.push_str(&format!(
                        "{}\"\n    TYPE FILE\n",
                        escape_nowrap(bmi.destination)
                    ));
                    if bmi.optional {
                        bmi_script.push_str("    OPTIONAL\n");
                    }
                    if !bmi.message_level.is_empty() {
                        bmi_script.push_str(&format!("    {}\n", bmi.message_level));
                    }
                    if !bmi.permissions.is_empty() {
                        bmi_script.push_str(&format!("    PERMISSIONS{}\n", bmi.permissions));
                    }
                    bmi_script.push_str(&format!("    FILES \"{bmi_path}\")\n"));
                    if bmi_dest_is_abs {
                        bmi_script.push_str(&format!(
                            "  list(APPEND CMAKE_ABSOLUTE_DESTINATION_FILES\n    \
                             \"{}\")\n  if \
                             (CMAKE_WARN_ON_ABSOLUTE_INSTALL_DESTINATION)\n    \
                             message(WARNING\n      \"ABSOLUTE path INSTALL \
                             DESTINATION : \
                             ${{CMAKE_ABSOLUTE_DESTINATION_FILES}}\")\n  endif ()\n  \
                             if (CMAKE_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION)\n    \
                             message(FATAL_ERROR\n      \"ABSOLUTE path INSTALL \
                             DESTINATION forbidden (by caller): \
                             ${{CMAKE_ABSOLUTE_DESTINATION_FILES}}\")\n  endif ()\n",
                            escape_nowrap(&util::filename_name(bmi_path))
                        ));
                    }
                    bmi_script.push_str("endif ()\n");
                }
            }
        }
    }

    for (path, _) in &exports {
        let mut text = std::fs::read_to_string(path).unwrap_or_default();
        text.push_str(")\n");
        write_file(path, &text, false)?;
    }

    for (src, reqs) in &public_source_requires {
        for req in reqs {
            if private_modules.contains(req) {
                errors.push_str(&format!(
                    "Public C++ module source `{src}` requires the `{req}` C++ \
                     module which is provided by a private source\n"
                ));
            }
        }
    }

    if let Some(bmi) = &export.bmi_installation {
        if !bmi.script_location.is_empty() {
            write_file(bmi.script_location, &bmi_script, true)?;
        }
    }

    if let Some((db, _, out)) = &build_db {
        write_file(out, &stream_value(db), false)?;
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.trim_end().to_owned())
    }
}

/// `cmGlobalNinjaGenerator::WriteDyndepFile` for CXX.
fn collate_cxx(tdi: &Value, dd: &str, paths: &[&String], modmapfmt: &str) -> Result<(), String> {
    let top = string(tdi, "dir-top-bld");
    let mut module_dir = string(tdi, "module-dir").to_owned();
    if !module_dir.is_empty() && !module_dir.ends_with('/') {
        module_dir.push('/');
    }
    let export = parse_export_info(tdi);

    let mut objects = vec![];
    for ddi in paths {
        objects.push(parse_p1689(ddi)?);
    }

    let mut usages = CxxUsage::default();
    let mut mod_files: BTreeMap<String, (String, bool)> = BTreeMap::new();

    // Populate the module map with those provided by linked targets first.
    for dir in array(tdi, "linked-target-dirs")
        .iter()
        .filter_map(Value::as_str)
    {
        let ltmn = format!("{dir}/CXXModules.json");
        let data = match std::fs::read(&ltmn) {
            Ok(d) => d,
            Err(_) => {
                return Err(format!(
                    "-E cmake_ninja_dyndep failed to open {ltmn} for module \
                     information"
                ));
            }
        };
        let ltm: Value = serde_json::from_slice(&data)
            .map_err(|e| format!("-E cmake_ninja_dyndep failed to parse {dir}: {e}"))?;
        if !ltm.is_object() {
            continue;
        }
        if let Some(mods) = ltm["modules"].as_object() {
            for (name, m) in mods {
                if m.is_object() {
                    mod_files.insert(
                        name.clone(),
                        (
                            string(m, "bmi").to_owned(),
                            m["is-private"].as_bool().unwrap_or(false),
                        ),
                    );
                }
            }
        }
        if let Some(refs) = ltm["references"].as_object() {
            for (name, r) in refs {
                if r.is_object() {
                    usages.reference.insert(
                        name.clone(),
                        (
                            string(r, "path").to_owned(),
                            string(r, "lookup-method").to_owned(),
                        ),
                    );
                }
            }
        }
        if let Some(us) = ltm["usages"].as_object() {
            for (name, set) in us {
                if let Some(arr) = set.as_array() {
                    usages
                        .usage
                        .entry(name.clone())
                        .or_default()
                        .extend(arr.iter().filter_map(|v| v.as_str().map(str::to_owned)));
                }
            }
        }
    }

    let modmap_fmt = match modmapfmt {
        "" => None,
        "clang" | "gcc" | "msvc" => Some(modmapfmt.to_owned()),
        other => {
            return Err(format!(
                "-E cmake_ninja_dyndep does not understand the {other} module \
                 map format"
            ));
        }
    };
    let module_ext = match modmap_fmt.as_deref() {
        Some("clang") => ".pcm",
        Some("gcc") => ".gcm",
        Some("msvc") => ".ifc",
        _ => ".bmi",
    };

    // Extend the module map with those provided by this target.  We do this
    // after loading the modules provided by linked targets in case we have
    // one of the same name that must be preferred.
    let mut target_modules = serde_json::Map::new();
    for object in &objects {
        for p in &object.provides {
            let bmi_only = export
                .object_to_file_set
                .get(&object.primary)
                .is_some_and(|f| f.bmi_only);
            let mod_path = if bmi_only {
                object.primary.clone()
            } else if !p.compiled_module_path.is_empty() {
                if util::is_full_path(&p.compiled_module_path) {
                    util::collapse_full_path(&p.compiled_module_path, top)
                } else {
                    p.compiled_module_path.clone()
                }
            } else {
                format!(
                    "{}{}{}",
                    module_dir,
                    p.logical.replace(':', "-"),
                    module_ext
                )
            };
            mod_files.insert(p.logical.clone(), (mod_path.clone(), false));
            target_modules.insert(
                p.logical.clone(),
                json!({
                    "bmi": mod_path,
                    "is-private": cxx_is_private(
                        export.object_to_file_set.get(&object.primary)),
                }),
            );
        }
    }

    if modmap_fmt.is_some() {
        let mut private_found = false;
        let cycles = cxx_usage_seed(&mod_files, &objects, &mut usages, &mut private_found, top)?;
        if !cycles.is_empty() {
            return Err(format!(
                "Circular dependency detected in the C++ module import graph. \
                 See modules named: \"{}\"",
                cycles.join("\", \"")
            ));
        }
        if private_found {
            return Err("PRIVATE module usage detected".to_owned());
        }
    }

    // Write the dyndep file.
    let mut text = String::from("ninja_dyndep_version = 1.0\n");
    for object in &objects {
        let mut build = Build::new("dyndep");
        build.outputs.push(ninja_path(top, &object.primary));
        for p in &object.provides {
            if let Some((bmi, _)) = mod_files.get(&p.logical) {
                let out = ninja_path(top, bmi);
                if out != build.outputs[0] {
                    build.implicit_outs.push(out);
                }
            }
        }
        for r in &object.requires {
            if let Some((bmi, _)) = mod_files.get(&r.logical) {
                build.implicit_deps.push(ninja_path(top, bmi));
            }
        }
        if !object.provides.is_empty() {
            build.variables.insert("restat".to_owned(), "1".to_owned());
        }
        write_build(&mut text, &build, 0);
    }
    write_file(dd, &text, false)?;

    // Write per-object module maps.
    if let Some(fmt) = &modmap_fmt {
        for object in &objects {
            let mm = cxx_modmap_content(fmt, &mod_files, top, object, &usages);
            write_file(&format!("{}.modmap", object.primary), &mm, true)?;
        }
    }

    // Write CXXModules.json for dependents.
    let mut target_module_info = serde_json::Map::new();
    target_module_info.insert("modules".to_owned(), Value::Object(target_modules));
    let usages_json: serde_json::Map<String, Value> = usages
        .usage
        .iter()
        .map(|(name, set)| {
            (
                name.clone(),
                Value::Array(set.iter().map(|s| json!(s)).collect()),
            )
        })
        .collect();
    target_module_info.insert("usages".to_owned(), Value::Object(usages_json));
    let refs_json: serde_json::Map<String, Value> = usages
        .reference
        .iter()
        .map(|(name, (path, method))| {
            (name.clone(), json!({"path": path, "lookup-method": method}))
        })
        .collect();
    target_module_info.insert("references".to_owned(), Value::Object(refs_json));

    // Forward info from forwarding targets.
    for dir in array(tdi, "forward-modules-from-target-dirs")
        .iter()
        .filter_map(Value::as_str)
    {
        let fmftn = format!("{dir}/CXXModules.json");
        let data = match std::fs::read(&fmftn) {
            Ok(d) => d,
            Err(_) => {
                return Err(format!(
                    "-E cmake_ninja_dyndep failed to open {fmftn} for module \
                     information"
                ));
            }
        };
        let fmft: Value = serde_json::from_slice(&data)
            .map_err(|e| format!("-E cmake_ninja_dyndep failed to parse {dir}: {e}"))?;
        if !fmft.is_object() {
            continue;
        }
        for key in ["modules", "references", "usages"] {
            if let (Some(dst), Some(src)) = (
                target_module_info
                    .get_mut(key)
                    .and_then(Value::as_object_mut),
                fmft[key].as_object(),
            ) {
                for (k, v) in src {
                    dst.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }

    write_file(
        &format!("{}/CXXModules.json", util::filename_path(dd)),
        &stream_value(&Value::Object(target_module_info)),
        true,
    )?;

    write_dyndep_metadata(&objects, &export, &mod_files)
}
