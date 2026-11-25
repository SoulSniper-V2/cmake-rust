// Version checks mirror cmFileAPI's `minor <= kMinor`, where several kMinor are 0.
#![allow(clippy::absurd_extreme_comparisons)]

//! The CMake file-based API (cmFileAPI.cxx and the per-kind dumpers
//! cmFileAPICache.cxx, cmFileAPICMakeFiles.cxx, cmFileAPIToolchains.cxx,
//! cmFileAPIConfigureLog.cxx, cmFileAPICodemodel.cxx).

use std::collections::{BTreeMap, BTreeSet};

use crate::commands::string::json::{Json, write_json_ostream};
use crate::interp::Interp;
use crate::presets::json as jsoncpp;

mod codemodel;

// ---------------------------------------------------------------------
// Json::Value construction helpers

pub(crate) fn jstr(s: impl AsRef<str>) -> Json {
    Json::Str(crate::bytes::encode(s.as_ref()).into_owned())
}

pub(crate) fn jobj() -> Json {
    Json::Object(BTreeMap::new())
}

pub(crate) fn jarr() -> Json {
    Json::Array(Vec::new())
}

pub(crate) fn juint(v: u64) -> Json {
    Json::UInt(v)
}

pub(crate) trait JsonExt {
    fn set(&mut self, key: &str, value: Json);
    fn push(&mut self, value: Json);
    fn get(&self, key: &str) -> Option<&Json>;
    fn is_empty_container(&self) -> bool;
}

impl JsonExt for Json {
    fn set(&mut self, key: &str, value: Json) {
        if !matches!(self, Json::Object(_)) {
            *self = jobj();
        }
        if let Json::Object(map) = self {
            map.insert(key.as_bytes().to_vec(), value);
        }
    }

    fn push(&mut self, value: Json) {
        if !matches!(self, Json::Array(_)) {
            *self = jarr();
        }
        if let Json::Array(items) = self {
            items.push(value);
        }
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(map) => map.get(key.as_bytes()),
            _ => None,
        }
    }

    /// `Json::Value::empty` for arrays and objects.
    fn is_empty_container(&self) -> bool {
        match self {
            Json::Array(items) => items.is_empty(),
            Json::Object(map) => map.is_empty(),
            Json::Null => true,
            _ => false,
        }
    }
}

/// Converts a parsed jsoncpp value into a writable value.
fn from_parsed(value: &jsoncpp::Value) -> Json {
    match &value.kind {
        jsoncpp::Kind::Null => Json::Null,
        jsoncpp::Kind::Int(v) => Json::Int(*v),
        jsoncpp::Kind::UInt(v) => Json::UInt(*v),
        jsoncpp::Kind::Real(v) => Json::Real(*v),
        jsoncpp::Kind::Str(s) => jstr(s),
        jsoncpp::Kind::Bool(b) => Json::Bool(*b),
        jsoncpp::Kind::Array(items) => Json::Array(items.iter().map(|i| from_parsed(i)).collect()),
        jsoncpp::Kind::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), from_parsed(v)))
                .collect(),
        ),
    }
}

// ---------------------------------------------------------------------
// cmFileAPI

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ObjectKind {
    CodeModel,
    ConfigureLog,
    Cache,
    CMakeFiles,
    Toolchains,
    InternalTest,
}

impl ObjectKind {
    fn name(self) -> &'static str {
        match self {
            ObjectKind::CodeModel => "codemodel",
            ObjectKind::ConfigureLog => "configureLog",
            ObjectKind::Cache => "cache",
            ObjectKind::CMakeFiles => "cmakeFiles",
            ObjectKind::Toolchains => "toolchains",
            ObjectKind::InternalTest => "__test",
        }
    }
}

const CODEMODEL_V2_MINOR: u32 = 8;
const CONFIGURE_LOG_V1_MINOR: u32 = 0;
const CACHE_V2_MINOR: u32 = 0;
const CMAKE_FILES_V1_MINOR: u32 = 1;
const TOOLCHAINS_V1_MINOR: u32 = 0;
const INTERNAL_TEST_V1_MINOR: u32 = 3;
const INTERNAL_TEST_V2_MINOR: u32 = 0;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Object {
    kind: ObjectKind,
    version: u64,
}

