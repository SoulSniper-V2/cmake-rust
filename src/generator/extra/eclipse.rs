//! `cmExtraEclipseCDT4Generator`: `<binary>/.project`, `<binary>/.cproject`
//! and `<binary>/.settings/org.eclipse.core.resources.prefs` files.

use std::collections::BTreeSet;

use super::xml::XmlWriter;
use super::{Shared, file_is_dir, filename_name, filename_path, write_file};
use crate::cmregex::Regex;
use crate::generator::ctx::GenCtx;
use crate::generator::flags;
use crate::generator::ninja::GlobalTarget;
use crate::messages::MessageType;
use crate::state::{SourceGroup, Target, TargetKind};
use crate::util;

/// `cmExtraEclipseCDT4Generator::LinkType`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkType {
    VirtualFolder,
    LinkToFolder,
    LinkToFile,
}

/// `cmSystemTools::GetFilenamePath` of the generated file's parent dir is
/// created by `cmGeneratedFileStreamBase` (`cmSystemTools::MakeDirectory`).
fn write_generated_file(path: &str, content: &str) {
    let dir = filename_path(path);
    if !dir.is_empty() {
        let _ = std::fs::create_dir_all(dir);
    }
    write_file(path, content);
}

/// `cmExtraEclipseCDT4Generator::GetEclipsePath`.  On non-Cygwin platforms
/// this is the identity (`cygpath -m` conversion only exists for a Cygwin
/// cmake build, which this port does not target).
fn get_eclipse_path(path: &str) -> &str {
    path
}

/// `cmExtraEclipseCDT4Generator::GetPathBasename`.
fn get_path_basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed)
}

/// `cmExtraEclipseCDT4Generator::GenerateProjectName`:
/// `<name>-<type>@<path>` (`-<type>` omitted when `type` is empty).
fn generate_project_name(name: &str, kind: &str, path: &str) -> String {
    let mut out = String::from(name);
    if !kind.is_empty() {
        out.push('-');
    }
    out.push_str(kind);
    out.push('@');
    out.push_str(path);
    out
}

/// The file-scope `AppendAttribute` helper:
/// `<attribute key="k" value="k"/>`.
fn append_attribute(xml: &mut XmlWriter, keyval: &str) {
    xml.start_element("attribute");
    xml.attribute("key", keyval);
    xml.attribute("value", keyval);
    xml.end_element();
}

/// The file-scope `AppendDictionary` helper:
/// `<dictionary><key>k</key><value>v</value></dictionary>`.
fn append_dictionary(xml: &mut XmlWriter, key: &str, value: &str) {
    xml.start_element("dictionary");
    xml.element_value("key", key);
    xml.element_value("value", value);
    xml.end_element();
}

/// `cmExtraEclipseCDT4Generator::AppendLinkedResource`.
fn append_linked_resource(xml: &mut XmlWriter, name: &str, path: &str, link_type: LinkType) {
    let mut location_tag = "location";
    let mut type_tag = "2";
    if link_type == LinkType::VirtualFolder {
        // ... and not a linked folder
        location_tag = "locationURI";
    }
    if link_type == LinkType::LinkToFile {
        type_tag = "1";
    }
    xml.start_element("link");
    xml.element_value("name", name);
    xml.element_value("type", type_tag);
    xml.element_value(location_tag, path);
    xml.end_element();
}

/// `cmExtraEclipseCDT4Generator::AppendTarget`.
fn append_target(
    xml: &mut XmlWriter,
    target: &str,
    make: &str,
    make_args: &str,
    path: &str,
    prefix: &str,
    make_target: Option<&str>,
) {
    xml.start_element("target");
    xml.attribute("name", &format!("{prefix}{target}"));
    xml.attribute("path", path);
    xml.attribute("targetID", "org.eclipse.cdt.make.MakeTargetBuilder");
    xml.element_value("buildCommand", get_eclipse_path(make));
    xml.element_value("buildArguments", make_args);
    xml.element_value("buildTarget", make_target.unwrap_or(target));
    xml.element_value("stopOnError", "true");
    xml.element_value("useDefaultCommand", "false");
    xml.end_element();
}

/// `cmExtraEclipseCDT4Generator::AppendScannerProfile`.
#[allow(clippy::too_many_arguments)] // mirrors the upstream signature
fn append_scanner_profile(
    xml: &mut XmlWriter,
    profile_id: &str,
    open_action_enabled: bool,
    open_action_file_path: &str,
    p_parser_enabled: bool,
    scanner_info_provider_id: &str,
    run_action_arguments: &str,
    run_action_command: &str,
    run_action_use_default: bool,
    sip_parser_enabled: bool,
) {
    xml.start_element("profile");
    xml.attribute("id", profile_id);

    xml.start_element("buildOutputProvider");
    xml.start_element("openAction");
    xml.attribute(
        "enabled",
        if open_action_enabled { "true" } else { "false" },
    );
    xml.attribute("filePath", open_action_file_path);
    xml.end_element(); // openAction
    xml.start_element("parser");
    xml.attribute("enabled", if p_parser_enabled { "true" } else { "false" });
    xml.end_element(); // parser
    xml.end_element(); // buildOutputProvider

    xml.start_element("scannerInfoProvider");
    xml.attribute("id", scanner_info_provider_id);
    xml.start_element("runAction");
    xml.attribute("arguments", run_action_arguments);
    xml.attribute("command", run_action_command);
    xml.attribute(
        "useDefault",
        if run_action_use_default {
            "true"
        } else {
            "false"
        },
    );
    xml.end_element(); // runAction
    xml.start_element("parser");
    xml.attribute("enabled", if sip_parser_enabled { "true" } else { "false" });
    xml.end_element(); // parser
    xml.end_element(); // scannerInfoProvider

    xml.end_element(); // profile
}

/// `cmExtraEclipseCDT4Generator::AppendStorageScanners`.
fn append_storage_scanners(s: &Shared, xml: &mut XmlWriter) {
    // we need the "make" and the C (or C++) compiler which are used, Alex
    let make = s.var_safe(0, "CMAKE_MAKE_PROGRAM");
    let mut compiler = s.var_safe(0, "CMAKE_C_COMPILER");
    let mut arg1 = s.var_safe(0, "CMAKE_C_COMPILER_ARG1");
    if compiler.is_empty() {
        compiler = s.var_safe(0, "CMAKE_CXX_COMPILER");
        arg1 = s.var_safe(0, "CMAKE_CXX_COMPILER_ARG1");
    }
    if compiler.is_empty() {
        // Hmm, what to do now ?
        compiler = "gcc".to_owned();
    }

    // the following right now hardcodes gcc behavior :-/
    let mut compiler_args = "-E -P -v -dD ${plugin_state_location}/${specs_file}".to_owned();
    if !arg1.is_empty() {
        arg1.push(' ');
        compiler_args = arg1 + &compiler_args;
    }

    xml.start_element("storageModule");
    xml.attribute("moduleId", "scannerConfiguration");

    xml.start_element("autodiscovery");
    xml.attribute("enabled", "true");
    xml.attribute("problemReportingEnabled", "true");
    xml.attribute(
        "selectedProfileId",
        "org.eclipse.cdt.make.core.GCCStandardMakePerProjectProfile",
    );
    xml.end_element(); // autodiscovery

    append_scanner_profile(
        xml,
        "org.eclipse.cdt.make.core.GCCStandardMakePerProjectProfile",
        true,
        "",
        true,
        "specsFile",
        &compiler_args,
        &compiler,
        true,
        true,
    );
    append_scanner_profile(
        xml,
        "org.eclipse.cdt.make.core.GCCStandardMakePerFileProfile",
        true,
        "",
        true,
        "makefileGenerator",
        "-f ${project_name}_scd.mk",
        &make,
        true,
        true,
    );

    xml.end_element(); // storageModule
}

