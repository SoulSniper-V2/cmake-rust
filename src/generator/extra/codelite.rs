//! `cmExtraCodeLiteGenerator`: `.workspace` + `.project` files.

use std::collections::{BTreeMap, BTreeSet};

use super::xml::XmlWriter;
use super::{
    HEADER_EXTENSIONS, Shared, file_exists, filename_path, filename_without_extension,
    is_known_source_extension, relative_path, source_extension, write_file,
};
use crate::generator::ctx::GenCtx;
use crate::generator::ninja::GlobalTarget;
use crate::state::{Target, TargetKind};
use crate::util;

/// `cmExtraCodeLiteGenerator::GetCodeLiteCompilerName`.
fn compiler_name(s: &Shared, dir: usize) -> String {
    let var = if s.lang_enabled("CXX") {
        "CMAKE_CXX_COMPILER_ID"
    } else {
        "CMAKE_C_COMPILER_ID"
    };
    let id = s.var_safe(dir, var);
    match id.as_str() {
        "MSVC" => "VC++".to_owned(),
        "Clang" => "clang++".to_owned(),
        "GNU" => "gnu g++".to_owned(),
        _ => "gnu g++".to_owned(),
    }
}

/// `cmExtraCodeLiteGenerator::GetConfigurationName` (trimmed; "NoConfig").
fn configuration_name(s: &Shared, dir: usize) -> String {
    let conf = s.build_type(dir).trim().to_owned();
    if conf.is_empty() {
        "NoConfig".to_owned()
    } else {
        conf
    }
}

/// `cmExtraCodeLiteGenerator::CollectSourceFiles` -> project type string.
fn collect_source_files(
    s: &Shared,
    target: &Target,
    c_files: &mut BTreeMap<String, ()>,
    other_files: &mut BTreeSet<String>,
) -> String {
    let project_type = match target.kind {
        TargetKind::Executable => "Executable",
        TargetKind::StaticLibrary => "Static Library",
        TargetKind::SharedLibrary | TargetKind::ModuleLibrary => "Dynamic Library",
        _ => "",
    };
    match target.kind {
        TargetKind::Executable
        | TargetKind::StaticLibrary
        | TargetKind::SharedLibrary
        | TargetKind::ModuleLibrary => {
            let config = s.build_type(target.makefile);
            for src in s.target_sources(target, &config).iter() {
                // cmake::IsAKnownSourceExtension on the lower-cased
                // extension.
                if is_known_source_extension(&source_extension(&src.path).to_lowercase()) {
                    c_files.insert(src.path.clone(), ());
                } else {
                    other_files.insert(src.path.clone());
                }
            }
        }
        _ => {}
    }
    project_type.to_owned()
}

/// `cmExtraCodeLiteGenerator::FindMatchingHeaderfiles`.
fn find_matching_headerfiles(c_files: &BTreeMap<String, ()>, other_files: &mut BTreeSet<String>) {
    for file in c_files.keys() {
        let basename = format!(
            "{}/{}",
            filename_path(file),
            filename_without_extension(file)
        );
        for ext in HEADER_EXTENSIONS {
            let hname = format!("{basename}.{ext}");
            if other_files.contains(&hname) {
                break;
            }
            if file_exists(&hname) {
                other_files.insert(hname);
                break;
            }
        }
    }
}

/// `cmExtraCodeLiteGenerator::CreateFoldersAndFiles`.
fn create_folders_and_files<I>(files: I, xml: &mut XmlWriter, project_path: &str)
where
    I: IntoIterator<Item = String>,
{
    let mut tmp_path: Vec<String> = Vec::new();
    let mut num_of_end_el;
    for cfile in files {
        let frelapath = relative_path(project_path, &cfile);
        // cmsys::SystemTools::SplitPath: leading empty root component, then
        // drop the file and root components.
        let mut components: Vec<String> = vec![String::new()];
        components.extend(frelapath.split('/').map(str::to_owned));
        components.pop();
        components.remove(0);
        let mut size_of_skip = 0usize;
        for i in 0..components.len() {
            if components[i] == ".." || components[i] == "." {
                size_of_skip += 1;
                continue;
            }
            if tmp_path.len() > i - size_of_skip && tmp_path[i - size_of_skip] == components[i] {
                continue;
            }
            if tmp_path.len() > i - size_of_skip {
                num_of_end_el = tmp_path.len() - i + size_of_skip;
                tmp_path.truncate(tmp_path.len() - num_of_end_el);
                for _ in 0..num_of_end_el {
                    xml.end_element();
                }
            }
            xml.start_element("VirtualDirectory");
            xml.attribute("Name", &components[i]);
            tmp_path.push(components[i].clone());
        }
        num_of_end_el = tmp_path.len() + size_of_skip - components.len();
        if num_of_end_el > 0 {
            tmp_path.truncate(tmp_path.len() - num_of_end_el);
            for _ in 0..num_of_end_el {
                xml.end_element();
            }
        }
        xml.start_element("File");
        xml.attribute("Name", &frelapath);
        xml.end_element();
    }
    for _ in 0..tmp_path.len() {
        xml.end_element();
    }
}

