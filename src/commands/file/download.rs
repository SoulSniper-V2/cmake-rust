//! file(DOWNLOAD) and file(UPLOAD): `HandleDownloadCommand` and
//! `HandleUploadCommand` of cmFileCommand.cxx with the helpers of cmCurl.cxx
//! (`cmCurlParseTLSVersion`, `cmCurlSetCAInfo`, `cmCurlSetNETRCOption`,
//! `cmCurlFixFileURL`, `cm_curl_easy_init`).
//!
//! Like CMake, the transfers run on libcurl. Kitware links a vendored curl;
//! this port loads the system libcurl (`libcurl.so.4`) at the first transfer,
//! so cmake-rust has no hard dependency on it. If it cannot be loaded, the
//! command fails with an explicit error instead of reporting a transfer
//! outcome that never happened. CURLcode values are ABI-stable, and status
//! text comes from the vendored curl's `curl_easy_strerror` table
//! (curl_strerror.rs, generated from the pinned sources), so messages match
//! CMake 4.1.0 whatever libcurl version the host has.

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_void};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::sync::OnceLock;

use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::util::is_on;

type Easy = *mut c_void;

// curl.h: CURLoption values (CURLOPTTYPE_LONG 0, OBJECTPOINT 10000,
// FUNCTIONPOINT 20000).
const CURLOPT_WRITEDATA: c_int = 10001;
const CURLOPT_URL: c_int = 10002;
const CURLOPT_USERPWD: c_int = 10005;
const CURLOPT_RANGE: c_int = 10007;
const CURLOPT_INFILE: c_int = 10009;
const CURLOPT_WRITEFUNCTION: c_int = 20011;
const CURLOPT_TIMEOUT: c_int = 13;
const CURLOPT_INFILESIZE: c_int = 14;
const CURLOPT_USERAGENT: c_int = 10018;
const CURLOPT_LOW_SPEED_LIMIT: c_int = 19;
const CURLOPT_LOW_SPEED_TIME: c_int = 20;
const CURLOPT_HTTPHEADER: c_int = 10023;
const CURLOPT_SSLVERSION: c_int = 32;
const CURLOPT_VERBOSE: c_int = 41;
const CURLOPT_NOPROGRESS: c_int = 43;
const CURLOPT_FAILONERROR: c_int = 45;
const CURLOPT_UPLOAD: c_int = 46;
const CURLOPT_NETRC: c_int = 51;
const CURLOPT_FOLLOWLOCATION: c_int = 52;
const CURLOPT_PROGRESSDATA: c_int = 10057;
const CURLOPT_SSL_VERIFYPEER: c_int = 64;
const CURLOPT_CAINFO: c_int = 10065;
const CURLOPT_HTTP_VERSION: c_int = 84;
const CURLOPT_DEBUGFUNCTION: c_int = 20094;
const CURLOPT_DEBUGDATA: c_int = 10095;
const CURLOPT_CAPATH: c_int = 10097;
const CURLOPT_NETRC_FILE: c_int = 10118;
const CURLOPT_XFERINFOFUNCTION: c_int = 20219;

const CURLE_OK: c_int = 0;
const CURLE_NOT_BUILT_IN: c_int = 4;
const CURLE_WRITE_ERROR: c_int = 23;
const CURLE_SSL_CONNECT_ERROR: c_int = 35;
const CURLE_PEER_FAILED_VERIFICATION: c_int = 60;

const CURL_GLOBAL_DEFAULT: c_long = 3;
const CURL_HTTP_VERSION_1_1: c_long = 2;
const CURL_WRITEFUNC_ERROR: usize = 0xFFFF_FFFF;

const CURL_NETRC_IGNORED: c_long = 0;
const CURL_NETRC_OPTIONAL: c_long = 1;
const CURL_NETRC_REQUIRED: c_long = 2;
const CURL_NETRC_LAST: c_long = 3;

// curl_infotype
const CURLINFO_TEXT: c_int = 0;
const CURLINFO_HEADER_IN: c_int = 1;
const CURLINFO_HEADER_OUT: c_int = 2;
const CURLINFO_DATA_IN: c_int = 3;
const CURLINFO_DATA_OUT: c_int = 4;
const CURLINFO_SSL_DATA_IN: c_int = 5;
const CURLINFO_SSL_DATA_OUT: c_int = 6;

const TLS_VERIFY_DEFAULT: bool = true;
const TLS_VERSION_DEFAULT: &str = "1.2";

/// The leading fields of `curl_version_info_data`.
#[repr(C)]
struct VersionInfo {
    age: c_int,
    version: *const c_char,
    version_num: c_uint,
}

/// The libcurl entry points cmFileCommand uses.
struct Curl {
    global_init: unsafe extern "C" fn(c_long) -> c_int,
    global_cleanup: unsafe extern "C" fn(),
    easy_init: unsafe extern "C" fn() -> Easy,
    easy_setopt: unsafe extern "C" fn(Easy, c_int, ...) -> c_int,
    easy_perform: unsafe extern "C" fn(Easy) -> c_int,
    easy_cleanup: unsafe extern "C" fn(Easy),
    slist_append: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    slist_free_all: unsafe extern "C" fn(*mut c_void),
    version_info: unsafe extern "C" fn(c_int) -> *const VersionInfo,
}

// SAFETY: the table holds plain function pointers into a library that
// stays loaded for the life of the process.
unsafe impl Send for Curl {}
unsafe impl Sync for Curl {}

fn curl() -> Result<&'static Curl, String> {
    static CURL: OnceLock<Result<Curl, String>> = OnceLock::new();
    CURL.get_or_init(load_curl).as_ref().map_err(Clone::clone)
}