/// `cmExtraEclipseCDT4Generator::AppendIncludeDirectories`.
fn append_include_directories(
    xml: &mut XmlWriter,
    include_dirs: &[String],
    emitted_dirs: &mut BTreeSet<String>,
) {
    let framework_rx = Regex::new("(.+/Frameworks)/.+\\.framework/").ok();
    for inc in include_dirs {
        if inc.is_empty() {
            continue;
        }
        let mut dir = util::collapse_full_path(inc, "");

        // handle framework include dirs on OSX, the remainder after the
        // Frameworks/ part has to be stripped
        //   /System/Library/Frameworks/GLUT.framework/Headers
        if let Some(rx) = &framework_rx
            && let Some(m) = rx.find(&dir)
            && let Some((start, end)) = m.groups[1]
        {
            dir = dir[start..end].to_owned();
        }

        if emitted_dirs.insert(dir.clone()) {
            xml.start_element("pathentry");
            xml.attribute("include", get_eclipse_path(&dir));
            xml.attribute("kind", "inc");
            xml.attribute("path", "");
            xml.attribute("system", "true");
            xml.end_element();
        }
    }
}

/// `cmGeneratorExpression::Find`: `$<` followed by a `>`.
fn find_genex(text: &str) -> bool {
    text.find("$<").is_some_and(|pos| text[pos..].contains('>'))
}

/// `cmGeneratorExpression::Split`: split on `;` at generator-expression
/// nesting depth zero, dropping empty list elements like `cmExpandList`.
fn genex_split(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth: i64 = 0;
    let bytes = input.as_bytes();
    let mut last = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if depth == 0 && bytes[i] == b';' {
            if i > last {
                out.push(input[last..i].to_owned());
            }
            last = i + 1;
            i += 1;
            continue;
        }
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'<') {
            depth += 1;
            i += 2;
            continue;
        }
        if depth > 0 && bytes[i] == b'>' {
            depth -= 1;
        }
        i += 1;
    }
    if last < input.len() {
        out.push(input[last..].to_owned());
    }
    out
}

/// A `cmSourceGroup` node: name, `'\\'`-joined full name, optional regex,
/// explicit file list, children and the sources assigned at generate time.
#[derive(Clone)]
struct GroupNode {
    name: String,
    full_name: String,
    regex: Option<String>,
    files: BTreeSet<String>,
    children: Vec<GroupNode>,
    sources: Vec<String>,
}

impl GroupNode {
    fn new(name: &str, regex: Option<String>, parent_name: &str) -> GroupNode {
        let full_name = if parent_name.is_empty() {
            name.to_owned()
        } else {
            format!("{parent_name}\\{name}")
        };
        GroupNode {
            name: name.to_owned(),
            full_name,
            regex,
            files: BTreeSet::new(),
            children: Vec::new(),
            sources: Vec::new(),
        }
    }

    /// `cmSourceGroup::MatchesFiles` (explicit `GroupFiles` membership).
    fn matches_files(&self, name: &str) -> bool {
        self.files.contains(name)
    }

    /// `cmSourceGroup::MatchesRegex` (`GroupRegex` defaults to `"^$"`).
    fn matches_regex(&self, name: &str) -> bool {
        Regex::new(self.regex.as_deref().unwrap_or("^$"))
            .ok()
            .is_some_and(|rx| rx.find(name).is_some())
    }

    /// `cmSourceGroup::MatchChildrenFiles` (non-mut): any explicit file
    /// match in this subtree.
    fn contains_file(&self, name: &str) -> bool {
        self.matches_files(name) || self.children.iter().any(|g| g.contains_file(name))
    }

    /// `cmSourceGroup::MatchChildrenFiles` + `AssignSource`.
    fn assign_by_files(&mut self, name: &str) -> bool {
        if self.matches_files(name) {
            self.sources.push(name.to_owned());
            return true;
        }
        for group in &mut self.children {
            if group.assign_by_files(name) {
                return true;
            }
        }
        false
    }

    /// `cmSourceGroup::MatchChildrenRegex` + `AssignSource`.
    fn assign_by_regex(&mut self, name: &str) -> bool {
        for group in &mut self.children {
            if group.assign_by_regex(name) {
                return true;
            }
        }
        if self.matches_regex(name) {
            self.sources.push(name.to_owned());
            return true;
        }
        false
    }

    fn lookup_child(&self, name: &str) -> Option<usize> {
        self.children.iter().position(|g| g.name == name)
    }
}

/// `cmMakefile::GetSourceGroup` + the tree-building half of
/// `AddSourceGroup(nameVector, regex)`: walks the `'\\'`-separated path,
/// creating missing intermediate groups (which get no regex), then applies
/// the leaf's regex and merges its explicit file list.
fn insert_group(groups: &mut Vec<GroupNode>, name: &str, sg: &SourceGroup) {
    let parts: Vec<&str> = name.split('\\').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return;
    }
    // Find (or append) the top-level group.
    let mut index = groups
        .iter()
        .position(|g| g.name == parts[0])
        .unwrap_or_else(|| {
            groups.push(GroupNode::new(parts[0], None, ""));
            groups.len() - 1
        });
    let mut node = &mut groups[index];
    for part in &parts[1..] {
        index = node.lookup_child(part).unwrap_or_else(|| {
            let full = node.full_name.clone();
            node.children.push(GroupNode::new(part, None, &full));
            node.children.len() - 1
        });
        node = &mut node.children[index];
    }
    // The group already exists or was just created: `SetGroupRegex`
    // replaces the regular expression only when one was given (a group
    // defined without a regex keeps `nullptr`'s `"^$"`, which never matches
    // a nonempty file name); `AddGroupFile` unions the explicit files.
    if sg.regex.is_some() {
        node.regex = sg.regex.clone();
    }
    node.files.extend(sg.files.iter().cloned());
}

