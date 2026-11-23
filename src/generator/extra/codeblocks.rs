//! `cmExtraCodeBlocksGenerator`: `<project>.cbp` files.

use std::collections::{BTreeMap, BTreeSet};

use super::xml::XmlWriter;
use super::{
    Shared, file_exists, filename_name, filename_path, filename_without_extension, relative_path,
    write_file,
};
use crate::generator::ctx::GenCtx;
use crate::generator::ninja::GlobalTarget;
use crate::state::{Target, TargetKind};
use crate::util;

/// `cm::IsACLikeSourceExtension` (cmake.cxx's CLikeSourceFileExtensions).
fn is_c_like_source_extension(ctx: &GenCtx, ext: &str) -> bool {
    // The default set from cmake.cxx; language-specific additions do not
    // change the C-like list.
    let _ = ctx;
    matches!(
        ext,
        "c" | "C" | "c++" | "cc" | "cpp" | "cppm" | "cxx" | "m" | "M" | "mm" | "ixx"
    )
}

/// `cmake::GetHeaderExtensions` (CMAKE_HEADER_FILE_EXTENSIONS).
fn header_extensions(ctx: &GenCtx, dir: usize) -> Vec<String> {
    ctx.var(dir, "CMAKE_HEADER_FILE_EXTENSIONS")
        .map_or_else(|| vec!["h".into()], |v| util::expand_list(&v, false))
}

/// `cmExtraCodeBlocksGenerator::GetCBCompilerId`.
fn compiler_id(s: &Shared, dir: usize) -> String {
    let user = s.var_safe(dir, "CMAKE_CODEBLOCKS_COMPILER_ID");
    if !user.is_empty() {
        return user;
    }
    let mut pure_fortran = false;
    let var = if s.lang_enabled("CXX") {
        "CMAKE_CXX_COMPILER_ID"
    } else if s.lang_enabled("C") {
        "CMAKE_C_COMPILER_ID"
    } else if s.lang_enabled("Fortran") {
        pure_fortran = true;
        "CMAKE_Fortran_COMPILER_ID"
    } else {
        ""
    };
    let id = if var.is_empty() {
        String::new()
    } else {
        s.var_safe(dir, var)
    };
    let mut compiler = "gcc".to_owned();
    match id.as_str() {
        "MSVC" => {
            compiler = if s.var(dir, "MSVC10").is_some() {
                "msvc10"
            } else {
                "msvc8"
            }
            .to_owned();
        }
        "Borland" => compiler = "bcc".to_owned(),
        "SDCC" => compiler = "sdcc".to_owned(),
        "Intel" => {
            compiler = if pure_fortran && s.var(dir, "WIN32").is_some() {
                "ifcwin"
            } else {
                "icc"
            }
            .to_owned();
        }
        "Watcom" | "OpenWatcom" => compiler = "ow".to_owned(),
        "Clang" => compiler = "clang".to_owned(),
        "PGI" => {
            compiler = if pure_fortran { "pgifortran" } else { "pgi" }.to_owned();
        }
        "LCC" => {
            compiler = if pure_fortran { "lfortran" } else { "lcc" }.to_owned();
        }
        "GNU" => {
            compiler = if pure_fortran { "gfortran" } else { "gcc" }.to_owned();
        }
        _ => {}
    }
    compiler
}

/// `cmExtraCodeBlocksGenerator::GetCBTargetType`.
fn cb_target_type(s: &Shared, target: &Target) -> i64 {
    match target.kind {
        TargetKind::Executable => {
            let config = s.build_type(target.makefile);
            let win32 = s.ctx.genex(
                target.props.get("WIN32_EXECUTABLE").unwrap_or("0"),
                Some(target),
                None,
            ) == "1";
            let bundle = s.ctx.genex(
                target.props.get("MACOSX_BUNDLE").unwrap_or("0"),
                Some(target),
                None,
            ) == "1";
            let _ = config;
            if win32 || bundle { 0 } else { 1 }
        }
        TargetKind::StaticLibrary | TargetKind::ObjectLibrary => 2,
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary => 3,
        _ => 4,
    }
}

/// `cmExtraCodeBlocksGenerator::BuildMakeCommand` (Ninja).
fn build_make_command(s: &Shared, make: &str, makefile: &str, target: &str, flags: &str) -> String {
    let mut command = make.to_owned();
    if !flags.is_empty() {
        command.push(' ');
        command.push_str(flags);
    }
    // Ninja: "ninja -v <target>"
    command.push_str(" -v ");
    command.push_str(target);
    let _ = makefile;
    let _ = s;
    command
}