fn load_curl() -> Result<Curl, String> {
    let mut handle = std::ptr::null_mut();
    let mut reason = String::new();
    for name in [c"libcurl.so.4", c"libcurl.so"] {
        // SAFETY: dlopen with a valid NUL-terminated name.
        handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if !handle.is_null() {
            break;
        }
        // SAFETY: dlerror returns a thread-local message or null.
        let message = unsafe { libc::dlerror() };
        if !message.is_null() && reason.is_empty() {
            // SAFETY: non-null dlerror result is a C string.
            reason = unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned();
        }
    }
    if handle.is_null() {
        return Err(reason);
    }
    macro_rules! sym {
        ($name:literal) => {{
            // SAFETY: handle is a loaded library; the name is NUL-terminated.
            let symbol = unsafe { libc::dlsym(handle, concat!($name, "\0").as_ptr().cast()) };
            if symbol.is_null() {
                return Err(format!("libcurl does not export {}", $name));
            }
            // SAFETY: the symbol has the C signature declared in Curl.
            unsafe { std::mem::transmute::<*mut c_void, _>(symbol) }
        }};
    }
    Ok(Curl {
        global_init: sym!("curl_global_init"),
        global_cleanup: sym!("curl_global_cleanup"),
        easy_init: sym!("curl_easy_init"),
        easy_setopt: sym!("curl_easy_setopt"),
        easy_perform: sym!("curl_easy_perform"),
        easy_cleanup: sym!("curl_easy_cleanup"),
        slist_append: sym!("curl_slist_append"),
        slist_free_all: sym!("curl_slist_free_all"),
        version_info: sym!("curl_version_info"),
    })
}

impl Curl {
    /// `curl_easy_strerror` of the vendored curl.
    fn strerror(&self, code: c_int) -> String {
        super::curl_strerror::curl_easy_strerror(code).to_owned()
    }

    fn version(&self) -> Option<(String, c_uint)> {
        // SAFETY: CURLVERSION_FIRST is always supported.
        let info = unsafe { (self.version_info)(0) };
        if info.is_null() {
            return None;
        }
        // SAFETY: info points at the library's static version data.
        let info = unsafe { &*info };
        let _ = info.age;
        // SAFETY: version is a static C string.
        let version = unsafe { CStr::from_ptr(info.version) }
            .to_string_lossy()
            .into_owned();
        Some((version, info.version_num))
    }

    fn set_long(&self, easy: Easy, option: c_int, value: c_long) -> c_int {
        // SAFETY: long-valued option on a live handle.
        unsafe { (self.easy_setopt)(easy, option, value) }
    }

    fn set_ptr(&self, easy: Easy, option: c_int, value: *const c_void) -> c_int {
        // SAFETY: pointer-valued option on a live handle; callers keep the
        // pointee alive for as long as libcurl may use it.
        unsafe { (self.easy_setopt)(easy, option, value) }
    }

    /// String options: libcurl copies the value.
    fn set_str(&self, easy: Easy, option: c_int, value: &str) -> c_int {
        let value = c_string(value);
        self.set_ptr(easy, option, value.as_ptr().cast())
    }

    /// `cm_curl_easy_init`.
    fn easy_init(&self) -> Easy {
        // SAFETY: plain libcurl call.
        let easy = unsafe { (self.easy_init)() };
        if !easy.is_null()
            && let Some((_, num)) = self.version()
            && (0x080700..0x080800).contains(&num)
        {
            // curl 8.7.x returns incorrect HTTP/2 error codes.
            self.set_long(easy, CURLOPT_HTTP_VERSION, CURL_HTTP_VERSION_1_1);
        }
        easy
    }
}