/// `cmMakefile::FindSourceGroup` + `cmSourceGroup::AssignSource`: first
/// search (reversed) for a group that lists the file explicitly, then
/// (reversed) for a group whose regex matches; fall back to the default
/// group.
fn assign_source(groups: &mut [GroupNode], source: &str) {
    for sg in groups.iter_mut().rev() {
        if sg.assign_by_files(source) {
            return;
        }
    }
    for sg in groups.iter_mut().rev() {
        if sg.assign_by_regex(source) {
            return;
        }
    }
    // Shouldn't get here, but just in case, use the default group.
    groups[0].sources.push(source.to_owned());
}

/// Mutable generator state: the non-static members of
/// `cmExtraEclipseCDT4Generator`.
struct Eclipse<'a, 'b> {
    s: &'b Shared<'a>,
    natures: BTreeSet<String>,
    home_directory: String,
    home_output_directory: String,
    is_out_of_source_build: bool,
    generate_source_project: bool,
    generate_linked_resources: bool,
    supports_virtual_folders: bool,
    supports_gmake_error_parser: bool,
    supports_macho64_parser: bool,
    c_enabled: bool,
    cxx_enabled: bool,
    src_linked_resources: Vec<String>,
}

impl<'a, 'b> Eclipse<'a, 'b> {
    fn new(s: &'b Shared<'a>) -> Eclipse<'a, 'b> {
        // `cmExtraEclipseCDT4Generator::EnableLanguage`.
        let mut natures = BTreeSet::new();
        let mut c_enabled = false;
        let mut cxx_enabled = false;
        for l in &s.ctx.interp.langs.enabled {
            match l.as_str() {
                "CXX" => {
                    natures.insert("org.eclipse.cdt.core.ccnature".to_owned());
                    natures.insert("org.eclipse.cdt.core.cnature".to_owned());
                    cxx_enabled = true;
                }
                "C" => {
                    natures.insert("org.eclipse.cdt.core.cnature".to_owned());
                    c_enabled = true;
                }
                "Java" => {
                    natures.insert("org.eclipse.jdt.core.javanature".to_owned());
                }
                _ => {}
            }
        }
        Eclipse {
            s,
            natures,
            home_directory: s.home_source().to_owned(),
            home_output_directory: s.home_binary().to_owned(),
            is_out_of_source_build: false,
            generate_source_project: false,
            generate_linked_resources: true,
            supports_virtual_folders: true,
            supports_gmake_error_parser: true,
            supports_macho64_parser: true,
            c_enabled,
            cxx_enabled,
            src_linked_resources: Vec::new(),
        }
    }

    /// `cmExtraEclipseCDT4Generator::Generate`.
    fn generate(&mut self) {
        let s = self.s;
        let eclipse_version = s.var_safe(0, "CMAKE_ECLIPSE_VERSION");
        if let Ok(regex) = Regex::new(".*([0-9]+\\.[0-9]+).*")
            && let Some(m) = regex.find(&eclipse_version)
            && let Some((start, end)) = m.groups[1]
        {
            // sscanf("%u.%u") on the digit-dot-digit regex match.
            let matched = &eclipse_version[start..end];
            if let Some((major, minor)) = matched
                .split_once('.')
                .and_then(|(a, b)| a.parse::<u64>().ok().zip(b.parse::<u64>().ok()))
            {
                let version = major * 1000 + minor;
                if version < 3006 {
                    // 3.6 is Helios
                    self.supports_virtual_folders = false;
                    self.supports_macho64_parser = false;
                }
                if version < 3007 {
                    // 3.7 is Indigo
                    self.supports_gmake_error_parser = false;
                }
            }
        }

        self.generate_linked_resources = s
            .var(0, "CMAKE_ECLIPSE_GENERATE_LINKED_RESOURCES")
            .is_some_and(|v| util::is_on(&v));

        self.is_out_of_source_build = self.home_directory != self.home_output_directory;

        self.generate_source_project = self.is_out_of_source_build
            && s.var(0, "CMAKE_ECLIPSE_GENERATE_SOURCE_PROJECT")
                .is_some_and(|v| util::is_on(&v));

        if !self.generate_source_project
            && s.var(0, "ECLIPSE_CDT4_GENERATE_SOURCE_PROJECT")
                .is_some_and(|v| util::is_on(&v))
        {
            s.ctx.message(
                MessageType::Warning,
                s.ctx.dir_backtrace(0),
                "ECLIPSE_CDT4_GENERATE_SOURCE_PROJECT is set to TRUE, \
                 but this variable is not supported anymore since CMake 2.8.7.\n\
                 Enable CMAKE_ECLIPSE_GENERATE_SOURCE_PROJECT instead.",
            );
        }

        if util::is_subdirectory(&self.home_output_directory, &self.home_directory) {
            s.ctx.message(
                MessageType::Warning,
                s.ctx.dir_backtrace(0),
                "The build directory is a subdirectory of the source directory.\n\
                 This is not supported well by Eclipse. It is strongly recommended \
                 to use a build directory which is a sibling of the source directory.",
            );
        }

        // NOTE: This is not good, since it pollutes the source tree. However,
        //       Eclipse doesn't allow CVS/SVN to work when the .project is not in
        //       the cvs/svn root directory. Hence, this is provided as an option.
        if self.generate_source_project {
            // create .project file in the source tree
            self.create_source_project_file();
        }

        // create a .project file
        self.create_project_file();

        // create a .cproject file
        self.create_cproject_file();

        // create resource settings
        self.create_settings_resource_prefs_file();
    }

    /// `cmExtraEclipseCDT4Generator::CreateSettingsResourcePrefsFile`.
    fn create_settings_resource_prefs_file(&self) {
        let s = self.s;
        let filename = format!(
            "{}/.settings/org.eclipse.core.resources.prefs",
            self.home_output_directory
        );
        let mut out = String::from("eclipse.preferences.version=1\n");
        if let Some(encoding) = s.var(0, "CMAKE_ECLIPSE_RESOURCE_ENCODING") {
            out.push_str("encoding/<project>=");
            out.push_str(&encoding);
            out.push('\n');
        }
        write_generated_file(&filename, &out);
    }

    /// `cmExtraEclipseCDT4Generator::CreateSourceProjectFile`.
    fn create_source_project_file(&mut self) {
        let s = self.s;

        // set up the project name: <project>-Source@<baseSourcePathName>
        let name = generate_project_name(
            &s.project_name(0),
            "Source",
            get_path_basename(&self.home_directory),
        );

        let filename = format!("{}/.project", self.home_directory);

        let mut xml = XmlWriter::new();
        xml.start_document("UTF-8");
        xml.start_element("projectDescription");
        xml.element_value("name", &name);
        xml.element_value("comment", "");
        xml.element_value("projects", "");
        xml.element_value("buildSpec", "");
        xml.element_value("natures", "");
        xml.start_element("linkedResources");

        if self.supports_virtual_folders {
            let base = self.home_directory.clone();
            self.create_links_to_subprojects(&mut xml, &base);
            self.src_linked_resources.clear();
        }

        xml.end_element(); // linkedResources
        xml.end_element(); // projectDescription
        xml.end_document();

        write_generated_file(&filename, &xml.out);
    }