/// `cmExtraCodeBlocksGenerator::CreateDummyTargetFile`.
fn create_dummy_target_file(s: &Shared, dir: usize, target: &Target) -> String {
    let filename = format!("{}/{}.objlib", s.target_dir(dir, target), target.name);
    write_file(
        &filename,
        &format!(
            "# This is a dummy file for the OBJECT library {} for the CMake CodeBlocks project generator.\n# Don't edit, this file will be overwritten.\n",
            target.name
        ),
    );
    filename
}

/// `cmExtraCodeBlocksGenerator::AppendTarget`.
#[allow(clippy::too_many_arguments)]
fn append_target(
    s: &Shared,
    xml: &mut XmlWriter,
    target_name: &str,
    target: Option<&Target>,
    make: &str,
    dir: usize,
    compiler: &str,
    make_flags: &str,
) {
    let makefile_name = format!("{}/Makefile", s.binary_dir(dir));
    xml.start_element("Target");
    xml.attribute("title", target_name);
    if let Some(target) = target {
        let cb_type = cb_target_type(s, target);
        let mut working_dir = s.binary_dir(dir).to_owned();
        if target.kind == TargetKind::Executable {
            if let Some(runtime_dir) = s.var(dir, "CMAKE_RUNTIME_OUTPUT_DIRECTORY") {
                working_dir = runtime_dir;
            } else if let Some(exe_dir) = s.var(dir, "EXECUTABLE_OUTPUT_PATH") {
                working_dir = exe_dir;
            }
        }
        let build_type = s.build_type(dir);
        let location = if target.kind == TargetKind::ObjectLibrary {
            create_dummy_target_file(s, dir, target)
        } else {
            s.target_location(target, &build_type)
        };
        xml.start_element("Option");
        xml.attribute("output", &location);
        xml.attribute_i("prefix_auto", 0);
        xml.attribute_i("extension_auto", 0);
        xml.end_element();
        xml.start_element("Option");
        xml.attribute("working_dir", &working_dir);
        xml.end_element();
        xml.start_element("Option");
        xml.attribute("object_output", "./");
        xml.end_element();
        xml.start_element("Option");
        xml.attribute_i("type", cb_type);
        xml.end_element();
        xml.start_element("Option");
        xml.attribute("compiler", compiler);
        xml.end_element();
        xml.start_element("Compiler");
        for def in crate::generator::flags::compile_definitions(s.ctx, target, &build_type, "C") {
            xml.start_element("Add");
            xml.attribute("option", &format!("-D{def}"));
            xml.end_element();
        }
        let mut all_includes: Vec<String> =
            crate::generator::flags::include_items(s.ctx, target, &build_type, "C")
                .into_iter()
                .map(|(path, _, _)| path)
                .collect();
        for var in [
            "CMAKE_EXTRA_GENERATOR_CXX_SYSTEM_INCLUDE_DIRS",
            "CMAKE_EXTRA_GENERATOR_C_SYSTEM_INCLUDE_DIRS",
        ] {
            let dirs = s.var_safe(dir, var);
            if !dirs.is_empty() {
                all_includes.extend(util::expand_list(&dirs, false));
            }
        }
        // cmRemoveDuplicates: keep the first occurrence of each entry.
        let mut seen = std::collections::HashSet::new();
        all_includes.retain(|inc| seen.insert(inc.clone()));
        for inc in all_includes {
            xml.start_element("Add");
            xml.attribute("directory", &inc);
            xml.end_element();
        }
        xml.end_element(); // Compiler
    } else {
        xml.start_element("Option");
        xml.attribute("working_dir", s.binary_dir(dir));
        xml.end_element();
        xml.start_element("Option");
        xml.attribute_i("type", 4);
        xml.end_element();
    }
    xml.start_element("MakeCommands");
    xml.start_element("Build");
    xml.attribute(
        "command",
        &build_make_command(s, make, &makefile_name, target_name, make_flags),
    );
    xml.end_element();
    xml.start_element("CompileFile");
    xml.attribute(
        "command",
        &build_make_command(s, make, &makefile_name, "\"$file\"", make_flags),
    );
    xml.end_element();
    xml.start_element("Clean");
    xml.attribute(
        "command",
        &build_make_command(s, make, &makefile_name, "clean", make_flags),
    );
    xml.end_element();
    xml.start_element("DistClean");
    xml.attribute(
        "command",
        &build_make_command(s, make, &makefile_name, "clean", make_flags),
    );
    xml.end_element();
    xml.end_element(); // MakeCommands
    xml.end_element(); // Target
}

/// The CodeBlocks virtual-folder tree of CMake list files.
#[derive(Default)]
struct Tree {
    path: String,
    folders: Vec<Tree>,
    files: BTreeSet<String>,
}

