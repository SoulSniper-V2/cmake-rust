//! `cmBuildDatabase`: the C++ module compilation database used by
//! `CMAKE_EXPORT_BUILD_DATABASE` and `-E cmake_module_compile_db`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::commands::string::json::{Json, parse_jsoncpp, stream_value};

pub(crate) const PLACEHOLDER_NAME: &str = "<__CMAKE_UNKNOWN>";

#[derive(Default)]
pub(crate) struct TranslationUnit {
    pub work_directory: String,
    pub source: String,
    pub object: Option<String>,
    pub private: bool,
    pub requires: Vec<String>,
    pub provides: BTreeMap<String, String>,
    pub baseline_arguments: Vec<String>,
    pub local_arguments: Vec<String>,
    pub arguments: Vec<String>,
}

#[derive(Default)]
pub(crate) struct DatabaseSet {
    pub name: String,
    pub family_name: String,
    pub visible_sets: Vec<String>,
    pub translation_units: Vec<TranslationUnit>,
}

#[derive(Default)]
pub(crate) struct BuildDatabase {
    pub sets: Vec<DatabaseSet>,
}

fn is_full_path(p: &str) -> bool {
    p.starts_with('/') || (p.len() >= 3 && p.as_bytes()[1] == b':' && p.as_bytes()[2] == b'/')
}

fn load_error(path: &str, detail: &str) -> Option<BuildDatabase> {
    crate::cmcmd::error(format_args!(
        "-E cmake_module_compile_db failed to parse {path}: {detail}"
    ));
    None
}

/// `cmcmd::cmcmd_cmake_module_compile_db`.
pub(crate) fn module_compile_db(args: &[String]) -> i32 {
    let mut command: Option<&str> = None;
    let mut output: Option<&str> = None;
    let mut inputs: Vec<&str> = Vec::new();

    let mut next_is_output = false;
    for arg in args {
        // The first argument is always the command.
        if command.is_none() {
            command = Some(arg);
            continue;
        }

        if arg == "-o" {
            next_is_output = true;
            continue;
        }
        if next_is_output {
            if output.is_some() {
                crate::cmcmd::error("-E cmake_module_compile_db only supports one output file");
                return 1;
            }

            output = Some(arg);
            next_is_output = false;
            continue;
        }

        inputs.push(arg);
    }

    let Some(command) = command else {
        crate::cmcmd::error("-E cmake_module_compile_db requires a subcommand");
        return 1;
    };

    let mut ret = 0;

    if command == "verify" {
        if output.is_some() {
            crate::cmcmd::error("-E cmake_module_compile_db verify does not support an output");
            return 1;
        }

        for input in &inputs {
            if BuildDatabase::load(input).is_none() {
                crate::cmcmd::error(format_args!("failed to verify {input}"));
                ret = 1;
            }
        }
    } else if command == "merge" {
        let Some(output) = output else {
            crate::cmcmd::error("-E cmake_module_compile_db verify requires an output");
            return 1;
        };

        let mut dbs = Vec::new();
        for input in &inputs {
            match BuildDatabase::load(input) {
                Some(db) => dbs.push(db),
                None => {
                    crate::cmcmd::error(format_args!("failed to read {input}"));
                    return 1;
                }
            }
        }

        let db = BuildDatabase::merge(&dbs);
        db.write(output);
    } else {
        crate::cmcmd::error(format_args!(
            "-E cmake_module_compile_db unknown subcommand {command}"
        ));
        return 1;
    }

    ret
}

fn json_string(v: &Json) -> Option<String> {
    match v {
        Json::Str(s) => Some(String::from_utf8_lossy(s).into_owned()),
        _ => None,
    }
}

fn member<'a>(obj: &'a Json, key: &[u8]) -> &'a Json {
    match obj {
        Json::Object(map) => map.get(key).unwrap_or(&Json::Null),
        _ => &Json::Null,
    }
}

fn parse_filename(v: &Json, make_full: bool, workdir: &Option<String>) -> Option<String> {
    let mut s = json_string(v)?;
    if make_full {
        if let Some(wd) = workdir {
            if !wd.is_empty() && !is_full_path(&s) {
                s = format!("{wd}/{s}");
            }
        }
    }
    Some(s)
}

