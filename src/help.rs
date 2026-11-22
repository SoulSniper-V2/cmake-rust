//! The help system: `cmDocumentation` (option parsing and the help file
//! lookups under `${CMAKE_ROOT}/Help`) and `cmRST` (the reStructuredText
//! to plain-text renderer).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use crate::cmregex::{Match, Regex};

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid help regex")
}

static CMAKE_DIRECTIVE: LazyLock<Regex> =
    LazyLock::new(|| re("^.. (cmake:)?(command|envvar|genex|signature|variable)::"));
static CMAKE_MODULE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. cmake-module::[ \t]+([^ \t\n]+)$"));
static PARSED_LITERAL_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. parsed-literal::[ \t]*(.*)$"));
static CODE_BLOCK_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. code-block::[ \t]*(.*)$"));
static REPLACE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. (\\|[^|]+\\|) replace::[ \t]*(.*)$"));
static INCLUDE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. include::[ \t]+([^ \t\n]+)$"));
static TOC_TREE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. toctree::[ \t]*(.*)$"));
static PRODUCTION_LIST_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. productionlist::[ \t]*(.*)$"));
static NOTE_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. note::[ \t]*(.*)$"));
static VERSION_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| re("^.. version(added|changed)::[ \t]*(.*)$"));
static MODULE_RST: LazyLock<Regex> = LazyLock::new(|| re("^#\\[(=*)\\[\\.rst:$"));
static CMAKE_ROLE: LazyLock<Regex> = LazyLock::new(|| {
    re("(:cmake)?:(cref|command|cpack_gen|generator|genex|variable|envvar|module|policy|prop_cache|prop_dir|prop_gbl|prop_inst|prop_sf|prop_test|prop_tgt|manual):`(<*([^`<]|[^` \t]<)*)([ \t]+<[^`]*>)?`")
});
static INLINE_LINK: LazyLock<Regex> = LazyLock::new(|| re("`(<*([^`<]|[^` \t]<)*)([ \t]+<[^`]*>)?`_"));
static INLINE_LITERAL: LazyLock<Regex> = LazyLock::new(|| re("``([^`]*)``"));
static SUBSTITUTION: LazyLock<Regex> = LazyLock::new(|| {
    re("(^|[^A-Za-z0-9_])((\\|[^| \t\r\n]([^|\r\n]*[^| \t\r\n])?\\|)(__|_|))([^A-Za-z0-9_]|$)")
});
static TOC_TREE_LINK: LazyLock<Regex> = LazyLock::new(|| re("^.*[ \t]+<([^>]+)>$"));

/// `RegularExpressionMatch::match(n)`: empty when the group did not take part.
fn group<'t>(m: &Match, text: &'t str, n: usize) -> &'t str {
    m.groups.get(n).copied().flatten().map_or("", |(start, end)| &text[start..end])
}

fn start(m: &Match, n: usize) -> usize {
    m.groups[n].map_or(0, |(start, _)| start)
}

