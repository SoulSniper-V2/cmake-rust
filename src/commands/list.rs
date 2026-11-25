//! `list()` command: port of `cmListCommand.cxx` and `cmList.cxx`.

use crate::cmregex::{Match, Regex};
use crate::interp::{CmdResult, Interp, err};
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::util;

pub fn register(interp: &mut Interp) {
    interp.register("list", list_command);
}

// ------------------------------------------------------------------
// cmList
// ------------------------------------------------------------------

/// Error matching `std::out_of_range` / `cmList::transform_error` messages.
struct OutOfRange(String);

impl From<OutOfRange> for crate::interp::CmdErr {
    fn from(e: OutOfRange) -> Self {
        crate::interp::CmdErr::Error(e.0)
    }
}

type LResult<T> = Result<T, OutOfRange>;

/// Compiles `pattern` with the kwsys engine port, which prints
/// cmsys::RegularExpression::compile()'s stdout diagnostics on failure.
fn compile_regex(pattern: &str) -> Result<Regex, ()> {
    Regex::new(pattern).map_err(|_| ())
}

#[derive(Default)]
struct CmList {
    values: Vec<String>,
}

impl CmList {
    fn from_string(value: &str, keep_empty: bool) -> CmList {
        CmList {
            values: util::expand_list(value, keep_empty),
        }
    }

    fn join(&self) -> String {
        util::join_list(&self.values)
    }

    /// `cmList::ComputeIndex` (bounds-checked).
    fn compute_index(&self, pos: i64) -> LResult<usize> {
        if self.values.is_empty() {
            return Err(OutOfRange(format!("index: {pos} out of range (0, 0)")));
        }
        let length = self.values.len() as i64;
        let mut index = pos;
        if index < 0 {
            index += length;
        }
        if index < 0 || length <= index {
            return Err(OutOfRange(format!(
                "index: {pos} out of range (-{}, {})",
                self.values.len(),
                self.values.len() - 1
            )));
        }
        Ok(index as usize)
    }

    /// `cmList::ComputeInsertIndex` (bounds-checked).
    fn compute_insert_index(&self, pos: i64) -> LResult<usize> {
        if self.values.is_empty() && pos != 0 {
            return Err(OutOfRange(format!("index: {pos} out of range (0, 0)")));
        }
        let length = self.values.len() as i64;
        let mut index = pos;
        if index < 0 {
            index += length;
        }
        if index < 0 || length < index {
            return Err(OutOfRange(format!(
                "index: {pos} out of range (-{}, {})",
                self.values.len(),
                self.values.len()
            )));
        }
        Ok(index as usize)
    }

    fn get_item(&self, pos: i64) -> LResult<&str> {
        let index = self.compute_index(pos)?;
        Ok(&self.values[index])
    }

    fn get_items(&self, indexes: &[i64]) -> LResult<CmList> {
        let mut out = CmList::default();
        for &index in indexes {
            out.values.push(self.get_item(index)?.to_owned());
        }
        Ok(out)
    }

    fn sublist(&self, pos: usize, length: usize) -> LResult<CmList> {
        if pos >= self.values.len() {
            return Err(OutOfRange(format!(
                "begin index: {pos} is out of range 0 - {}",
                self.values.len() - 1
            )));
        }
        let count = if length == usize::MAX || pos + length > self.values.len() {
            self.values.len()
        } else {
            pos + length
        };
        Ok(CmList {
            values: self.values[pos..count].to_vec(),
        })
    }

    fn find(&self, value: &str) -> Option<usize> {
        self.values.iter().position(|item| item == value)
    }

    fn remove_duplicates(&mut self) {
        let mut seen: Vec<String> = Vec::new();
        self.values.retain(|item| {
            if seen.iter().any(|v| v == item) {
                false
            } else {
                seen.push(item.clone());
                true
            }
        });
    }

    /// `cmList::RemoveItems(std::vector<index_type>)`.
    fn remove_at(&mut self, indexes: &[i64]) -> LResult<()> {
        if indexes.is_empty() {
            return Ok(());
        }
        let mut idx: Vec<usize> = indexes
            .iter()
            .map(|&index| self.compute_index(index))
            .collect::<LResult<Vec<usize>>>()?;
        idx.sort_unstable_by(|a, b| b.cmp(a));
        idx.dedup();
        for index in idx {
            self.values.remove(index);
        }
        Ok(())
    }

    /// `cmList::RemoveItems(std::vector<std::string>)`.
    fn remove_items(&mut self, items: &[String]) {
        let mut items = items.to_vec();
        items.sort();
        items.dedup();
        self.values
            .retain(|item| items.binary_search(item).is_err());
    }

    /// `cmList::insert_items` with ExpandElements::No, EmptyElements::Yes.
    fn insert_items(&mut self, index: i64, items: &[String]) -> LResult<()> {
        let offset = self.compute_insert_index(index)?;
        for (i, item) in items.iter().enumerate() {
            self.values.insert(offset + i, item.clone());
        }
        Ok(())
    }