impl Tree {
    fn insert_path(&mut self, split: &[String], start: usize, file_name: &str) {
        if start == split.len() {
            self.files.insert(file_name.to_owned());
            return;
        }
        for folder in &mut self.folders {
            if folder.path == split[start] {
                if start + 1 < split.len() {
                    folder.insert_path(split, start + 1, file_name);
                } else {
                    folder.files.insert(file_name.to_owned());
                }
                return;
            }
        }
        let mut folder = Tree {
            path: split[start].clone(),
            ..Tree::default()
        };
        if start + 1 < split.len() {
            folder.insert_path(split, start + 1, file_name);
        } else {
            folder.files.insert(file_name.to_owned());
        }
        self.folders.push(folder);
    }

    fn build_virtual_folder(&self, xml: &mut XmlWriter) {
        xml.start_element("Option");
        let mut virtual_folders = "CMake Files\\;".to_owned();
        for folder in &self.folders {
            folder.build_virtual_folder_impl(&mut virtual_folders, "");
        }
        xml.attribute("virtualFolders", &virtual_folders);
        xml.end_element();
    }

    fn build_virtual_folder_impl(&self, virtual_folders: &mut String, prefix: &str) {
        *virtual_folders += &format!("CMake Files\\{prefix}{}\\;", self.path);
        for folder in &self.folders {
            folder.build_virtual_folder_impl(virtual_folders, &format!("{prefix}{}\\", self.path));
        }
    }

    fn build_unit(&self, xml: &mut XmlWriter, fs_path: &str) {
        for f in &self.files {
            xml.start_element("Unit");
            xml.attribute("filename", &format!("{fs_path}{f}"));
            xml.start_element("Option");
            xml.attribute("virtualFolder", "CMake Files\\");
            xml.end_element();
            xml.end_element();
        }
        for folder in &self.folders {
            folder.build_unit_impl(xml, "", fs_path);
        }
    }

    fn build_unit_impl(&self, xml: &mut XmlWriter, virtual_folder_path: &str, fs_path: &str) {
        for f in &self.files {
            xml.start_element("Unit");
            xml.attribute("filename", &format!("{fs_path}{}/{f}", self.path));
            xml.start_element("Option");
            xml.attribute(
                "virtualFolder",
                &format!("CMake Files\\{virtual_folder_path}{}\\", self.path),
            );
            xml.end_element();
            xml.end_element();
        }
        for folder in &self.folders {
            folder.build_unit_impl(
                xml,
                &format!("{virtual_folder_path}{}\\", self.path),
                &format!("{fs_path}{}/", self.path),
            );
        }
    }
}