fn c_string(value: &str) -> CString {
    // Arguments cannot hold NUL bytes; truncate like c_str() would be read.
    let bytes = value.as_bytes();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// `cmCurlParseTLSVersion`.
fn parse_tls_version(version: &str) -> Option<c_long> {
    match version {
        "1.0" => Some(4),
        "1.1" => Some(5),
        "1.2" => Some(6),
        "1.3" => Some(7),
        _ => None,
    }
}

/// cmCurl.cxx's `check_curl_result`: accumulate errors, ignoring
/// CURLE_NOT_BUILT_IN.
fn accumulate(curl: &Curl, e: &mut String, result: c_int, prefix: &str) {
    if result != CURLE_OK && result != CURLE_NOT_BUILT_IN {
        if !e.is_empty() {
            e.push('\n');
        }
        e.push_str(prefix);
        e.push_str(&curl.strerror(result));
    }
}

/// `cmCurlSetCAInfo` as built with the vendored curl (CMAKE_FIND_CAFILE).
fn set_ca_info(curl: &Curl, easy: Easy, cafile: &str) -> String {
    let mut e = String::new();
    let set = |e: &mut String, option: c_int, value: &str, prefix: &str| {
        let result = curl.set_str(easy, option, value);
        accumulate(curl, e, result, prefix);
    };
    let env = |name: &str| std::env::var(name).ok();
    let file_exists = |path: &str| Path::new(path).is_file();
    let is_dir = |path: &str| Path::new(path).is_dir();
    if !cafile.is_empty() {
        set(
            &mut e,
            CURLOPT_CAINFO,
            cafile,
            "Unable to set TLS/SSL Verify CAINFO: ",
        );
    } else if let Some(ca) = env("SSL_CERT_FILE").filter(|ca| file_exists(ca)) {
        set(
            &mut e,
            CURLOPT_CAINFO,
            &ca,
            "Unable to set TLS/SSL Verify CAINFO: ",
        );
    } else if let Some(ca) = env("SSL_CERT_DIR").filter(|ca| is_dir(ca)) {
        set(
            &mut e,
            CURLOPT_CAPATH,
            &ca,
            "Unable to set TLS/SSL Verify CAINFO: ",
        );
    } else if file_exists("/etc/pki/tls/certs/ca-bundle.crt") {
        set(
            &mut e,
            CURLOPT_CAINFO,
            "/etc/pki/tls/certs/ca-bundle.crt",
            "Unable to set TLS/SSL Verify CAINFO: ",
        );
    } else {
        if file_exists("/etc/ssl/certs/ca-certificates.crt") {
            set(
                &mut e,
                CURLOPT_CAINFO,
                "/etc/ssl/certs/ca-certificates.crt",
                "Unable to set TLS/SSL Verify CAINFO: ",
            );
        }
        if is_dir("/etc/ssl/certs") {
            set(
                &mut e,
                CURLOPT_CAPATH,
                "/etc/ssl/certs",
                "Unable to set TLS/SSL Verify CAPATH: ",
            );
        }
    }
    e
}

/// `cmCurlSetNETRCOption`.
fn set_netrc_option(curl: &Curl, easy: Easy, level: &str, file: &str) -> String {
    let mut e = String::new();
    let mut netrc = CURL_NETRC_LAST;
    if !level.is_empty() {
        netrc = match level {
            "OPTIONAL" => CURL_NETRC_OPTIONAL,
            "REQUIRED" => CURL_NETRC_REQUIRED,
            "IGNORED" => CURL_NETRC_IGNORED,
            _ => return format!("NETRC accepts OPTIONAL, IGNORED or REQUIRED but got: {level}"),
        };
    }
    if netrc != CURL_NETRC_LAST && netrc != CURL_NETRC_IGNORED {
        let result = curl.set_long(easy, CURLOPT_NETRC, netrc);
        accumulate(curl, &mut e, result, "Unable to set netrc level: ");
        if !e.is_empty() {
            return e;
        }
        if !file.is_empty() {
            let result = curl.set_str(easy, CURLOPT_NETRC_FILE, file);
            accumulate(curl, &mut e, result, "Unable to set .netrc file path : ");
        }
    }
    e
}

/// `cmCurlFixFileURL`.
fn fix_file_url(url: String) -> String {
    if url.starts_with("file://") {
        url.replace(' ', "%20")
    } else {
        url
    }
}

/// `atol`: leading whitespace, optional sign, decimal digits.
fn atol(text: &str) -> c_long {
    let text = text.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let mut value: c_long = 0;
    for byte in digits.bytes().take_while(u8::is_ascii_digit) {
        value = value
            .wrapping_mul(10)
            .wrapping_add(c_long::from(byte - b'0'));
    }
    if negative {
        value.wrapping_neg()
    } else {
        value
    }
}

/// `cmCryptoHash::New` names.
fn known_hash(algo: &str) -> bool {
    matches!(
        algo,
        "MD5"
            | "SHA1"
            | "SHA224"
            | "SHA256"
            | "SHA384"
            | "SHA512"
            | "SHA3_224"
            | "SHA3_256"
            | "SHA3_384"
            | "SHA3_512"
    )
}

/// `cmCryptoHash::HashFile`: empty when the file cannot be read.
fn hash_file(algo: &str, path: &str) -> String {
    let mut data = Vec::new();
    match File::open(path).and_then(|mut file| file.read_to_end(&mut data)) {
        Ok(_) => {}
        Err(_) => return String::new(),
    }
    let digest: Vec<u8> = match algo {
        "MD5" => <md5::Md5 as md5::Digest>::digest(&data).to_vec(),
        "SHA1" => <sha1::Sha1 as sha1::Digest>::digest(&data).to_vec(),
        "SHA224" => <sha2::Sha224 as sha2::Digest>::digest(&data).to_vec(),
        "SHA256" => <sha2::Sha256 as sha2::Digest>::digest(&data).to_vec(),
        "SHA384" => <sha2::Sha384 as sha2::Digest>::digest(&data).to_vec(),
        "SHA512" => <sha2::Sha512 as sha2::Digest>::digest(&data).to_vec(),
        "SHA3_224" => <sha3::Sha3_224 as sha3::Digest>::digest(&data).to_vec(),
        "SHA3_256" => <sha3::Sha3_256 as sha3::Digest>::digest(&data).to_vec(),
        "SHA3_384" => <sha3::Sha3_384 as sha3::Digest>::digest(&data).to_vec(),
        _ => <sha3::Sha3_512 as sha3::Digest>::digest(&data).to_vec(),
    };
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The output stream of a download (`cmsys::ofstream`): writes go through
/// a buffer and any failure sticks, like the stream's failbit.
struct Sink {
    out: Option<BufWriter<File>>,
    failed: bool,
}

/// `cmWriteToFileCallback`.
extern "C" fn write_to_file(
    ptr: *mut c_char,
    size: usize,
    nmemb: usize,
    data: *mut c_void,
) -> usize {
    let realsize = size * nmemb;
    if data.is_null() {
        return realsize;
    }
    // SAFETY: data is the Sink registered as CURLOPT_WRITEDATA, alive
    // for the duration of curl_easy_perform.
    let sink = unsafe { &mut *data.cast::<Sink>() };
    if let Some(out) = sink.out.as_mut() {
        // SAFETY: libcurl passes realsize readable bytes.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), realsize) };
        if sink.failed || out.write_all(bytes).is_err() {
            sink.failed = true;
            return CURL_WRITEFUNC_ERROR;
        }
    }
    realsize
}

/// `cmWriteToMemoryCallback`.
extern "C" fn write_to_memory(
    ptr: *mut c_char,
    size: usize,
    nmemb: usize,
    data: *mut c_void,
) -> usize {
    let realsize = size * nmemb;
    // SAFETY: data is the Vec registered as CURLOPT_WRITEDATA.
    let buffer = unsafe { &mut *data.cast::<Vec<u8>>() };
    // SAFETY: libcurl passes realsize readable bytes.
    buffer.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), realsize) });
    realsize
}