    fn filter(&mut self, pattern: &str, include: bool) -> LResult<()> {
        let regex = compile_regex(pattern).map_err(|_| {
            OutOfRange(format!(
                "sub-command FILTER, mode REGEX failed to compile regex \"{pattern}\"."
            ))
        })?;
        self.values
            .retain(|item| regex.find(item).is_some() == include);
        Ok(())
    }

    fn sort(&mut self, config: &SortConfig) {
        let compare = |a: &str, b: &str| -> std::cmp::Ordering {
            let filter = |s: &str| -> String {
                let mut result = s.to_owned();
                if config.compare == CompareMethod::FileBasename {
                    result = util::filename_name(&result);
                }
                if config.case == CaseSensitivity::Insensitive {
                    result = result.to_ascii_lowercase();
                }
                result
            };
            let af = filter(a);
            let bf = filter(b);
            let ord = if config.compare == CompareMethod::Natural {
                strverscmp(&af, &bf)
            } else {
                af.cmp(&bf)
            };
            if config.order == OrderMode::Descending {
                ord.reverse()
            } else {
                ord
            }
        };
        if config.compare == CompareMethod::String
            && config.case == CaseSensitivity::Sensitive
            && config.order == OrderMode::Ascending
        {
            self.values.sort_unstable();
        } else {
            self.values.sort_unstable_by(|a, b| compare(a, b));
        }
    }
}

/// `cmSystemTools::strverscmp`.
fn strverscmp(lhs: &str, rhs: &str) -> std::cmp::Ordering {
    let l = lhs.as_bytes();
    let r = rhs.as_bytes();
    let at = |s: &[u8], i: usize| s.get(i).copied().unwrap_or(0);
    let first_difference = |a: &[u8], b: &[u8]| {
        let mut i = 0;
        while at(a, i) == at(b, i) && at(a, i) != 0 {
            i += 1;
        }
        i
    };
    let digits_begin = |s: &[u8], mut i: usize| {
        while i > 0 && s[i - 1].is_ascii_digit() {
            i -= 1;
        }
        i
    };
    let digits_end = |s: &[u8], mut i: usize| {
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let count_leading_zeros = |s: &[u8], b: usize| {
        let mut i = b;
        while at(s, i) == b'0' && at(s, i + 1).is_ascii_digit() {
            i += 1;
        }
        i - b
    };

    let i = first_difference(l, r);
    if at(l, i) != at(r, i) {
        let b = digits_begin(l, i);
        if b != i || (at(l, i).is_ascii_digit() && at(r, i).is_ascii_digit()) {
            let lz = count_leading_zeros(l, b);
            let rz = count_leading_zeros(r, b);
            if lz != rz {
                return if rz > lz {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                };
            }
            if lz == 0 {
                let le = digits_end(l, i);
                let re = digits_end(r, i);
                if le != re {
                    return if le > re {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    };
                }
            }
        }
    }
    (at(l, i) as i8 as i32 - at(r, i) as i8 as i32).cmp(&0)
}

// ------------------------------------------------------------------
// cmStringReplaceHelper
// ------------------------------------------------------------------

/// One piece of a parsed replace expression: literal text or group number.
enum Replacement {
    Text(String),
    Group(usize),
}

struct ReplaceHelper {
    regex_string: String,
    regex: Regex,
    replace_expression: String,
    replacements: Vec<Replacement>,
    error: Option<String>,
    num_groups: usize,
}

/// Count capture groups in the kwsys regex dialect: every unescaped `(`
/// outside a bracket expression. Bracket expressions do not treat `\` as
/// an escape; an immediate `]` or `-` is literal, matching kwsys `regnpar`.
fn count_groups(pattern: &str) -> usize {
    let bytes = pattern.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 1;
            }
            b'[' => {
                i += 1;
                if i < bytes.len() && bytes[i] == b'^' {
                    i += 1;
                }
                if i < bytes.len() && matches!(bytes[i], b']' | b'-') {
                    i += 1;
                }
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
            }
            b'(' => count += 1,
            _ => {}
        }
        i += 1;
    }
    count
}

impl ReplaceHelper {
    fn new(regex: &str, replace: &str) -> LResult<ReplaceHelper> {
        let Ok(compiled) = compile_regex(regex) else {
            return Err(OutOfRange(format!(
                "sub-command TRANSFORM, action REPLACE: Failed to compile regex \"{regex}\"."
            )));
        };
        let mut helper = ReplaceHelper {
            regex_string: regex.to_owned(),
            num_groups: count_groups(regex),
            regex: compiled,
            replace_expression: replace.to_owned(),
            replacements: Vec::new(),
            error: None,
        };
        helper.parse_replace_expression();
        if let Some(error) = &helper.error {
            return Err(OutOfRange(format!(
                "sub-command TRANSFORM, action REPLACE: {error}."
            )));
        }
        Ok(helper)
    }