#[derive(Default)]
struct Query {
    known: Vec<Object>,
    unknown: Vec<String>,
}

struct ClientRequest {
    object: Object,
    error: String,
}

#[derive(Default)]
struct ClientRequests {
    requests: Vec<ClientRequest>,
    error: String,
}

#[derive(Default)]
struct ClientQueryJson {
    error: String,
    client_value: Option<Json>,
    requests_value: Option<Json>,
    requests: ClientRequests,
}

#[derive(Default)]
struct ClientQuery {
    dir_query: Query,
    have_query_json: bool,
    query_json: ClientQueryJson,
}

/// Identifies the situation in which `WriteReplies` is called.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IndexFor {
    Success,
    FailedConfigure,
    /// `FailedCompute` and `FailedGenerate` write the same replies; the
    /// generator reports both as a failed generate.
    FailedGenerate,
}

pub struct FileApi {
    api_v1: String,
    user_api_v1: String,
    reply_files: BTreeSet<String>,
    query_exists: bool,
    top_query: Query,
    client_queries: BTreeMap<String, ClientQuery>,
    reply_index_objects: BTreeMap<Object, Json>,
    reply_index_for: IndexFor,
}

/// `cmSystemTools::GetCMakeConfigDirectory`.
fn cmake_config_directory() -> Option<String> {
    if let Ok(dir) = std::env::var("CMAKE_CONFIG_DIR") {
        return Some(dir);
    }
    let config = std::env::var("XDG_CONFIG_HOME").ok().or_else(|| {
        let home = std::env::var("HOME").ok()?;
        Some(format!("{home}/.config"))
    })?;
    Some(format!("{config}/cmake"))
}

/// `cmFileAPI::LoadDir`.
fn load_dir(dir: &str) -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn build_version(major: u32, minor: u32) -> Json {
    let mut version = jobj();
    version.set("major", juint(major.into()));
    version.set("minor", juint(minor.into()));
    version
}

fn build_reply_error(error: &str) -> Json {
    let mut e = jobj();
    e.set("error", jstr(error));
    e
}

/// The jsoncpp reader `cmFileAPI` configures: comments and trailing commas
/// allowed, failIfExtra, duplicate keys accepted, strictRoot.
const QUERY_FEATURES: jsoncpp::Features = jsoncpp::Features {
    allow_comments: true,
    allow_trailing_commas: true,
    strict_root: true,
    fail_if_extra: true,
    reject_dup_keys: false,
};

struct RequestVersion {
    major: u32,
    minor: u32,
}

impl FileApi {
    pub fn new(home_binary: &str) -> FileApi {
        FileApi {
            api_v1: format!("{home_binary}/.cmake/api/v1"),
            user_api_v1: cmake_config_directory()
                .map(|dir| format!("{dir}/api/v1"))
                .unwrap_or_default(),
            reply_files: BTreeSet::new(),
            query_exists: false,
            top_query: Query::default(),
            client_queries: BTreeMap::new(),
            reply_index_objects: BTreeMap::new(),
            reply_index_for: IndexFor::Success,
        }
    }

    /// `ReadQueries`.
    pub fn read_queries(&mut self) {
        let query_dir = format!("{}/query", self.api_v1);
        let user_query_dir = format!("{}/query", self.user_api_v1);
        self.query_exists = std::path::Path::new(&query_dir).is_dir();
        if !self.user_api_v1.is_empty() {
            self.query_exists = self.query_exists || std::path::Path::new(&user_query_dir).is_dir();
        }
        if !self.query_exists {
            return;
        }
        let mut queries = load_dir(&query_dir);
        if !self.user_api_v1.is_empty() {
            queries.extend(load_dir(&user_query_dir));
        }
        for query in queries {
            if query.starts_with("client-") {
                self.read_client(&query);
            } else if !read_query(&query, &mut self.top_query.known) {
                self.top_query.unknown.push(query);
            }
        }
    }