/// `cmFileCommandCurlDebugCallback`.
extern "C" fn debug_callback(
    _easy: Easy,
    kind: c_int,
    ptr: *mut c_char,
    size: usize,
    data: *mut c_void,
) -> c_int {
    // SAFETY: data is the Vec registered as CURLOPT_DEBUGDATA.
    let buffer = unsafe { &mut *data.cast::<Vec<u8>>() };
    match kind {
        CURLINFO_TEXT | CURLINFO_HEADER_IN | CURLINFO_HEADER_OUT => {
            // SAFETY: libcurl passes size readable bytes.
            buffer.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) });
        }
        CURLINFO_DATA_IN | CURLINFO_DATA_OUT | CURLINFO_SSL_DATA_IN | CURLINFO_SSL_DATA_OUT => {
            buffer.extend_from_slice(format!("[{size} bytes data]\n").as_bytes());
        }
        _ => {}
    }
    0
}

/// `cURLProgressHelper`.
struct Progress {
    interp: *mut Interp,
    text: &'static str,
    current: i64,
}

impl Progress {
    fn update(&mut self, value: i64, total: i64) {
        let old = self.current;
        if total > 0 {
            self.current = ((value as f64) / (total as f64) * 100.0).round() as i64;
            if self.current > 100 {
                // Avoid extra progress reports for unexpected data beyond total.
                self.current = 100;
            }
        }
        if old != self.current {
            let status = format!("[{} {}% complete]", self.text, self.current);
            // SAFETY: the interpreter outlives curl_easy_perform and is not
            // otherwise borrowed while the transfer runs.
            let interp = unsafe { &mut *self.interp };
            // cmMakefile::DisplayStatus
            crate::commands::message::status_line(interp, &status);
        }
    }
}

/// `cmFileDownloadProgressCallback`.
extern "C" fn download_progress(
    client: *mut c_void,
    dltotal: i64,
    dlnow: i64,
    _ultotal: i64,
    _ulnow: i64,
) -> c_int {
    // SAFETY: client is the Progress registered as CURLOPT_PROGRESSDATA.
    unsafe { &mut *client.cast::<Progress>() }.update(dlnow, dltotal);
    0
}

/// `cmFileUploadProgressCallback`.
extern "C" fn upload_progress(
    client: *mut c_void,
    _dltotal: i64,
    _dlnow: i64,
    ultotal: i64,
    ulnow: i64,
) -> c_int {
    // SAFETY: client is the Progress registered as CURLOPT_PROGRESSDATA.
    unsafe { &mut *client.cast::<Progress>() }.update(ulnow, ultotal);
    0
}

type WriteFn = extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize;
type DebugFn = extern "C" fn(Easy, c_int, *mut c_char, usize, *mut c_void) -> c_int;
type XferFn = extern "C" fn(*mut c_void, i64, i64, i64, i64) -> c_int;

/// `cURLEasyGuard`.
struct EasyGuard<'a> {
    curl: &'a Curl,
    easy: Easy,
}

impl Drop for EasyGuard<'_> {
    fn drop(&mut self) {
        if !self.easy.is_null() {
            // SAFETY: the handle came from curl_easy_init.
            unsafe { (self.curl.easy_cleanup)(self.easy) };
        }
    }
}

/// The C string of a byte buffer (`vector<char>::data()` after
/// `push_back(0)`): everything before the first NUL.
fn c_str_of(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn load(action: &str) -> Result<&'static Curl, crate::interp::CmdErr> {
    curl().or_else(|reason| {
        err(format!(
            "{action} requires libcurl, which could not be loaded (libcurl.so.4): {reason}"
        ))
    })
}

/// The TLS verify and version settings shared by DOWNLOAD and UPLOAD:
/// argument, then CMAKE_TLS_* variable, then environment, then default.
fn tls_defaults(
    interp: &Interp,
    verify: Option<bool>,
    version: Option<String>,
) -> (bool, bool, String, bool) {
    let mut verify = verify;
    if verify.is_none()
        && let Some(value) = interp.get_def("CMAKE_TLS_VERIFY")
    {
        verify = Some(is_on(value));
    }
    if verify.is_none()
        && let Ok(value) = std::env::var("CMAKE_TLS_VERIFY")
    {
        verify = Some(is_on(&value));
    }
    let verify_defaulted = verify.is_none();
    let verify = verify.unwrap_or(TLS_VERIFY_DEFAULT);
    let mut version = version;
    if version.is_none()
        && let Some(value) = interp.get_def("CMAKE_TLS_VERSION")
    {
        version = Some(value.to_owned());
    }
    if version.is_none()
        && let Ok(value) = std::env::var("CMAKE_TLS_VERSION")
    {
        version = Some(value);
    }
    let version_defaulted = version.is_none();
    let version = version.unwrap_or_else(|| TLS_VERSION_DEFAULT.to_owned());
    (verify, verify_defaulted, version, version_defaulted)
}

fn status_text(curl: &Curl, result: c_int, verify_defaulted: bool) -> String {
    let mut message = curl.strerror(result);
    if (result == CURLE_SSL_CONNECT_ERROR || result == CURLE_PEER_FAILED_VERIFICATION)
        && verify_defaulted
    {
        message.push_str(
            ".  If this is due to https certificate verification failure, one may set environment variable CMAKE_TLS_VERIFY=0 to suppress it.",
        );
    }
    format!("{result};\"{message}\"")
}