    /// `cmExtraEclipseCDT4Generator::AddEnvVar`.  The upstream version
    /// additionally writes a `CMAKE_ECLIPSE_ENVVAR_<name>` cache entry and
    /// saves the cache; that cache mutation is not possible here (the
    /// generation context is read-only), so only the value selection is
    /// mirrored.
    fn add_env_var(&self, out: &mut String, env_var: &str) {
        let s = self.s;
        let env_var_value = std::env::var_os(env_var).map(|v| v.to_string_lossy().into_owned());
        let cache_value = s
            .ctx
            .interp
            .cache
            .value(&format!("CMAKE_ECLIPSE_ENVVAR_{env_var}"))
            .map(str::to_owned);

        // now we have both, decide which one to use
        let value_to_use = match (env_var_value, cache_value) {
            (None, None) => String::new(),
            (Some(env), None) => env,
            (None, Some(cache)) => cache,
            (Some(env), Some(cache)) => {
                // It is both in the cache and in the env.
                // Use the version from the env. except if the value from the
                // env is completely contained in the value from the cache.
                if cache.contains(&env) { cache } else { env }
            }
        };

        if !value_to_use.is_empty() {
            out.push_str(env_var);
            out.push('=');
            out.push_str(&value_to_use);
            out.push('|');
        }
    }

    /// `cmExtraEclipseCDT4Generator::CreateProjectFile`.
    fn create_project_file(&mut self) {
        let s = self.s;
        let filename = format!("{}/.project", self.home_output_directory);

        let mut compiler_id = s.var_safe(0, "CMAKE_C_COMPILER_ID");
        if compiler_id.is_empty() {
            // no C compiler, try the C++ compiler:
            compiler_id = s.var_safe(0, "CMAKE_CXX_COMPILER_ID");
        }

        let mut xml = XmlWriter::new();
        xml.start_document("UTF-8");
        xml.start_element("projectDescription");

        xml.element_value(
            "name",
            &generate_project_name(
                &s.project_name(0),
                &s.build_type(0),
                get_path_basename(&self.home_output_directory),
            ),
        );

        xml.element_value("comment", "");
        xml.element_value("projects", "");

        xml.start_element("buildSpec");
        xml.start_element("buildCommand");
        xml.element_value("name", "org.eclipse.cdt.make.core.makeBuilder");
        xml.element_value("triggers", "clean,full,incremental,");
        xml.start_element("arguments");

        // use clean target
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.cleanBuildTarget",
            "clean",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.enableCleanBuild",
            "true",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.append_environment",
            "true",
        );
        append_dictionary(&mut xml, "org.eclipse.cdt.make.core.stopOnError", "true");