fn end(m: &Match, n: usize) -> usize {
    m.groups[n].map_or(0, |(_, end)| end)
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// `cmSystemTools::GetLineFromStream` over a whole file: lines without
/// their newline (and a trailing carriage return).
fn stream_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .split_inclusive('\n')
        .map(|line| {
            let line = line.strip_suffix('\n').unwrap_or(line);
            line.strip_suffix('\r').unwrap_or(line).to_owned()
        })
        .collect();
    if text.is_empty() {
        lines.clear();
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Markup {
    None,
    Empty,
    Normal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Directive {
    None,
    ParsedLiteral,
    LiteralBlock,
    CodeBlock,
    Replace,
    TocTree,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Include {
    Normal,
    Module,
    TocTree,
}

/// `cmRST`.
pub struct Rst<'o> {
    out: &'o mut String,
    doc_root: String,
    doc_dir: String,
    include_depth: usize,
    output_line_pending: bool,
    last_line_ended_in_colon_colon: bool,
    markup: Markup,
    directive: Directive,
    markup_lines: Vec<String>,
    replace_name: String,
    replace: BTreeMap<String, String>,
    replaced: BTreeSet<String>,
}

impl<'o> Rst<'o> {
    pub fn new(out: &'o mut String, doc_root: &str) -> Self {
        let mut replace = BTreeMap::new();
        replace.insert("|release|".to_owned(), crate::interp::VERSION_STRING.to_owned());
        Self {
            out,
            doc_root: doc_root.to_owned(),
            doc_dir: String::new(),
            include_depth: 0,
            output_line_pending: false,
            last_line_ended_in_colon_colon: false,
            markup: Markup::None,
            directive: Directive::None,
            markup_lines: Vec::new(),
            replace_name: String::new(),
            replace,
            replaced: BTreeSet::new(),
        }
    }

    /// `cmRST::ProcessFile`.
    pub fn process_file(&mut self, fname: &str, is_module: bool) -> bool {
        let Ok(bytes) = std::fs::read(fname) else {
            return false;
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.doc_dir = crate::util::filename_path(fname);
        if is_module {
            self.process_module(&text);
        } else {
            self.process_rst(&text);
        }
        self.output_line_pending = true;
        true
    }

    fn process_rst(&mut self, text: &str) {
        for line in stream_lines(text) {
            self.process_line(&line);
        }
        self.reset();
    }

    /// `cmRST::ProcessModule`: the `.rst:` comment blocks of a module.
    fn process_module(&mut self, text: &str) {
        let mut rst = String::new();
        for mut line in stream_lines(text) {
            if !rst.is_empty() && rst != "#" {
                // Bracket mode: check for end bracket.
                match line.find(&rst) {
                    None => self.process_line(&line),
                    Some(pos) => {
                        if !line.starts_with('#') {
                            line.truncate(pos);
                            self.process_line(&line);
                        }
                        rst.clear();
                        self.reset();
                        self.output_line_pending = true;
                    }
                }
            } else {
                // Line mode: check for .rst start (bracket or line).
                if rst == "#" {
                    if line == "#" {
                        self.process_line("");
                        continue;
                    }
                    if let Some(rest) = line.strip_prefix("# ") {
                        let rest = rest.to_owned();
                        self.process_line(&rest);
                        continue;
                    }
                    rst.clear();
                    self.reset();
                    self.output_line_pending = true;
                }
                if line == "#.rst:" {
                    rst = "#".to_owned();
                } else if let Some(m) = MODULE_RST.find(&line) {
                    rst = format!("]{}]", group(&m, &line, 1));
                }
            }
        }
        if rst == "#" {
            self.reset();
        }
    }

    /// `cmRST::Reset`.
    fn reset(&mut self) {
        if !self.markup_lines.is_empty() {
            unindent_lines(&mut self.markup_lines);
        }
        match self.directive {
            Directive::None => {}
            Directive::ParsedLiteral => self.output_markup_lines(true),
            Directive::LiteralBlock | Directive::CodeBlock => self.output_markup_lines(false),
            Directive::Replace => {
                let replacement = self.replace.entry(self.replace_name.clone()).or_default();
                replacement.push_str(&self.markup_lines.join(" "));
                self.replace_name.clear();
            }
            Directive::TocTree => self.process_directive_toc_tree(),
        }
        self.markup = Markup::None;
        self.directive = Directive::None;
        self.markup_lines.clear();
    }

    /// `cmRST::ProcessLine`.
    fn process_line(&mut self, line: &str) {
        let last_line_ended_in_colon_colon = self.last_line_ended_in_colon_colon;
        self.last_line_ended_in_colon_colon = false;
        let bytes = line.as_bytes();
        // A line starting in .. is an explicit markup start.
        if line == ".." || (bytes.len() >= 3 && bytes[0] == b'.' && bytes[1] == b'.' && is_space(bytes[2])) {
            self.reset();
            self.markup = if line[2..].bytes().all(|b| b == b' ' || b == b'\t') {
                Markup::Empty
            } else {
                Markup::Normal
            };
            if CMAKE_DIRECTIVE.find(line).is_some() {
                // Output cmake domain directives and their content normally.
                self.normal_line(line);
            } else if let Some(m) = CMAKE_MODULE_DIRECTIVE.find(line) {
                // Process cmake-module directive: scan .cmake file comments.
                let file = group(&m, line, 1).to_owned();
                if file.is_empty() || !self.process_include(file, Include::Module) {
                    self.normal_line(line);
                }
            } else if let Some(m) = PARSED_LITERAL_DIRECTIVE.find(line) {
                self.directive = Directive::ParsedLiteral;
                self.markup_lines.push(group(&m, line, 1).to_owned());
            } else if CODE_BLOCK_DIRECTIVE.find(line).is_some() {
                // Ignore the language spec and record the opening line as blank.
                self.directive = Directive::CodeBlock;
                self.markup_lines.push(String::new());
            } else if let Some(m) = REPLACE_DIRECTIVE.find(line) {
                self.directive = Directive::Replace;
                self.replace_name = group(&m, line, 1).to_owned();
                self.markup_lines.push(group(&m, line, 2).to_owned());
            } else if let Some(m) = INCLUDE_DIRECTIVE.find(line) {
                let file = group(&m, line, 1).to_owned();
                if file.is_empty() || !self.process_include(file, Include::Normal) {
                    self.normal_line(line);
                }
            } else if let Some(m) = TOC_TREE_DIRECTIVE.find(line) {
                self.directive = Directive::TocTree;
                self.markup_lines.push(group(&m, line, 1).to_owned());
            } else if PRODUCTION_LIST_DIRECTIVE.find(line).is_some()
                || NOTE_DIRECTIVE.find(line).is_some()
                || VERSION_DIRECTIVE.find(line).is_some()
            {
                self.normal_line(line);
            }
        }
        // An explicit markup start followed by nothing but whitespace and a
        // blank line does not consume any indented text following.
        else if self.markup == Markup::Empty && line.is_empty() {
            self.normal_line(line);
        }
        // Indented lines following an explicit markup start are explicit markup.
        else if self.markup != Markup::None && (line.is_empty() || is_space(bytes[0])) {
            self.markup = Markup::Normal;
            // Record markup lines if the start line was recorded.
            if !self.markup_lines.is_empty() {
                self.markup_lines.push(line.to_owned());
            }
        }
        // A blank line following a paragraph ending in "::" starts a literal block.
        else if last_line_ended_in_colon_colon && line.is_empty() {
            self.markup = Markup::Normal;
            self.directive = Directive::LiteralBlock;
            self.markup_lines.push(String::new());
            self.output_line("", false);
        }
        // Print non-markup lines.
        else {
            self.normal_line(line);
            self.last_line_ended_in_colon_colon =
                bytes.len() >= 2 && bytes[bytes.len() - 2] == b':' && bytes[bytes.len() - 1] == b':';
        }
    }

    fn normal_line(&mut self, line: &str) {
        self.reset();
        self.output_line(line, true);
    }

    /// `cmRST::OutputLine`.
    fn output_line(&mut self, line_in: &str, inline_markup: bool) {
        if self.output_line_pending {
            self.out.push('\n');
            self.output_line_pending = false;
        }
        if !inline_markup {
            self.out.push_str(line_in);
            self.out.push('\n');
            return;
        }
        let line = self.replace_substitutions(line_in);
        let mut pos = 0;
        loop {
            let rest = &line[pos..];
            let role = CMAKE_ROLE.find(rest);
            let literal = INLINE_LITERAL.find(rest);
            let link = INLINE_LINK.find(rest);
            // The earliest match wins; ties keep role, then literal, then link.
            let mut first: Option<(usize, u8)> = role.as_ref().map(|m| (start(m, 0), 0));
            if let Some(m) = &literal {
                let at = start(m, 0);
                if first.is_none_or(|(best, _)| at < best) {
                    first = Some((at, 1));
                }
            }
            if let Some(m) = &link {
                let at = start(m, 0);
                if first.is_none_or(|(best, _)| at < best) {
                    first = Some((at, 2));
                }
            }
            match first {
                Some((at, 0)) => {
                    let m = role.expect("role match");
                    self.out.push_str(&rest[..at]);
                    let mut text = group(&m, rest, 3).to_owned();
                    // A command reference with no explicit target and no
                    // explicit "(...)" gets "()".
                    if group(&m, rest, 2) == "command"
                        && group(&m, rest, 5).is_empty()
                        && !text.contains(['(', ')'])
                    {
                        text.push_str("()");
                    }
                    self.out.push_str("``");
                    self.out.push_str(&text);
                    self.out.push_str("``");
                    pos += end(&m, 0);
                }
                Some((at, 1)) => {
                    let m = literal.expect("literal match");
                    self.out.push_str(&rest[..at]);
                    let text = group(&m, rest, 1).to_owned();
                    pos += end(&m, 0);
                    self.out.push_str("``");
                    self.out.push_str(&text);
                    self.out.push_str("``");
                }
                Some((at, _)) => {
                    let m = link.expect("link match");
                    self.out.push_str(&rest[..at]);
                    let text = group(&m, rest, 1);
                    let mut escaped = false;
                    for c in text.chars() {
                        if escaped {
                            escaped = false;
                            self.out.push(c);
                        } else if c == '\\' {
                            escaped = true;
                        } else {
                            self.out.push(c);
                        }
                    }
                    pos += end(&m, 0);
                }
                None => break,
            }
        }
        self.out.push_str(&line[pos..]);
        self.out.push('\n');
    }

    /// `cmRST::ReplaceSubstitutions`.
    fn replace_substitutions(&mut self, line: &str) -> String {
        let mut out = String::new();
        let mut pos = 0;
        while let Some(m) = SUBSTITUTION.find(&line[pos..]) {
            let rest = &line[pos..];
            let start2 = start(&m, 2);
            let end2 = end(&m, 2);
            let mut substitute = group(&m, rest, 3).to_owned();
            if let Some(replacement) = self.replace.get(&substitute).cloned()
                && self.replaced.insert(substitute.clone())
            {
                let key = substitute.clone();
                substitute = self.replace_substitutions(&replacement);
                self.replaced.remove(&key);
            }
            out.push_str(&rest[..start2]);
            out.push_str(&substitute);
            pos += end2;
        }
        out.push_str(&line[pos..]);
        out
    }

    /// `cmRST::OutputMarkupLines`.
    fn output_markup_lines(&mut self, inline_markup: bool) {
        let lines = std::mem::take(&mut self.markup_lines);
        for line in &lines {
            let line = if line.is_empty() { String::new() } else { format!(" {line}") };
            self.output_line(&line, inline_markup);
        }
        self.markup_lines = lines;
        self.output_line_pending = true;
    }

    /// `cmRST::ProcessInclude`.
    fn process_include(&mut self, file: String, kind: Include) -> bool {
        if self.include_depth >= 10 {
            return false;
        }
        let path = if file.starts_with('/') {
            format!("{}{file}", self.doc_root)
        } else {
            format!("{}/{file}", self.doc_dir)
        };
        let doc_root = self.doc_root.clone();
        let pending = self.output_line_pending;
        let depth = self.include_depth + 1;
        let replace = (kind != Include::TocTree).then(|| self.replace.clone());
        let (found, pending, replace) = {
            let mut nested = Rst::new(self.out, &doc_root);
            nested.include_depth = depth;
            nested.output_line_pending = pending;
            if let Some(replace) = replace {
                nested.replace = replace;
            }
            let found = nested.process_file(&path, kind == Include::Module);
            (found, nested.output_line_pending, nested.replace)
        };
        if kind != Include::TocTree {
            self.replace = replace;
        }
        self.output_line_pending = pending;
        found
    }

    /// `cmRST::ProcessDirectiveTocTree`.
    fn process_directive_toc_tree(&mut self) {
        let lines = self.markup_lines.clone();
        for line in lines {
            if !line.is_empty() && !line.starts_with(':') {
                match TOC_TREE_LINK.find(&line) {
                    Some(m) => {
                        let link = group(&m, &line, 1).to_owned();
                        self.process_include(format!("{link}.rst"), Include::TocTree);
                    }
                    None => {
                        self.process_include(format!("{line}.rst"), Include::TocTree);
                    }
                }
            }
        }
    }
}

/// `cmRST::UnindentLines`.
fn unindent_lines(lines: &mut Vec<String>) {
    // Remove the common indentation from the second and later lines.
    let mut indent_text = String::new();
    let mut indent_end = 0usize;
    let mut first = true;
    for line in lines.iter().skip(1) {
        if line.is_empty() {
            continue;
        }
        if first {
            first = false;
            indent_end = line.find(|c| c != ' ' && c != '\t').unwrap_or(line.len());
            indent_text = line[..indent_end].to_owned();
            continue;
        }
        indent_end = indent_end.min(line.len());
        let (a, b) = (line.as_bytes(), indent_text.as_bytes());
        for j in 0..indent_end {
            if a[j] != b[j] {
                indent_end = j;
                break;
            }
        }
    }
    for line in lines.iter_mut().skip(1) {
        if !line.is_empty() {
            *line = line[indent_end.min(line.len())..].to_owned();
        }
    }
    let leading = lines.iter().take_while(|line| line.is_empty()).count();
    let trailing = lines.iter().rev().take_while(|line| line.is_empty()).count();
    if leading + trailing >= lines.len() {
        // All lines are empty. The markup block is empty. Leave only one.
        lines.truncate(1);
        return;
    }
    // Move the leading empty lines after the content, then drop both.
    lines.truncate(lines.len() - trailing);
    lines.drain(..leading);
}

// ---------------------------------------------------------------------
// cmDocumentation

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HelpType {
    None,
    Version,
    Usage,
    Help,
    Full,
    ListManuals,
    ListCommands,
    ListModules,
    ListProperties,
    ListVariables,
    ListPolicies,
    OneArbitrary,
    OneManual,
    OneCommand,
    OneModule,
    OneProperty,
    OneVariable,
    OnePolicy,
    OldCustomModules,
}

#[derive(Clone, Debug)]
pub struct HelpItem {
    pub kind: HelpType,
    pub argument: String,
    pub filename: String,
}

/// `isOption`.
fn is_option(arg: &str) -> bool {
    arg.starts_with('-') || arg == "/V" || arg == "/?"
}

/// `cmSystemTools::HelpFileName`.
fn help_file_name(name: &str) -> String {
    name.replace(['<', '>'], "")
}

/// `cmDocumentation::WarnFormFromFilename`.
fn warn_form_from_filename(item: &mut HelpItem, result: &mut bool) {
    let ext = crate::util::filename_last_extension(&item.filename).to_ascii_uppercase();
    let message = if ext == ".HTM" || ext == ".HTML" {
        Some("Warning: HTML help format no longer supported")
    } else if ext == ".DOCBOOK" {
        Some("Warning: Docbook help format no longer supported")
    } else if ext.len() == 2 && (b'1'..=b'9').contains(&ext.as_bytes()[1]) {
        Some("Warning: Man help format no longer supported")
    } else {
        None
    };
    if let Some(message) = message {
        item.kind = HelpType::None;
        *result = true;
        eprintln!("{message}");
    }
}

/// `cmDocumentation::CheckOptions(argc, argv, "--")`: the requested help
/// items, or `None` when no help option was given. `args` excludes argv[0].
pub fn check_options(args: &[String]) -> Option<Vec<HelpItem>> {
    if args.is_empty() {
        return Some(vec![HelpItem { kind: HelpType::Usage, argument: String::new(), filename: String::new() }]);
    }
    let mut items = Vec::new();
    let mut result = false;
    let mut i = 0;
    let optional = |i: usize| args.get(i).filter(|arg| !is_option(arg)).cloned();
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return result.then_some(items);
        }
        let mut item = HelpItem { kind: HelpType::None, argument: String::new(), filename: String::new() };
        let take = |i: &mut usize, target: &mut String| {
            if let Some(value) = optional(*i + 1) {
                *target = value;
                *i += 1;
            }
        };
        let manual = |name: &str| (HelpType::OneManual, name.to_owned());
        match arg {
            "-help" | "--help" | "/?" | "-usage" | "-h" | "-H" => {
                item.kind = HelpType::Help;
                take(&mut i, &mut item.argument);
                if !item.argument.is_empty() {
                    item.kind = HelpType::OneArbitrary;
                    take(&mut i, &mut item.filename);
                }
            }
            "--help-properties" | "--help-policies" | "--help-variables" | "--help-modules" | "--help-commands" => {
                let (kind, argument) = manual(match arg {
                    "--help-properties" => "cmake-properties.7",
                    "--help-policies" => "cmake-policies.7",
                    "--help-variables" => "cmake-variables.7",
                    "--help-modules" => "cmake-modules.7",
                    _ => "cmake-commands.7",
                });
                item.kind = kind;
                item.argument = argument;
                take(&mut i, &mut item.filename);
                warn_form_from_filename(&mut item, &mut result);
            }
            "--help-custom-modules" => {
                take(&mut i, &mut item.filename);
                eprintln!("Warning: --help-custom-modules no longer supported");
                if item.filename.is_empty() {
                    return Some(items);
                }
                item.kind = HelpType::OldCustomModules;
                item.argument = crate::util::filename_name(&item.filename);
            }
            "--help-compatcommands" => {
                eprintln!("Warning: --help-compatcommands no longer supported");
                return Some(items);
            }
            "--help-full" => {
                item.kind = HelpType::Full;
                take(&mut i, &mut item.filename);
                warn_form_from_filename(&mut item, &mut result);
            }
            "--help-html" | "--help-man" => {
                eprintln!("Warning: {arg} no longer supported");
                return Some(items);
            }
            "--help-command" | "--help-module" | "--help-property" | "--help-policy" | "--help-variable"
            | "--help-manual" => {
                item.kind = match arg {
                    "--help-command" => HelpType::OneCommand,
                    "--help-module" => HelpType::OneModule,
                    "--help-property" => HelpType::OneProperty,
                    "--help-policy" => HelpType::OnePolicy,
                    "--help-variable" => HelpType::OneVariable,
                    _ => HelpType::OneManual,
                };
                take(&mut i, &mut item.argument);
                take(&mut i, &mut item.filename);
                if arg == "--help-command" {
                    item.argument = item.argument.to_ascii_lowercase();
                }
                warn_form_from_filename(&mut item, &mut result);
            }
            "--help-command-list" | "--help-module-list" | "--help-property-list" | "--help-variable-list"
            | "--help-policy-list" | "--help-manual-list" => {
                item.kind = match arg {
                    "--help-command-list" => HelpType::ListCommands,
                    "--help-module-list" => HelpType::ListModules,
                    "--help-property-list" => HelpType::ListProperties,
                    "--help-variable-list" => HelpType::ListVariables,
                    "--help-policy-list" => HelpType::ListPolicies,
                    _ => HelpType::ListManuals,
                };
                take(&mut i, &mut item.filename);
            }
            "--copyright" => {
                eprintln!("Warning: --copyright no longer supported");
                return Some(items);
            }
            "--version" | "-version" | "/V" => {
                item.kind = HelpType::Version;
                take(&mut i, &mut item.filename);
            }
            _ => {}
        }
        if item.kind != HelpType::None {
            result = true;
            items.push(item);
        }
        i += 1;
    }
    result.then_some(items)
}

/// kwsys `Glob::PatternToRegex` for one path component (no recursion).
fn component_matches(pattern: &str, name: &str) -> bool {
    fn matches(p: &[u8], n: &[u8]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some(b'*') => (0..=n.len()).any(|k| matches(&p[1..], &n[k..])),
            Some(b'?') => !n.is_empty() && matches(&p[1..], &n[1..]),
            Some(b'[') => {
                let Some(close) = p.iter().skip(2).position(|&b| b == b']').map(|i| i + 2) else {
                    return !n.is_empty() && n[0] == b'[' && matches(&p[1..], &n[1..]);
                };
                let Some(&c) = n.first() else {
                    return false;
                };
                let mut set = &p[1..close];
                let negate = matches!(set.first(), Some(b'!' | b'^'));
                if negate {
                    set = &set[1..];
                }
                let mut hit = false;
                let mut k = 0;
                while k < set.len() {
                    if k + 2 < set.len() && set[k + 1] == b'-' {
                        hit |= set[k] <= c && c <= set[k + 2];
                        k += 3;
                    } else {
                        hit |= set[k] == c;
                        k += 1;
                    }
                }
                hit != negate && matches(&p[close + 1..], &n[1..])
            }
            Some(&b) => !n.is_empty() && n[0] == b && matches(&p[1..], &n[1..]),
        }
    }
    matches(pattern.as_bytes(), name.as_bytes())
}

fn has_wildcard(component: &str) -> bool {
    component.contains(['*', '?', '['])
}

/// `cmDocumentation::GlobHelp`: files matching `${CMAKE_ROOT}/Help/<pattern>.rst`.
fn glob_help(root: &str, pattern: &str) -> Vec<String> {
    let full = format!("{root}/Help/{pattern}.rst");
    let components: Vec<&str> = full.split('/').collect();
    let mut paths = vec![String::new()];
    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == components.len();
        let mut next = Vec::new();
        for base in &paths {
            if !has_wildcard(component) {
                let path = if index == 0 { (*component).to_owned() } else { format!("{base}/{component}") };
                next.push(path);
                continue;
            }
            let dir = if base.is_empty() { "/".to_owned() } else { base.clone() };
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') && !component.starts_with('.') {
                    continue;
                }
                if component_matches(component, &name) {
                    next.push(format!("{base}/{name}"));
                }
            }
        }
        paths = next;
        if last {
            paths.retain(|path| std::path::Path::new(path).is_file());
        }
    }
    paths
}