    fn parse_replace_expression(&mut self) {
        let expr = self.replace_expression.as_bytes();
        let mut l = 0usize;
        while l < expr.len() {
            let r = expr[l..].iter().position(|&b| b == b'\\').map(|p| l + p);
            match r {
                None => {
                    self.replacements
                        .push(Replacement::Text(self.replace_expression[l..].to_owned()));
                    break;
                }
                Some(r) => {
                    if r > l {
                        self.replacements
                            .push(Replacement::Text(self.replace_expression[l..r].to_owned()));
                    }
                    if r == expr.len() - 1 {
                        self.error = Some("replace-expression ends in a backslash".to_owned());
                        return;
                    }
                    match expr[r + 1] {
                        b'0'..=b'9' => self
                            .replacements
                            .push(Replacement::Group((expr[r + 1] - b'0') as usize)),
                        b'n' => self.replacements.push(Replacement::Text("\n".to_owned())),
                        b'\\' => self.replacements.push(Replacement::Text("\\".to_owned())),
                        other => {
                            self.error = Some(format!(
                                "Unknown escape \"\\{}\" in replace-expression",
                                other as char
                            ));
                            return;
                        }
                    }
                    l = r + 2;
                }
            }
        }
    }

    /// Repeated find with the CMP0186 anchor semantics.
    fn find_at(
        &self,
        input: &[u8],
        base: usize,
        bol_at_offset: bool,
        nonempty_at_offset: bool,
    ) -> Option<Match> {
        let mut start = base;
        loop {
            // `^` can match at `start` when bol_at_offset (regbol == offset)
            // or when start is the beginning of the full text.
            let not_bol = if bol_at_offset { false } else { start != 0 };
            let found = self.regex.find_bytes(input, start, not_bol)?;
            if nonempty_at_offset && found.groups[0] == Some((base, base)) {
                // Match ending at the reject position: skip to next offset.
                start = base + 1;
                continue;
            }
            return Some(found);
        }
    }

    fn replace(&self, interp: &mut Interp, input: &str, output: &mut String) -> LResult<()> {
        // Byte ranges may split multi-byte characters; assemble bytes.
        let input_bytes = crate::bytes::encode(input);
        let mut bytes = Vec::<u8>::new();
        let bol_at_offset = interp.policy_status("CMP0186") != PolicyStatus::New;

        let mut base = 0usize;
        let mut nonempty = false;
        while let Some(found) = self.find_at(&input_bytes, base, bol_at_offset, nonempty) {
            let (start, end) = found.groups[0].unwrap_or((0, 0));
            crate::state::clear_matches(interp);
            crate::state::store_matches(interp, input, &found);

            // Concatenate the part of the input that was not matched.
            bytes.extend_from_slice(&input_bytes[base..start]);

            // Concatenate the replacement for the match.
            for replacement in &self.replacements {
                match replacement {
                    Replacement::Text(text) => bytes.extend_from_slice(&crate::bytes::encode(text)),
                    Replacement::Group(n) => {
                        if *n > self.num_groups {
                            return Err(OutOfRange(format!(
                                "replace expression \"{}\" contains an out-of-range escape for regex \"{}\"",
                                self.replace_expression, self.regex_string
                            )));
                        }
                        if let Some((s, e)) = found.groups[*n] {
                            bytes.extend_from_slice(&input_bytes[s..e]);
                        }
                    }
                }
            }

            // Move past the match.
            base = end;
            if start == input_bytes.len() {
                break;
            }
            nonempty = start == end;
        }
        bytes.extend_from_slice(&input_bytes[base..]);
        *output = crate::bytes::decode(&bytes);
        Ok(())
    }
}

// ------------------------------------------------------------------
// helpers
// ------------------------------------------------------------------

/// `GetIndexArg` with CMP0121 handling.
fn get_index_arg(interp: &mut Interp, arg: &str) -> Option<i64> {
    let (mut value, ok) = util::strtol(arg, 10);
    if !ok {
        match interp.policy_status("CMP0121") {
            PolicyStatus::Warn => {
                let index = crate::policy::index_of("CMP0121").unwrap();
                let text = format!(
                    "{} Invalid list index \"{arg}\".",
                    crate::policy::policy_warning(index)
                );
                interp.issue(MessageType::AuthorWarning, &text);
            }
            PolicyStatus::New => return None,
            PolicyStatus::Old => {}
        }
    }
    // Truncation to int as in the C++ code.
    value = value as i32 as i64;
    Some(value)
}

fn get_list_string(interp: &Interp, var: &str) -> Option<String> {
    interp.get_def(var).map(str::to_owned)
}

/// `GetList`: variable must be defined; empty string is an empty list;
/// otherwise expand keeping empty elements.
fn get_list(interp: &Interp, var: &str) -> Option<CmList> {
    let value = get_list_string(interp, var)?;
    if value.is_empty() {
        return Some(CmList::default());
    }
    Some(CmList::from_string(&value, true))
}

