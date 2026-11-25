//! `cmDependsFortran`: the Fortran scanner behind `cmake -E cmake_depends`
//! for the Makefile generators, and `-E cmake_copy_f90_mod`.
//!
//! Sources are scanned with the dependency parser shared with
//! `cmake_ninja_depends` (cmFortranParser); this module ports the
//! Makefile-side bookkeeping: module stamps, `fortran.internal`,
//! `cmake_clean_Fortran.cmake` and the `depend.make` rules.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;

use super::depends::{LocalGen, write_generated};
use super::fortran::Parser;
use crate::commands::sysinfo::convert_to_unix_output_path;
use crate::interp::Interp;
use crate::outconv::{FLAG_ALLOW_MAKE_VARIABLES, FLAG_IS_UNIX, FLAG_MAKE, escape_for_shell};
use crate::util;

/// `cmFortranModuleAppendUpperLower`.
fn append_upper_lower(module: &str, upper: &mut String, lower: &mut String) {
    let ext_len = if module.ends_with(".mod") || module.ends_with(".sub") {
        4
    } else if module.ends_with(".smod") {
        5
    } else {
        0
    };
    let (name, ext) = module.split_at(module.len() - ext_len);
    upper.push_str(&name.to_ascii_uppercase());
    upper.push_str(ext);
    lower.push_str(module);
}

/// `cmFortranSourceInfo`.
#[derive(Default)]
struct SourceInfo {
    source: String,
    provides: BTreeSet<String>,
    requires: BTreeSet<String>,
    intrinsics: BTreeSet<String>,
    includes: BTreeSet<String>,
}

/// `cmSystemTools::FileExists(name, isFile = true)`.
fn is_file(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.is_dir())
}

/// `cmOutputConverter::ConvertToOutputFormat(path, SHELL)` for a Unix
/// Makefile local generator.
fn shell(path: &str) -> String {
    escape_for_shell(path, FLAG_MAKE | FLAG_ALLOW_MAKE_VARIABLES | FLAG_IS_UNIX)
}

pub(super) struct DependsFortran<'a> {
    lg: &'a LocalGen,
    mf: &'a Interp,
    target_dir: String,
    include_path: Vec<String>,
    pp_definitions: BTreeSet<String>,
    smod_sep: String,
    smod_ext: String,
    object_info: BTreeMap<String, SourceInfo>,
    target_provides: BTreeSet<String>,
    target_requires: BTreeMap<String, String>,
}

impl<'a> DependsFortran<'a> {
    /// `cmDependsFortran::cmDependsFortran(lg)`.
    pub(super) fn new(lg: &'a LocalGen, mf: &'a Interp, target_dir: &str) -> Self {
        // cmDepends::SetIncludePathFromLanguage("Fortran").
        let mut include_path = Vec::new();
        if let Some(value) = mf.get_def("CMAKE_Fortran_TARGET_INCLUDE_PATH") {
            util::expand_list_into(value, false, &mut include_path);
        } else if let Some(value) = mf.get_def("CMAKE_Fortran_INCLUDE_PATH") {
            util::expand_list_into(value, false, &mut include_path);
        }
        // FOO=BAR defines FOO for the preprocessor conditionals.
        let pp_definitions = util::expand_list(
            mf.get_def("CMAKE_TARGET_DEFINITIONS_Fortran").unwrap_or(""),
            false,
        )
        .into_iter()
        .map(|def| match def.split_once('=') {
            Some((name, _)) => name.to_owned(),
            None => def,
        })
        .collect();
        DependsFortran {
            lg,
            mf,
            target_dir: target_dir.to_owned(),
            include_path,
            pp_definitions,
            smod_sep: mf.get_safe("CMAKE_Fortran_SUBMODULE_SEP"),
            smod_ext: mf.get_safe("CMAKE_Fortran_SUBMODULE_EXT"),
            object_info: BTreeMap::new(),
            target_provides: BTreeSet::new(),
            target_requires: BTreeMap::new(),
        }
    }

    /// `cmDepends::Write`: scan every object's sources, then finalize.
    pub(super) fn write(
        &mut self,
        make_depends: &mut String,
        internal_depends: &mut String,
    ) -> bool {
        let pairs = util::expand_list(
            self.mf.get_def("CMAKE_DEPENDS_CHECK_Fortran").unwrap_or(""),
            false,
        );
        let mut dependencies: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut it = pairs.into_iter();
        while let Some(src) = it.next() {
            let Some(obj) = it.next() else { break };
            dependencies.entry(obj).or_default().insert(src);
        }
        for (obj, sources) in &dependencies {
            if !self.write_dependencies(sources, obj) {
                return false;
            }
        }
        self.finalize(make_depends, internal_depends)
    }