pub struct Documentation {
    root: String,
}

impl Documentation {
    pub fn new(root: &str) -> Self {
        Self { root: root.to_owned() }
    }

    /// `cmDocumentation::PrintNames`.
    fn print_names(&self, out: &mut String, pattern: &str) {
        let mut names = Vec::new();
        for file in glob_help(&self.root, pattern) {
            let Ok(bytes) = std::fs::read(&file) else {
                continue;
            };
            for line in stream_lines(&String::from_utf8_lossy(&bytes)) {
                if line.as_bytes().first().is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'<') {
                    names.push(line);
                    break;
                }
            }
        }
        names.sort();
        for name in names {
            out.push_str(&name);
            out.push('\n');
        }
    }

    /// `cmDocumentation::PrintFiles`.
    fn print_files(&self, out: &mut String, pattern: &str) -> bool {
        let mut files = glob_help(&self.root, pattern);
        files.sort();
        let mut rst = Rst::new(out, &format!("{}/Help", self.root));
        let mut found = false;
        for file in files {
            found = rst.process_file(&file, false) || found;
        }
        found
    }

    /// `cmDocumentation::GeneralizeKeyword`.
    fn generalize_keyword(mut name: String) -> String {
        let conversions: [(&str, &[&str]); 2] = [
            ("CONFIG", &["DEBUG", "RELEASE", "RELWITHDEBINFO", "MINSIZEREL"]),
            (
                "LANG",
                &[
                    "C", "CXX", "CSharp", "CUDA", "OBJC", "OBJCXX", "Fortran", "HIP", "ISPC", "Swift", "ASM",
                    "ASM_NASM", "ASM_MARMASM", "ASM_MASM", "ASM-ATT",
                ],
            ),
        ];
        for (generic, specifics) in conversions {
            for specific in specifics {
                let reg = re(&format!("(^|_)({specific})(\\.|$|_)"));
                if let Some(m) = reg.find(&name) {
                    let at = start(&m, 2);
                    name.replace_range(at..at + specific.len(), generic);
                }
            }
        }
        name
    }

    /// Prints one help item; false when the request failed.
    pub fn print(&self, item: &HelpItem, out: &mut String) -> bool {
        let arg = &item.argument;
        match item.kind {
            HelpType::Full => self.print_files(out, "index"),
            HelpType::OneManual => {
                let mut name = arg.clone();
                let bytes = name.as_bytes();
                let len = bytes.len();
                if len > 3 && bytes[len - 3] == b'(' && bytes[len - 1] == b')' {
                    name = format!("{}.{}", &arg[..len - 3], &arg[len - 2..len - 1]);
                }
                if self.print_files(out, &format!("manual/{name}"))
                    || self.print_files(out, &format!("manual/{name}.[0-9]"))
                {
                    return true;
                }
                out.push_str(&format!(
                    "Argument \"{arg}\" to --help-manual is not an available manual.  Use --help-manual-list to see all available manuals.\n"
                ));
                false
            }
            HelpType::ListManuals => {
                self.print_names(out, "manual/*");
                true
            }
            HelpType::OneArbitrary => {
                let word = help_file_name(arg);
                let word_m = Self::generalize_keyword(word.clone());
                let mut found = self.print_files(out, &format!("*/{word}"));
                if found {
                    out.push('\n');
                }
                found = self.print_files(out, &format!("command/{}", word.to_ascii_lowercase())) || found;
                if found {
                    return true;
                }
                found = self.print_files(out, &format!("*/{word_m}"));
                if found {
                    out.push('\n');
                }
                found = self.print_files(out, &format!("command/{}", word_m.to_ascii_lowercase())) || found;
                if found {
                    return true;
                }
                out.push_str(&format!(
                    "Argument \"{arg}\" to --help did not match any keywords.  Use --help without any arguments to print CMake help information.\n"
                ));
                false
            }
            HelpType::OneCommand => {
                if self.print_files(out, &format!("command/{}", arg.to_ascii_lowercase())) {
                    return true;
                }
                out.push_str(&format!(
                    "Argument \"{arg}\" to --help-command is not a CMake command.  Use --help-command-list to see all commands.\n"
                ));
                false
            }
            HelpType::ListCommands => {
                self.print_names(out, "command/*");
                true
            }
            HelpType::OneModule => {
                if self.print_files(out, &format!("module/{arg}")) {
                    return true;
                }
                out.push_str(&format!("Argument \"{arg}\" to --help-module is not a CMake module.\n"));
                false
            }
            HelpType::ListModules => {
                let mut modules: Vec<String> = glob_help(&self.root, "module/*")
                    .iter()
                    .map(|file| {
                        let name = crate::util::filename_name(file);
                        name[..name.len() - 4].to_owned()
                    })
                    .collect();
                modules.sort();
                for module in modules {
                    out.push_str(&module);
                    out.push('\n');
                }
                true
            }
            HelpType::OneProperty => {
                if self.print_files(out, &format!("prop_*/{}", help_file_name(arg))) {
                    return true;
                }
                out.push_str(&format!(
                    "Argument \"{arg}\" to --help-property is not a CMake property.  Use --help-property-list to see all properties.\n"
                ));
                false
            }
            HelpType::ListProperties => {
                self.print_names(out, "prop_*/*");
                true
            }
            HelpType::OnePolicy => {
                if self.print_files(out, &format!("policy/{arg}")) {
                    return true;
                }
                out.push_str(&format!("Argument \"{arg}\" to --help-policy is not a CMake policy.\n"));
                false
            }
            HelpType::ListPolicies => {
                self.print_names(out, "policy/*");
                true
            }
            HelpType::OneVariable => {
                if self.print_files(out, &format!("variable/{}", help_file_name(arg))) {
                    return true;
                }
                out.push_str(&format!(
                    "Argument \"{arg}\" to --help-variable is not a defined variable.  Use --help-variable-list to see all defined variables.\n"
                ));
                false
            }
            HelpType::ListVariables => {
                self.print_names(out, "variable/*");
                true
            }
            HelpType::OldCustomModules => {
                let filename = arg;
                let ext = crate::util::filename_last_extension(filename).to_ascii_uppercase();
                let name = crate::util::filename_without_last_extension(filename);
                let summary = "cmake --help-custom-modules no longer supported\n";
                let detail = "CMake versions prior to 3.0 exposed their internal module help page\ngeneration functionality through the --help-custom-modules option.\nCMake versions 3.0 and above use other means to generate their module\nhelp pages so this functionality is no longer available to be exposed.\n\nThis file was generated as a placeholder to provide this information.\n";
                if ext == ".HTM" || ext == ".HTML" {
                    out.push_str(&format!("<html><title>{name}</title><body>\n{summary}<p/>\n{detail}</body></html>\n"));
                } else if ext.len() == 2 && (b'1'..=b'9').contains(&ext.as_bytes()[1]) {
                    // The date comes from cmSystemTools::GetCurrentDateTime("%B %d, %Y").
                    let date = crate::util::current_date_time("%B %d, %Y");
                    out.push_str(&format!(
                        ".TH {name} {} \"{date}\" \"cmake {}\"\n.SH NAME\n.PP\n{name} \\- {summary}\n.SH DESCRIPTION\n.PP\n{detail}",
                        &ext[1..2],
                        crate::interp::VERSION_STRING
                    ));
                } else {
                    out.push_str(&format!("{name}\n\n{summary}\n{detail}"));
                }
                true
            }
            HelpType::None | HelpType::Version | HelpType::Usage | HelpType::Help => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unindent() {
        let mut lines = vec!["".to_owned(), "".to_owned(), "   a".to_owned(), "     b".to_owned(), "".to_owned()];
        unindent_lines(&mut lines);
        assert_eq!(lines, vec!["a".to_owned(), "  b".to_owned()]);
    }

    #[test]
    fn wildcard() {
        assert!(component_matches("prop_*", "prop_tgt"));
        assert!(component_matches("cmake.[0-9].rst", "cmake.1.rst"));
        assert!(!component_matches("cmake.[0-9].rst", "cmake.x.rst"));
    }

    #[test]
    fn generalize() {
        assert_eq!(Documentation::generalize_keyword("CMAKE_CXX_IGNORE_EXTENSIONS".into()), "CMAKE_LANG_IGNORE_EXTENSIONS");
        assert_eq!(Documentation::generalize_keyword("CMAKE_C_FLAGS_DEBUG".into()), "CMAKE_LANG_FLAGS_CONFIG");
    }
}