/// `cmExtraCodeBlocksGenerator::Generate` + `CreateProjectFile` +
/// `CreateNewProjectFile`.
pub fn generate(ctx: &GenCtx, globals: &[GlobalTarget]) {
    let s = Shared::new(ctx, globals);
    for (_, lgs) in s.project_map() {
        let dir = lgs[0];
        let project_name = s.project_name(dir);
        let filename = format!("{}/{}.cbp", s.binary_dir(dir), project_name);
        let mut tree = Tree::default();
        for (_, group) in s.project_map() {
            let mut list_files: Vec<String> = Vec::new();
            for lg in &group {
                list_files.extend(s.list_files(*lg).iter().cloned());
            }
            for list_file in &list_files {
                if list_file.starts_with(&ctx.interp.cmake_root) {
                    continue;
                }
                let relative = relative_path(s.source_dir(group[0]), list_file);
                // cmSystemTools::SplitPath: a leading empty root component.
                let mut split: Vec<String> = vec![String::new()];
                split.extend(relative.split('/').map(str::to_owned));
                let file_name = split.pop().unwrap_or_default();
                let exclude_external = s
                    .var(group[0], "CMAKE_CODEBLOCKS_EXCLUDE_EXTERNAL_FILES")
                    .is_some_and(|v| util::is_on(&v));
                if !split.is_empty()
                    && (!exclude_external || !relative.contains(".."))
                    && !relative.contains("CMakeFiles")
                {
                    tree.insert_path(&split, 1, &file_name);
                }
            }
        }

        let mf_dir = dir;
        let compiler = compiler_id(&s, mf_dir);
        let make = s.var_safe(mf_dir, "CMAKE_MAKE_PROGRAM");
        let make_args = s.var_safe(mf_dir, "CMAKE_CODEBLOCKS_MAKE_ARGUMENTS");

        let mut xml = XmlWriter::new();
        xml.start_document("UTF-8");
        xml.start_element("CodeBlocks_project_file");
        xml.start_element("FileVersion");
        xml.attribute_i("major", 1);
        xml.attribute_i("minor", 6);
        xml.end_element();
        xml.start_element("Project");
        xml.start_element("Option");
        xml.attribute("title", &project_name);
        xml.end_element();
        xml.start_element("Option");
        xml.attribute_i("makefile_is_custom", 1);
        xml.end_element();
        xml.start_element("Option");
        xml.attribute("compiler", &compiler);
        xml.end_element();
        tree.build_virtual_folder(&mut xml);

        xml.start_element("Build");
        append_target(&s, &mut xml, "all", None, &make, dir, &compiler, &make_args);
        for lg in &lgs {
            let dir = *lg;
            for target in s.targets(dir) {
                let name = target.name.clone();
                match target.kind {
                    TargetKind::GlobalTarget if dir == 0 => {
                        append_target(&s, &mut xml, &name, None, &make, dir, &compiler, &make_args);
                    }
                    TargetKind::GlobalTarget => {}
                    TargetKind::Utility => {
                        if (name.starts_with("Nightly") && name != "Nightly")
                            || (name.starts_with("Continuous") && name != "Continuous")
                            || (name.starts_with("Experimental") && name != "Experimental")
                        {
                            continue;
                        }
                        append_target(&s, &mut xml, &name, None, &make, dir, &compiler, &make_args);
                    }
                    TargetKind::Executable
                    | TargetKind::StaticLibrary
                    | TargetKind::SharedLibrary
                    | TargetKind::ModuleLibrary
                    | TargetKind::ObjectLibrary => {
                        append_target(
                            &s,
                            &mut xml,
                            &name,
                            Some(target),
                            &make,
                            dir,
                            &compiler,
                            &make_args,
                        );
                        let fast = format!("{name}/fast");
                        append_target(
                            &s,
                            &mut xml,
                            &fast,
                            Some(target),
                            &make,
                            dir,
                            &compiler,
                            &make_args,
                        );
                    }
                    _ => {}
                }
            }
            // GLOBAL_TARGET copies (edit_cache, rebuild_cache, ...) in the
            // top-level directory only.
            if dir == 0 {
                for name in s.global_target_names(dir) {
                    append_target(&s, &mut xml, name, None, &make, dir, &compiler, &make_args);
                }
            }
        }
        xml.end_element(); // Build

        // Collect all used source files in the project.
        let mut all_files: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut c_files: Vec<String> = Vec::new();
        let header_exts = header_extensions(ctx, dir);
        for lg in &lgs {
            let dir = *lg;
            for target in s.targets(dir) {
                match target.kind {
                    TargetKind::Executable
                    | TargetKind::StaticLibrary
                    | TargetKind::SharedLibrary
                    | TargetKind::ModuleLibrary
                    | TargetKind::ObjectLibrary
                    | TargetKind::Utility => {}
                    _ => continue,
                }
                let build_type = s.build_type(dir);
                for src in s.target_sources(target, &build_type).iter() {
                    if target.kind == TargetKind::Utility && src.generated {
                        continue;
                    }
                    let mut is_c_file = false;
                    if matches!(src.language.as_str(), "C" | "CXX" | "CUDA" | "HIP") {
                        let ext = src.path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
                        is_c_file = is_c_like_source_extension(ctx, ext);
                    }
                    let full_path = src.path.clone();
                    let relative = relative_path(s.source_dir(dir), &full_path);
                    if s.var(dir, "CMAKE_CODEBLOCKS_EXCLUDE_EXTERNAL_FILES")
                        .is_some_and(|v| util::is_on(&v))
                        && relative.contains("..")
                    {
                        continue;
                    }
                    if is_c_file {
                        c_files.push(full_path.clone());
                    }
                    all_files
                        .entry(full_path)
                        .or_default()
                        .push(target.name.clone());
                }
            }
        }
        for file_name in &c_files {
            let header_base = format!(
                "{}/{}",
                filename_path(file_name),
                filename_without_extension(file_name)
            );
            for ext in &header_exts {
                let hname = format!("{header_base}.{ext}");
                if all_files.contains_key(&hname) {
                    break;
                }
                if file_exists(&hname) {
                    all_files.insert(hname, all_files[file_name].clone());
                    break;
                }
            }
        }
        for (unit_filename, targets) in &all_files {
            xml.start_element("Unit");
            xml.attribute("filename", unit_filename);
            for tgt in targets {
                xml.start_element("Option");
                xml.attribute("target", tgt);
                xml.end_element();
            }
            xml.end_element();
        }
        tree.build_unit(&mut xml, &format!("{}/", s.home_source()));
        xml.end_element(); // Project
        xml.end_element(); // CodeBlocks_project_file
        xml.end_document();
        write_file(&filename, &xml.out);
        let _ = filename_name("");
    }
}