// ------------------------------------------------------------------
// subcommands
// ------------------------------------------------------------------

fn handle_length(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 3 {
        return err("sub-command LENGTH requires two arguments.");
    }
    let list = get_list(interp, &args[1]);
    let size = list.map_or(0, |l| l.values.len());
    interp.set_def(args[2].clone(), size.to_string());
    Ok(())
}

fn handle_get(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 4 {
        return err("sub-command GET requires at least three arguments.");
    }
    let variable_name = args.last().unwrap().clone();
    let Some(list) = get_list(interp, &args[1]) else {
        interp.set_def(variable_name, "NOTFOUND");
        return Ok(());
    };
    if list.values.is_empty() {
        return err("GET given empty list");
    }
    let mut indexes = Vec::new();
    for arg in &args[2..args.len() - 1] {
        let Some(index) = get_index_arg(interp, arg) else {
            return err(format!("index: {arg} is not a valid index"));
        };
        indexes.push(index);
    }
    match list.get_items(&indexes) {
        Ok(values) => {
            interp.set_def(variable_name, values.join());
            Ok(())
        }
        Err(e) => err(e.0),
    }
}

fn handle_append(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return Ok(());
    }
    let list_name = &args[1];
    let mut list_string = get_list_string(interp, list_name).unwrap_or_default();
    let value = util::join_list(&args[2..]);
    if list_string.is_empty() {
        list_string = value;
    } else {
        list_string.push(';');
        list_string.push_str(&value);
    }
    interp.set_def(list_name.clone(), list_string);
    Ok(())
}

fn handle_prepend(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return Ok(());
    }
    let list_name = &args[1];
    let mut list_string = get_list_string(interp, list_name).unwrap_or_default();
    let value = util::join_list(&args[2..]);
    if list_string.is_empty() {
        list_string = value;
    } else {
        list_string = format!("{value};{list_string}");
    }
    interp.set_def(list_name.clone(), list_string);
    Ok(())
}

fn handle_pop(interp: &mut Interp, args: &[String], back: bool) -> CmdResult {
    let list_name = args[1].clone();
    let out_vars = &args[2..];
    let Some(mut list) = get_list(interp, &list_name) else {
        for name in out_vars {
            interp.unset_def(name);
        }
        return Ok(());
    };
    if !list.values.is_empty() {
        if out_vars.is_empty() {
            if back {
                list.values.pop();
            } else {
                list.values.remove(0);
            }
        } else {
            let mut assigned = 0;
            while !list.values.is_empty() && assigned < out_vars.len() {
                let value = if back {
                    list.values.pop().unwrap()
                } else {
                    list.values.remove(0)
                };
                interp.set_def(out_vars[assigned].clone(), value);
                assigned += 1;
            }
            for name in &out_vars[assigned..] {
                interp.unset_def(name);
            }
        }
        interp.set_def(list_name, list.join());
    } else {
        for name in out_vars {
            interp.unset_def(name);
        }
    }
    Ok(())
}

fn handle_find(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 4 {
        return err("sub-command FIND requires three arguments.");
    }
    let variable_name = args.last().unwrap().clone();
    let Some(list) = get_list(interp, &args[1]) else {
        interp.set_def(variable_name, "-1");
        return Ok(());
    };
    let index = list
        .find(&args[2])
        .map_or("-1".to_owned(), |i| i.to_string());
    interp.set_def(variable_name, index);
    Ok(())
}

fn handle_insert(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 4 {
        return err("sub-command INSERT requires at least three arguments.");
    }
    let list_name = args[1].clone();
    let Some(index) = get_index_arg(interp, &args[2]) else {
        return err(format!("index: {} is not a valid index", args[2]));
    };
    let mut list = get_list(interp, &list_name).unwrap_or_default();
    match list.insert_items(index, &args[3..]) {
        Ok(()) => {
            interp.set_def(list_name, list.join());
            Ok(())
        }
        Err(e) => err(e.0),
    }
}

fn handle_join(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 4 {
        return err(format!(
            "sub-command JOIN requires three arguments ({} found).",
            args.len() - 1
        ));
    }
    let Some(list) = get_list(interp, &args[1]) else {
        interp.set_def(args[3].clone(), "");
        return Ok(());
    };
    interp.set_def(args[3].clone(), list.values.join(&args[2]));
    Ok(())
}

fn handle_remove_item(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() == 2 {
        return Ok(());
    }
    let list_name = args[1].clone();
    let Some(mut list) = get_list(interp, &list_name) else {
        return Ok(());
    };
    list.remove_items(&args[2..]);
    interp.set_def(list_name, list.join());
    Ok(())
}

fn handle_reverse(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() > 2 {
        return err("sub-command REVERSE only takes one argument.");
    }
    let list_name = args[1].clone();
    let Some(mut list) = get_list(interp, &list_name) else {
        return Ok(());
    };
    list.values.reverse();
    interp.set_def(list_name, list.join());
    Ok(())
}