    /// The `cmake_file_api(QUERY)` requests recorded during configure.
    pub fn add_project_queries(&mut self, interp: &Interp) {
        for (keyword, (major, minor)) in &interp.ext.file_api_queries {
            let kind = match keyword.as_str() {
                "CODEMODEL" => ObjectKind::CodeModel,
                "CACHE" => ObjectKind::Cache,
                "CMAKEFILES" => ObjectKind::CMakeFiles,
                "TOOLCHAINS" => ObjectKind::Toolchains,
                _ => continue,
            };
            self.add_project_query(kind, *major, *minor);
        }
    }

    /// `AddProjectQuery` (the cmake_file_api() command).
    pub fn add_project_query(&mut self, kind: ObjectKind, major: u32, minor: u32) -> bool {
        let ok = match kind {
            ObjectKind::CodeModel => major == 2 && minor <= CODEMODEL_V2_MINOR,
            ObjectKind::Cache => major == 2 && minor <= CACHE_V2_MINOR,
            ObjectKind::CMakeFiles => major == 1 && minor <= CMAKE_FILES_V1_MINOR,
            ObjectKind::Toolchains => major == 1 && minor <= TOOLCHAINS_V1_MINOR,
            ObjectKind::ConfigureLog | ObjectKind::InternalTest => false,
        };
        if !ok {
            return false;
        }
        let query = Object {
            kind,
            version: major.into(),
        };
        if !self.top_query.known.contains(&query) {
            self.top_query.known.push(query);
            self.query_exists = true;
        }
        true
    }

    fn read_client(&mut self, client: &str) {
        let client_dir = format!("{}/query/{client}", self.api_v1);
        let queries = load_dir(&client_dir);
        let mut client_query = self.client_queries.remove(client).unwrap_or_default();
        for query in queries {
            if query == "query.json" {
                client_query.have_query_json = true;
                client_query.query_json = self.read_client_query(client);
            } else if !read_query(&query, &mut client_query.dir_query.known) {
                client_query.dir_query.unknown.push(query);
            }
        }
        self.client_queries.insert(client.to_owned(), client_query);
    }

    fn read_client_query(&self, client: &str) -> ClientQueryJson {
        let mut q = ClientQueryJson::default();
        let file = format!("{}/query/{client}/query.json", self.api_v1);
        let query = match read_json_file(&file) {
            Ok(query) => query,
            Err(error) => {
                q.error = error;
                return q;
            }
        };
        if !query.is_object() {
            q.error = "query root is not an object".to_owned();
            return q;
        }
        if let Some(client_value) = query.member("client")
            && !client_value.is_null()
        {
            q.client_value = Some(from_parsed(&client_value));
        }
        let requests = query.member("requests");
        q.requests_value = requests.as_ref().map(|r| from_parsed(r));
        q.requests = build_client_requests(requests.as_ref());
        q
    }

    /// `WriteReplies`.
    pub fn write_replies(&mut self, interp: &Interp, index_for: IndexFor) {
        let success = index_for == IndexFor::Success;
        self.reply_index_for = index_for;
        if self.query_exists {
            let _ = std::fs::create_dir_all(format!("{}/reply", self.api_v1));
            let index = self.build_reply_index(interp);
            self.write_json_file(&index, if success { "index" } else { "error" }, true);
        }
        if success {
            self.remove_old_reply_files();
        }
    }