impl BuildDatabase {
    pub(crate) fn to_value(&self) -> Value {
        let sets: Vec<Value> = self
            .sets
            .iter()
            .map(|set| {
                let tus: Vec<Value> = set
                    .translation_units
                    .iter()
                    .map(|tu| {
                        let mut o = serde_json::Map::new();
                        if !tu.work_directory.is_empty() {
                            o.insert("work-directory".into(), json!(tu.work_directory));
                        }
                        o.insert("source".into(), json!(tu.source));
                        if let Some(object) = &tu.object {
                            o.insert("object".into(), json!(object));
                        }
                        o.insert("private".into(), json!(tu.private));
                        o.insert("requires".into(), json!(tu.requires));
                        o.insert("provides".into(), json!(tu.provides));
                        o.insert("baseline-arguments".into(), json!(tu.baseline_arguments));
                        o.insert("local-arguments".into(), json!(tu.local_arguments));
                        o.insert("arguments".into(), json!(tu.arguments));
                        Value::Object(o)
                    })
                    .collect();
                json!({
                    "name": set.name,
                    "family-name": set.family_name,
                    "visible-sets": set.visible_sets,
                    "translation-units": tus,
                })
            })
            .collect();
        json!({
            "version": 1,
            "revision": 0,
            "sets": sets,
        })
    }

    pub(crate) fn write(&self, path: &str) {
        let text = stream_value(&self.to_value());
        let _ = std::fs::write(path, text.as_bytes());
    }

    pub(crate) fn merge(dbs: &[BuildDatabase]) -> BuildDatabase {
        let mut db = BuildDatabase::default();
        for component in dbs {
            for set in &component.sets {
                db.sets.push(DatabaseSet {
                    name: set.name.clone(),
                    family_name: set.family_name.clone(),
                    visible_sets: set.visible_sets.clone(),
                    translation_units: set
                        .translation_units
                        .iter()
                        .map(|tu| TranslationUnit {
                            work_directory: tu.work_directory.clone(),
                            source: tu.source.clone(),
                            object: tu.object.clone(),
                            private: tu.private,
                            requires: tu.requires.clone(),
                            provides: tu.provides.clone(),
                            baseline_arguments: tu.baseline_arguments.clone(),
                            local_arguments: tu.local_arguments.clone(),
                            arguments: tu.arguments.clone(),
                        })
                        .collect(),
                });
            }
        }
        db
    }

    /// `GenerateLookupTable`: source path -> (set, translation unit)
    /// indices, the later unit winning for a repeated source.
    pub(crate) fn generate_lookup_table(&self) -> BTreeMap<String, (usize, usize)> {
        let mut lut = BTreeMap::new();
        for (si, set) in self.sets.iter().enumerate() {
            for (ti, tu) in set.translation_units.iter().enumerate() {
                lut.insert(tu.source.clone(), (si, ti));
            }
        }
        lut
    }

    pub(crate) fn unit_mut(&mut self, (si, ti): (usize, usize)) -> &mut TranslationUnit {
        &mut self.sets[si].translation_units[ti]
    }

    /// `cmBuildDatabase::HasPlaceholderNames`.
    pub(crate) fn has_placeholder_names(&self) -> bool {
        self.sets.iter().any(|set| {
            set.translation_units.iter().any(|tu| {
                tu.provides
                    .iter()
                    .any(|(k, v)| k == PLACEHOLDER_NAME || v == PLACEHOLDER_NAME)
            })
        })
    }