    /// `cmDependsFortran::WriteDependencies`: parse each source.
    fn write_dependencies(&mut self, sources: &BTreeSet<String>, obj: &str) -> bool {
        if sources.is_empty() || sources.iter().next().is_some_and(String::is_empty) {
            super::error("Cannot scan dependencies without a source file.");
            return false;
        }
        if obj.is_empty() {
            super::error("Cannot scan dependencies without an object file.");
            return false;
        }
        let mut okay = true;
        for src in sources {
            let info = self
                .object_info
                .entry(obj.to_owned())
                .or_insert_with(|| SourceInfo {
                    source: src.clone(),
                    ..SourceInfo::default()
                });
            let mut parser = Parser::new(self.include_path.clone(), &self.smod_sep, &self.smod_ext)
                .with_definitions(&self.pp_definitions);
            if let Err(error) = parser.scan(src) {
                okay = false;
                eprintln!(
                    "warning: failed to parse dependencies from Fortran source '{src}': {error}"
                );
            }
            info.provides.extend(parser.provides);
            info.requires.extend(parser.requires);
            info.intrinsics.extend(parser.intrinsics);
            info.includes.extend(parser.includes);
        }
        okay
    }

    /// `cmDependsFortran::Finalize`.
    fn finalize(&mut self, make_depends: &mut String, internal_depends: &mut String) -> bool {
        if !self.locate_modules() {
            return false;
        }
        let stamp_dir = self.target_dir.clone();
        let mut mod_dir = self.mf.get_safe("CMAKE_Fortran_TARGET_MODULE_DIR");
        if mod_dir.is_empty() {
            mod_dir = self.lg.current_binary.clone();
        }
        // Before 4.0 the variable name was misspelled; both are honored.
        let building_intrinsics = !self
            .mf
            .get_safe("CMAKE_Fortran_TARGET_BUILDING_INTRINSIC_MODULES")
            .is_empty()
            || !self
                .mf
                .get_safe("CMAKE_Fortran_TARGET_BUILDING_INSTRINSIC_MODULES")
                .is_empty();

        let objects: Vec<String> = self.object_info.keys().cloned().collect();
        for obj in &objects {
            self.write_dependencies_real(
                obj,
                &mod_dir,
                &stamp_dir,
                make_depends,
                internal_depends,
                building_intrinsics,
            );
        }

        // The list of modules provided by this target.
        let mut fortran_internal =
            "# The fortran modules provided by this target.\nprovides\n".to_owned();
        for module in &self.target_provides {
            fortran_internal.push_str(&format!(" {module}\n"));
        }
        write_generated(
            &format!("{}/fortran.internal", self.target_dir),
            &fortran_internal,
            false,
        );

        // A script to clean the modules.
        if !self.target_provides.is_empty() {
            let mut clean =
                "# Remove fortran modules provided by this target.\nFILE(REMOVE".to_owned();
            for module in &self.target_provides {
                let mut upper = format!("{mod_dir}/");
                let mut lower = format!("{mod_dir}/");
                append_upper_lower(module, &mut upper, &mut lower);
                let stamp = format!("{stamp_dir}/{module}.stamp");
                for path in [&lower, &upper, &stamp] {
                    clean.push_str(&format!(
                        "\n  \"{}\"",
                        self.lg.maybe_relative_to_cur_bin_dir(path)
                    ));
                }
                clean.push('\n');
            }
            clean.push_str("  )\n");
            write_generated(
                &format!("{}/cmake_clean_Fortran.cmake", self.target_dir),
                &clean,
                false,
            );
        }
        true
    }

    /// `cmDependsFortran::LocateModules`.
    fn locate_modules(&mut self) -> bool {
        for info in self.object_info.values() {
            self.target_provides.extend(info.provides.iter().cloned());
            for required in &info.requires {
                self.target_requires.insert(required.clone(), String::new());
            }
        }
        if self.target_requires.is_empty() {
            return true;
        }
        // MatchLocalModules.
        let stamp_dir = self.target_dir.clone();
        for module in self.target_provides.clone() {
            self.consider_module(&module, &stamp_dir);
        }
        // Modules provided by linked targets.
        let info_files = util::expand_list(
            self.mf
                .get_def("CMAKE_Fortran_TARGET_LINKED_INFO_FILES")
                .unwrap_or(""),
            false,
        );
        for info_file in info_files {
            let target_dir = util::filename_path(&info_file);
            let name = format!("{target_dir}/fortran.internal");
            let mut text = Vec::new();
            let opened = std::fs::File::open(&name)
                .and_then(|mut f| f.read_to_end(&mut text))
                .is_ok();
            if !opened {
                super::error(format!(
                    "-E cmake_depends failed to open {name} for module information"
                ));
                return false;
            }
            self.match_remote_modules(&crate::bytes::decode(&text), &target_dir);
        }
        true
    }