/// `cmExtraCodeLiteGenerator::GetBuildCommand` (Ninja: just `make`).
fn build_command(s: &Shared, dir: usize, target_name: &str) -> String {
    let generator = s.var_safe(dir, "CMAKE_GENERATOR");
    let make = s.var_safe(dir, "CMAKE_MAKE_PROGRAM");
    let mut cmd = make;
    if generator == "MinGW Makefiles" || generator == "Unix Makefiles" {
        cmd.push_str(" -f$(ProjectPath)/Makefile");
    }
    if !target_name.is_empty() {
        cmd.push(' ');
        cmd.push_str(target_name);
    }
    cmd
}

/// `cmExtraCodeLiteGenerator::GetCleanCommand`.
fn clean_command(s: &Shared, dir: usize, target_name: &str) -> String {
    let generator = s.var_safe(dir, "CMAKE_GENERATOR");
    let base = build_command(s, dir, "");
    if !target_name.is_empty() && generator == "Ninja" {
        format!("{base} -t clean {target_name}")
    } else {
        format!("{base} clean")
    }
}

/// `cmExtraCodeLiteGenerator::GetRebuildCommand`.
fn rebuild_command(s: &Shared, dir: usize, target_name: &str) -> String {
    format!(
        "{} && {}",
        clean_command(s, dir, target_name),
        build_command(s, dir, target_name)
    )
}