fn handle_remove_duplicates(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() > 2 {
        return err("sub-command REMOVE_DUPLICATES only takes one argument.");
    }
    let list_name = args[1].clone();
    let Some(mut list) = get_list(interp, &list_name) else {
        return Ok(());
    };
    list.remove_duplicates();
    interp.set_def(list_name, list.join());
    Ok(())
}

// --- TRANSFORM ---

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransformAction {
    Append,
    Prepend,
    ToUpper,
    ToLower,
    Strip,
    GenexStrip,
    Replace,
}

impl TransformAction {
    fn descriptor(name: &str) -> Option<(TransformAction, usize)> {
        match name {
            "APPEND" => Some((TransformAction::Append, 1)),
            "PREPEND" => Some((TransformAction::Prepend, 1)),
            "TOUPPER" => Some((TransformAction::ToUpper, 0)),
            "TOLOWER" => Some((TransformAction::ToLower, 0)),
            "STRIP" => Some((TransformAction::Strip, 0)),
            "GENEX_STRIP" => Some((TransformAction::GenexStrip, 0)),
            "REPLACE" => Some((TransformAction::Replace, 2)),
            _ => None,
        }
    }
}

enum Selector {
    All,
    Regex(Regex),
    At(Vec<i64>),
    For { start: i64, stop: i64, step: i64 },
}

impl Selector {
    fn tag(&self) -> &'static str {
        match self {
            Selector::All => "NO SELECTOR",
            Selector::Regex(_) => "REGEX",
            Selector::At(_) => "AT",
            Selector::For { .. } => "FOR",
        }
    }

    fn in_selection(&self, value: &str) -> bool {
        match self {
            Selector::Regex(re) => re.find(value).is_some(),
            _ => true,
        }
    }

    /// Normalize index for AT/FOR (TransformSelectorIndexes::NormalizeIndex).
    fn normalize_index(&self, mut index: i64, count: usize) -> LResult<usize> {
        if index < 0 {
            index += count as i64;
        }
        if index < 0 || count <= index as usize {
            return Err(OutOfRange(format!(
                "sub-command TRANSFORM, selector {}, index: {} out of range (-{}, {}).",
                self.tag(),
                index,
                count,
                (count as u64).wrapping_sub(1)
            )));
        }
        Ok(index as usize)
    }

    /// `TransformSelector::Transform`: apply `f` in place per selector kind.
    fn transform(
        &self,
        values: &mut [String],
        f: &mut dyn FnMut(&str) -> LResult<String>,
    ) -> LResult<()> {
        match self {
            Selector::At(indexes) => {
                let mut normalized = Vec::with_capacity(indexes.len());
                for &index in indexes {
                    normalized.push(self.normalize_index(index, values.len())?);
                }
                for index in normalized {
                    values[index] = f(&values[index])?;
                }
            }
            Selector::For { start, stop, step } => {
                let start = self.normalize_index(*start, values.len())? as i64;
                let stop = self.normalize_index(*stop, values.len())? as i64;
                if start > stop {
                    return Err(OutOfRange(format!(
                        "sub-command TRANSFORM, selector FOR expects <start> to be no greater than <stop> ({start} > {stop})"
                    )));
                }
                let mut index = start;
                while index <= stop {
                    values[index as usize] = f(&values[index as usize])?;
                    index += step;
                }
            }
            _ => {
                for value in values.iter_mut() {
                    *value = f(value)?;
                }
            }
        }
        Ok(())
    }
}

/// `cmGeneratorExpression::StripEmptyListElements`.
fn strip_empty_list_elements(input: &str) -> String {
    if !input.contains(';') {
        return input.to_owned();
    }
    let bytes = input.as_bytes();
    let mut result = String::new();
    let mut last = 0usize;
    let mut skip = true;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b';' {
            if skip {
                result.push_str(&input[last..i]);
                last = i + 1;
            }
            skip = true;
        } else {
            skip = false;
        }
    }
    result.push_str(&input[last..]);
    if result.ends_with(';') {
        result.pop();
    }
    result
}