    /// `cmDependsFortran::MatchRemoteModules`.
    fn match_remote_modules(&mut self, text: &str, stamp_dir: &str) {
        let mut doing_provides = false;
        // cmSystemTools::GetLineFromStream strips a trailing '\r'.
        for line in text.split_inclusive('\n') {
            let line = line.strip_suffix('\n').unwrap_or(line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() || line.starts_with('#') || line.starts_with('\r') {
                continue;
            }
            if line.starts_with(' ') {
                if doing_provides {
                    let mut module = line.to_owned();
                    if !module.ends_with(".mod")
                        && !module.ends_with(".smod")
                        && !module.ends_with(".sub")
                    {
                        // fortran.internal files from older CMake versions.
                        module.push_str(".mod");
                    }
                    self.consider_module(&module[1..], stamp_dir);
                }
            } else {
                doing_provides = line == "provides";
            }
        }
    }

    /// `cmDependsFortran::ConsiderModule`.
    fn consider_module(&mut self, name: &str, stamp_dir: &str) {
        if let Some(location) = self.target_requires.get_mut(name)
            && location.is_empty()
        {
            *location = format!("{stamp_dir}/{name}.stamp");
        }
    }

    /// `cmDependsFortran::WriteDependenciesReal`.
    fn write_dependencies_real(
        &mut self,
        obj: &str,
        mod_dir: &str,
        stamp_dir: &str,
        make_depends: &mut String,
        internal_depends: &mut String,
        building_intrinsics: bool,
    ) {
        let info = &self.object_info[obj];
        let obj_i = self.lg.maybe_relative_to_top_bin_dir(obj);
        let obj_m = convert_to_unix_output_path(&obj_i);
        internal_depends.push_str(&format!("{obj_i}\n {}\n", info.source));
        if !info.includes.is_empty() {
            let global = &self.lg.global;
            if global.supports_long_line_deps {
                make_depends.push_str(&obj_m);
                make_depends.push(':');
            }
            for include in &info.includes {
                let dependee =
                    convert_to_unix_output_path(&self.lg.maybe_relative_to_top_bin_dir(include));
                if global.supports_long_line_deps {
                    make_depends.push_str(&format!(" {} {dependee}", global.line_continue));
                } else {
                    make_depends.push_str(&format!("{obj_m}: {dependee}\n"));
                }
                internal_depends.push_str(&format!(" {include}\n"));
            }
            make_depends.push('\n');
        }

        let mut required = info.requires.clone();
        if building_intrinsics {
            required.extend(info.intrinsics.iter().cloned());
        }
        for module in &required {
            // Modules provided by the same source are not required.
            if info.provides.contains(module) {
                continue;
            }
            let location = self
                .target_requires
                .get(module)
                .expect("every required module was located");
            if !location.is_empty() {
                let stamp =
                    convert_to_unix_output_path(&self.lg.maybe_relative_to_top_bin_dir(location));
                make_depends.push_str(&format!("{obj_m}: {stamp}\n"));
            } else if let Some(found) = self.find_module(module) {
                let found =
                    convert_to_unix_output_path(&self.lg.maybe_relative_to_top_bin_dir(&found));
                make_depends.push_str(&format!("{obj_m}: {found}\n"));
            }
        }

        if !info.provides.is_empty() {
            let compiler_id = self.mf.get_safe("CMAKE_Fortran_COMPILER_ID");
            let provides: Vec<String> = info.provides.iter().cloned().collect();
            for module in &provides {
                self.target_provides.insert(module.clone());
                // The stamp file name is always lower case; cmake_copy_f90_mod
                // tries the cases of the real module file.
                let mod_file = shell(
                    &self
                        .lg
                        .maybe_relative_to_top_bin_dir(&format!("{mod_dir}/{module}")),
                );
                let stamp_file = self
                    .lg
                    .maybe_relative_to_top_bin_dir(&format!("{stamp_dir}/{module}.stamp"));
                let stamp_for_shell = shell(&stamp_file);
                let stamp_for_make = convert_to_unix_output_path(&stamp_file);
                make_depends.push_str(&format!("{obj_m}.provides.build: {stamp_for_make}\n"));
                make_depends.push_str(&format!("{stamp_for_make}: {obj_m}\n"));
                make_depends.push_str(&format!(
                    "\t$(CMAKE_COMMAND) -E cmake_copy_f90_mod {mod_file} {stamp_for_shell}"
                ));
                if !compiler_id.is_empty() {
                    make_depends.push(' ');
                    make_depends.push_str(&compiler_id);
                }
                make_depends.push('\n');
            }
            make_depends.push_str(&format!("{obj_m}.provides.build:\n"));
            make_depends.push_str(&format!(
                "\t$(CMAKE_COMMAND) -E touch {obj_m}.provides.build\n"
            ));
            let driver = convert_to_unix_output_path(
                &self
                    .lg
                    .maybe_relative_to_top_bin_dir(&format!("{}/build", self.target_dir)),
            );
            make_depends.push_str(&format!("{driver}: {obj_m}.provides.build\n"));
        }
    }

    /// `cmDependsFortran::FindModule`: search the include path for the
    /// module file in lower, then upper case.
    fn find_module(&self, name: &str) -> Option<String> {
        let mut upper = String::new();
        let mut lower = String::new();
        append_upper_lower(name, &mut upper, &mut lower);
        for dir in &self.include_path {
            for candidate in [format!("{dir}/{lower}"), format!("{dir}/{upper}")] {
                if is_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

/// `cmDependsFortran::CopyModule`: `cmake -E cmake_copy_f90_mod input.mod
/// output.mod.stamp [compiler-id]`.
pub(super) fn copy_module(args: &[String]) -> bool {
    let mut module = args[0].clone();
    let stamp = &args[1];
    let compiler_id = args.get(2).map_or("", String::as_str);
    if !module.ends_with(".mod") && !module.ends_with(".smod") && !module.ends_with(".sub") {
        // depend.make files from older CMake versions.
        module.push_str(".mod");
    }
    let mut dir = util::filename_path(&module);
    if !dir.is_empty() {
        dir.push('/');
    }
    let mut upper = dir.clone();
    let mut lower = dir;
    append_upper_lower(&util::filename_name(&module), &mut upper, &mut lower);
    for candidate in [&upper, &lower] {
        if is_file(candidate) {
            if modules_differ(candidate, stamp, compiler_id)
                && std::fs::copy(candidate, stamp).is_err()
            {
                eprintln!("Error copying Fortran module from \"{candidate}\" to \"{stamp}\".");
                return false;
            }
            return true;
        }
    }
    eprintln!(
        "Error copying Fortran module \"{}\".  Tried \"{upper}\" and \"{lower}\".",
        args[0]
    );
    false
}

/// `cmFortranStreamContainsSequence`: advance past the first occurrence of
/// `seq` (assumed to have no repeating subsequence).
fn skip_past(data: &[u8], pos: &mut usize, seq: &[u8]) -> bool {
    let mut cur = 0;
    while cur < seq.len() {
        let Some(&byte) = data.get(*pos) else {
            return false;
        };
        *pos += 1;
        if byte == seq[cur] {
            cur += 1;
        } else {
            cur = 0;
        }
    }
    true
}

/// `cmDependsFortran::ModulesDiffer`.
fn modules_differ(mod_file: &str, stamp_file: &str, compiler_id: &str) -> bool {
    if compiler_id == "SunPro" {
        // cmSystemTools::FilesDiffer.
        return std::fs::read(mod_file).ok() != std::fs::read(stamp_file).ok()
            || std::fs::metadata(mod_file).is_err();
    }
    let (Ok(module), Ok(stamp)) = (std::fs::read(mod_file), std::fs::read(stamp_file)) else {
        // At least one of the files does not exist.
        return true;
    };
    let (mut mp, mut sp) = (0usize, 0usize);
    if compiler_id == "GNU" {
        // GNU Fortran >= 4.9 writes gzip-compressed modules without a date.
        if !(module.len() >= 2 && module[0] == 0x1f && module[1] == 0x8b) {
            if !skip_past(&module, &mut mp, b"\n") {
                eprintln!("{compiler_id} fortran module {mod_file} has unexpected format.");
                return true;
            }
            if !skip_past(&stamp, &mut sp, b"\n") {
                return true;
            }
        }
    } else if compiler_id == "Intel" || compiler_id == "IntelLLVM" {
        // Skip the leading version byte.
        mp = mp.saturating_add(usize::from(!module.is_empty()));
        sp = sp.saturating_add(usize::from(!stamp.is_empty()));
        if !skip_past(&module, &mut mp, b"\n\0") {
            eprintln!("{compiler_id} fortran module {mod_file} has unexpected format.");
            return true;
        }
        if !skip_past(&stamp, &mut sp, b"\n\0") {
            return true;
        }
    }
    module[mp..] != stamp[sp..]
}