/// `cmExtraCodeLiteGenerator::CreateProjectSourceEntries`.
#[allow(clippy::too_many_arguments)]
fn create_project_source_entries(
    s: &Shared,
    c_files: BTreeMap<String, ()>,
    mut other_files: BTreeSet<String>,
    xml: &mut XmlWriter,
    project_path: &str,
    dir: usize,
    project_type: &str,
    target_name: &str,
) {
    find_matching_headerfiles(&c_files, &mut other_files);
    xml.start_element("VirtualDirectory");
    xml.attribute("Name", "src");
    create_folders_and_files(c_files.into_keys(), xml, project_path);
    xml.end_element();
    xml.start_element("VirtualDirectory");
    xml.attribute("Name", "include");
    create_folders_and_files(other_files, xml, project_path);
    xml.end_element();

    xml.start_element("Settings");
    xml.attribute("Type", project_type);
    xml.start_element("Configuration");
    xml.attribute("Name", &configuration_name(s, dir));
    xml.attribute("CompilerType", &compiler_name(s, dir));
    xml.attribute("DebuggerType", "GNU gdb debugger");
    xml.attribute("Type", project_type);
    xml.attribute("BuildCmpWithGlobalSettings", "append");
    xml.attribute("BuildLnkWithGlobalSettings", "append");
    xml.attribute("BuildResWithGlobalSettings", "append");

    xml.start_element("Compiler");
    xml.attribute("Options", "-g");
    xml.attribute("Required", "yes");
    xml.attribute("PreCompiledHeader", "");
    xml.start_element("IncludePath");
    xml.attribute("Value", ".");
    xml.end_element();
    xml.end_element(); // Compiler

    xml.start_element("Linker");
    xml.attribute("Options", "");
    xml.attribute("Required", "yes");
    xml.end_element();
    xml.start_element("ResourceCompiler");
    xml.attribute("Options", "");
    xml.attribute("Required", "no");
    xml.end_element();

    xml.start_element("General");
    let mut output_path = s.var_safe(dir, "CMAKE_RUNTIME_OUTPUT_DIRECTORY");
    if output_path.is_empty() {
        output_path = s.var_safe(dir, "EXECUTABLE_OUTPUT_PATH");
    }
    let mut relapath = String::new();
    if !output_path.is_empty() {
        relapath = relative_path(project_path, &output_path);
        xml.attribute("OutputFile", &format!("{relapath}/$(ProjectName)"));
    } else {
        xml.attribute("OutputFile", "$(IntermediateDirectory)/$(ProjectName)");
    }
    xml.attribute("IntermediateDirectory", "./");
    xml.attribute("Command", "./$(ProjectName)");
    xml.attribute("CommandArguments", "");
    if !output_path.is_empty() {
        xml.attribute("WorkingDirectory", &relapath);
    } else {
        xml.attribute("WorkingDirectory", "$(IntermediateDirectory)");
    }
    xml.attribute("PauseExecWhenProcTerminates", "yes");
    xml.end_element(); // General

    xml.start_element("Debugger");
    xml.attribute("IsRemote", "no");
    xml.attribute("RemoteHostName", "");
    xml.attribute("RemoteHostPort", "");
    xml.attribute("DebuggerPath", "");
    xml.element("PostConnectCommands");
    xml.element("StartupCommands");
    xml.end_element(); // Debugger

    xml.element("PreBuild");
    xml.element("PostBuild");

    xml.start_element("CustomBuild");
    xml.attribute("Enabled", "yes");
    xml.element_value("RebuildCommand", &rebuild_command(s, dir, target_name));
    xml.element_value("CleanCommand", &clean_command(s, dir, target_name));
    xml.element_value("BuildCommand", &build_command(s, dir, target_name));
    xml.element_value("SingleFileCommand", "");
    xml.element("PreprocessFileCommand");
    xml.element_value("WorkingDirectory", "$(WorkspacePath)");
    xml.end_element(); // CustomBuild

    xml.start_element("AdditionalRules");
    xml.element("CustomPostBuild");
    xml.element("CustomPreBuild");
    xml.end_element(); // AdditionalRules
    xml.end_element(); // Configuration

    xml.start_element("GlobalSettings");
    xml.start_element("Compiler");
    xml.attribute("Options", "");
    xml.start_element("IncludePath");
    xml.attribute("Value", ".");
    xml.end_element();
    xml.end_element(); // Compiler
    xml.start_element("Linker");
    xml.attribute("Options", "");
    xml.start_element("LibraryPath");
    xml.attribute("Value", ".");
    xml.end_element();
    xml.end_element(); // Linker
    xml.start_element("ResourceCompiler");
    xml.attribute("Options", "");
    xml.end_element();
    xml.end_element(); // GlobalSettings
    xml.end_element(); // Settings
}

/// `cmExtraCodeLiteGenerator::CreateNewProjectFile` (project maps).
fn create_project_file_by_map(s: &Shared, lgs: &[usize], filename: &str) {
    let dir = lgs[0];
    let mut xml = XmlWriter::new();
    xml.start_document("utf-8");
    xml.start_element("CodeLite_Project");
    xml.attribute("Name", &s.project_name(dir));
    xml.attribute("InternalType", "");
    let mut c_files: BTreeMap<String, ()> = BTreeMap::new();
    let mut other_files: BTreeSet<String> = BTreeSet::new();
    let mut project_type = String::new();
    for lg in lgs {
        for target in s.targets(*lg) {
            project_type = collect_source_files(s, target, &mut c_files, &mut other_files);
        }
        // GLOBAL_TARGET copies return "" for the type, like upstream.
        for _ in s.global_target_names(*lg) {
            project_type = String::new();
        }
    }
    let project_path = filename_path(filename).to_owned();
    create_project_source_entries(
        s,
        c_files,
        other_files,
        &mut xml,
        &project_path,
        dir,
        &project_type,
        "",
    );
    xml.end_element();
    write_file(filename, &xml.out);
}