/// `cmGeneratorExpression::Preprocess` with StripAllGeneratorExpressions.
fn strip_all_generator_expressions(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut result = String::new();
    let mut last_pos = 0usize;
    // stack of { name, start position of value }
    let mut genexps: Vec<(String, usize)> = Vec::new();
    while let Some(open) = input[last_pos..].find("$<").map(|o| last_pos + o) {
        result.push_str(&input[last_pos..open]);
        let mut pos = open + 2;
        // Scan for the end of the genex.
        let mut c = pos;
        let mut c_name: Option<usize> = Some(pos);
        let c_start = pos;
        while c < bytes.len() {
            if bytes[c] == b'$' && bytes.get(c + 1) == Some(&b'<') {
                c += 1;
                c_name = Some(c + 1);
                c += 1;
                continue;
            }
            if bytes[c] == b':' && c_name.is_some() {
                genexps.push((
                    input[pos + (c_name.unwrap() - c_start)..c].to_owned(),
                    pos + (c + 1 - c_start),
                ));
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
        let traversed = c - c_start + 1;
        if c >= bytes.len() {
            result.push_str("$<");
            result.push_str(&input[pos..pos + traversed.min(bytes.len() - pos)]);
        }
        pos += traversed;
        last_pos = pos;
    }
    if genexps.is_empty() {
        result.push_str(&input[last_pos..]);
    }
    strip_empty_list_elements(&result)
}

fn handle_transform(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err("sub-command TRANSFORM requires an action to be specified.");
    }
    let list_name = args[1].clone();
    let mut index = 2usize;

    let Some((action, arity)) = TransformAction::descriptor(&args[index]) else {
        return err(format!(
            " sub-command TRANSFORM, {} invalid action.",
            args[index]
        ));
    };
    let action_name = args[index].clone();

    index += 1;
    if args.len() < index + arity {
        return err(format!(
            "sub-command TRANSFORM, action {action_name} expects {arity} argument(s)."
        ));
    }
    let action_args: Vec<String> = if arity > 0 {
        args[3..index + arity].to_vec()
    } else {
        Vec::new()
    };
    index += arity;

    // `std::stoi`-like: leading whitespace, sign, digits; must consume all.
    fn stoi(s: &str) -> Result<i64, ()> {
        let t = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let bytes = t.as_bytes();
        let mut i = 0usize;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start || i != bytes.len() {
            return Err(());
        }
        // std::stoi throws out_of_range past i32: abort like std::terminate.
        let value: i64 = t[..i].parse().unwrap_or_else(|_| unsafe { libc::abort() });
        if value > i32::MAX as i64 || value < i32::MIN as i64 {
            unsafe { libc::abort() }
        }
        Ok(value)
    }

    let mut selector: Option<Selector> = None;
    let mut output_name = list_name.clone();
    while args.len() > index {
        let arg = &args[index];
        if let "REGEX" | "AT" | "FOR" = arg.as_str()
            && let Some(existing) = &selector
        {
            return err(format!(
                "sub-command TRANSFORM, selector already specified ({}).",
                existing.tag()
            ));
        }
        match arg.as_str() {
            "REGEX" => {
                index += 1;
                if args.len() == index {
                    return err(
                        "sub-command TRANSFORM, selector REGEX expects 'regular expression' argument."
                            .to_owned(),
                    );
                }
                let pattern = &args[index];
                let regex = compile_regex(pattern).map_err(|_| {
                    crate::interp::CmdErr::Error(format!(
                        "sub-command TRANSFORM, selector REGEX failed to compile regex \"{pattern}\"."
                    ))
                })?;
                selector = Some(Selector::Regex(regex));
                index += 1;
            }
            "AT" => {
                let mut indexes = Vec::new();
                while args.len() > index + 1 {
                    match stoi(&args[index + 1]) {
                        Ok(value) => {
                            indexes.push(value);
                            index += 1;
                        }
                        Err(()) => break,
                    }
                }
                if indexes.is_empty() {
                    return err(
                        "sub-command TRANSFORM, selector AT expects at least one numeric value."
                            .to_owned(),
                    );
                }
                index += 1;
                selector = Some(Selector::At(indexes));
            }
            "FOR" => {
                index += 1;
                if args.len() <= index + 1 {
                    return err(
                        "sub-command TRANSFORM, selector FOR expects, at least, two arguments."
                            .to_owned(),
                    );
                }
                let mut valid = true;
                let start = match stoi(&args[index]) {
                    Ok(v) => v,
                    Err(()) => {
                        valid = false;
                        0
                    }
                };
                let mut stop = 0;
                if valid {
                    index += 1;
                    match stoi(&args[index]) {
                        Ok(v) => stop = v,
                        Err(()) => valid = false,
                    }
                }
                if !valid {
                    return err(
                        "sub-command TRANSFORM, selector FOR expects, at least, two numeric values."
                            .to_owned(),
                    );
                }
                let mut step = 1i64;
                index += 1;
                if args.len() > index {
                    if let Ok(v) = stoi(&args[index]) {
                        step = v;
                        index += 1;
                    }
                }
                if step <= 0 {
                    return err(
                        "sub-command TRANSFORM, selector FOR expects positive numeric value for <step>."
                            .to_owned(),
                    );
                }
                selector = Some(Selector::For { start, stop, step });
            }
            "OUTPUT_VARIABLE" => {
                index += 1;
                if args.len() == index {
                    return err(
                        "sub-command TRANSFORM, OUTPUT_VARIABLE expects variable name argument."
                            .to_owned(),
                    );
                }
                output_name = args[index].clone();
                index += 1;
            }
            _ => {
                return err(format!(
                    "sub-command TRANSFORM, '{}': unexpected argument(s).",
                    args[index..].join(" ")
                ));
            }
        }
    }

    let Some(mut list) = get_list(interp, &list_name) else {
        interp.set_def(output_name, "");
        return Ok(());
    };
    let selector = selector.unwrap_or(Selector::All);

    // Action Initialize (REPLACE compiles its regex before selector runs).
    let helper = match action {
        TransformAction::Replace => Some(
            ReplaceHelper::new(&action_args[0], &action_args[1])
                .map_err(|e| crate::interp::CmdErr::Error(e.0))?,
        ),
        _ => None,
    };

    let mut apply = |s: &str| -> LResult<String> {
        if !selector.in_selection(s) {
            return Ok(s.to_owned());
        }
        Ok(match action {
            TransformAction::Append => format!("{}{}", s, action_args[0]),
            TransformAction::Prepend => format!("{}{}", action_args[0], s),
            TransformAction::ToUpper => s.to_ascii_uppercase(),
            TransformAction::ToLower => s.to_ascii_lowercase(),
            TransformAction::Strip => s.trim_matches(|c: char| c.is_whitespace()).to_owned(),
            TransformAction::GenexStrip => strip_all_generator_expressions(s),
            TransformAction::Replace => {
                let mut out = String::new();
                helper
                    .as_ref()
                    .unwrap()
                    .replace(interp, s, &mut out)
                    .map_err(|e| {
                        OutOfRange(format!("sub-command TRANSFORM, action REPLACE: {}.", e.0))
                    })?;
                out
            }
        })
    };

    match selector.transform(&mut list.values, &mut apply) {
        Ok(()) => {
            interp.set_def(output_name, list.join());
            Ok(())
        }
        Err(e) => err(e.0),
    }
}

// --- SORT ---

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompareMethod {
    String,
    FileBasename,
    Natural,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaseSensitivity {
    Sensitive,
    Insensitive,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OrderMode {
    Ascending,
    Descending,
}

struct SortConfig {
    compare: CompareMethod,
    case: CaseSensitivity,
    order: OrderMode,
}

fn handle_sort(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() > 8 {
        return err("sub-command SORT only takes up to six arguments.");
    }
    const HINT: &str = "sub-command SORT ";
    let mut compare: Option<CompareMethod> = None;
    let mut case: Option<CaseSensitivity> = None;
    let mut order: Option<OrderMode> = None;
    let mut i = 2usize;
    while i < args.len() {
        let option = &args[i];
        i += 1;
        match option.as_str() {
            "COMPARE" => {
                if compare.is_some() {
                    return err(format!(
                        "{HINT}option \"{option}\" has been specified multiple times."
                    ));
                }
                if i >= args.len() {
                    return err(format!("{HINT}missing argument for option \"{option}\"."));
                }
                let argument = &args[i];
                i += 1;
                compare = Some(match argument.as_str() {
                    "STRING" => CompareMethod::String,
                    "FILE_BASENAME" => CompareMethod::FileBasename,
                    "NATURAL" => CompareMethod::Natural,
                    _ => {
                        return err(format!(
                            "{HINT}value \"{argument}\" for option \"{option}\" is invalid."
                        ));
                    }
                });
            }
            "CASE" => {
                if case.is_some() {
                    return err(format!(
                        "{HINT}option \"{option}\" has been specified multiple times."
                    ));
                }
                if i >= args.len() {
                    return err(format!("{HINT}missing argument for option \"{option}\"."));
                }
                let argument = &args[i];
                i += 1;
                case = Some(match argument.as_str() {
                    "SENSITIVE" => CaseSensitivity::Sensitive,
                    "INSENSITIVE" => CaseSensitivity::Insensitive,
                    _ => {
                        return err(format!(
                            "{HINT}value \"{argument}\" for option \"{option}\" is invalid."
                        ));
                    }
                });
            }
            "ORDER" => {
                if order.is_some() {
                    return err(format!(
                        "{HINT}option \"{option}\" has been specified multiple times."
                    ));
                }
                if i >= args.len() {
                    return err(format!("{HINT}missing argument for option \"{option}\"."));
                }
                let argument = &args[i];
                i += 1;
                order = Some(match argument.as_str() {
                    "ASCENDING" => OrderMode::Ascending,
                    "DESCENDING" => OrderMode::Descending,
                    _ => {
                        return err(format!(
                            "{HINT}value \"{argument}\" for option \"{option}\" is invalid."
                        ));
                    }
                });
            }
            _ => {
                return err(format!("{HINT}option \"{option}\" is unknown."));
            }
        }
    }
    let config = SortConfig {
        compare: compare.unwrap_or(CompareMethod::String),
        case: case.unwrap_or(CaseSensitivity::Sensitive),
        order: order.unwrap_or(OrderMode::Ascending),
    };

    let list_name = args[1].clone();
    let Some(mut list) = get_list(interp, &list_name) else {
        return Ok(());
    };
    list.sort(&config);
    interp.set_def(list_name, list.join());
    Ok(())
}

fn handle_sublist(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() != 5 {
        return err(format!(
            "sub-command SUBLIST requires four arguments ({} found).",
            args.len() - 1
        ));
    }
    let list_name = &args[1];
    let variable_name = args.last().unwrap().clone();
    let Some(list) = get_list(interp, list_name) else {
        interp.set_def(variable_name, "");
        return Ok(());
    };
    if list.values.is_empty() {
        interp.set_def(variable_name, "");
        return Ok(());
    }
    let Some(start) = get_index_arg(interp, &args[2]) else {
        return err(format!("index: {} is not a valid index", args[2]));
    };
    let Some(length) = get_index_arg(interp, &args[3]) else {
        return err(format!("index: {} is not a valid index", args[3]));
    };
    if start < 0 {
        return err(format!(
            "begin index: {start} is out of range 0 - {}",
            list.values.len() - 1
        ));
    }
    if length < -1 {
        return err(format!("length: {length} should be -1 or greater"));
    }
    match list.sublist(start as usize, length as usize) {
        Ok(sub) => {
            interp.set_def(variable_name, sub.join());
            Ok(())
        }
        Err(e) => err(e.0),
    }
}

fn handle_remove_at(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 3 {
        return err("sub-command REMOVE_AT requires at least two arguments.");
    }
    let list_name = args[1].clone();
    let Some(list) = get_list(interp, &list_name) else {
        let mut text = String::from("index: ");
        for (i, arg) in args[1..].iter().enumerate() {
            text.push_str(arg);
            if i != args.len() - 2 {
                text.push_str(", ");
            }
        }
        text.push_str(" out of range (0, 0)");
        return err(text);
    };
    if list.values.is_empty() {
        let mut text = String::from("index: ");
        for (i, arg) in args[1..].iter().enumerate() {
            text.push_str(arg);
            if i != args.len() - 2 {
                text.push_str(", ");
            }
        }
        text.push_str(" out of range (0, 0)");
        return err(text);
    }
    let mut indexes = Vec::new();
    for arg in &args[2..] {
        let Some(index) = get_index_arg(interp, arg) else {
            return err(format!("index: {arg} is not a valid index"));
        };
        indexes.push(index);
    }
    let mut list = list;
    match list.remove_at(&indexes) {
        Ok(()) => {
            interp.set_def(list_name, list.join());
            Ok(())
        }
        Err(e) => err(e.0),
    }
}

fn handle_filter(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("sub-command FILTER requires a list to be specified.");
    }
    if args.len() < 3 {
        return err("sub-command FILTER requires an operator to be specified.");
    }
    if args.len() < 4 {
        return err("sub-command FILTER requires a mode to be specified.");
    }
    let include = match args[2].as_str() {
        "INCLUDE" => true,
        "EXCLUDE" => false,
        op => {
            return err(format!(
                "sub-command FILTER does not recognize operator {op}"
            ));
        }
    };
    let list_name = args[1].clone();
    let Some(mut list) = get_list(interp, &list_name) else {
        return Ok(());
    };
    if args[3] != "REGEX" {
        return err(format!(
            "sub-command FILTER does not recognize mode {}",
            args[3]
        ));
    }
    if args.len() != 5 {
        return err("sub-command FILTER, mode REGEX requires five arguments.");
    }
    match list.filter(&args[4], include) {
        Ok(()) => {
            interp.set_def(list_name, list.join());
            Ok(())
        }
        Err(e) => err(e.0),
    }
}

// ------------------------------------------------------------------
// dispatch
// ------------------------------------------------------------------

fn list_command(interp: &mut Interp, args: &[String]) -> CmdResult {
    if args.len() < 2 {
        return err("must be called with at least two arguments.");
    }
    match args[0].as_str() {
        "LENGTH" => handle_length(interp, args),
        "GET" => handle_get(interp, args),
        "APPEND" => handle_append(interp, args),
        "PREPEND" => handle_prepend(interp, args),
        "POP_BACK" => handle_pop(interp, args, true),
        "POP_FRONT" => handle_pop(interp, args, false),
        "FIND" => handle_find(interp, args),
        "INSERT" => handle_insert(interp, args),
        "JOIN" => handle_join(interp, args),
        "REMOVE_AT" => handle_remove_at(interp, args),
        "REMOVE_ITEM" => handle_remove_item(interp, args),
        "REMOVE_DUPLICATES" => handle_remove_duplicates(interp, args),
        "TRANSFORM" => handle_transform(interp, args),
        "SORT" => handle_sort(interp, args),
        "SUBLIST" => handle_sublist(interp, args),
        "REVERSE" => handle_reverse(interp, args),
        "FILTER" => handle_filter(interp, args),
        other => err(format!("does not recognize sub-command {other}")),
    }
}
