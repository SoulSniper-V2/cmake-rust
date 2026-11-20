//! Support code for `string()`: TIMESTAMP (`cmTimestamp`), RANDOM
//! (`HandleRandomCommand` + glibc `rand()`), UUID (`cmUuid`), GENEX_STRIP
//! (`cmGeneratorExpression::Preprocess`), and HASH (`cmCryptoHash`).

use std::cell::RefCell;
use std::ffi::CString;
use std::io::Write;

use crate::interp::{CmdResult, Interp, err};

use super::atoi;

// ---------------------------------------------------------------------
// HASH

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(*byte >> 4), 16).unwrap());
        out.push(char::from_digit(u32::from(*byte & 0xf), 16).unwrap());
    }
    out
}

/// `cmCryptoHash::HashString` (lowercase hex digest).
pub fn hash_string(algo: &str, input: &str) -> String {
    let bytes = input.as_bytes();
    let digest: Vec<u8> = match algo {
        "MD5" => <md5::Md5 as md5::Digest>::digest(bytes).to_vec(),
        "SHA1" => <sha1::Sha1 as sha1::Digest>::digest(bytes).to_vec(),
        "SHA224" => <sha2::Sha224 as sha2::Digest>::digest(bytes).to_vec(),
        "SHA256" => <sha2::Sha256 as sha2::Digest>::digest(bytes).to_vec(),
        "SHA384" => <sha2::Sha384 as sha2::Digest>::digest(bytes).to_vec(),
        "SHA512" => <sha2::Sha512 as sha2::Digest>::digest(bytes).to_vec(),
        "SHA3_224" => <sha3::Sha3_224 as sha3::Digest>::digest(bytes).to_vec(),
        "SHA3_256" => <sha3::Sha3_256 as sha3::Digest>::digest(bytes).to_vec(),
        "SHA3_384" => <sha3::Sha3_384 as sha3::Digest>::digest(bytes).to_vec(),
        "SHA3_512" => <sha3::Sha3_512 as sha3::Digest>::digest(bytes).to_vec(),
        _ => unreachable!("unknown hash algorithm"),
    };
    hex(&digest)
}

// ---------------------------------------------------------------------
// TIMESTAMP (`cmTimestamp::CurrentTime`/`AddTimestampComponent`)

/// `iss >> time_t` on SOURCE_DATE_EPOCH: skips leading space, parses an
/// optional sign and digits, and requires end-of-stream afterwards.
fn parse_epoch(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && super::is_space(bytes[i]) {
        i += 1;
    }
    let rest = &text[i..];
    let digits = rest
        .strip_prefix('+')
        .or_else(|| rest.strip_prefix('-'))
        .unwrap_or(rest);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<i64>().ok()
}

fn current_time() -> (i64, u32) {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    // SAFETY: `tv` is a valid out pointer.
    unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };
    (tv.tv_sec, tv.tv_usec as u32)
}

fn break_down(time: i64, utc: bool) -> Option<libc::tm> {
    let raw = time as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `tm` is a valid out pointer; the *_r functions write to it.
    let ptr = unsafe {
        if utc {
            libc::gmtime_r(&raw, &mut tm)
        } else {
            libc::localtime_r(&raw, &mut tm)
        }
    };
    if ptr.is_null() { None } else { Some(tm) }
}

/// `strftime` into a 16-byte buffer for one `%X` component.
fn strftime_component(tm: &libc::tm, flag: u8) -> String {
    let format = CString::new(format!("%{}", flag as char)).unwrap();
    let mut buffer = [0u8; 16];
    // SAFETY: `format` is NUL-terminated, `tm` is a valid struct tm, and the
    // buffer is 16 bytes.
    let size = unsafe {
        libc::strftime(
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
            format.as_ptr(),
            tm,
        )
    };
    String::from_utf8_lossy(&buffer[..size.min(buffer.len())]).into_owned()
}