/// `cmExtraCodeLiteGenerator::CreateNewProjectFile` (targets).
fn create_project_file_by_target(s: &Shared, target: &Target, filename: &str) {
    let dir = target.makefile;
    let mut xml = XmlWriter::new();
    xml.start_document("utf-8");
    xml.start_element("CodeLite_Project");
    let visualname = match target.kind {
        TargetKind::StaticLibrary | TargetKind::SharedLibrary | TargetKind::ModuleLibrary => {
            format!("lib{}", target.name)
        }
        _ => target.name.clone(),
    };
    xml.attribute("Name", &visualname);
    xml.attribute("InternalType", "");
    let mut c_files: BTreeMap<String, ()> = BTreeMap::new();
    let mut other_files: BTreeSet<String> = BTreeSet::new();
    let project_type = collect_source_files(s, target, &mut c_files, &mut other_files);
    let project_path = filename_path(filename).to_owned();
    create_project_source_entries(
        s,
        c_files,
        other_files,
        &mut xml,
        &project_path,
        dir,
        &project_type,
        &target.name.clone(),
    );
    xml.end_element();
    write_file(filename, &xml.out);
}

/// `cmExtraCodeLiteGenerator::Generate`.
pub fn generate(ctx: &GenCtx, globals: &[GlobalTarget]) {
    let s = Shared::new(ctx, globals);
    // Locate the root project (the local generator at the top binary dir).
    let mut workspace_project_name = String::new();
    let mut workspace_output_dir = String::new();
    for (_, lgs) in s.project_map() {
        let dir = lgs[0];
        if dir == 0 {
            workspace_output_dir = s.binary_dir(dir).to_owned();
            workspace_project_name = s.project_name(dir);
            break;
        }
    }
    let workspace_path = workspace_output_dir.clone();
    let workspace_file = format!("{workspace_output_dir}/{workspace_project_name}.workspace");
    let use_targets = s
        .var(0, "CMAKE_CODELITE_USE_TARGETS")
        .is_some_and(|v| util::is_on(&v));

    let mut xml = XmlWriter::new();
    xml.start_document("utf-8");
    xml.start_element("CodeLite_Workspace");
    xml.attribute("Name", &workspace_project_name);

    let mut project_names: Vec<String> = Vec::new();
    if use_targets {
        // CreateProjectsByTarget
        for dir in 0..s.makefile_count() {
            for target in s.targets(dir) {
                let output_dir = s.binary_dir(dir);
                let filename = format!("{output_dir}/{}.project", target.name);
                project_names.push(target.name.clone());
                let relafilename = relative_path(&workspace_path, &filename);
                let mut visualname = target.name.clone();
                match target.kind {
                    TargetKind::SharedLibrary
                    | TargetKind::StaticLibrary
                    | TargetKind::ModuleLibrary => {
                        visualname = format!("lib{visualname}");
                        xml.start_element("Project");
                        xml.attribute("Name", &visualname);
                        xml.attribute("Path", &relafilename);
                        xml.attribute("Active", "No");
                        xml.end_element();
                        create_project_file_by_target(&s, target, &filename);
                    }
                    TargetKind::Executable => {
                        xml.start_element("Project");
                        xml.attribute("Name", &visualname);
                        xml.attribute("Path", &relafilename);
                        xml.attribute("Active", "No");
                        xml.end_element();
                        create_project_file_by_target(&s, target, &filename);
                    }
                    _ => {}
                }
            }
            // The generator's global targets (GLOBAL_TARGET rows at the end
            // of every directory's target list) are named in the build
            // matrix but get no project file.
            for name in s.global_target_names(dir) {
                project_names.push(name.to_owned());
            }
        }
    } else {
        // CreateProjectsByProjectMaps
        for (_, lgs) in s.project_map() {
            let dir = lgs[0];
            let output_dir = s.binary_dir(dir);
            let project_name = s.project_name(dir);
            project_names.push(project_name.clone());
            let filename = relative_path(
                &workspace_path,
                &format!("{output_dir}/{project_name}.project"),
            );
            create_project_file_by_map(&s, &lgs, &format!("{output_dir}/{project_name}.project"));
            xml.start_element("Project");
            xml.attribute("Name", &project_name);
            xml.attribute("Path", &filename);
            xml.attribute("Active", "No");
            xml.end_element();
        }
    }

    xml.start_element("BuildMatrix");
    xml.start_element("WorkspaceConfiguration");
    xml.attribute("Name", &configuration_name(&s, 0));
    xml.attribute("Selected", "yes");
    for name in &project_names {
        xml.start_element("Project");
        xml.attribute("Name", name);
        xml.attribute("ConfigName", &configuration_name(&s, 0));
        xml.end_element();
    }
    xml.end_element(); // WorkspaceConfiguration
    xml.end_element(); // BuildMatrix
    xml.end_element(); // CodeLite_Workspace
    write_file(&workspace_file, &xml.out);
}