    pub(crate) fn load(path: &str) -> Option<BuildDatabase> {
        // An unreadable file parses as an empty stream.
        let doc = std::fs::read(path).unwrap_or_default();
        let mcdb = match parse_jsoncpp(&doc) {
            Ok(j) => j,
            Err(formatted) => {
                crate::cmcmd::error(format_args!(
                    "-E cmake_module_compile_db failed to parse {path}{formatted}"
                ));
                return None;
            }
        };

        let version = member(&mcdb, b"version");
        let version_uint = match version {
            Json::UInt(u) => Some(*u),
            Json::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        };
        if let Some(v) = version_uint {
            if v > 1 {
                crate::cmcmd::error(format_args!(
                    "-E cmake_module_compile_db failed to parse {path}: version {v}"
                ));
                return None;
            }
        }

        let mut db = BuildDatabase::default();

        if let Json::Array(sets) = member(&mcdb, b"sets") {
            for set in sets {
                let mut s = DatabaseSet::default();

                match json_string(member(set, b"name")) {
                    Some(v) => s.name = v,
                    None => return load_error(path, "name is not a string"),
                }
                match json_string(member(set, b"family-name")) {
                    Some(v) => s.family_name = v,
                    None => return load_error(path, "family-name is not a string"),
                }

                match member(set, b"visible-sets") {
                    Json::Array(items) => {
                        for item in items {
                            match json_string(item) {
                                Some(v) => s.visible_sets.push(v),
                                None => {
                                    return load_error(path, "a visible-sets item is not a string");
                                }
                            }
                        }
                    }
                    _ => return load_error(path, "visible-sets is not an array"),
                }

                match member(set, b"translation-units") {
                    Json::Array(items) => {
                        for item in items {
                            if !matches!(item, Json::Object(_)) {
                                return load_error(
                                    path,
                                    "a translation-units item is not an object",
                                );
                            }
                            let mut tu = TranslationUnit::default();

                            let mut work_directory = None;
                            let workdir = member(item, b"work-directory");
                            match workdir {
                                Json::Str(_) => {
                                    tu.work_directory = match parse_filename(workdir, false, &None)
                                    {
                                        Some(v) => v,
                                        None => return load_error(path, "invalid blob"),
                                    };
                                    work_directory = Some(tu.work_directory.clone());
                                }
                                Json::Null => {}
                                _ => return load_error(path, "work-directory is not a string"),
                            }

                            match parse_filename(member(item, b"source"), true, &work_directory) {
                                Some(v) => tu.source = v,
                                None => return load_error(path, "invalid filename"),
                            }

                            if let Json::Object(map) = item {
                                if let Some(object) = map.get(b"object".as_slice()) {
                                    if !matches!(object, Json::Null) {
                                        match parse_filename(object, false, &work_directory) {
                                            Some(v) => tu.object = Some(v),
                                            None => return load_error(path, "invalid filename"),
                                        }
                                    }
                                }
                            }

                            if let Json::Object(map) = item {
                                if let Some(priv_val) = map.get(b"private".as_slice()) {
                                    match priv_val {
                                        Json::Bool(b) => tu.private = *b,
                                        _ => return load_error(path, "private is not a boolean"),
                                    }
                                }
                                if let Some(reqs) = map.get(b"requires".as_slice()) {
                                    match reqs {
                                        Json::Array(items) => {
                                            for r in items {
                                                match json_string(r) {
                                                    Some(v) => tu.requires.push(v),
                                                    None => {
                                                        return load_error(
                                                            path,
                                                            "a requires item is not a string",
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        _ => return load_error(path, "requires is not an array"),
                                    }
                                }
                                if let Some(provides) = map.get(b"provides".as_slice()) {
                                    match provides {
                                        Json::Object(entries) => {
                                            for (k, v) in entries {
                                                match json_string(v) {
                                                    Some(vv) => {
                                                        tu.provides.insert(
                                                            String::from_utf8_lossy(k).into_owned(),
                                                            vv,
                                                        );
                                                    }
                                                    None => {
                                                        return load_error(
                                                            path,
                                                            "a provides value is not a string",
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        _ => return load_error(path, "provides is not an object"),
                                    }
                                }

                                macro_rules! parse_arguments {
                                    ($key:literal, $dst:expr, $not_array:literal,
                                     $not_string:literal) => {
                                        if let Some(args) = map.get($key.as_slice()) {
                                            match args {
                                                Json::Array(items) => {
                                                    for a in items {
                                                        match json_string(a) {
                                                            Some(v) => $dst.push(v),
                                                            None => {
                                                                return load_error(
                                                                    path,
                                                                    $not_string,
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                _ => return load_error(path, $not_array),
                                            }
                                        }
                                    };
                                }
                                parse_arguments!(
                                    b"baseline-arguments",
                                    tu.baseline_arguments,
                                    "baseline_arguments is not an array",
                                    "a baseline argument is not a string"
                                );
                                parse_arguments!(
                                    b"local-arguments",
                                    tu.local_arguments,
                                    "local_arguments is not an array",
                                    "a local argument is not a string"
                                );
                                parse_arguments!(
                                    b"arguments",
                                    tu.arguments,
                                    "arguments is not an array",
                                    "an argument is not a string"
                                );
                            }
                            s.translation_units.push(tu);
                        }
                    }
                    _ => return load_error(path, "translation-units is not an array"),
                }

                db.sets.push(s);
            }
        }

        Some(db)
    }
}