/// `cmTimestamp::CreateTimestampFromTimeT`.
fn create_timestamp(time: i64, micros: u32, format: &str, utc: bool) -> String {
    let format = if format.is_empty() {
        if utc {
            "%Y-%m-%dT%H:%M:%SZ".to_owned()
        } else {
            "%Y-%m-%dT%H:%M:%S".to_owned()
        }
    } else {
        format.to_owned()
    };
    let Some(tm) = break_down(time, utc) else {
        return String::new();
    };
    let bytes = format.as_bytes();
    let mut result = String::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let c1 = bytes[i];
        let c2 = if i + 1 < bytes.len() { bytes[i + 1] } else { 0 };
        if c1 == b'%' && c2 != 0 {
            // `AddTimestampComponent`: only the listed flags reach strftime;
            // unknown flags pass through as literal "%X".
            match c2 {
                b'a' | b'A' | b'b' | b'B' | b'd' | b'H' | b'I' | b'j' | b'm' | b'M' | b'S'
                | b'U' | b'V' | b'w' | b'y' | b'Y' | b'%' | b'Z' | b'z' => {
                    result.push_str(&strftime_component(&tm, c2));
                }
                b's' => {
                    // Seconds since the UNIX epoch; timegm(1970-01-01) == 0.
                    result.push_str(&time.to_string());
                }
                b'f' => {
                    result.push_str(&format!("{:06}", micros % 1_000_000));
                }
                _ => {
                    result.push('%');
                    result.push(c2 as char);
                }
            }
            i += 1;
        } else {
            result.push(c1 as char);
        }
        i += 1;
    }
    result
}

pub fn handle_timestamp(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("sub-command TIMESTAMP requires at least one argument.");
    }
    if args.len() > 4 {
        return err("sub-command TIMESTAMP takes at most three arguments.");
    }

    let mut index = 1usize;
    let output_variable = &args[index];
    index += 1;

    let mut format = String::new();
    if args.len() > index && args[index] != "UTC" {
        format = args[index].clone();
        index += 1;
    }

    let mut utc = false;
    if args.len() > index {
        if args[index] == "UTC" {
            utc = true;
        } else {
            return err(format!(
                " TIMESTAMP sub-command does not recognize option {}.",
                args[index]
            ));
        }
    }

    // cmTimestamp::CurrentTime
    let (mut time, mut micros) = current_time();
    if let Ok(epoch) = std::env::var("SOURCE_DATE_EPOCH")
        && !epoch.is_empty()
    {
        match parse_epoch(&epoch) {
            Some(parsed) => {
                time = parsed;
                micros = 0;
            }
            None => {
                let _ = writeln!(
                    std::io::stderr(),
                    "CMake Error: Cannot parse SOURCE_DATE_EPOCH as integer"
                );
                std::process::exit(27);
            }
        }
    }
    // `currentTimeT == static_cast<time_t>(-1)`
    if time == -1 {
        interp.set_def(output_variable.clone(), String::new());
        return Ok(());
    }

    let result = create_timestamp(time, micros, &format, utc);
    interp.set_def(output_variable.clone(), result);
    Ok(())
}

// ---------------------------------------------------------------------
// RANDOM (`HandleRandomCommand` + glibc rand())

/// glibc `random()` TYPE_3 generator (what `rand()`/`srand()` use).
struct GlibcRand {
    state: [u32; 31],
    fptr: usize,
    rptr: usize,
}

impl GlibcRand {
    fn new(seed: u32) -> GlibcRand {
        let mut state = [0u32; 31];
        state[0] = if seed == 0 { 1 } else { seed };
        // glibc keeps `word` in a signed int32; a seed of e.g. 0xFFFFFFFF
        // starts the warmup sequence at -1.
        let mut word = i64::from(state[0] as i32);
        for slot in state.iter_mut().skip(1) {
            let hi = word / 127_773;
            let lo = word % 127_773;
            word = 16_807 * lo - 2_836 * hi;
            if word < 0 {
                word += 2_147_483_647;
            }
            *slot = word as u32;
        }
        let mut rand = GlibcRand {
            state,
            fptr: 3,
            rptr: 0,
        };
        // srandom() discards 10 * deg outputs.
        for _ in 0..310 {
            rand.next();
        }
        rand
    }

    fn next(&mut self) -> u32 {
        self.state[self.fptr] = self.state[self.fptr].wrapping_add(self.state[self.rptr]);
        let result = self.state[self.fptr] >> 1;
        self.fptr = if self.fptr + 1 >= 31 {
            0
        } else {
            self.fptr + 1
        };
        self.rptr = if self.rptr + 1 >= 31 {
            0
        } else {
            self.rptr + 1
        };
        result
    }
}

thread_local! {
    /// The `static bool seeded` + global `rand()` state.
    static RAND_STATE: RefCell<Option<GlibcRand>> = const { RefCell::new(None) };
}

fn random_seed() -> u32 {
    // cmSystemTools::RandomSeed: nondeterministic seed material.
    let mut value = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0);
    value ^= std::process::id();
    value
}