        // set the make command
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.enabledIncrementalBuild",
            "true",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.build.command",
            get_eclipse_path(&s.var_safe(0, "CMAKE_MAKE_PROGRAM")),
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.contents",
            "org.eclipse.cdt.make.core.activeConfigSettings",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.build.target.inc",
            "all",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.build.arguments",
            &s.var_safe(0, "CMAKE_ECLIPSE_MAKE_ARGUMENTS"),
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.buildLocation",
            get_eclipse_path(&self.home_output_directory),
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.useDefaultBuildCmd",
            "false",
        );

        // set project specific environment
        let mut environment = String::from("VERBOSE=1|CMAKE_NO_VERBOSE=1|"); // verbose Makefile output
        // set vsvars32.bat environment available at CMake time,
        //   but not necessarily when eclipse is open
        if compiler_id == "MSVC" {
            self.add_env_var(&mut environment, "PATH");
            self.add_env_var(&mut environment, "INCLUDE");
            self.add_env_var(&mut environment, "LIB");
            self.add_env_var(&mut environment, "LIBPATH");
        } else if compiler_id == "Intel" {
            // if the env.var is set, use this one and put it in the cache
            // if the env.var is not set, but the value is in the cache,
            // use it from the cache:
            self.add_env_var(&mut environment, "INTEL_LICENSE_FILE");
        }
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.environment",
            &environment,
        );

        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.enableFullBuild",
            "true",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.build.target.auto",
            "all",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.enableAutoBuild",
            "false",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.build.target.clean",
            "clean",
        );
        append_dictionary(&mut xml, "org.eclipse.cdt.make.core.fullBuildTarget", "all");
        append_dictionary(&mut xml, "org.eclipse.cdt.make.core.buildArguments", "");
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.make.core.build.location",
            get_eclipse_path(&self.home_output_directory),
        );
        append_dictionary(&mut xml, "org.eclipse.cdt.make.core.autoBuildTarget", "all");

        // set error parsers
        let mut error_output_parser = String::new();
        if compiler_id == "MSVC" {
            error_output_parser.push_str("org.eclipse.cdt.core.VCErrorParser;");
        } else if compiler_id == "Intel" {
            error_output_parser.push_str("org.eclipse.cdt.core.ICCErrorParser;");
        }

        if self.supports_gmake_error_parser {
            error_output_parser.push_str("org.eclipse.cdt.core.GmakeErrorParser;");
        } else {
            error_output_parser.push_str("org.eclipse.cdt.core.MakeErrorParser;");
        }

        error_output_parser.push_str(
            "org.eclipse.cdt.core.GCCErrorParser;\
             org.eclipse.cdt.core.GASErrorParser;\
             org.eclipse.cdt.core.GLDErrorParser;",
        );
        append_dictionary(
            &mut xml,
            "org.eclipse.cdt.core.errorOutputParser",
            &error_output_parser,
        );

        xml.end_element(); // arguments
        xml.end_element(); // buildCommand
        xml.start_element("buildCommand");
        xml.element_value("name", "org.eclipse.cdt.make.core.ScannerConfigBuilder");
        xml.start_element("arguments");
        xml.end_element(); // arguments
        xml.end_element(); // buildCommand
        xml.end_element(); // buildSpec

        // set natures for c/c++ projects
        xml.start_element("natures");
        xml.element_value("nature", "org.eclipse.cdt.make.core.makeNature");
        xml.element_value("nature", "org.eclipse.cdt.make.core.ScannerConfigNature");

        for n in &self.natures {
            xml.element_value("nature", n);
        }

        if let Some(extra_natures_prop) = s.ctx.interp.global_props.get("ECLIPSE_EXTRA_NATURES") {
            for n in util::expand_list(extra_natures_prop, false) {
                xml.element_value("nature", &n);
            }
        }

        xml.end_element(); // natures

        xml.start_element("linkedResources");
        // create linked resources
        if self.is_out_of_source_build {
            // create a linked resource to CMAKE_SOURCE_DIR
            // (this is not done anymore for each project because of
            // https://gitlab.kitware.com/cmake/cmake/-/issues/9978 and because I
            // found it actually quite confusing in bigger projects with many
            // directories and projects, Alex)

            let source_linked_resource_name = "[Source directory]";
            let link_source_directory = get_eclipse_path(s.source_dir(0)).to_owned();
            // .project dir can't be subdir of a linked resource dir
            if !util::is_subdirectory(&self.home_output_directory, &link_source_directory) {
                append_linked_resource(
                    &mut xml,
                    source_linked_resource_name,
                    get_eclipse_path(&link_source_directory),
                    LinkType::LinkToFolder,
                );
                self.src_linked_resources
                    .push(source_linked_resource_name.to_owned());
            }
        }

        if self.supports_virtual_folders {
            let base = self.home_output_directory.clone();
            self.create_links_to_subprojects(&mut xml, &base);
            self.create_links_for_targets(&mut xml);
        }

        xml.end_element(); // linkedResources
        xml.end_element(); // projectDescription

        write_generated_file(&filename, &xml.out);
    }

    /// `cmExtraEclipseCDT4Generator::WriteGroups`.
    fn write_groups(&self, source_groups: &[GroupNode], link_name: &str, xml: &mut XmlWriter) {
        for sg in source_groups {
            let link_name3 = format!("{link_name}/{}", sg.full_name).replace('\\', "/");

            append_linked_resource(
                xml,
                &link_name3,
                "virtual:/virtual",
                LinkType::VirtualFolder,
            );
            if !sg.children.is_empty() {
                self.write_groups(&sg.children, link_name, xml);
            }
            for file in &sg.sources {
                if !file_is_dir(file) {
                    let link_name4 = format!("{link_name3}/{}", filename_name(file));
                    append_linked_resource(
                        xml,
                        &link_name4,
                        get_eclipse_path(file),
                        LinkType::LinkToFile,
                    );
                }
            }
        }
    }

    /// `cmMakefile::GetSourceGroups` for `dir`: the default groups followed
    /// by the directory's `source_group()`-created groups, plus the
    /// `$<TARGET_OBJECTS:...>` files `cmMakefile::AddTargetObject` files in
    /// the reserved "Object Libraries" group.
    ///
    /// User groups follow in creation order (`SourceGroup::seq`).
    fn build_source_groups(&self, dir: usize) -> Vec<GroupNode> {
        // cmMakefile's default source groups, in constructor order.
        const DEFAULTS: &[(&str, &str)] = &[
            ("", "^.*$"),
            (
                "Source Files",
                "\\.(C|F|M|c|c\\+\\+|cc|cpp|mpp|cxx|ixx|cppm|ccm|cxxm|c\\+\\+m|cu|f|f90|for|fpp|ftn|m|mm|rc|def|r|odl|idl|hpj|bat)$",
            ),
            ("Header Files", "\\.(h|hh|h\\+\\+|hm|hpp|hxx|in|txx|inl)$"),
            ("Precompile Header File", "cmake_pch(_[^.]+)?\\.(h|hxx)$"),
            ("CMake Rules", "\\.rule$"),
            (
                "Resources",
                "\\.(pdf|plist|png|jpeg|jpg|storyboard|xcassets)$",
            ),
            ("Object Files", "\\.(lo|o|obj)$"),
            ("Object Libraries", "^MATCH_NO_SOURCES$"),
        ];
        let mut groups: Vec<GroupNode> = DEFAULTS
            .iter()
            .map(|(name, regex)| GroupNode::new(name, Some((*regex).to_owned()), ""))
            .collect();

        if let Some(user) = self.s.ctx.interp.ext.source_groups.get(&dir) {
            let mut user: Vec<_> = user.iter().collect();
            user.sort_by_key(|(_, sg)| sg.seq);
            for (name, sg) in user {
                insert_group(&mut groups, name, sg);
            }
        }

        groups
    }

    /// `cmMakefile::AddTargetObject`: `$<TARGET_OBJECTS:...>` object files
    /// referenced from `dir` are filed in the reserved "Object Libraries"
    /// group.  Called after each target's group copy is taken, mirroring
    /// the upstream timing where the calls happen inside `GetSourceFiles`
    /// (so a target's own objects are grouped by regex while later targets
    /// see the explicit "Object Libraries" file claims).
    fn seed_object_libraries(&self, dir: usize, groups: &mut [GroupNode]) {
        let objects: Vec<String> = self
            .s
            .ctx
            .target_objects
            .borrow()
            .iter()
            .filter(|((odir, _), _)| *odir == dir)
            .map(|((_, path), _)| path.clone())
            .collect();
        if !objects.is_empty()
            && let Some(group) = groups.iter_mut().find(|g| g.name == "Object Libraries")
        {
            group.files.extend(objects);
        }
    }

    /// The `HEADERS` file-set rule of `AddFileSetEntries`
    /// (cmGeneratorTarget_Sources.cxx): file-set header files not already
    /// claimed by an explicit group file list are added to "Header Files".
    /// Mutates the shared per-directory groups just like the upstream
    /// makefile's `SourceGroups`.
    fn seed_header_file_sets(&self, target: &Target, groups: &mut [GroupNode]) {
        for set in target.file_sets.values() {
            if set.kind != "HEADERS" {
                continue;
            }
            // `eval_file_set_dirs`/`eval_file_set_files` (codemodel).
            let mut dirs = Vec::new();
            for entry in &set.base_dirs {
                for dir in util::expand_list(&self.s.ctx.genex(&entry.0, Some(target), None), false)
                {
                    if !dir.is_empty() {
                        dirs.push(dir);
                    }
                }
            }
            let mut paths = Vec::new();
            for entry in &set.files {
                for file in
                    util::expand_list(&self.s.ctx.genex(&entry.0, Some(target), None), false)
                {
                    if file.is_empty() {
                        continue;
                    }
                    if util::is_full_path(&file) {
                        paths.push(file);
                        continue;
                    }
                    for dir in &dirs {
                        paths.push(format!("{dir}/{file}"));
                    }
                }
            }
            for path in paths {
                if groups.iter().all(|g| !g.contains_file(&path))
                    && let Some(headers) = groups.iter_mut().find(|g| g.name == "Header Files")
                {
                    headers.files.insert(path);
                }
            }
        }
    }

    /// `cmExtraEclipseCDT4Generator::CreateLinksForTargets`.
    fn create_links_for_targets(&self, xml: &mut XmlWriter) {
        let s = self.s;
        let link_name = "[Targets]";
        append_linked_resource(xml, link_name, "virtual:/virtual", LinkType::VirtualFolder);

        // `makefile->GetSourceGroups()` accumulates per directory (file-set
        // and object-library seeding persists across targets), while the
        // source assignment copy is fresh for each target.
        let mut dir_groups: Vec<Option<Vec<GroupNode>>> =
            (0..s.makefile_count()).map(|_| None).collect();

        // By the time the extra generator runs, the main generator has
        // already computed every target's sources, so every
        // cmMakefile::AddTargetObject claim is in place before any target's
        // group copy is taken.
        for dir in 0..dir_groups.len() {
            let build_type = s.build_type(dir);
            for target in s.targets(dir) {
                let _ = s.target_sources(target, &build_type);
            }
        }
        for (dir, groups_opt) in dir_groups.iter_mut().enumerate() {
            if groups_opt.is_none() {
                let mut groups = self.build_source_groups(dir);
                self.seed_object_libraries(dir, &mut groups);
                *groups_opt = Some(groups);
            }
            for target in s.targets(dir) {
                let prefix = match target.kind {
                    TargetKind::Executable => "[exe] ",
                    TargetKind::StaticLibrary
                    | TargetKind::SharedLibrary
                    | TargetKind::ModuleLibrary
                    | TargetKind::ObjectLibrary => "[lib] ",
                    // ignore all others:
                    _ => continue,
                };
                let link_name2 = format!("{link_name}/{prefix}{}", target.name);
                append_linked_resource(
                    xml,
                    &link_name2,
                    "virtual:/virtual",
                    LinkType::VirtualFolder,
                );
                if !self.generate_linked_resources {
                    continue; // skip generating the linked resources to the source files
                }
                let groups_template =
                    groups_opt.get_or_insert_with(|| self.build_source_groups(dir));
                // Upstream copies `makefile->GetSourceGroups()` *before*
                // `GetSourceFiles` seeds this target's file-set headers into
                // the makefile's real groups: that seeding only affects
                // *later* targets' copies.
                let mut groups = groups_template.clone();
                self.seed_header_file_sets(target, groups_template);

                // get the files from the source lists then add them to the groups
                let build_type = s.build_type(dir);
                for sf in s.target_sources(target, &build_type).iter() {
                    // Add the file to the list of sources.
                    assign_source(&mut groups, &sf.path);
                }

                self.write_groups(&groups, &link_name2, xml);
            }
        }
    }

    /// `cmExtraEclipseCDT4Generator::CreateLinksToSubprojects`.
    fn create_links_to_subprojects(&self, xml: &mut XmlWriter, base_dir: &str) {
        let s = self.s;
        if !self.generate_linked_resources {
            return;
        }

        // for each sub project create a linked resource to the source dir
        // - only if it is an out-of-source build
        append_linked_resource(
            xml,
            "[Subprojects]",
            "virtual:/virtual",
            LinkType::VirtualFolder,
        );

        for (name, dirs) in s.project_map() {
            let link_source_directory = get_eclipse_path(s.source_dir(dirs[0])).to_owned();
            // a linked resource must not point to a parent directory of
            // .project or .project itself
            if base_dir != link_source_directory
                && !util::is_subdirectory(base_dir, &link_source_directory)
            {
                let link_name = format!("[Subprojects]/{name}");
                append_linked_resource(
                    xml,
                    &link_name,
                    get_eclipse_path(&link_source_directory),
                    LinkType::LinkToFolder,
                );
                // Don't add it to the srcLinkedResources, because listing
                // multiple directories confuses the Eclipse indexer (#13596).
            }
        }
    }

    /// `cmExtraEclipseCDT4Generator::CreateCProjectFile`.
    fn create_cproject_file(&self) {
        let s = self.s;
        let mut emitted: BTreeSet<String> = BTreeSet::new();

        let filename = format!("{}/.cproject", self.home_output_directory);

        let mut xml = XmlWriter::new();

        // add header
        xml.start_document("UTF-8");
        xml.processing_instruction("fileVersion", "4.0.0");
        xml.start_element("cproject");
        xml.start_element("storageModule");
        xml.attribute("moduleId", "org.eclipse.cdt.core.settings");

        xml.start_element("cconfiguration"); // noqa: spellcheck disable-line
        xml.attribute("id", "org.eclipse.cdt.core.default.config.1");

        // Configuration settings...
        xml.start_element("storageModule");
        xml.attribute(
            "buildSystemId",
            "org.eclipse.cdt.core.defaultConfigDataProvider",
        );
        xml.attribute("id", "org.eclipse.cdt.core.default.config.1");
        xml.attribute("moduleId", "org.eclipse.cdt.core.settings");
        xml.attribute("name", "Configuration");
        xml.element("externalSettings");
        xml.start_element("extensions");

        // TODO: refactor this out...
        let executable_format = s.var_safe(0, "CMAKE_EXECUTABLE_FORMAT");
        if executable_format == "ELF" {
            xml.start_element("extension");
            xml.attribute("id", "org.eclipse.cdt.core.ELF");
            xml.attribute("point", "org.eclipse.cdt.core.BinaryParser");
            xml.end_element(); // extension

            xml.start_element("extension");
            xml.attribute("id", "org.eclipse.cdt.core.GNU_ELF");
            xml.attribute("point", "org.eclipse.cdt.core.BinaryParser");
            append_attribute(&mut xml, "addr2line");
            append_attribute(&mut xml, "c++filt");
            xml.end_element(); // extension
        } else {
            let system_name = s.var_safe(0, "CMAKE_SYSTEM_NAME");
            if system_name == "CYGWIN" || system_name == "MSYS" {
                xml.start_element("extension");
                xml.attribute("id", "org.eclipse.cdt.core.Cygwin_PE");
                xml.attribute("point", "org.eclipse.cdt.core.BinaryParser");
                append_attribute(&mut xml, "addr2line");
                append_attribute(&mut xml, "c++filt");
                append_attribute(&mut xml, "cygpath");
                append_attribute(&mut xml, "nm");
                xml.end_element(); // extension
            } else if system_name == "Windows" {
                xml.start_element("extension");
                xml.attribute("id", "org.eclipse.cdt.core.PE");
                xml.attribute("point", "org.eclipse.cdt.core.BinaryParser");
                xml.end_element(); // extension
            } else if system_name == "Darwin" {
                xml.start_element("extension");
                xml.attribute(
                    "id",
                    if self.supports_macho64_parser {
                        "org.eclipse.cdt.core.MachO64"
                    } else {
                        "org.eclipse.cdt.core.MachO"
                    },
                );
                xml.attribute("point", "org.eclipse.cdt.core.BinaryParser");
                append_attribute(&mut xml, "c++filt");
                xml.end_element(); // extension
            } else {
                // *** Should never get here ***
                xml.element("error_toolchain_type");
            }
        }

        xml.end_element(); // extensions
        xml.end_element(); // storageModule

        // ???
        xml.start_element("storageModule");
        xml.attribute("moduleId", "org.eclipse.cdt.core.language.mapping");
        xml.element("project-mappings");
        xml.end_element(); // storageModule

        // ???
        xml.start_element("storageModule");
        xml.attribute("moduleId", "org.eclipse.cdt.core.externalSettings");
        xml.end_element(); // storageModule

        // set the path entries (includes, libs, source dirs, etc.)
        xml.start_element("storageModule");
        xml.attribute("moduleId", "org.eclipse.cdt.core.pathentry");

        // for each sub project with a linked resource to the source dir:
        // - make it type 'src'
        // - and exclude it from type 'out'
        let mut exclude_from_out = String::new();
        /* I don't know what the pathentry kind="src" are good for, e.g.
         * autocompletion
         * works also without them. Done wrong, the indexer complains, see
         * #12417 and #12213.
         * According to #13596, this entry at least limits the directories the
         * indexer is searching for files. So now the "src" entry contains only
         * the linked resource to CMAKE_SOURCE_DIR. Alex */
        // include subprojects directory to the src pathentry
        // eclipse cdt indexer uses this entries as reference to index source
        // files
        if self.generate_linked_resources {
            xml.start_element("pathentry");
            xml.attribute("kind", "src");
            xml.attribute("path", "[Subprojects]");
            xml.end_element();
        }

        for p in &self.src_linked_resources {
            xml.start_element("pathentry");
            xml.attribute("kind", "src");
            xml.attribute("path", p);
            xml.end_element();

            // exclude source directory from output search path
            // - only if not named the same as an output directory
            if !file_is_dir(&format!("{}/{p}", self.home_output_directory)) {
                exclude_from_out.push_str(p);
                exclude_from_out.push_str("/|");
            }
        }

        exclude_from_out.push_str("**/CMakeFiles/");

        xml.start_element("pathentry");
        xml.attribute("excluding", &exclude_from_out);
        xml.attribute("kind", "out");
        xml.attribute("path", "");
        xml.end_element();

        // add pre-processor definitions to allow eclipse to gray out sections
        emitted.clear();
        for dir in 0..s.makefile_count() {
            if let Some(cdefs) = s.prop(dir, "COMPILE_DEFINITIONS") {
                // Expand the list.
                for d in genex_split(&cdefs) {
                    if find_genex(&d) {
                        continue;
                    }

                    let (def, val) = match d.find('=') {
                        // we have -DFOO=BAR
                        Some(equals) => (&d[..equals], &d[equals + 1..]),
                        // we have -DFOO
                        None => (d.as_str(), ""),
                    };

                    // insert the definition if not already added.
                    if emitted.insert(def.to_owned()) {
                        xml.start_element("pathentry");
                        xml.attribute("kind", "mac");
                        xml.attribute("name", def);
                        xml.attribute("path", "");
                        xml.attribute("value", val);
                        xml.end_element();
                    }
                }
            }
        }
        // add system defined c and c++ macros
        for (enabled, var) in [
            (
                self.c_enabled,
                "CMAKE_EXTRA_GENERATOR_C_SYSTEM_DEFINED_MACROS",
            ),
            (
                self.cxx_enabled,
                "CMAKE_EXTRA_GENERATOR_CXX_SYSTEM_DEFINED_MACROS",
            ),
        ] {
            if !enabled {
                continue;
            }
            if let Some(defs) = s.var(0, var) {
                // Expand the list.
                let defs = util::expand_list(&defs, true);

                // the list must contain only definition-value pairs:
                if defs.len().is_multiple_of(2) {
                    for pair in defs.chunks(2) {
                        let def = &pair[0];
                        let val = &pair[1];

                        // insert the definition if not already added.
                        if emitted.insert(def.clone()) {
                            xml.start_element("pathentry");
                            xml.attribute("kind", "mac");
                            xml.attribute("name", def);
                            xml.attribute("path", "");
                            xml.attribute("value", val);
                            xml.end_element();
                        }
                    }
                }
            }
        }

        // include dirs
        emitted.clear();
        for dir in 0..s.makefile_count() {
            // The directory's global targets (`install`, `edit_cache`, ...)
            // follow its own targets: `cmGlobalGenerator::CreateGlobalTarget`
            // makes them Normal targets at the end of configure, so they
            // carry the directory's INCLUDE_DIRECTORIES and the
            // CMAKE_INCLUDE_CURRENT_DIR entries too.
            let globals: Vec<Target> = s
                .global_target_names(dir)
                .into_iter()
                .map(|name| global_target(s, dir, name))
                .collect();
            let targets = s.targets(dir).into_iter().chain(globals.iter());
            for target in targets {
                if target.kind == TargetKind::InterfaceLibrary {
                    continue;
                }
                // `cmLocalGenerator::GetIncludeDirectories(target, "C", config)`
                // with `mf` == the top-level makefile's config.
                let config = s.build_type(0);
                let include_dirs: Vec<String> =
                    flags::include_items_stripped(s.ctx, target, &config, "C")
                        .into_iter()
                        .map(|(path, _, _)| path)
                        .collect();
                append_include_directories(&mut xml, &include_dirs, &mut emitted);
            }
        }
        // now also the system include directories, in case we found them in
        // CMakeSystemSpecificInformation.cmake. This makes Eclipse find the
        // standard headers.
        let compiler = s.var_safe(0, "CMAKE_C_COMPILER");
        if self.c_enabled && !compiler.is_empty() {
            let dirs = util::expand_list(
                &s.var_safe(0, "CMAKE_EXTRA_GENERATOR_C_SYSTEM_INCLUDE_DIRS"),
                false,
            );
            append_include_directories(&mut xml, &dirs, &mut emitted);
        }
        let compiler = s.var_safe(0, "CMAKE_CXX_COMPILER");
        if self.cxx_enabled && !compiler.is_empty() {
            let dirs = util::expand_list(
                &s.var_safe(0, "CMAKE_EXTRA_GENERATOR_CXX_SYSTEM_INCLUDE_DIRS"),
                false,
            );
            append_include_directories(&mut xml, &dirs, &mut emitted);
        }

        xml.end_element(); // storageModule

        // add build targets
        xml.start_element("storageModule");
        xml.attribute("moduleId", "org.eclipse.cdt.make.core.buildtargets");
        xml.start_element("buildTargets");
        emitted.clear();
        let make = s.var_safe(0, "CMAKE_MAKE_PROGRAM");
        let make_args = s.var_safe(0, "CMAKE_ECLIPSE_MAKE_ARGUMENTS");

        // `cmGlobalNinjaGenerator`/`cmGlobalUnixMakefileGenerator3`
        // `GetAllTargetName`/`GetCleanTargetName`.
        let all_target = "all";
        let clean_target = "clean";

        // add all executable and library targets and some of the GLOBAL
        // and UTILITY targets
        for dir in 0..s.makefile_count() {
            let mut subdir = s.maybe_relative_to_top_bin_dir(s.binary_dir(dir));
            if subdir == "." {
                subdir.clear();
            }

            for target in s.targets(dir) {
                let target_name = target.name.as_str();
                match target.kind {
                    // Only add the global targets from CMAKE_BINARY_DIR,
                    // not from the subdirs
                    TargetKind::GlobalTarget if subdir.is_empty() => {
                        append_target(
                            &mut xml,
                            target_name,
                            &make,
                            &make_args,
                            &subdir,
                            ": ",
                            None,
                        );
                    }
                    TargetKind::GlobalTarget => {}
                    TargetKind::Utility => {
                        // Add all utility targets, except the
                        // Nightly/Continuous/Experimental-"sub"targets as e.g.
                        // NightlyStart
                        if (target_name.starts_with("Nightly") && target_name != "Nightly")
                            || (target_name.starts_with("Continuous")
                                && target_name != "Continuous")
                            || (target_name.starts_with("Experimental")
                                && target_name != "Experimental")
                        {
                            continue;
                        }

                        append_target(
                            &mut xml,
                            target_name,
                            &make,
                            &make_args,
                            &subdir,
                            ": ",
                            None,
                        );
                    }
                    TargetKind::Executable
                    | TargetKind::StaticLibrary
                    | TargetKind::SharedLibrary
                    | TargetKind::ModuleLibrary
                    | TargetKind::ObjectLibrary => {
                        let prefix = if target.kind == TargetKind::Executable {
                            "[exe] "
                        } else {
                            "[lib] "
                        };
                        append_target(
                            &mut xml,
                            target_name,
                            &make,
                            &make_args,
                            &subdir,
                            prefix,
                            None,
                        );
                        let fast_target = format!("{target_name}/fast");
                        append_target(
                            &mut xml,
                            &fast_target,
                            &make,
                            &make_args,
                            &subdir,
                            prefix,
                            None,
                        );

                        // Add Build and Clean targets in the virtual folder of
                        // targets:
                        if self.supports_virtual_folders {
                            let virt_dir = format!("[Targets]/{prefix}{target_name}");
                            // `lgen->GetBinaryDirectory()` is the home
                            // output directory, not the local one.
                            let build_args =
                                format!("-C \"{}\" {}", self.home_output_directory, make_args);
                            append_target(
                                &mut xml,
                                "Build",
                                &make,
                                &build_args,
                                &virt_dir,
                                "",
                                Some(target_name),
                            );

                            // `lg->GetTargetDirectory(target)`: the Ninja
                            // local generator inherits
                            // `cmLocalCommonGenerator::GetTargetDirectory`
                            // (`<name>.dir`), unlike Unix Makefiles'
                            // `<object-root>/<name>.dir`.
                            let clean_args = format!(
                                "-E chdir \"{}\" \"{}\" -P \"{}.dir/cmake_clean.cmake\"",
                                s.object_output_root(dir),
                                s.ctx.interp.cmake_command,
                                target.name,
                            );
                            let cmake_command = s.ctx.interp.cmake_command.clone();
                            append_target(
                                &mut xml,
                                "Clean",
                                &cmake_command,
                                &clean_args,
                                &virt_dir,
                                "",
                                Some(""),
                            );
                        }
                    }
                    _ => {}
                }
            }

            // the per-directory GLOBAL_TARGET copies (edit_cache,
            // rebuild_cache, install, ...) — only from CMAKE_BINARY_DIR.
            if subdir.is_empty() {
                for name in s.global_target_names(dir) {
                    append_target(&mut xml, name, &make, &make_args, &subdir, ": ", None);
                }
            }

            // insert the all and clean targets in every subdir (upstream
            // checks GetAllTargetName/GetCleanTargetName for null; the
            // Ninja global generator always returns "all"/"clean")
            append_target(&mut xml, all_target, &make, &make_args, &subdir, ": ", None);
            append_target(
                &mut xml,
                clean_target,
                &make,
                &make_args,
                &subdir,
                ": ",
                None,
            );

            // insert rules for compiling, preprocessing and assembling
            // individual files: `cmLocalGenerator::GetIndividualFileTargets`
            // is only overridden by the Unix Makefiles local generator; the
            // Ninja local generator inherits the empty base implementation,
            // so this list is always empty here.
            let object_file_targets: Vec<String> = Vec::new();
            for f in &object_file_targets {
                let mut prefix = "[obj] ";
                if f.ends_with('s') {
                    prefix = "[to asm] ";
                } else if f.ends_with('i') {
                    prefix = "[pre] ";
                }
                append_target(&mut xml, f, &make, &make_args, &subdir, prefix, None);
            }
        }

        xml.end_element(); // buildTargets
        xml.end_element(); // storageModule

        append_storage_scanners(s, &mut xml);

        // noqa: spellcheck off
        xml.end_element(); // cconfiguration
        // noqa: spellcheck on
        xml.end_element(); // storageModule

        xml.start_element("storageModule");
        xml.attribute("moduleId", "cdtBuildSystem");
        xml.attribute("version", "4.0.0");

        xml.start_element("project");
        xml.attribute("id", &format!("{}.null.1", s.project_name(0)));
        xml.attribute("name", &s.project_name(0));
        xml.end_element(); // project

        xml.end_element(); // storageModule

        // Append additional cproject contents without applying any XML
        // formatting
        if let Some(extra) = s
            .ctx
            .interp
            .global_props
            .get("ECLIPSE_EXTRA_CPROJECT_CONTENTS")
        {
            xml.out.push_str(extra);
        }

        xml.end_element(); // cproject

        write_generated_file(&filename, &xml.out);
    }
}

/// `cmExtraEclipseCDT4Generator::Generate` entry point.
pub fn generate(ctx: &GenCtx, globals: &[GlobalTarget]) {
    let s = Shared::new(ctx, globals);
    Eclipse::new(&s).generate();
}

/// The `GLOBAL_TARGET` `cmGlobalGenerator::CreateGlobalTarget` adds to
/// `dir`: a Normal target whose INCLUDE_DIRECTORIES start from the
/// directory property, as `cmTarget`'s constructor initializes it.
fn global_target(s: &Shared, dir: usize, name: &str) -> Target {
    let interp = s.ctx.interp;
    let mut target = Target::new(name, TargetKind::GlobalTarget, dir);
    if let Some(value) = interp.makefiles[dir].props.get("INCLUDE_DIRECTORIES") {
        target.props.set("INCLUDE_DIRECTORIES", value.to_owned());
        target.provenance.insert(
            "INCLUDE_DIRECTORIES".to_owned(),
            crate::commands::target::directory_entries(interp, dir, "INCLUDE_DIRECTORIES"),
        );
    }
    if let Some(system) = interp.ext.directory_system_includes.get(&dir) {
        target.system_include_directories.clone_from(system);
    }
    target
}