/// `HandleDownloadCommand`.
pub fn download(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("DOWNLOAD must be called with at least two arguments.");
    }
    let mut url = args[1].clone();
    let mut file = String::new();
    let mut timeout: c_long = 0;
    let mut inactivity_timeout: c_long = 0;
    let mut log_var = String::new();
    let mut status_var = String::new();
    let mut tls_version: Option<String> = None;
    let mut tls_verify: Option<bool> = None;
    let mut cainfo = interp
        .get_def("CMAKE_TLS_CAINFO")
        .unwrap_or_default()
        .to_owned();
    let mut netrc_level = interp.get_safe("CMAKE_NETRC");
    let mut netrc_file = interp.get_safe("CMAKE_NETRC_FILE");
    let mut expected_hash = String::new();
    let mut hash_match_msg = String::new();
    let mut hash: Option<String> = None;
    let mut show_progress = false;
    let mut userpwd = String::new();
    let mut headers: Vec<String> = Vec::new();
    let mut ranges: Vec<(String, Option<String>)> = Vec::new();

    let mut i = 2;
    while i < args.len() {
        let arg = args[i].as_str();
        let next = args.get(i + 1);
        match arg {
            "TIMEOUT" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing time for TIMEOUT.");
                };
                timeout = atol(value);
            }
            "INACTIVITY_TIMEOUT" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing time for INACTIVITY_TIMEOUT.");
                };
                inactivity_timeout = atol(value);
            }
            "LOG" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing VAR for LOG.");
                };
                log_var = value.clone();
            }
            "STATUS" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing VAR for STATUS.");
                };
                status_var = value.clone();
            }
            "TLS_VERSION" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing value for TLS_VERSION.");
                };
                tls_version = Some(value.clone());
            }
            "TLS_VERIFY" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing bool value for TLS_VERIFY.");
                };
                tls_verify = Some(is_on(value));
            }
            "TLS_CAINFO" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing file value for TLS_CAINFO.");
                };
                cainfo = value.clone();
            }
            "NETRC_FILE" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing file value for NETRC_FILE.");
                };
                netrc_file = value.clone();
            }
            "NETRC" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing level value for NETRC.");
                };
                netrc_level = value.clone();
            }
            "EXPECTED_MD5" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing sum value for EXPECTED_MD5.");
                };
                hash = Some("MD5".to_owned());
                hash_match_msg = "MD5 sum".to_owned();
                expected_hash = value.to_ascii_lowercase();
            }
            "SHOW_PROGRESS" => show_progress = true,
            "EXPECTED_HASH" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing ALGO=value for EXPECTED_HASH.");
                };
                let Some(pos) = value.find('=') else {
                    return err(format!(
                        "DOWNLOAD EXPECTED_HASH expects ALGO=value but got: {value}"
                    ));
                };
                let algo = &value[..pos];
                expected_hash = value[pos + 1..].to_ascii_lowercase();
                if !known_hash(algo) {
                    return err(format!("DOWNLOAD EXPECTED_HASH given unknown ALGO: {algo}"));
                }
                hash = Some(algo.to_owned());
                hash_match_msg = format!("{algo} hash");
            }
            "USERPWD" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing string for USERPWD.");
                };
                userpwd = value.clone();
            }
            "HTTPHEADER" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing string for HTTPHEADER.");
                };
                headers.push(value.clone());
            }
            "RANGE_START" => {
                i += 1;
                let Some(value) = next else {
                    return err("DOWNLOAD missing value for RANGE_START.");
                };
                ranges.push((value.clone(), None));
            }
            "RANGE_END" => {
                i += 1;
                // The C++ dereferences the next argument unchecked; a
                // trailing RANGE_END reads as empty here.
                let value = next.cloned().unwrap_or_default();
                match ranges.last_mut() {
                    None => ranges.push(("0".to_owned(), Some(value))),
                    Some(last) if last.1.is_none() => last.1 = Some(value),
                    Some(_) => {
                        return err(
                            "Multiple RANGE_END values is provided without the corresponding RANGE_START.",
                        );
                    }
                }
            }
            _ if file.is_empty() => file = arg.to_owned(),
            _ => {
                // Do not return error for compatibility reason.
                interp.issue(
                    MessageType::AuthorWarning,
                    &format!("Unexpected argument: {arg}"),
                );
            }
        }
        i += 1;
    }

    let (verify, verify_defaulted, version, version_defaulted) =
        tls_defaults(interp, tls_verify, tls_version);

    // Can't calculate hash if we don't save the file.
    if file.is_empty() && hash.is_some() {
        return err("DOWNLOAD cannot calculate hash if file is not saved.");
    }
    // If the file exists already with the expected hash, simply return.
    if let Some(algo) = &hash
        && Path::new(&file).exists()
        && hash_file(algo, &file) == expected_hash
    {
        if !status_var.is_empty() {
            let message = format!(
                "skipping download as file already exists with expected {hash_match_msg}\""
            );
            interp.set_def(status_var, format!("0;\"{message}"));
        }
        return Ok(());
    }
    // Make sure the parent directory exists.
    if !file.is_empty() {
        let dir = crate::util::filename_path(&file);
        if !dir.is_empty() && !Path::new(&dir).exists() && std::fs::create_dir_all(&dir).is_err() {
            return err(format!(
                "DOWNLOAD error: cannot create directory '{dir}' - Specify file by full path name and verify that you have directory creation and file write privileges."
            ));
        }
    }
    let mut sink = Sink {
        out: None,
        failed: false,
    };
    if !file.is_empty() {
        match File::create(&file) {
            Ok(out) => sink.out = Some(BufWriter::new(out)),
            Err(_) => {
                return err(format!(
                    "DOWNLOAD cannot open file for write\n  file: \"{file}\""
                ));
            }
        }
    }

    url = fix_file_url(url);

    let curl = load("DOWNLOAD")?;
    // SAFETY: plain libcurl call.
    unsafe { (curl.global_init)(CURL_GLOBAL_DEFAULT) };
    let easy = curl.easy_init();
    if easy.is_null() {
        return err("DOWNLOAD error initializing curl.");
    }
    let mut guard = EasyGuard { curl, easy };
    macro_rules! check {
        ($result:expr, $prefix:expr) => {{
            let result = $result;
            if result != CURLE_OK {
                return err(format!("{}{}", $prefix, curl.strerror(result)));
            }
        }};
    }
    check!(
        curl.set_str(easy, CURLOPT_URL, &url),
        "DOWNLOAD cannot set url: "
    );
    check!(
        curl.set_long(easy, CURLOPT_FAILONERROR, 1),
        "DOWNLOAD cannot set http failure option: "
    );
    let agent = format!(
        "curl/{}",
        curl.version()
            .map(|(version, _)| version)
            .unwrap_or_default()
    );
    check!(
        curl.set_str(easy, CURLOPT_USERAGENT, &agent),
        "DOWNLOAD cannot set user agent option: "
    );
    check!(
        curl.set_ptr(
            easy,
            CURLOPT_WRITEFUNCTION,
            write_to_file as WriteFn as *const c_void
        ),
        "DOWNLOAD cannot set write function: "
    );
    check!(
        curl.set_ptr(
            easy,
            CURLOPT_DEBUGFUNCTION,
            debug_callback as DebugFn as *const c_void
        ),
        "DOWNLOAD cannot set debug function: "
    );
    match parse_tls_version(&version) {
        Some(value) => {
            let mut result = curl.set_long(easy, CURLOPT_SSLVERSION, value);
            if version_defaulted && result == CURLE_NOT_BUILT_IN {
                result = CURLE_OK;
            }
            check!(
                result,
                format!("DOWNLOAD cannot set TLS/SSL version {version}: ")
            );
        }
        None => return err(format!("DOWNLOAD given unknown TLS/SSL version {version}")),
    }
    if verify {
        check!(
            curl.set_long(easy, CURLOPT_SSL_VERIFYPEER, 1),
            "DOWNLOAD cannot set TLS/SSL Verify on: "
        );
    } else {
        check!(
            curl.set_long(easy, CURLOPT_SSL_VERIFYPEER, 0),
            "DOWNLOAD cannot set TLS/SSL Verify off: "
        );
    }
    for (start, end) in &ranges {
        let range = format!("{start}-{}", end.as_deref().unwrap_or(""));
        check!(
            curl.set_str(easy, CURLOPT_RANGE, &range),
            "DOWNLOAD cannot set range: "
        );
    }
    let cainfo_err = set_ca_info(curl, easy, &cainfo);
    if !cainfo_err.is_empty() {
        return err(cainfo_err);
    }
    let netrc_err = set_netrc_option(curl, easy, &netrc_level.to_ascii_uppercase(), &netrc_file);
    if !netrc_err.is_empty() {
        return err(netrc_err);
    }
    let mut debug: Vec<u8> = Vec::new();
    let write_data: *const c_void = if file.is_empty() {
        std::ptr::null()
    } else {
        (&raw mut sink).cast_const().cast()
    };
    check!(
        curl.set_ptr(easy, CURLOPT_WRITEDATA, write_data),
        "DOWNLOAD cannot set write data: "
    );
    check!(
        curl.set_ptr(
            easy,
            CURLOPT_DEBUGDATA,
            (&raw mut debug).cast_const().cast()
        ),
        "DOWNLOAD cannot set debug data: "
    );
    check!(
        curl.set_long(easy, CURLOPT_FOLLOWLOCATION, 1),
        "DOWNLOAD cannot set follow-redirect option: "
    );
    if !log_var.is_empty() {
        check!(
            curl.set_long(easy, CURLOPT_VERBOSE, 1),
            "DOWNLOAD cannot set verbose: "
        );
    }
    if timeout > 0 {
        check!(
            curl.set_long(easy, CURLOPT_TIMEOUT, timeout),
            "DOWNLOAD cannot set timeout: "
        );
    }
    if inactivity_timeout > 0 {
        // Give up if there is no progress for a long time.
        curl.set_long(easy, CURLOPT_LOW_SPEED_LIMIT, 1);
        curl.set_long(easy, CURLOPT_LOW_SPEED_TIME, inactivity_timeout);
    }
    let mut progress = Progress {
        interp: std::ptr::from_mut(interp),
        text: "download",
        current: -1,
    };
    if show_progress {
        check!(
            curl.set_long(easy, CURLOPT_NOPROGRESS, 0),
            "DOWNLOAD cannot set noprogress value: "
        );
        check!(
            curl.set_ptr(
                easy,
                CURLOPT_XFERINFOFUNCTION,
                download_progress as XferFn as *const c_void
            ),
            "DOWNLOAD cannot set progress function: "
        );
        check!(
            curl.set_ptr(
                easy,
                CURLOPT_PROGRESSDATA,
                (&raw mut progress).cast_const().cast()
            ),
            "DOWNLOAD cannot set progress data: "
        );
    }
    if !userpwd.is_empty() {
        check!(
            curl.set_str(easy, CURLOPT_USERPWD, &userpwd),
            "DOWNLOAD cannot set user password: "
        );
    }
    let header_list = append_headers(curl, &headers);
    curl.set_ptr(easy, CURLOPT_HTTPHEADER, header_list.cast_const());

    // SAFETY: every pointer registered above outlives the transfer.
    let mut result = unsafe { (curl.easy_perform)(easy) };

    // SAFETY: the list came from curl_slist_append (or is null).
    unsafe { (curl.slist_free_all)(header_list) };
    // always cleanup
    guard.easy = std::ptr::null_mut();
    // SAFETY: the handle came from curl_easy_init.
    unsafe { (curl.easy_cleanup)(easy) };

    // Explicitly close the file so we can check for write errors.
    if let Some(mut out) = sink.out.take() {
        if out.flush().is_err() {
            sink.failed = true;
        }
        if sink.failed {
            result = CURLE_WRITE_ERROR;
        }
    }

    if !status_var.is_empty() {
        interp.set_def(
            status_var.clone(),
            status_text(curl, result, verify_defaulted),
        );
    }
    // SAFETY: plain libcurl call.
    unsafe { (curl.global_cleanup)() };

    // Ensure requested curl logs are returned (especially in case of failure).
    if !log_var.is_empty() {
        interp.set_def(log_var, c_str_of(&debug));
    }

    // Verify the hash if requested.
    if let Some(algo) = &hash {
        if result != CURLE_OK {
            return err(format!(
                "DOWNLOAD cannot compute hash on failed download\n  from url: \"{url}\"\n  status: [{result};\"{}\"]",
                curl.strerror(result)
            ));
        }
        let actual = hash_file(algo, &file);
        if actual.is_empty() {
            return err(format!(
                "DOWNLOAD cannot compute hash on download\n  for file: \"{file}\""
            ));
        }
        if expected_hash != actual {
            if !status_var.is_empty() && result == 0 {
                interp.set_def(
                    status_var,
                    format!("1;HASH mismatch: expected: {expected_hash} actual: {actual}"),
                );
            }
            return err(format!(
                "DOWNLOAD HASH mismatch\n  for file: \"{file}\"\n    expected hash: \"{expected_hash}\"\n      actual hash: \"{actual}\"\n"
            ));
        }
    }
    Ok(())
}