pub fn handle_random(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 || args.len() == 3 || args.len() == 5 {
        return err("sub-command RANDOM requires at least one argument.");
    }

    let mut force_seed = false;
    let mut seed: u32 = 0;
    let mut length: i32 = 5;
    const DEFAULT_ALPHABET: &[u8] = b"qwertyuiopasdfghjklzxcvbnm\
                                      QWERTYUIOPASDFGHJKLZXCVBNM\
                                      0123456789";
    let mut alphabet: Vec<u8> = Vec::new();

    if args.len() > 3 {
        let stop_at = args.len() - 2;
        let mut i = 1usize;
        while i < stop_at {
            match args[i].as_str() {
                "LENGTH" => {
                    i += 1;
                    length = atoi(&args[i]);
                }
                "ALPHABET" => {
                    i += 1;
                    alphabet = args[i].as_bytes().to_vec();
                }
                "RANDOM_SEED" => {
                    i += 1;
                    seed = atoi(&args[i]) as u32;
                    force_seed = true;
                }
                _ => {}
            }
            i += 1;
        }
    }
    if alphabet.is_empty() {
        alphabet = DEFAULT_ALPHABET.to_vec();
    }

    let sizeof_alphabet = alphabet.len() as f64;
    if sizeof_alphabet < 1.0 {
        return err("sub-command RANDOM invoked with bad alphabet.");
    }
    if length < 1 {
        return err("sub-command RANDOM invoked with bad length.");
    }
    let variable_name = args.last().unwrap();

    let mut result = String::new();
    RAND_STATE.with(|state| {
        let mut slot = state.borrow_mut();
        if slot.is_none() || force_seed {
            *slot = Some(GlibcRand::new(if force_seed {
                seed
            } else {
                random_seed()
            }));
        }
        let rand = slot.as_mut().unwrap();
        let mut bytes = Vec::with_capacity(length as usize);
        for _ in 0..length {
            let idx = (sizeof_alphabet * f64::from(rand.next()) / (2_147_483_647.0 + 1.0)) as usize;
            bytes.push(alphabet[idx]);
        }
        result = String::from_utf8_lossy(&bytes).into_owned();
    });

    interp.set_def(variable_name.clone(), result);
    Ok(())
}

// ---------------------------------------------------------------------
// UUID (`cmUuid`)

fn int_from_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 0xa),
        b'A'..=b'F' => Some(byte - b'A' + 0xa),
        _ => None,
    }
}

/// `cmUuid::StringToBinary`: "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx".
fn uuid_string_to_binary(input: &str) -> Option<Vec<u8>> {
    const GROUPS: [usize; 5] = [4, 2, 2, 2, 6];
    let bytes = input.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    let mut output = Vec::with_capacity(16);
    let mut index = 0usize;
    for (group_index, group) in GROUPS.iter().enumerate() {
        if group_index != 0 {
            if bytes[index] != b'-' {
                return None;
            }
            index += 1;
        }
        let digits = group * 2;
        for i in (index..index + digits).step_by(2) {
            let hi = int_from_hex_digit(bytes[i])?;
            let lo = int_from_hex_digit(bytes[i + 1])?;
            output.push(hi << 4 | lo);
        }
        index += digits;
    }
    Some(output)
}

/// `cmUuid::BinaryToString`.
fn uuid_binary_to_string(input: &[u8]) -> String {
    const GROUPS: [usize; 5] = [4, 2, 2, 2, 6];
    let mut output = String::new();
    let mut index = 0usize;
    for (group_index, group) in GROUPS.iter().enumerate() {
        if group_index != 0 {
            output.push('-');
        }
        for _ in 0..*group {
            let byte = input[index];
            index += 1;
            output.push(char::from_digit(u32::from(byte >> 4), 16).unwrap());
            output.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap());
        }
    }
    output
}

/// `cmUuid::FromDigest`.
fn uuid_from_digest(digest: &[u8], version: u8) -> String {
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&digest[..16]);
    uuid[6] &= 0x0f;
    uuid[6] |= version << 4;
    uuid[8] &= 0x3f;
    uuid[8] |= 0x80;
    uuid_binary_to_string(&uuid)
}