    fn remove_old_reply_files(&self) {
        let reply_dir = format!("{}/reply", self.api_v1);
        for f in load_dir(&reply_dir) {
            if !self.reply_files.contains(&f) {
                let path = format!("{reply_dir}/{f}");
                if !std::path::Path::new(&path).is_dir() {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }

    /// `WriteJsonFile`; `time_suffix` selects `ComputeSuffixTime` over
    /// `ComputeSuffixHash`.
    fn write_json_file(&mut self, value: &Json, prefix: &str, time_suffix: bool) -> String {
        let tmp_file = format!("{}/tmp.json", self.api_v1);
        let mut text = write_json_ostream(value);
        text.push('\n');
        if std::fs::write(&tmp_file, &text).is_err() {
            let _ = std::fs::remove_file(&tmp_file);
            return String::new();
        }
        let mut suffix = if time_suffix {
            compute_suffix_time()
        } else {
            use sha3::Digest;
            let digest = sha3::Sha3_256::digest(text.as_bytes());
            let mut hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            hash.truncate(20);
            hash
        };
        let mut suffix_with_extension = format!("-{suffix}.json");
        let mut file_name = format!("{prefix}{suffix_with_extension}");
        const MAX_FILE_NAME_LENGTH: usize = 140;
        if file_name.len() > MAX_FILE_NAME_LENGTH {
            const NEW_HASH_LENGTH: usize = 20;
            let new_suffix_length = suffix_with_extension.len() - suffix.len() + NEW_HASH_LENGTH;
            let over_length = file_name.len() - MAX_FILE_NAME_LENGTH + new_suffix_length;
            let start = file_name.len() - over_length;
            let to_be_removed = file_name[start..].to_owned();
            use sha2::Digest;
            let digest = sha2::Sha256::digest(to_be_removed.as_bytes());
            suffix = digest.iter().map(|b| format!("{b:02x}")).collect();
            suffix.truncate(NEW_HASH_LENGTH);
            suffix_with_extension = format!("-{suffix}.json");
            file_name.replace_range(start..start + over_length, &suffix_with_extension);
        }
        let dir = format!("{}/reply", self.api_v1);
        let _ = std::fs::create_dir_all(&dir);
        let file = format!("{dir}/{file_name}");
        if std::path::Path::new(&file).exists() || std::fs::rename(&tmp_file, &file).is_err() {
            let _ = std::fs::remove_file(&tmp_file);
        }
        self.reply_files.insert(file_name.clone());
        file_name
    }

    fn build_reply_index(&mut self, interp: &Interp) -> Json {
        let mut index = jobj();
        index.set("cmake", build_cmake(interp));
        let top = std::mem::take(&mut self.top_query);
        let mut reply = self.build_reply(interp, &top);
        self.top_query = top;
        let clients = std::mem::take(&mut self.client_queries);
        for (name, client) in &clients {
            let client_reply = self.build_client_reply(interp, client);
            reply.set(name, client_reply);
        }
        self.client_queries = clients;
        index.set("reply", reply);
        let mut objects = jarr();
        for entry in self.reply_index_objects.values() {
            objects.push(entry.clone());
        }
        index.set("objects", objects);
        index
    }

    fn build_reply(&mut self, interp: &Interp, q: &Query) -> Json {
        let mut reply = jobj();
        for o in &q.known {
            let entry = self.build_reply_entry(interp, *o);
            reply.set(&object_name(*o), entry);
        }
        for name in &q.unknown {
            reply.set(name, build_reply_error("unknown query file"));
        }
        reply
    }

    fn build_reply_entry(&mut self, interp: &Interp, object: Object) -> Json {
        if self.reply_index_for != IndexFor::Success && object.kind != ObjectKind::ConfigureLog {
            return build_reply_error("no buildsystem generated");
        }
        self.add_reply_index_object(interp, object)
    }

    fn add_reply_index_object(&mut self, interp: &Interp, o: Object) -> Json {
        if let Some(entry) = self.reply_index_objects.get(&o) {
            return entry.clone();
        }
        let object = self.build_object(interp, o);
        let mut entry = jobj();
        entry.set("kind", object.get("kind").cloned().unwrap_or(Json::Null));
        entry.set(
            "version",
            object.get("version").cloned().unwrap_or(Json::Null),
        );
        let file = self.write_json_file(&object, &object_name(o), false);
        entry.set("jsonFile", jstr(file));
        self.reply_index_objects.insert(o, entry.clone());
        entry
    }

    fn build_object(&mut self, interp: &Interp, object: Object) -> Json {
        let (mut value, version) = match object.kind {
            ObjectKind::CodeModel => (
                codemodel::build(self, interp),
                build_version(2, CODEMODEL_V2_MINOR),
            ),
            ObjectKind::ConfigureLog => (
                dump_configure_log(interp),
                build_version(1, CONFIGURE_LOG_V1_MINOR),
            ),
            ObjectKind::Cache => (dump_cache(interp), build_version(2, CACHE_V2_MINOR)),
            ObjectKind::CMakeFiles => (
                dump_cmake_files(interp),
                build_version(1, CMAKE_FILES_V1_MINOR),
            ),
            ObjectKind::Toolchains => (
                dump_toolchains(interp),
                build_version(1, TOOLCHAINS_V1_MINOR),
            ),
            ObjectKind::InternalTest => (
                jobj(),
                if object.version == 2 {
                    build_version(2, INTERNAL_TEST_V2_MINOR)
                } else {
                    build_version(1, INTERNAL_TEST_V1_MINOR)
                },
            ),
        };
        value.set("kind", jstr(object.kind.name()));
        value.set("version", version);
        value
    }

    fn build_client_reply(&mut self, interp: &Interp, q: &ClientQuery) -> Json {
        let mut reply = self.build_reply(interp, &q.dir_query);
        if !q.have_query_json {
            return reply;
        }
        let qj = &q.query_json;
        let reply_query_json = if !qj.error.is_empty() {
            build_reply_error(&qj.error)
        } else {
            let mut r = jobj();
            if let Some(client) = &qj.client_value {
                r.set("client", client.clone());
            }
            if let Some(requests) = &qj.requests_value
                && !matches!(requests, Json::Null)
            {
                r.set("requests", requests.clone());
            }
            let responses = if !qj.requests.error.is_empty() {
                build_reply_error(&qj.requests.error)
            } else {
                let mut responses = jarr();
                for request in &qj.requests.requests {
                    let response = if !request.error.is_empty() {
                        build_reply_error(&request.error)
                    } else {
                        self.build_reply_entry(interp, request.object)
                    };
                    responses.push(response);
                }
                responses
            };
            r.set("responses", responses);
            r
        };
        reply.set("query.json", reply_query_json);
        reply
    }
}

fn object_name(o: Object) -> String {
    format!("{}-v{}", o.kind.name(), o.version)
}

/// `cmFileAPI::ReadQuery`.
fn read_query(query: &str, objects: &mut Vec<Object>) -> bool {
    let Some((kind_name, version)) = query.split_once('-') else {
        return false;
    };
    let (kind, version) = match (kind_name, version) {
        ("codemodel", "v2") => (ObjectKind::CodeModel, 2),
        ("configureLog", "v1") => (ObjectKind::ConfigureLog, 1),
        ("cache", "v2") => (ObjectKind::Cache, 2),
        ("cmakeFiles", "v1") => (ObjectKind::CMakeFiles, 1),
        ("toolchains", "v1") => (ObjectKind::Toolchains, 1),
        ("__test", "v1") => (ObjectKind::InternalTest, 1),
        ("__test", "v2") => (ObjectKind::InternalTest, 2),
        _ => return false,
    };
    objects.push(Object { kind, version });
    true
}

/// `cmFileAPI::ReadJsonFile`.
fn read_json_file(file: &str) -> Result<jsoncpp::V, String> {
    if std::path::Path::new(file).is_dir() {
        return Err("failed to read from file".to_owned());
    }
    let Ok(content) = std::fs::read(file) else {
        return Err("failed to read from file".to_owned());
    };
    let parsed = jsoncpp::parse_with(&content, QUERY_FEATURES);
    if !parsed.errors.is_empty() {
        return Err(parsed.formatted_errors(&content));
    }
    Ok(std::rc::Rc::new(parsed.root))
}

fn build_client_requests(requests: Option<&jsoncpp::V>) -> ClientRequests {
    let mut result = ClientRequests::default();
    let Some(requests) = requests.filter(|r| !r.is_null()) else {
        result.error = "'requests' member missing".to_owned();
        return result;
    };
    if !requests.is_array() {
        result.error = "'requests' member is not an array".to_owned();
        return result;
    }
    for request in requests.items() {
        result.requests.push(build_client_request(request));
    }
    result
}

fn build_client_request(request: &jsoncpp::V) -> ClientRequest {
    let mut r = ClientRequest {
        object: Object {
            kind: ObjectKind::CodeModel,
            version: 0,
        },
        error: String::new(),
    };
    if !request.is_object() {
        r.error = "request is not an object".to_owned();
        return r;
    }
    let Some(kind) = request.member("kind").filter(|k| !k.is_null()) else {
        r.error = "'kind' member missing".to_owned();
        return r;
    };
    if !kind.is_string() {
        r.error = "'kind' member is not a string".to_owned();
        return r;
    }
    let kind_name = kind.as_string();
    r.object.kind = match kind_name.as_str() {
        "codemodel" => ObjectKind::CodeModel,
        "configureLog" => ObjectKind::ConfigureLog,
        "cache" => ObjectKind::Cache,
        "cmakeFiles" => ObjectKind::CMakeFiles,
        "toolchains" => ObjectKind::Toolchains,
        "__test" => ObjectKind::InternalTest,
        _ => {
            r.error = format!("unknown request kind '{kind_name}'");
            return r;
        }
    };
    let Some(version) = request.member("version").filter(|v| !v.is_null()) else {
        r.error = "'version' member missing".to_owned();
        return r;
    };
    let mut versions = Vec::new();
    if let Err(error) = read_request_versions(&version, &mut versions) {
        r.error = error;
        return r;
    }
    let supported = |v: &RequestVersion| -> bool {
        match r.object.kind {
            ObjectKind::CodeModel => v.major == 2 && v.minor <= CODEMODEL_V2_MINOR,
            ObjectKind::ConfigureLog => v.major == 1 && v.minor <= CONFIGURE_LOG_V1_MINOR,
            ObjectKind::Cache => v.major == 2 && v.minor <= CACHE_V2_MINOR,
            ObjectKind::CMakeFiles => v.major == 1 && v.minor <= CMAKE_FILES_V1_MINOR,
            ObjectKind::Toolchains => v.major == 1 && v.minor <= TOOLCHAINS_V1_MINOR,
            ObjectKind::InternalTest => {
                (v.major == 1 && v.minor <= INTERNAL_TEST_V1_MINOR)
                    || (v.major == 2 && v.minor <= INTERNAL_TEST_V2_MINOR)
            }
        }
    };
    if let Some(v) = versions.iter().find(|v| supported(v)) {
        r.object.version = v.major.into();
    }
    if r.object.version == 0 {
        // NoSupportedVersion
        let mut message = "no supported version specified".to_owned();
        if !versions.is_empty() {
            message += " among:";
            for v in &versions {
                message += &format!(" {}.{}", v.major, v.minor);
            }
        }
        r.error = message;
    }
    r
}

fn read_request_versions(
    version: &jsoncpp::V,
    versions: &mut Vec<RequestVersion>,
) -> Result<(), String> {
    if version.is_array() {
        for v in version.items() {
            read_request_version(v, true, versions)?;
        }
        Ok(())
    } else {
        read_request_version(version, false, versions)
    }
}

fn read_request_version(
    version: &jsoncpp::V,
    in_array: bool,
    result: &mut Vec<RequestVersion>,
) -> Result<(), String> {
    if version.is_uint() {
        result.push(RequestVersion {
            major: version.as_uint(),
            minor: 0,
        });
        return Ok(());
    }
    if !version.is_object() {
        return Err(if in_array {
            "'version' array entry is not a non-negative integer or object".to_owned()
        } else {
            "'version' member is not a non-negative integer, object, or array".to_owned()
        });
    }
    let Some(major) = version.member("major").filter(|m| !m.is_null()) else {
        return Err("'version' object 'major' member missing".to_owned());
    };
    if !major.is_uint() {
        return Err("'version' object 'major' member is not a non-negative integer".to_owned());
    }
    let mut v = RequestVersion {
        major: major.as_uint(),
        minor: 0,
    };
    match version.member("minor") {
        Some(minor) if minor.is_uint() => v.minor = minor.as_uint(),
        Some(minor) if !minor.is_null() => {
            return Err("'version' object 'minor' member is not a non-negative integer".to_owned());
        }
        _ => {}
    }
    result.push(v);
    Ok(())
}

/// `ComputeSuffixTime`: UTC `%Y-%m-%dT%H-%M-%S-<ms>`.
fn compute_suffix_time() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let ms = now.subsec_millis();
    // SAFETY: gmtime_r writes into the provided struct.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&secs, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}T{:02}-{:02}-{:02}-{:04}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ms
    )
}

/// `cmFileAPI::BuildCMake`.
fn build_cmake(interp: &Interp) -> Json {
    let mut cmake = jobj();
    let mut version = jobj();
    version.set("string", jstr(crate::interp::VERSION_STRING));
    version.set("major", juint(crate::interp::VERSION.0.into()));
    version.set("minor", juint(crate::interp::VERSION.1.into()));
    version.set("suffix", jstr(""));
    version.set("isDirty", Json::Bool(false));
    version.set("patch", juint(crate::interp::VERSION.2.into()));
    cmake.set("version", version);
    let bin_dir = crate::util::filename_path(&interp.cmake_command);
    let mut paths = jobj();
    paths.set("cmake", jstr(&interp.cmake_command));
    paths.set("ctest", jstr(format!("{bin_dir}/ctest")));
    paths.set("cpack", jstr(format!("{bin_dir}/cpack")));
    paths.set("root", jstr(&interp.cmake_root));
    cmake.set("paths", paths);
    let mut generator = jobj();
    generator.set(
        "name",
        jstr(interp.cache.value("CMAKE_GENERATOR").unwrap_or("Ninja")),
    );
    generator.set("multiConfig", Json::Bool(interp.ext.multi_config));
    cmake.set("generator", generator);
    cmake
}

// ---------------------------------------------------------------------
// cmFileAPICache.cxx

fn dump_cache(interp: &Interp) -> Json {
    let mut entries = jarr();
    let mut names: Vec<&String> = interp.cache.entries.keys().collect();
    names.sort();
    for name in names {
        let entry = &interp.cache.entries[name];
        let mut e = jobj();
        e.set("name", jstr(name));
        e.set("type", jstr(entry.kind.as_str()));
        e.set("value", jstr(&entry.value));
        let mut properties = jarr();
        let mut props: Vec<&String> = entry.properties.keys().collect();
        props.sort();
        for prop in props {
            let mut p = jobj();
            p.set("name", jstr(prop));
            p.set("value", jstr(&entry.properties[prop]));
            properties.push(p);
        }
        if !properties.is_empty_container() {
            e.set("properties", properties);
        }
        entries.push(e);
    }
    let mut cache = jobj();
    cache.set("entries", entries);
    cache
}

// ---------------------------------------------------------------------
// cmFileAPICMakeFiles.cxx

fn dump_cmake_files(interp: &Interp) -> Json {
    let top_source = &interp.home_source;
    let top_build = &interp.home_binary;
    let out_of_source = top_build != top_source;
    let cmake_modules = format!("{}/Modules", interp.cmake_root);
    let mut paths = jobj();
    paths.set("source", jstr(top_source));
    paths.set("build", jstr(top_build));
    let mut inputs = jarr();
    for mf in &interp.makefiles {
        for file in &mf.list_files {
            let mut input = jobj();
            let is_cmake = crate::util::is_subdirectory(file, &cmake_modules);
            if is_cmake {
                input.set("isCMake", Json::Bool(true));
            }
            if !crate::util::is_subdirectory(file, top_source)
                && !crate::util::is_subdirectory(file, top_build)
            {
                input.set("isExternal", Json::Bool(true));
            }
            if out_of_source && crate::util::is_subdirectory(file, top_build) {
                input.set("isGenerated", Json::Bool(true));
            }
            let mut path = file.clone();
            if !is_cmake && crate::util::is_subdirectory(&path, top_source) {
                path = crate::util::relative_if_under(top_source, &path);
            }
            input.set("path", jstr(path));
            inputs.push(input);
        }
    }
    let mut cmake_files = jobj();
    cmake_files.set("paths", paths);
    cmake_files.set("inputs", inputs);
    // cmGlobVerificationManager::GetCacheEntries: one entry per key, in key
    // order (Recurse, ListDirectories, FollowSymlinks, Relative, Expression).
    let mut globs: BTreeMap<
        (bool, bool, bool, String, String),
        &crate::commands::file::GlobDependency,
    > = BTreeMap::new();
    for entry in &interp.ext.glob_dependencies {
        globs
            .entry((
                entry.recurse,
                entry.list_directories,
                entry.follow_symlinks,
                entry.relative.clone(),
                entry.expression.clone(),
            ))
            .or_insert(entry);
    }
    let mut globs_dependent = jarr();
    for entry in globs.values() {
        let mut g = jobj();
        g.set("expression", jstr(&entry.expression));
        if entry.recurse {
            g.set("recurse", Json::Bool(true));
        }
        if entry.list_directories {
            g.set("listDirectories", Json::Bool(true));
        }
        if entry.follow_symlinks {
            g.set("followSymlinks", Json::Bool(true));
        }
        if !entry.relative.is_empty() {
            g.set("relative", jstr(&entry.relative));
        }
        let mut paths = jarr();
        for file in &entry.files {
            paths.push(jstr(file));
        }
        g.set("paths", paths);
        globs_dependent.push(g);
    }
    if !globs_dependent.is_empty_container() {
        cmake_files.set("globsDependent", globs_dependent);
    }
    cmake_files
}

// ---------------------------------------------------------------------
// cmFileAPIToolchains.cxx

/// `cmMakefile::GetDefinition` on the top-level directory after configure.
pub(crate) fn top_definition(interp: &Interp, name: &str) -> Option<String> {
    match interp
        .dir_final_vars
        .first()
        .and_then(|vars| vars.get(name))
    {
        Some(value) => value.clone(),
        None => interp.cache.value(name).map(str::to_owned),
    }
}

fn dump_toolchains(interp: &Interp) -> Json {
    let mut languages = interp.langs.enabled.clone();
    languages.sort();
    languages.dedup();
    let mut toolchains = jarr();
    let set_var = |object: &mut Json, lang: &str, key: &str, suffix: &str, list: bool| {
        let name = format!("CMAKE_{lang}_{suffix}");
        if let Some(value) = top_definition(interp, &name) {
            if list {
                let mut array = jarr();
                for item in crate::util::expand_list(&value, false) {
                    array.push(jstr(item));
                }
                object.set(key, array);
            } else {
                object.set(key, jstr(value));
            }
        }
    };
    for lang in &languages {
        let mut toolchain = jobj();
        toolchain.set("language", jstr(lang));
        let mut compiler = jobj();
        set_var(&mut compiler, lang, "path", "COMPILER", false);
        set_var(&mut compiler, lang, "id", "COMPILER_ID", false);
        set_var(&mut compiler, lang, "version", "COMPILER_VERSION", false);
        set_var(&mut compiler, lang, "target", "COMPILER_TARGET", false);
        let mut implicit = jobj();
        set_var(
            &mut implicit,
            lang,
            "includeDirectories",
            "IMPLICIT_INCLUDE_DIRECTORIES",
            true,
        );
        set_var(
            &mut implicit,
            lang,
            "linkDirectories",
            "IMPLICIT_LINK_DIRECTORIES",
            true,
        );
        set_var(
            &mut implicit,
            lang,
            "linkFrameworkDirectories",
            "IMPLICIT_LINK_FRAMEWORK_DIRECTORIES",
            true,
        );
        set_var(
            &mut implicit,
            lang,
            "linkLibraries",
            "IMPLICIT_LINK_LIBRARIES",
            true,
        );
        compiler.set("implicit", implicit);
        toolchain.set("compiler", compiler);
        set_var(
            &mut toolchain,
            lang,
            "sourceFileExtensions",
            "SOURCE_FILE_EXTENSIONS",
            true,
        );
        toolchains.push(toolchain);
    }
    let mut out = jobj();
    out.set("toolchains", toolchains);
    out
}

// ---------------------------------------------------------------------
// cmFileAPIConfigureLog.cxx

fn dump_configure_log(interp: &Interp) -> Json {
    let mut out = jobj();
    out.set(
        "path",
        jstr(format!(
            "{}/CMakeFiles/CMakeConfigureLog.yaml",
            interp.home_binary
        )),
    );
    let mut names = jarr();
    for name in [
        "message-v1",
        "try_compile-v1",
        "try_run-v1",
        "find-v1",
        "find_package-v1",
    ] {
        names.push(jstr(name));
    }
    out.set("eventKindNames", names);
    out
}