fn append_headers(curl: &Curl, headers: &[String]) -> *mut c_void {
    let mut list = std::ptr::null_mut();
    for header in headers {
        let header = c_string(header);
        // SAFETY: curl_slist_append copies the string.
        list = unsafe { (curl.slist_append)(list, header.as_ptr()) };
    }
    list
}

/// `HandleUploadCommand`.
pub fn upload(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err("UPLOAD must be called with at least three arguments.");
    }
    let filename = args[1].clone();
    let mut url = args[2].clone();
    let mut timeout: c_long = 0;
    let mut inactivity_timeout: c_long = 0;
    let mut log_var = String::new();
    let mut status_var = String::new();
    let mut show_progress = false;
    let mut tls_version: Option<String> = None;
    let mut tls_verify: Option<bool> = None;
    let mut cainfo = interp
        .get_def("CMAKE_TLS_CAINFO")
        .unwrap_or_default()
        .to_owned();
    let mut userpwd = String::new();
    let mut netrc_level = interp.get_safe("CMAKE_NETRC");
    let mut netrc_file = interp.get_safe("CMAKE_NETRC_FILE");
    let mut headers: Vec<String> = Vec::new();

    let mut i = 3;
    while i < args.len() {
        let arg = args[i].as_str();
        let next = args.get(i + 1);
        match arg {
            "TIMEOUT" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing time for TIMEOUT.");
                };
                timeout = atol(value);
            }
            "INACTIVITY_TIMEOUT" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing time for INACTIVITY_TIMEOUT.");
                };
                inactivity_timeout = atol(value);
            }
            "LOG" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing VAR for LOG.");
                };
                log_var = value.clone();
            }
            "STATUS" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing VAR for STATUS.");
                };
                status_var = value.clone();
            }
            "SHOW_PROGRESS" => show_progress = true,
            "TLS_VERSION" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing value for TLS_VERSION.");
                };
                tls_version = Some(value.clone());
            }
            "TLS_VERIFY" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing bool value for TLS_VERIFY.");
                };
                tls_verify = Some(is_on(value));
            }
            "TLS_CAINFO" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing file value for TLS_CAINFO.");
                };
                cainfo = value.clone();
            }
            "NETRC_FILE" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing file value for NETRC_FILE.");
                };
                netrc_file = value.clone();
            }
            "NETRC" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing level value for NETRC.");
                };
                netrc_level = value.clone();
            }
            "USERPWD" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing string for USERPWD.");
                };
                userpwd = value.clone();
            }
            "HTTPHEADER" => {
                i += 1;
                let Some(value) = next else {
                    return err("UPLOAD missing string for HTTPHEADER.");
                };
                headers.push(value.clone());
            }
            _ => {
                // Do not return error for compatibility reason.
                interp.issue(
                    MessageType::AuthorWarning,
                    &format!("Unexpected argument: {arg}"),
                );
            }
        }
        i += 1;
    }

    let (verify, verify_defaulted, version, version_defaulted) =
        tls_defaults(interp, tls_verify, tls_version);

    // Open file for reading.
    let path = c_string(&filename);
    // SAFETY: fopen with valid C strings.
    let fin = unsafe { libc::fopen(path.as_ptr(), c"rb".as_ptr()) };
    if fin.is_null() {
        return err(format!("UPLOAD cannot open file '{filename}' for reading."));
    }
    struct Fin(*mut libc::FILE);
    impl Drop for Fin {
        fn drop(&mut self) {
            // SAFETY: the stream came from fopen and is closed once.
            unsafe { libc::fclose(self.0) };
        }
    }
    let fin = Fin(fin);
    let file_size = std::fs::metadata(&filename).map_or(0, |meta| meta.len());

    url = fix_file_url(url);

    let curl = load("UPLOAD")?;
    // SAFETY: plain libcurl call.
    unsafe { (curl.global_init)(CURL_GLOBAL_DEFAULT) };
    let easy = curl.easy_init();
    if easy.is_null() {
        return err("UPLOAD error initializing curl.");
    }
    let mut guard = EasyGuard { curl, easy };
    macro_rules! check {
        ($result:expr, $prefix:expr) => {{
            let result = $result;
            if result != CURLE_OK {
                return err(format!("{}{}", $prefix, curl.strerror(result)));
            }
        }};
    }
    check!(
        curl.set_long(easy, CURLOPT_FAILONERROR, 1),
        "UPLOAD cannot set fail on error flag: "
    );
    check!(
        curl.set_long(easy, CURLOPT_UPLOAD, 1),
        "UPLOAD cannot set upload flag: "
    );
    check!(
        curl.set_str(easy, CURLOPT_URL, &url),
        "UPLOAD cannot set url: "
    );
    check!(
        curl.set_ptr(
            easy,
            CURLOPT_WRITEFUNCTION,
            write_to_memory as WriteFn as *const c_void
        ),
        "UPLOAD cannot set write function: "
    );
    check!(
        curl.set_ptr(
            easy,
            CURLOPT_DEBUGFUNCTION,
            debug_callback as DebugFn as *const c_void
        ),
        "UPLOAD cannot set debug function: "
    );
    match parse_tls_version(&version) {
        Some(value) => {
            let mut result = curl.set_long(easy, CURLOPT_SSLVERSION, value);
            if version_defaulted && result == CURLE_NOT_BUILT_IN {
                result = CURLE_OK;
            }
            check!(
                result,
                format!("UPLOAD cannot set TLS/SSL version {version}: ")
            );
        }
        None => return err(format!("UPLOAD given unknown TLS/SSL version {version}")),
    }
    if verify {
        check!(
            curl.set_long(easy, CURLOPT_SSL_VERIFYPEER, 1),
            "UPLOAD cannot set TLS/SSL Verify on: "
        );
    } else {
        check!(
            curl.set_long(easy, CURLOPT_SSL_VERIFYPEER, 0),
            "UPLOAD cannot set TLS/SSL Verify off: "
        );
    }
    let cainfo_err = set_ca_info(curl, easy, &cainfo);
    if !cainfo_err.is_empty() {
        return err(cainfo_err);
    }
    let mut response: Vec<u8> = Vec::new();
    let mut debug: Vec<u8> = Vec::new();
    check!(
        curl.set_ptr(
            easy,
            CURLOPT_WRITEDATA,
            (&raw mut response).cast_const().cast()
        ),
        "UPLOAD cannot set write data: "
    );
    check!(
        curl.set_ptr(
            easy,
            CURLOPT_DEBUGDATA,
            (&raw mut debug).cast_const().cast()
        ),
        "UPLOAD cannot set debug data: "
    );
    check!(
        curl.set_long(easy, CURLOPT_FOLLOWLOCATION, 1),
        "UPLOAD cannot set follow-redirect option: "
    );
    if !log_var.is_empty() {
        check!(
            curl.set_long(easy, CURLOPT_VERBOSE, 1),
            "UPLOAD cannot set verbose: "
        );
    }
    if timeout > 0 {
        check!(
            curl.set_long(easy, CURLOPT_TIMEOUT, timeout),
            "UPLOAD cannot set timeout: "
        );
    }
    if inactivity_timeout > 0 {
        // Give up if there is no progress for a long time.
        curl.set_long(easy, CURLOPT_LOW_SPEED_LIMIT, 1);
        curl.set_long(easy, CURLOPT_LOW_SPEED_TIME, inactivity_timeout);
    }
    let mut progress = Progress {
        interp: std::ptr::from_mut(interp),
        text: "upload",
        current: -1,
    };
    if show_progress {
        check!(
            curl.set_long(easy, CURLOPT_NOPROGRESS, 0),
            "UPLOAD cannot set noprogress value: "
        );
        check!(
            curl.set_ptr(
                easy,
                CURLOPT_XFERINFOFUNCTION,
                upload_progress as XferFn as *const c_void
            ),
            "UPLOAD cannot set progress function: "
        );
        check!(
            curl.set_ptr(
                easy,
                CURLOPT_PROGRESSDATA,
                (&raw mut progress).cast_const().cast()
            ),
            "UPLOAD cannot set progress data: "
        );
    }
    // Now specify which file to upload; libcurl's default read callback
    // freads the FILE*.
    check!(
        curl.set_ptr(easy, CURLOPT_INFILE, fin.0.cast_const().cast()),
        "UPLOAD cannot set input file: "
    );
    // The C++ casts the unsigned long size to long.
    check!(
        curl.set_long(easy, CURLOPT_INFILESIZE, file_size as c_long),
        "UPLOAD cannot set input file size: "
    );
    if !userpwd.is_empty() {
        check!(
            curl.set_str(easy, CURLOPT_USERPWD, &userpwd),
            "UPLOAD cannot set user password: "
        );
    }
    let netrc_err = set_netrc_option(curl, easy, &netrc_level.to_ascii_uppercase(), &netrc_file);
    if !netrc_err.is_empty() {
        return err(netrc_err);
    }
    let header_list = append_headers(curl, &headers);
    curl.set_ptr(easy, CURLOPT_HTTPHEADER, header_list.cast_const());

    // SAFETY: every pointer registered above outlives the transfer.
    let result = unsafe { (curl.easy_perform)(easy) };

    // SAFETY: the list came from curl_slist_append (or is null).
    unsafe { (curl.slist_free_all)(header_list) };
    // always cleanup
    guard.easy = std::ptr::null_mut();
    // SAFETY: the handle came from curl_easy_init.
    unsafe { (curl.easy_cleanup)(easy) };

    if !status_var.is_empty() {
        interp.set_def(status_var, status_text(curl, result, verify_defaulted));
    }
    // SAFETY: plain libcurl call.
    unsafe { (curl.global_cleanup)() };
    drop(fin);

    if !log_var.is_empty() {
        let mut log = String::new();
        if !response.is_empty() {
            log.push_str("Response:\n");
            log.push_str(&c_str_of(&response));
            log.push('\n');
        }
        if !debug.is_empty() {
            log.push_str("Debug:\n");
            log.push_str(&c_str_of(&debug));
            log.push('\n');
        }
        interp.set_def(log_var, log);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atol_matches_libc() {
        assert_eq!(atol("30"), 30);
        assert_eq!(atol("  -7x"), -7);
        assert_eq!(atol("abc"), 0);
        assert_eq!(atol("+12"), 12);
    }

    #[test]
    fn file_urls_encode_spaces() {
        assert_eq!(fix_file_url("file:///a b".into()), "file:///a%20b");
        assert_eq!(fix_file_url("http://a b".into()), "http://a b");
    }
}