pub fn handle_uuid(interp: &mut Interp, args: &[String]) -> CmdResult {
    let mut index = 1usize;

    if args.len() < 2 {
        return err("UUID sub-command requires an output variable.");
    }

    let output_variable = &args[index];
    index += 1;

    let mut namespace_string = String::new();
    let mut name = String::new();
    let mut uuid_type = String::new();
    let mut upper_case = false;

    while args.len() > index {
        match args[index].as_str() {
            "NAMESPACE" => {
                index += 1;
                if index >= args.len() {
                    return err("UUID sub-command, NAMESPACE requires a value.");
                }
                namespace_string = args[index].clone();
                index += 1;
            }
            "NAME" => {
                index += 1;
                if index >= args.len() {
                    return err("UUID sub-command, NAME requires a value.");
                }
                name = args[index].clone();
                index += 1;
            }
            "TYPE" => {
                index += 1;
                if index >= args.len() {
                    return err("UUID sub-command, TYPE requires a value.");
                }
                uuid_type = args[index].clone();
                index += 1;
            }
            "UPPER" => {
                index += 1;
                upper_case = true;
            }
            other => {
                return err(format!(
                    "UUID sub-command does not recognize option {other}."
                ));
            }
        }
    }

    let Some(namespace) = uuid_string_to_binary(&namespace_string) else {
        return err("UUID sub-command, malformed NAMESPACE UUID.");
    };

    // CreateHashInput: namespace bytes followed by name bytes.
    let mut hash_input = namespace;
    hash_input.extend_from_slice(name.as_bytes());

    let uuid = match uuid_type.as_str() {
        "MD5" => uuid_from_digest(&<md5::Md5 as md5::Digest>::digest(&hash_input), 3),
        "SHA1" => uuid_from_digest(&<sha1::Sha1 as sha1::Digest>::digest(&hash_input), 5),
        _ => {
            return err(format!("UUID sub-command, unknown TYPE '{uuid_type}'."));
        }
    };

    let uuid = if upper_case {
        uuid.to_ascii_uppercase()
    } else {
        uuid
    };

    interp.set_def(output_variable.clone(), uuid);
    Ok(())
}

// ---------------------------------------------------------------------
// GENEX_STRIP (`cmGeneratorExpression::Preprocess` +
// `extractAllGeneratorExpressions` + `StripEmptyListElements`)

fn strip_empty_list_elements(input: &str) -> String {
    if !input.contains(';') {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut result: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut last = 0usize;
    let mut skip_semi_colons = true;
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b';' {
            if skip_semi_colons {
                result.extend_from_slice(&bytes[last..index]);
                last = index + 1;
            }
            skip_semi_colons = true;
        } else {
            skip_semi_colons = false;
        }
        index += 1;
    }
    result.extend_from_slice(&bytes[last..]);
    if result.last() == Some(&b';') {
        result.pop();
    }
    String::from_utf8_lossy(&result).into_owned()
}

fn starts_with_genex(bytes: &[u8], i: usize) -> bool {
    bytes.get(i) == Some(&b'$') && bytes.get(i + 1) == Some(&b'<')
}

/// `extractAllGeneratorExpressions` (the collected-output form is unused by
/// GENEX_STRIP).
pub fn strip_all_generator_expressions(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut result = String::new();
    let mut last_pos = 0usize;
    // Stack of value-start positions; names are only needed for the
    // (unused) collected output.
    let mut genexps: Vec<usize> = Vec::new();
    loop {
        if last_pos > bytes.len() {
            break;
        }
        let Some(found) = input[last_pos..].find("$<") else {
            break;
        };
        let mut pos = last_pos + found;
        result.push_str(&input[last_pos..pos]);
        pos += 2;
        let c_start = pos;
        let mut c = pos;
        let mut c_name: Option<usize> = Some(pos);
        loop {
            if c >= bytes.len() {
                break;
            }
            if starts_with_genex(bytes, c) {
                c += 1;
                c_name = Some(c + 1);
                c += 1;
                continue;
            }
            if bytes[c] == b':' && c_name.is_some() {
                genexps.push(pos + (c + 1 - c_start));
                c_name = None;
            } else if bytes[c] == b'>' {
                if c_name.is_none() && !genexps.is_empty() {
                    genexps.pop();
                }
                if genexps.is_empty() {
                    break;
                }
            }
            c += 1;
        }
        let traversed = (c - c_start) + 1;
        if c >= bytes.len() {
            result.push_str("$<");
            result.push_str(&input[pos..pos + traversed.min(bytes.len() - pos)]);
        }
        pos += traversed;
        last_pos = pos;
    }
    if genexps.is_empty() {
        // `input.substr(lastPos)` throws std::out_of_range when lastPos
        // advanced past the end of an unterminated `$<`.
        if last_pos > input.len() {
            let _ = writeln!(
                std::io::stderr(),
                "terminate called after throwing an instance of 'std::out_of_range'"
            );
            let _ = writeln!(
                std::io::stderr(),
                "  what():  basic_string::substr: __pos (which is {last_pos}) \
                 > this->size() (which is {})",
                input.len()
            );
            std::process::abort();
        }
        result.push_str(&input[last_pos..]);
    }
    strip_empty_list_elements(&result)
}
