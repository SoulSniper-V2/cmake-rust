//! `cmFileAPICodemodel.cxx`: the codemodel-v2 object kind.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use md5::Digest as Md5Digest;
use sha3::Digest;

use super::{FileApi, JsonExt, jarr, jobj, jstr, juint};
use crate::commands::string::json::Json;
use crate::generator::ctx::GenCtx;
use crate::generator::install::{generates_for_config, genex_at};
use crate::generator::{flags, link, pch, target as gt};
use crate::install_model::{CopyKind, InstallAction};
use crate::interp::Interp;
use crate::messages::Frame;
use crate::state::{FileSet, Scope, Target, TargetKind};
use crate::util::{
    collapse_full_path, expand_list, filename_path, is_full_path, is_on_opt, is_subdirectory,
    relative_if_under, relative_path,
};

/// The deduplicated `backtraceGraph` (`BacktraceData`).
#[derive(Default)]
struct BtGraph {
    files: Vec<String>,
    file_map: HashMap<String, u32>,
    commands: Vec<String>,
    command_map: HashMap<String, u32>,
    nodes: Vec<Json>,
    node_map: HashMap<(u32, usize, Option<u32>, Option<u32>), u32>,
    top_source: String,
}

impl BtGraph {
    fn file(&mut self, path: &str) -> u32 {
        if let Some(i) = self.file_map.get(path) {
            return *i;
        }
        let i = self.files.len() as u32;
        self.file_map.insert(path.to_owned(), i);
        self.files.push(relative_if_under(&self.top_source, path));
        i
    }

    fn command(&mut self, name: &str) -> u32 {
        if let Some(i) = self.command_map.get(name) {
            return *i;
        }
        let i = self.commands.len() as u32;
        self.command_map.insert(name.to_owned(), i);
        self.commands.push(name.to_owned());
        i
    }

    /// `BacktraceData::Add`. `frames` runs outermost-first; `frames.last()`
    /// is the innermost (top of stack) context.
    fn add(&mut self, frames: &[Frame]) -> Option<u32> {
        if frames.is_empty() {
            return None;
        }
        let index = self.add_from(frames, frames.len() - 1);
        Some(index)
    }

    fn add_from(&mut self, frames: &[Frame], depth: usize) -> u32 {
        let frame = &frames[depth];
        let file = self.file(&frame.file);
        let command = frame
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(|name| self.command(name));
        let parent = (depth > 0).then(|| self.add_from(frames, depth - 1));
        let key = (file, frame.line, command, parent);
        if let Some(i) = self.node_map.get(&key) {
            return *i;
        }
        let mut node = jobj();
        node.set("file", juint(file as u64));
        if frame.line > 0 {
            node.set("line", Json::Int(frame.line as i64));
        }
        if let Some(command) = command {
            node.set("command", juint(command as u64));
        }
        if let Some(parent) = parent {
            node.set("parent", juint(parent as u64));
        }
        let i = self.nodes.len() as u32;
        self.node_map.insert(key, i);
        self.nodes.push(node);
        i
    }

    fn dump(self) -> Json {
        let mut graph = jobj();
        graph.set(
            "commands",
            Json::Array(self.commands.into_iter().map(jstr).collect()),
        );
        graph.set(
            "files",
            Json::Array(self.files.into_iter().map(jstr).collect()),
        );
        graph.set("nodes", Json::Array(self.nodes));
        graph
    }
}

fn jbt(graph: &mut BtGraph, object: &mut Json, frames: &[Frame]) {
    if let Some(index) = graph.add(frames) {
        object.set("backtrace", juint(index as u64));
    }
}

/// `TargetId`: name + "::@" + sha3_256(rel binary dir)[..20].
fn target_id(top_build: &str, interp: &Interp, target: &Target) -> String {
    let binary = &interp.makefiles[target.makefile].binary_dir;
    let path = relative_if_under(top_build, binary);
    let mut hasher = sha3::Sha3_256::new();
    hasher.update(path.as_bytes());
    let hash: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..20]
        .to_owned();
    format!("{}::@{hash}", target.name)
}

fn fragment(graph: &mut BtGraph, value: &str, bt: &[Frame], role: Option<&str>) -> Json {
    let mut frag = jobj();
    frag.set("fragment", jstr(value));
    if let Some(role) = role {
        frag.set("role", jstr(role));
    }
    jbt(graph, &mut frag, bt);
    frag
}

fn type_name(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Executable => "EXECUTABLE",
        TargetKind::StaticLibrary => "STATIC_LIBRARY",
        TargetKind::SharedLibrary => "SHARED_LIBRARY",
        TargetKind::ModuleLibrary => "MODULE_LIBRARY",
        TargetKind::ObjectLibrary => "OBJECT_LIBRARY",
        TargetKind::InterfaceLibrary => "INTERFACE_LIBRARY",
        _ => "UTILITY",
    }
}

struct ProjectData {
    name: String,
    parent: Option<u32>,
    children: Vec<u32>,
    directory_indexes: Vec<u32>,
    target_indexes: Vec<u32>,
}

struct DirData {
    makefile: usize,
    parent: Option<u32>,
    children: Vec<u32>,
    project_index: u32,
    target_indexes: Vec<u32>,
    has_install_rule: bool,
}

pub(super) struct Codemodel<'a, 'i> {
    api: &'a mut FileApi,
    ctx: &'a GenCtx<'i>,
    config: String,
    top_source: String,
    top_build: String,
    dirs: Vec<DirData>,
    dir_index: HashMap<usize, u32>,
    projects: Vec<ProjectData>,
    target_index: HashMap<usize, u32>,
    target_ids: HashMap<usize, String>,
}

impl<'a, 'i> Codemodel<'a, 'i> {
    fn project_name(&self, mf: usize) -> String {
        let mut index = Some(mf);
        while let Some(i) = index {
            let m = &self.ctx.interp.makefiles[i];
            if let Some(name) = m.props.get("PROJECT_NAME") {
                return name.to_owned();
            }
            index = m.parent;
        }
        String::new()
    }

    /// `ProcessDirectories` + `AddProject`.
    fn process_directories(&mut self) {
        for (index, mf) in self.ctx.interp.makefiles.iter().enumerate() {
            let di = self.dirs.len() as u32;
            self.dir_index.insert(index, di);
            let parent = mf.parent.and_then(|p| self.dir_index.get(&p).copied());
            let name = self.project_name(index);
            let project_index = match parent {
                Some(p) if self.project_name(mf.parent.unwrap()) == name => {
                    self.dirs[p as usize].project_index
                }
                _ => {
                    let pi = self.projects.len() as u32;
                    let parent_project = parent.map(|p| {
                        let parent = self.dirs[p as usize].project_index;
                        self.projects[parent as usize].children.push(pi);
                        parent
                    });
                    self.projects.push(ProjectData {
                        name,
                        parent: parent_project,
                        children: Vec::new(),
                        directory_indexes: Vec::new(),
                        target_indexes: Vec::new(),
                    });
                    pi
                }
            };
            if let Some(p) = parent {
                self.dirs[p as usize].children.push(di);
            }
            self.projects[project_index as usize]
                .directory_indexes
                .push(di);
            self.dirs.push(DirData {
                makefile: index,
                parent,
                children: Vec::new(),
                project_index,
                target_indexes: Vec::new(),
                has_install_rule: false,
            });
        }
        // hasInstallRule: own rules or any child's (children processed first).
        for di in (0..self.dirs.len()).rev() {
            let mf = self.dirs[di].makefile;
            let mut has = self
                .ctx
                .interp
                .ext
                .install
                .rules
                .get(&mf)
                .is_some_and(|rules| !rules.is_empty());
            if !has {
                let children = self.dirs[di].children.clone();
                has = children
                    .iter()
                    .any(|c| self.dirs[*c as usize].has_install_rule);
            }
            self.dirs[di].has_install_rule = has;
        }
    }

    fn dump(&mut self) -> Json {
        self.process_directories();
        let mut configuration = jobj();
        configuration.set("name", jstr(&self.config));
        configuration.set("targets", self.dump_targets());
        configuration.set("directories", self.dump_directories());
        configuration.set("projects", self.dump_projects());
        let mut paths = jobj();
        paths.set("source", jstr(&self.top_source));
        paths.set("build", jstr(&self.top_build));
        let mut codemodel = jobj();
        codemodel.set("paths", paths);
        codemodel.set("configurations", Json::Array(vec![configuration]));
        codemodel
    }

    fn dump_targets(&mut self) -> Json {
        let mut result = jarr();
        let mut targets: Vec<&Target> = self
            .ctx
            .targets_in_order()
            .into_iter()
            .filter(|t| {
                crate::generator::link::in_build_system(t)
                    && t.kind != TargetKind::GlobalTarget
                    && !t.name.starts_with("__cmake_")
            })
            .collect();
        targets.sort_by(|a, b| a.name.cmp(&b.name));
        for target in targets {
            let ti = result_index(&result);
            self.target_index.insert(target as *const _ as usize, ti);
            self.target_ids.insert(
                target as *const _ as usize,
                target_id(&self.top_build, self.ctx.interp, target),
            );
            let entry = self.dump_target_index(target, ti);
            result.push(entry);
        }
        result
    }

    fn dump_target_index(&mut self, target: &Target, ti: u32) -> Json {
        let mut prefix = format!("target-{}", target.name);
        if !self.config.is_empty() {
            prefix += &format!("-{}", self.config);
        }
        let object = self.dump_target(target);
        let file = self.api.write_json_file(&object, &prefix, false);
        let mut entry = jobj();
        entry.set("jsonFile", jstr(file));
        entry.set("name", jstr(&target.name));
        entry.set("id", jstr(&self.target_ids[&(target as *const _ as usize)]));
        let di = self.dir_index[&target.makefile];
        entry.set("directoryIndex", juint(di as u64));
        let pi = self.dirs[di as usize].project_index;
        entry.set("projectIndex", juint(pi as u64));
        self.dirs[di as usize].target_indexes.push(ti);
        self.projects[pi as usize].target_indexes.push(ti);
        entry
    }

    fn dump_directories(&mut self) -> Json {
        let mut result = jarr();
        for di in 0..self.dirs.len() {
            result.push(self.dump_directory(di as u32));
        }
        result
    }

    fn dump_directory(&mut self, di: u32) -> Json {
        let mf = self.dirs[di as usize].makefile;
        let m = &self.ctx.interp.makefiles[mf];
        let source_rel = relative_if_under(&self.top_source, &m.source_dir);
        let build_rel = relative_if_under(&self.top_build, &m.binary_dir);

        let mut prefix = "directory".to_owned();
        if !crate::util::is_full_path(&build_rel) {
            prefix = format!("{prefix}-{build_rel}");
        } else if !crate::util::is_full_path(&source_rel) {
            prefix = format!("{prefix}-{source_rel}");
        }
        prefix = prefix.replace(['/', '\\'], ".");
        if !self.config.is_empty() {
            prefix += &format!("-{}", self.config);
        }
        let object = self.dump_directory_object(mf);
        let file = self.api.write_json_file(&object, &prefix, false);

        let mut entry = jobj();
        entry.set("jsonFile", jstr(file));
        entry.set("source", jstr(&source_rel));
        entry.set("build", jstr(&build_rel));
        let d = &self.dirs[di as usize];
        if let Some(parent) = d.parent {
            entry.set("parentIndex", juint(parent as u64));
        }
        if !d.children.is_empty() {
            entry.set(
                "childIndexes",
                Json::Array(d.children.iter().map(|c| juint(*c as u64)).collect()),
            );
        }
        entry.set("projectIndex", juint(d.project_index as u64));
        if !d.target_indexes.is_empty() {
            entry.set(
                "targetIndexes",
                Json::Array(d.target_indexes.iter().map(|t| juint(*t as u64)).collect()),
            );
        }
        if let Some(Some(version)) = self
            .ctx
            .interp
            .dir_final_vars
            .get(mf)
            .and_then(|defs| defs.get("CMAKE_MINIMUM_REQUIRED_VERSION"))
        {
            let mut v = jobj();
            v.set("string", jstr(version.clone()));
            entry.set("minimumCMakeVersion", v);
        }
        if d.has_install_rule {
            entry.set("hasInstallRule", Json::Bool(true));
        }
        entry
    }

    fn dump_directory_object(&mut self, mf: usize) -> Json {
        let mut graph = BtGraph {
            top_source: self.top_source.clone(),
            ..Default::default()
        };
        let m = &self.ctx.interp.makefiles[mf];
        let mut paths = jobj();
        paths.set(
            "source",
            jstr(relative_if_under(&self.top_source, &m.source_dir)),
        );
        paths.set(
            "build",
            jstr(relative_if_under(&self.top_build, &m.binary_dir)),
        );
        let mut installers = jarr();
        if let Some(rules) = self.ctx.interp.ext.install.rules.get(&mf) {
            for rule in rules {
                self.dump_installer(mf, &mut graph, &mut installers, rule);
            }
        }
        let mut object = jobj();
        object.set("paths", paths);
        object.set("installers", installers);
        object.set("backtraceGraph", graph.dump());
        object
    }

    fn dump_installer(
        &mut self,
        mf: usize,
        graph: &mut BtGraph,
        installers: &mut Json,
        rule: &crate::install_model::InstallRule,
    ) {
        let opts = &rule.options;
        if !generates_for_config(&opts.configurations, &self.config) {
            return;
        }
        use crate::install_model::Namelink;
        let mut installer = jobj();
        let dest = || genex_at(self.ctx, mf, &opts.destination);
        match &rule.action {
            InstallAction::Target {
                name,
                namelink,
                importlink,
                implib,
                imported,
            } => {
                installer.set("destination", jstr(dest()));
                if *imported {
                    // `cmInstallImportedRuntimeArtifactsGenerator`.
                    installer.set("type", jstr("importedRuntimeArtifacts"));
                    if opts.optional {
                        installer.set("isOptional", Json::Bool(true));
                    }
                    installer.set("component", jstr(&opts.component));
                    if opts.exclude {
                        installer.set("isExcludeFromAll", Json::Bool(true));
                    }
                    jbt(graph, &mut installer, &rule.backtrace);
                    installers.push(installer);
                    return;
                }
                installer.set("type", jstr("target"));
                let Some(target) = self.find_target(name) else {
                    return;
                };
                installer.set("targetId", jstr(self.id_of(target)));
                installer.set("targetIndex", juint(self.index_of(target) as u64));
                let mut paths = jarr();
                if target.kind == TargetKind::ObjectLibrary {
                    // `GetFiles` for OBJECT libraries: the object dir under
                    // `objects[-<config>]/<target>`.
                    let from_dir = gt::object_directory(self.ctx, target, &self.config);
                    let to_dir = crate::generator::install::object_subdir(target, &self.config);
                    for obj in crate::generator::install::object_names(self.ctx, target) {
                        paths.push(installer_path(
                            &relative_if_under(&self.top_build, &format!("{from_dir}/{obj}")),
                            &format!("{to_dir}/{obj}"),
                        ));
                    }
                } else {
                    let names = gt::names(self.ctx, target, &self.config);
                    let (_, _, mode, pairs) = crate::generator::install::target_files(
                        self.ctx,
                        target,
                        &names,
                        *namelink,
                        *importlink,
                        *implib,
                        &rule.backtrace,
                    );
                    if pairs.is_empty() {
                        // `DumpInstaller`: no files means no installer.
                        return;
                    }
                    for (from, to) in &pairs {
                        paths.push(installer_path(
                            &relative_if_under(&self.top_build, from),
                            to,
                        ));
                    }
                    match mode {
                        Namelink::None => {}
                        Namelink::Only => installer.set("targetInstallNamelink", jstr("only")),
                        Namelink::Skip => installer.set("targetInstallNamelink", jstr("skip")),
                    }
                }
                installer.set("paths", paths);
                if opts.optional {
                    installer.set("isOptional", Json::Bool(true));
                }
                if *implib {
                    installer.set("targetIsImportLibrary", Json::Bool(true));
                }
            }
            InstallAction::Copy { kind, paths, .. } => {
                installer.set(
                    "type",
                    jstr(if matches!(kind, CopyKind::Directory) {
                        "directory"
                    } else {
                        "file"
                    }),
                );
                installer.set("destination", jstr(dest()));
                let rename = genex_at(self.ctx, mf, &opts.rename);
                let mut out_paths = jarr();
                if matches!(kind, CopyKind::Directory) {
                    let mut evaluated = Vec::new();
                    for dir in paths {
                        evaluated.extend(expand_list(&genex_at(self.ctx, mf, dir), false));
                    }
                    for dir in &evaluated {
                        if let Some(stripped) = dir.strip_suffix('/') {
                            out_paths.push(installer_path(
                                &relative_if_under(&self.top_source, stripped),
                                ".",
                            ));
                        } else {
                            out_paths.push(installer_path(
                                &relative_if_under(&self.top_source, dir),
                                &crate::util::filename_name(dir),
                            ));
                        }
                    }
                } else {
                    let mut evaluated = Vec::new();
                    for path in paths {
                        evaluated.extend(expand_list(&genex_at(self.ctx, mf, path), false));
                    }
                    if !rename.is_empty() && evaluated.len() == 1 {
                        out_paths.push(installer_path(
                            &relative_if_under(&self.top_source, &evaluated[0]),
                            &rename,
                        ));
                    } else {
                        for path in &evaluated {
                            out_paths.push(jstr(relative_if_under(&self.top_source, path)));
                        }
                    }
                }
                if out_paths.is_empty_container() {
                    return;
                }
                installer.set("paths", out_paths);
                if opts.optional {
                    installer.set("isOptional", Json::Bool(true));
                }
            }
            InstallAction::Script {
                text,
                is_code,
                all_components: _,
            } => {
                if *is_code {
                    installer.set("type", jstr("code"));
                } else {
                    installer.set("type", jstr("script"));
                    installer.set(
                        "scriptFile",
                        jstr(relative_if_under(&self.top_source, text)),
                    );
                }
            }
            InstallAction::Export(export) => {
                self.dump_export_installer(
                    &mut installer,
                    dest(),
                    &export.set,
                    &opts.destination,
                    mf,
                    &export.filename,
                );
            }
            // cmInstallPackageInfoExportGenerator is a cmInstallExportGenerator
            // whose file name is GetPackageFileName().
            InstallAction::PackageInfo { set, arguments } => {
                self.dump_export_installer(
                    &mut installer,
                    dest(),
                    set,
                    &opts.destination,
                    mf,
                    &arguments.package_file_name(),
                );
            }
            InstallAction::FileSet { name, set } => {
                let Some(target) = self.find_target(name) else {
                    return;
                };
                let Some(file_set) = target.file_sets.get(set) else {
                    return;
                };
                installer.set("type", jstr("fileSet"));
                installer.set("destination", jstr(dest()));
                let dirs = self.eval_file_set_dirs(target, file_set);
                let files = self.eval_file_set_files(target, file_set, &dirs);
                let mut out_paths = jarr();
                for (dir, file) in &files {
                    let d = if dir.is_empty() {
                        String::new()
                    } else {
                        format!("{dir}/")
                    };
                    out_paths.push(installer_path(
                        &relative_if_under(&self.top_source, file),
                        &format!("{}{}", d, crate::util::filename_name(file)),
                    ));
                }
                installer.set("paths", out_paths);
                installer.set("fileSetName", jstr(file_set_name(set)));
                installer.set("fileSetType", jstr(&file_set.kind));
                installer.set(
                    "fileSetDirectories",
                    Json::Array(
                        dirs.iter()
                            .map(|d| jstr(relative_if_under(&self.top_source, d)))
                            .collect(),
                    ),
                );
                let mut tgt = jobj();
                tgt.set("id", jstr(self.id_of(target)));
                tgt.set("index", juint(self.index_of(target) as u64));
                installer.set("fileSetTarget", tgt);
                if opts.optional {
                    installer.set("isOptional", Json::Bool(true));
                }
            }
            InstallAction::Bmi(name) => {
                installer.set("type", jstr("cxxModuleBmi"));
                installer.set("destination", jstr(dest()));
                if let Some(target) = self.find_target(name) {
                    let mut tgt = jobj();
                    tgt.set("id", jstr(self.id_of(target)));
                    tgt.set("index", juint(self.index_of(target) as u64));
                    installer.set("cxxModuleBmiTarget", tgt);
                }
                if opts.optional {
                    installer.set("isOptional", Json::Bool(true));
                }
            }
            InstallAction::RuntimeDependencies { .. } => {
                // `cmInstallGetRuntimeDependenciesGenerator` is an
                // implementation detail and is not reported.
                return;
            }
            InstallAction::RuntimeDepSet { kind, set, .. } => {
                installer.set("type", jstr("runtimeDependencySet"));
                installer.set("destination", jstr(dest()));
                let name = set_name(set);
                if !name.is_empty() {
                    installer.set("runtimeDependencySetName", jstr(name));
                }
                installer.set(
                    "runtimeDependencySetType",
                    jstr(if *kind == crate::install_model::DepSetKind::Framework {
                        "framework"
                    } else {
                        "library"
                    }),
                );
            }
            InstallAction::LegacyFiles { args, programs } => {
                installer.set("type", jstr("file"));
                installer.set("destination", jstr(dest()));
                let _ = (args, programs);
            }
            InstallAction::LegacyTarget(_) => return,
        }
        installer.set("component", jstr(&opts.component));
        if matches!(
            &rule.action,
            InstallAction::Script {
                all_components: true,
                ..
            }
        ) {
            installer.set("isForAllComponents", Json::Bool(true));
        }
        if opts.exclude {
            installer.set("isExcludeFromAll", Json::Bool(true));
        }
        jbt(graph, &mut installer, &rule.backtrace);
        installers.push(installer);
    }

    fn find_target(&self, name: &str) -> Option<&'i Target> {
        self.ctx.target(name)
    }

    fn id_of(&self, target: &Target) -> String {
        target_id(&self.top_build, self.ctx.interp, target)
    }

    fn index_of(&self, target: &Target) -> u32 {
        self.target_index
            .get(&(target as *const _ as usize))
            .copied()
            .unwrap_or(0)
    }

    fn dump_projects(&self) -> Json {
        let mut result = jarr();
        for p in &self.projects {
            let mut project = jobj();
            project.set("name", jstr(&p.name));
            if let Some(parent) = p.parent {
                project.set("parentIndex", juint(parent as u64));
            }
            if !p.children.is_empty() {
                project.set(
                    "childIndexes",
                    Json::Array(p.children.iter().map(|c| juint(*c as u64)).collect()),
                );
            }
            project.set(
                "directoryIndexes",
                Json::Array(
                    p.directory_indexes
                        .iter()
                        .map(|d| juint(*d as u64))
                        .collect(),
                ),
            );
            if !p.target_indexes.is_empty() {
                project.set(
                    "targetIndexes",
                    Json::Array(p.target_indexes.iter().map(|t| juint(*t as u64)).collect()),
                );
            }
            result.push(project);
        }
        result
    }

    // -----------------------------------------------------------------
    // Target object (Target::Dump).

    fn dump_target(&mut self, target: &Target) -> Json {
        let mut graph = BtGraph {
            top_source: self.top_source.clone(),
            ..Default::default()
        };
        let mut t = jobj();
        t.set("name", jstr(&target.name));
        t.set("type", jstr(type_name(target.kind)));
        t.set("id", jstr(self.id_of(target)));
        let m = &self.ctx.interp.makefiles[target.makefile];
        let mut paths = jobj();
        paths.set(
            "source",
            jstr(relative_if_under(&self.top_source, &m.source_dir)),
        );
        paths.set(
            "build",
            jstr(relative_if_under(&self.top_build, &m.binary_dir)),
        );
        t.set("paths", paths);
        if target.generator_provided {
            t.set("isGeneratorProvided", Json::Bool(true));
        }
        jbt(&mut graph, &mut t, &target.backtrace);
        if target.have_install_rule {
            t.set("install", self.dump_install(&mut graph, target));
        }
        if matches!(
            target.kind,
            TargetKind::Executable
                | TargetKind::SharedLibrary
                | TargetKind::ModuleLibrary
                | TargetKind::StaticLibrary
                | TargetKind::ObjectLibrary
        ) {
            let artifacts = self.dump_artifacts(target);
            if !artifacts.is_empty_container() {
                t.set("artifacts", artifacts);
            }
        }
        match target.kind {
            TargetKind::Executable | TargetKind::SharedLibrary | TargetKind::ModuleLibrary => {
                t.set(
                    "nameOnDisk",
                    jstr(gt::full_name(
                        self.ctx,
                        target,
                        &self.config,
                        gt::ArtifactType::Runtime,
                    )),
                );
                t.set("link", self.dump_link(&mut graph, target));
            }
            TargetKind::StaticLibrary => {
                t.set(
                    "nameOnDisk",
                    jstr(gt::full_name(
                        self.ctx,
                        target,
                        &self.config,
                        gt::ArtifactType::Runtime,
                    )),
                );
                t.set("archive", self.dump_archive(&mut graph, target));
            }
            _ => {}
        }
        if target.kind == TargetKind::Executable {
            let launchers = self.dump_launchers(target);
            if !launchers.is_empty_container() {
                t.set("launchers", launchers);
            }
        }
        if let Some(wd) = target.props.get("DEBUGGER_WORKING_DIRECTORY") {
            let mut debugger = jobj();
            debugger.set("workingDirectory", jstr(wd));
            t.set("debugger", debugger);
        }
        let deps = self.dump_dependencies(&mut graph, target);
        if !deps.is_empty_container() {
            t.set("dependencies", deps);
        }

        // ProcessLanguages.
        let sources = gt::sources(self.ctx, target, &self.config);
        let mut languages = BTreeSet::new();
        gt::target_languages(self.ctx, target, &self.config, &sources, &mut languages);
        let mut lang_data: BTreeMap<String, CompileData> = BTreeMap::new();
        for lang in &languages {
            lang_data.insert(lang.clone(), self.compile_data(&mut graph, target, lang));
        }

        // File sets + fsdb.
        let mut fsdb: BTreeMap<String, u32> = BTreeMap::new();
        let file_sets = self.dump_file_sets(target, &mut fsdb);
        if !file_sets.is_empty_container() && !matches!(file_sets, Json::Null) {
            t.set("fileSets", file_sets);
        }

        // Sources + source groups + compile groups.
        let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
        let mut compile_groups: Vec<(CompileData, Vec<u32>)> = Vec::new();
        let mut out_sources = jarr();
        for (si, source) in sources.iter().enumerate() {
            out_sources.push(self.dump_source(
                &mut graph,
                target,
                source,
                si as u32,
                &fsdb,
                &mut groups,
                &mut compile_groups,
                &lang_data,
            ));
        }
        t.set("sources", out_sources);
        if let Some(folder) = target.props.get("FOLDER") {
            let mut f = jobj();
            f.set("name", jstr(folder));
            t.set("folder", f);
        }
        if !groups.is_empty() {
            let mut sg = jarr();
            for (name, indexes) in groups {
                let mut g = jobj();
                g.set("name", jstr(name));
                g.set(
                    "sourceIndexes",
                    Json::Array(indexes.into_iter().map(|i| juint(i as u64)).collect()),
                );
                sg.push(g);
            }
            t.set("sourceGroups", sg);
        }
        if !compile_groups.is_empty() {
            let mut cg = jarr();
            for (data, indexes) in compile_groups {
                let merged = merge_compile_data(&data, &lang_data);
                let mut g = self.dump_compile_data(&mut graph, &merged);
                g.set(
                    "sourceIndexes",
                    Json::Array(indexes.into_iter().map(|i| juint(i as u64)).collect()),
                );
                cg.push(g);
            }
            t.set("compileGroups", cg);
        }
        t.set("backtraceGraph", graph.dump());
        t
    }

    /// Target-level CompileData (`ProcessLanguage`).
    /// The cmInstallExportGenerator branch of `DumpInstaller`.
    fn dump_export_installer(
        &self,
        installer: &mut Json,
        destination: String,
        set_name: &str,
        raw_destination: &str,
        mf: usize,
        file_name: &str,
    ) {
        installer.set("type", jstr("export"));
        installer.set("destination", jstr(destination));
        installer.set("exportName", jstr(set_name));
        let mut targets = jarr();
        if let Some(set) = self.ctx.interp.ext.install.export_sets.get(set_name) {
            for t in &set.targets {
                let mut obj = jobj();
                if let Some(target) = self.find_target(&t.name) {
                    obj.set("id", jstr(self.id_of(target)));
                    obj.set("index", juint(self.index_of(target) as u64));
                }
                targets.push(obj);
            }
        }
        installer.set("exportTargets", targets);
        // `GetMainImportFile`: CMakeFiles/Export/<md5(dest)>/<file>.
        let temp = format!(
            "{}/CMakeFiles/Export/{:x}",
            self.ctx.interp.makefiles[mf].binary_dir,
            md5::Md5::digest(raw_destination.as_bytes()),
        );
        let mut paths = jarr();
        paths.push(jstr(relative_if_under(
            &self.top_build,
            &format!("{temp}/{file_name}"),
        )));
        installer.set("paths", paths);
    }

    /// `Target::ProcessLanguage`: the backtraces enter the graph as
    /// `ToJBT` converts them: flags, defines, include directories, precompile
    /// headers, then the language standard.
    fn compile_data(&self, graph: &mut BtGraph, target: &Target, lang: &str) -> CompileData {
        let sysroot = self
            .ctx
            .var(target.makefile, "CMAKE_SYSROOT_COMPILE")
            .or_else(|| self.ctx.var(target.makefile, "CMAKE_SYSROOT"))
            .unwrap_or_default();
        let flags = flags::flag_fragments(self.ctx, target, &self.config, lang);
        for (_, bt) in &flags {
            graph.add(bt);
        }
        let defines = flags::define_items(self.ctx, target, &self.config, lang);
        for (_, bt) in &defines {
            graph.add(bt);
        }
        let (includes, frameworks) = split_frameworks(
            target,
            flags::include_items(self.ctx, target, &self.config, lang),
            graph,
        );
        let pch = flags::pch_items(self.ctx, target, &self.config, lang);
        for (_, bt) in &pch {
            graph.add(bt);
        }
        let standard = language_standard(self.ctx, target, &self.config, lang);
        if let Some((_, bts)) = &standard {
            for bt in bts {
                graph.add(bt);
            }
        }
        CompileData {
            language: lang.to_owned(),
            sysroot,
            flags,
            defines,
            includes,
            frameworks,
            pch,
            standard,
        }
    }

    /// Per-source CompileData (`BuildCompileData`).
    fn source_compile_data(
        &self,
        graph: &mut BtGraph,
        target: &Target,
        source: &gt::SourceInfo,
        lang: &str,
    ) -> CompileData {
        let _ = graph;
        let mf = target.makefile;
        let mut data = CompileData {
            language: lang.to_owned(),
            ..CompileData::default()
        };
        let prop = |name: &str| gt::source_prop(self.ctx, mf, &source.path, name);
        if let Some(flags_raw) = prop("COMPILE_FLAGS") {
            let evaluated = self.ctx.genex(&flags_raw, Some(target), Some(lang));
            if !evaluated.trim().is_empty() {
                data.flags.push((evaluated, Vec::new()));
            }
        }
        // cmSourceFile::GetCompileOptions: one fragment per backtraced entry.
        for (options, bt) in gt::source_prop_entries(self.ctx, mf, &source.path, "COMPILE_OPTIONS")
        {
            let mut tmp = String::new();
            for option in expand_list(&self.ctx.genex(&options, Some(target), Some(lang)), false) {
                for o in target_options_split(&option) {
                    crate::generator::flags::append(&mut tmp, &o);
                }
            }
            if !tmp.is_empty() {
                data.flags.push((tmp, bt));
            }
        }
        // PCH options.
        if !lang.is_empty() && !is_on_opt(prop("SKIP_PRECOMPILE_HEADERS").as_deref()) {
            let mut pch_sources = std::collections::HashMap::new();
            for arch in pch::pch_archs(self.ctx, target, &self.config, lang) {
                let pch_source = pch::pch_source(self.ctx, target, &self.config, lang, &arch);
                if !pch_source.is_empty() {
                    pch_sources.insert(pch_source, arch);
                }
            }
            if !pch_sources.is_empty() {
                let options = match pch_sources.get(&source.path) {
                    Some(arch) => {
                        pch::pch_create_options(self.ctx, target, &self.config, lang, arch)
                    }
                    None => pch::pch_use_options(self.ctx, target, &self.config, lang, ""),
                };
                let evaluated = self.ctx.genex(&options, Some(target), Some(lang));
                let mut tmp = String::new();
                for o in expand_list(&evaluated, false) {
                    crate::generator::flags::append(&mut tmp, &o);
                }
                if !tmp.is_empty() {
                    data.flags.push((tmp, Vec::new()));
                }
            }
        }
        // cmSourceFile::GetIncludeDirectories: each backtraced entry
        // expands to a list whose items share the entry backtrace.
        let mut file_includes = Vec::new();
        for (includes, bt) in
            gt::source_prop_entries(self.ctx, mf, &source.path, "INCLUDE_DIRECTORIES")
        {
            for dir in expand_list(&self.ctx.genex(&includes, Some(target), Some(lang)), false) {
                let system =
                    flags::is_system_include_directory(self.ctx, target, &self.config, lang, &dir);
                file_includes.push((dir, system, bt.clone()));
            }
        }
        let (includes, frameworks) = split_frameworks(target, file_includes, graph);
        data.includes = includes;
        data.frameworks = frameworks;
        // cmSourceFile::GetCompileDefinitions likewise.
        for (defines, bt) in
            gt::source_prop_entries(self.ctx, mf, &source.path, "COMPILE_DEFINITIONS")
        {
            for define in expand_list(&self.ctx.genex(&defines, Some(target), Some(lang)), false) {
                let d = define.strip_prefix("-D").unwrap_or(&define).to_owned();
                data.defines.push((d, bt.clone()));
            }
        }
        let config_prop = format!("COMPILE_DEFINITIONS_{}", self.config.to_ascii_uppercase());
        if !self.config.is_empty()
            && let Some(defines) = prop(&config_prop)
        {
            for define in expand_list(&self.ctx.genex(&defines, Some(target), Some(lang)), false) {
                let d = define.strip_prefix("-D").unwrap_or(&define).to_owned();
                data.defines.push((d, Vec::new()));
            }
        }
        data.defines.sort_by(|a, b| a.0.cmp(&b.0));
        data.defines.dedup_by(|a, b| a.0 == b.0);
        data
    }

    fn dump_file_sets(&self, target: &Target, fsdb: &mut BTreeMap<String, u32>) -> Json {
        if target.file_sets.is_empty() {
            return Json::Null;
        }
        let mut out = jarr();
        for (index, (name, set)) in target.file_sets.iter().enumerate() {
            let dirs = self.eval_file_set_dirs(target, set);
            let mut fs = jobj();
            fs.set("name", jstr(name));
            fs.set("type", jstr(&set.kind));
            fs.set(
                "visibility",
                jstr(match set.scope {
                    Scope::Private => "PRIVATE",
                    Scope::Public => "PUBLIC",
                    Scope::Interface => "INTERFACE",
                }),
            );
            fs.set(
                "baseDirectories",
                Json::Array(
                    dirs.iter()
                        .map(|d| jstr(relative_if_under(&self.top_source, d)))
                        .collect(),
                ),
            );
            out.push(fs);
            for (dir, file) in self.eval_file_set_files(target, set, &dirs) {
                let key = if dir.is_empty() {
                    file
                } else {
                    format!("{dir}/{file}")
                };
                fsdb.insert(key, index as u32);
            }
        }
        out
    }

    fn eval_file_set_dirs(&self, target: &Target, set: &FileSet) -> Vec<String> {
        let source_dir = &self.ctx.interp.makefiles[target.makefile].source_dir;
        let mut dirs = Vec::new();
        for entry in &set.base_dirs {
            for part in expand_list(&entry.0, false) {
                for dir in expand_list(&self.ctx.genex(&part, Some(target), None), false) {
                    if dir.is_empty() {
                        continue;
                    }
                    if is_full_path(&dir) {
                        dirs.push(dir);
                    } else {
                        dirs.push(format!("{source_dir}/{dir}"));
                    }
                }
            }
        }
        dirs
    }

    fn eval_file_set_files(
        &self,
        target: &Target,
        set: &FileSet,
        dirs: &[String],
    ) -> Vec<(String, String)> {
        let source_dir = &self.ctx.interp.makefiles[target.makefile].source_dir;
        let mut out = Vec::new();
        for entry in &set.files {
            for part in expand_list(&entry.0, false) {
                for file in expand_list(&self.ctx.genex(&part, Some(target), None), false) {
                    if file.is_empty() {
                        continue;
                    }
                    let file = if is_full_path(&file) {
                        file
                    } else {
                        format!("{source_dir}/{file}")
                    };
                    let collapsed_file = collapse_full_path(&file, source_dir);
                    let mut rel_dir = None;
                    for dir in dirs {
                        let collapsed_dir = collapse_full_path(dir, source_dir);
                        if is_subdirectory(&collapsed_file, &collapsed_dir) {
                            let rel = relative_path(&collapsed_dir, &collapsed_file);
                            rel_dir = Some(filename_path(&rel));
                            break;
                        }
                    }
                    if let Some(rel_dir) = rel_dir {
                        out.push((rel_dir, file));
                    }
                }
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn dump_source(
        &self,
        graph: &mut BtGraph,
        target: &Target,
        source: &gt::SourceInfo,
        si: u32,
        fsdb: &BTreeMap<String, u32>,
        groups: &mut Vec<(String, Vec<u32>)>,
        compile_groups: &mut Vec<(CompileData, Vec<u32>)>,
        _lang_data: &BTreeMap<String, CompileData>,
    ) -> Json {
        let mut s = jobj();
        s.set(
            "path",
            jstr(relative_if_under(&self.top_source, &source.path)),
        );
        if source.generated {
            s.set("isGenerated", Json::Bool(true));
        }
        jbt(graph, &mut s, &source.backtrace);
        if let Some(index) = fsdb.get(&source.path) {
            s.set("fileSetIndex", juint(*index as u64));
        }
        // cmMakefile::AddTargetObject files the object in the reserved
        // "Object Libraries" group, which wins over the "Object Files" regex.
        let group_name = if source.object_library.is_some() {
            Some("Object Libraries".to_owned())
        } else if let Some(name) = self.explicit_source_group(target, &source.path) {
            Some(name)
        } else {
            // addFileSetEntry files HEADERS set members in "Header Files"
            // unless an explicit source_group() assignment claimed them.
            let headers = fsdb
                .get(&source.path)
                .and_then(|i| target.file_sets.iter().nth(*i as usize))
                .is_some_and(|(_, set)| set.kind == "HEADERS");
            if headers {
                Some("Header Files".to_owned())
            } else {
                self.source_group(target, &source.path)
            }
        };
        if let Some(name) = group_name {
            let pos = groups
                .iter()
                .position(|(n, _)| *n == name)
                .unwrap_or_else(|| {
                    groups.push((name.clone(), Vec::new()));
                    groups.len() - 1
                });
            groups[pos].1.push(si);
            s.set("sourceGroupIndex", juint(pos as u64));
        }
        if matches!(source.kind, gt::SourceKind::ObjectSource) && !source.language.is_empty() {
            let data = self.source_compile_data(graph, target, source, &source.language);
            let pos = compile_groups
                .iter()
                .position(|(d, _)| *d == data)
                .unwrap_or_else(|| {
                    compile_groups.push((data.clone(), Vec::new()));
                    compile_groups.len() - 1
                });
            compile_groups[pos].1.push(si);
            s.set("compileGroupIndex", juint(pos as u64));
        }
        s
    }

    /// The explicit-file pass of `cmMakefile::FindSourceGroup`
    /// (`MatchChildrenFiles`): a `source_group()`-assigned group for `path`.
    fn explicit_source_group(&self, target: &Target, path: &str) -> Option<String> {
        let groups = self.ctx.interp.ext.source_groups.get(&target.makefile);
        let mut named: Vec<(&String, &crate::state::SourceGroup)> =
            groups.map(|g| g.iter().collect()).unwrap_or_default();
        named.sort_by_key(|(_, group)| group.seq);
        for (name, group) in named.iter().rev() {
            if group.files.contains(path)
                || group.files.iter().any(|f| path.ends_with(&format!("/{f}")))
            {
                return Some((*name).clone());
            }
        }
        None
    }

    /// `cmMakefile::FindSourceGroup`.
    fn source_group(&self, target: &Target, path: &str) -> Option<String> {
        let groups = self.ctx.interp.ext.source_groups.get(&target.makefile);
        let mut named: Vec<(&String, &crate::state::SourceGroup)> =
            groups.map(|g| g.iter().collect()).unwrap_or_default();
        named.sort_by_key(|(_, group)| group.seq);
        // Check explicit file lists first (reverse order), then regexes.
        if let Some(name) = self.explicit_source_group(target, path) {
            return Some(name);
        }
        let check = |name: &str, regex: &str| -> Option<String> {
            crate::cmregex::Regex::new(regex)
                .ok()
                .and_then(|r| r.find(path).map(|_| name.to_owned()))
        };
        for (name, group) in named.iter().rev() {
            if let Some(regex) = &group.regex
                && let Some(found) = check(name, regex)
            {
                return Some(found);
            }
        }
        let _ = &mut named;
        // cmMakefile's default groups, in constructor order; FindSourceGroup
        // scans them in reverse.
        const DEFAULTS: &[(&str, &str)] = &[
            ("Object Libraries", "^MATCH_NO_SOURCES$"),
            ("Object Files", "\\.(lo|o|obj)$"),
            (
                "Resources",
                "\\.(pdf|plist|png|jpeg|jpg|storyboard|xcassets)$",
            ),
            ("CMake Rules", "\\.rule$"),
            ("Precompile Header File", "cmake_pch(_[^.]+)?\\.(h|hxx)$"),
            ("Header Files", "\\.(h|hh|h\\+\\+|hm|hpp|hxx|in|txx|inl)$"),
            (
                "Source Files",
                "\\.(C|F|M|c|c\\+\\+|cc|cpp|mpp|cxx|ixx|cppm|ccm|cxxm|c\\+\\+m|cu|f|f90|for|fpp|ftn|m|mm|rc|def|r|odl|idl|hpj|bat)$",
            ),
            ("", ".*"),
        ];
        for (name, regex) in DEFAULTS {
            if let Some(found) = check(name, regex) {
                return Some(found);
            }
        }
        None
    }

    fn dump_compile_data(&self, graph: &mut BtGraph, data: &CompileData) -> Json {
        let mut out = jobj();
        if !data.language.is_empty() {
            out.set("language", jstr(&data.language));
        }
        if !data.sysroot.is_empty() {
            let mut sysroot = jobj();
            sysroot.set("path", jstr(&data.sysroot));
            out.set("sysroot", sysroot);
        }
        if !data.flags.is_empty() {
            out.set(
                "compileCommandFragments",
                Json::Array(
                    data.flags
                        .iter()
                        .map(|(v, bt)| fragment(graph, v, bt, None))
                        .collect(),
                ),
            );
        }
        if !data.includes.is_empty() {
            out.set(
                "includes",
                Json::Array(
                    data.includes
                        .iter()
                        .map(|(path, system, bt)| {
                            let mut inc = jobj();
                            inc.set("path", jstr(path));
                            if *system {
                                inc.set("isSystem", Json::Bool(true));
                            }
                            jbt(graph, &mut inc, bt);
                            inc
                        })
                        .collect(),
                ),
            );
        }
        if !data.frameworks.is_empty() {
            out.set(
                "frameworks",
                Json::Array(
                    data.frameworks
                        .iter()
                        .map(|(path, system, bt)| {
                            let mut framework = jobj();
                            framework.set("path", jstr(path));
                            if *system {
                                framework.set("isSystem", Json::Bool(true));
                            }
                            jbt(graph, &mut framework, bt);
                            framework
                        })
                        .collect(),
                ),
            );
        }
        if !data.defines.is_empty() {
            out.set(
                "defines",
                Json::Array(
                    data.defines
                        .iter()
                        .map(|(v, bt)| {
                            let mut d = jobj();
                            d.set("define", jstr(v));
                            jbt(graph, &mut d, bt);
                            d
                        })
                        .collect(),
                ),
            );
        }
        if !data.pch.is_empty() {
            out.set(
                "precompileHeaders",
                Json::Array(
                    data.pch
                        .iter()
                        .map(|(v, bt)| {
                            let mut p = jobj();
                            p.set("header", jstr(v));
                            jbt(graph, &mut p, bt);
                            p
                        })
                        .collect(),
                ),
            );
        }
        if let Some((value, bts)) = &data.standard {
            let mut standard = jobj();
            standard.set("standard", jstr(value));
            if !bts.is_empty() {
                let mut bt_nodes = jarr();
                for frames in bts {
                    if let Some(index) = graph.add(frames) {
                        bt_nodes.push(juint(index as u64));
                    }
                }
                if !bt_nodes.is_empty_container() {
                    standard.set("backtraces", bt_nodes);
                }
            }
            out.set("languageStandard", standard);
        }
        out
    }

    fn dump_artifacts(&self, target: &Target) -> Json {
        let mut out = jarr();
        if target.kind == TargetKind::ObjectLibrary {
            let obj_dir = gt::object_directory(self.ctx, target, &self.config);
            for name in crate::generator::install::object_source_names(self.ctx, target) {
                let mut artifact = jobj();
                artifact.set(
                    "path",
                    jstr(relative_if_under(
                        &self.top_build,
                        &format!("{obj_dir}{name}"),
                    )),
                );
                out.push(artifact);
            }
            return out;
        }
        let mut artifact = jobj();
        artifact.set(
            "path",
            jstr(relative_if_under(
                &self.top_build,
                &gt::full_path(
                    self.ctx,
                    target,
                    &self.config,
                    gt::ArtifactType::Runtime,
                    false,
                ),
            )),
        );
        out.push(artifact);
        // Add Windows-specific artifacts produced by the linker.
        if gt::has_import_library(self.ctx, target, &self.config) {
            let mut artifact = jobj();
            artifact.set(
                "path",
                jstr(relative_if_under(
                    &self.top_build,
                    &gt::full_path(
                        self.ctx,
                        target,
                        &self.config,
                        gt::ArtifactType::Import,
                        false,
                    ),
                )),
            );
            out.push(artifact);
        }
        if target.platform.dll && target.kind != TargetKind::StaticLibrary {
            let pdb_dir = gt::pdb_directory(self.ctx, target, &self.config);
            if !pdb_dir.is_empty() {
                let names = gt::names(self.ctx, target, &self.config);
                let mut artifact = jobj();
                artifact.set(
                    "path",
                    jstr(relative_if_under(
                        &self.top_build,
                        &format!("{pdb_dir}/{}", names.pdb),
                    )),
                );
                out.push(artifact);
            }
        }
        out
    }

    fn dump_link(&self, graph: &mut BtGraph, target: &Target) -> Json {
        let language = gt::linker_language(self.ctx, target, &self.config);
        let mut link_obj = jobj();
        link_obj.set("language", jstr(&language));
        let fragments = self.link_fragments(graph, target, &language);
        if !fragments.is_empty_container() {
            link_obj.set("commandFragments", fragments);
        }
        if let Some(sysroot) = self
            .ctx
            .var(target.makefile, "CMAKE_SYSROOT_LINK")
            .or_else(|| self.ctx.var(target.makefile, "CMAKE_SYSROOT"))
        {
            let mut s = jobj();
            s.set("path", jstr(sysroot));
            link_obj.set("sysroot", s);
        }
        if flags::ipo_enabled(self.ctx, target, &self.config, &language) {
            link_obj.set("lto", Json::Bool(true));
        }
        link_obj
    }

    fn dump_archive(&self, graph: &mut BtGraph, target: &Target) -> Json {
        let language = gt::linker_language(self.ctx, target, &self.config);
        let mut archive = jobj();
        let fragments = self.link_fragments(graph, target, &language);
        if !fragments.is_empty_container() {
            archive.set("commandFragments", fragments);
        }
        if flags::ipo_enabled(self.ctx, target, &self.config, &language) {
            archive.set("lto", Json::Bool(true));
        }
        archive
    }

    /// `DumpLinkCommandFragments` (flags/libraryPath/libraries roles).
    fn link_fragments(&self, graph: &mut BtGraph, target: &Target, language: &str) -> Json {
        let mut out = jarr();
        let (language_flags, option_frags) =
            crate::generator::flags::codemodel_link_flags(self.ctx, target, &self.config, language);
        if !language_flags.is_empty() {
            out.push(fragment(graph, &language_flags, &[], Some("flags")));
        }
        for (value, bt) in &option_frags {
            out.push(fragment(graph, value.trim(), bt, Some("flags")));
        }
        if target.kind != TargetKind::StaticLibrary {
            let line = link::link_line(self.ctx, target, &self.config, language);
            let framework_path = line.framework_path.trim();
            if !framework_path.is_empty() {
                out.push(fragment(graph, framework_path, &[], Some("frameworkPath")));
            }
            let dir_bts = link_dir_backtraces(self.ctx, target, &self.config, language);
            for (text, dir) in &line.path_fragments {
                let bt = dir_bts
                    .iter()
                    .find(|(d, _)| Some(d) == dir.as_ref())
                    .map(|(_, bt)| bt.clone())
                    .unwrap_or_default();
                out.push(fragment(graph, text.trim(), &bt, Some("libraryPath")));
            }
            for (value, bt) in &line.lib_fragments {
                out.push(fragment(graph, value.trim(), bt, Some("libraries")));
            }
        }
        out
    }

    fn dump_dependencies(&self, graph: &mut BtGraph, target: &Target) -> Json {
        // `cmGlobalGenerator::GetTargetDirectDepends`: the
        // cmComputeTargetDepends final edges (link deps with
        // target_link_libraries backtraces, interface deps with the top-level
        // dependency's backtrace injected, and add_dependencies utilities),
        // in the dependee's target order index.
        let mut out = jarr();
        for depend in link::target_depends(self.ctx, target).iter() {
            let mut dep = jobj();
            dep.set(
                "id",
                jstr(self.id_of(&self.ctx.interp.ext.targets[&depend.key])),
            );
            jbt(graph, &mut dep, &depend.backtrace);
            out.push(dep);
        }
        out
    }

    fn dump_launchers(&self, target: &Target) -> Json {
        let mut out = jarr();
        for (prop, kind) in [
            ("TEST_LAUNCHER", "test"),
            ("CROSSCOMPILING_EMULATOR", "emulator"),
        ] {
            // DumpLaunchers gates the emulator on the directory's
            // CMAKE_CROSSCOMPILING variable, not the toolchain flag.
            if kind == "emulator"
                && !is_on_opt(
                    self.ctx
                        .var(target.makefile, "CMAKE_CROSSCOMPILING")
                        .as_deref(),
                )
            {
                continue;
            }
            let Some(raw) = target.props.get(prop) else {
                continue;
            };
            let evaluated = self.ctx.genex(raw, Some(target), None);
            let mut parts = expand_list(&evaluated, false).into_iter();
            let Some(command) = parts.next() else {
                continue;
            };
            if command.is_empty() {
                continue;
            }
            let mut launcher = jobj();
            launcher.set(
                "command",
                jstr(relative_if_under(&self.top_source, &command)),
            );
            launcher.set("type", jstr(kind));
            let args: Vec<Json> = parts.map(jstr).collect();
            if !args.is_empty() {
                launcher.set("arguments", Json::Array(args));
            }
            out.push(launcher);
        }
        out
    }

    fn dump_install(&self, graph: &mut BtGraph, target: &Target) -> Json {
        let mut install = jobj();
        let mut prefix = jobj();
        let mut path = self
            .ctx
            .var(target.makefile, "CMAKE_INSTALL_PREFIX")
            .unwrap_or_default();
        prefix.set("path", jstr(path.replace('\\', "/")));
        let _ = &mut path;
        install.set("prefix", prefix);
        let mut destinations = jarr();
        for rule in self.ctx.interp.ext.install.rules.values().flatten() {
            if let InstallAction::Target { name, .. } = &rule.action
                && name == &target.name
            {
                let mut dest = jobj();
                dest.set(
                    "path",
                    jstr(genex_at(
                        self.ctx,
                        target.makefile,
                        &rule.options.destination,
                    )),
                );
                jbt(graph, &mut dest, &rule.backtrace);
                destinations.push(dest);
            }
        }
        install.set("destinations", destinations);
        install
    }
}

/// An include directory or framework: path, whether it is a system one,
/// and its backtrace.
type IncludeEntry = (String, bool, Vec<Frame>);

#[derive(Clone, Default, PartialEq)]
struct CompileData {
    language: String,
    sysroot: String,
    flags: Vec<flags::BtFrag>,
    defines: Vec<flags::BtFrag>,
    includes: Vec<IncludeEntry>,
    frameworks: Vec<IncludeEntry>,
    pch: Vec<flags::BtFrag>,
    standard: Option<(String, Vec<Vec<Frame>>)>,
}

/// `ProcessLanguage`/`BuildCompileData`: on Apple, include directories that
/// are paths to frameworks are listed as frameworks. Each entry's backtrace
/// enters the graph in list order.
fn split_frameworks(
    target: &Target,
    entries: Vec<IncludeEntry>,
    graph: &mut BtGraph,
) -> (Vec<IncludeEntry>, Vec<IncludeEntry>) {
    let mut includes = Vec::new();
    let mut frameworks = Vec::new();
    for entry in entries {
        graph.add(&entry.2);
        if target.platform.apple && crate::apple::is_path_to_framework(&entry.0) {
            frameworks.push(entry);
        } else {
            includes.push(entry);
        }
    }
    (includes, frameworks)
}

/// `MergeCompileData`: target-wide data merged with a source's file data.
fn merge_compile_data(
    file: &CompileData,
    lang_data: &BTreeMap<String, CompileData>,
) -> CompileData {
    let Some(target) = lang_data.get(&file.language) else {
        return file.clone();
    };
    let mut merged = CompileData {
        language: file.language.clone(),
        sysroot: target.sysroot.clone(),
        pch: target.pch.clone(),
        standard: target.standard.clone(),
        ..CompileData::default()
    };
    merged.flags.extend(target.flags.iter().cloned());
    merged.flags.extend(file.flags.iter().cloned());
    merged.includes.extend(file.includes.iter().cloned());
    merged.includes.extend(target.includes.iter().cloned());
    merged.frameworks.extend(file.frameworks.iter().cloned());
    merged.frameworks.extend(target.frameworks.iter().cloned());
    merged.defines.extend(target.defines.iter().cloned());
    merged.defines.extend(file.defines.iter().cloned());
    merged.defines.sort_by(|a, b| a.0.cmp(&b.0));
    merged.defines.dedup_by(|a, b| a.0 == b.0);
    merged
}

/// `cmGeneratorTarget::GetLanguageStandardProperty`: the feature-derived
/// `LanguageStandardMap` entry (whose backtraces accumulate one entry per
/// compile feature that set or maintained the required standard), falling
/// back to the target's `<LANG>_STANDARD` LanguageStandardProperties entry.
fn language_standard(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    lang: &str,
) -> Option<(String, Vec<Vec<Frame>>)> {
    let prop = format!("{lang}_STANDARD");
    let prop_value = target.props.get(&prop).map(str::to_owned);

    // cmGeneratorTarget::ComputeCompileFeatures: iterate the evaluated
    // COMPILE_FEATURES in order; a feature whose level exceeds the current
    // standard updates LanguageStandardMap[config-lang], and every feature
    // that yields a non-empty required standard appends its backtrace.
    let prefix = match lang {
        "C" => "c_",
        "CXX" => "cxx_",
        "CUDA" => "cuda_",
        "HIP" => "hip_",
        "OBJC" => "objc_",
        "OBJCXX" => "objcxx_",
        "ISPC" => "ispc_",
        _ => "",
    };
    let standards = flags::levels(lang);
    let mut mapped: Option<String> = None;
    let mut bts: Vec<Vec<Frame>> = Vec::new();
    if !standards.is_empty() {
        let default = ctx
            .var(target.makefile, &format!("CMAKE_{lang}_STANDARD_DEFAULT"))
            .filter(|value| !value.is_empty());
        for (feature, bt) in
            flags::usage_item_fragments(ctx, target, config, lang, "COMPILE_FEATURES")
        {
            if prefix.is_empty() || !feature.starts_with(prefix) {
                continue;
            }
            // CompileFeatureStandardLevel: the highest level listing the
            // feature in CMAKE_<lang><level>_COMPILE_FEATURES.
            let mut feature_level = None;
            for (i, std) in standards.iter().enumerate() {
                let items = ctx
                    .var(
                        target.makefile,
                        &format!("CMAKE_{lang}{std}_COMPILE_FEATURES"),
                    )
                    .unwrap_or_default();
                if expand_list(&items, false)
                    .iter()
                    .any(|item| item == &feature)
                {
                    feature_level = Some(i);
                }
            }
            // GetNewRequiredStandard: keep the current standard unless the
            // feature requires a higher one than the current (or default).
            let mut new_required = mapped.clone().or_else(|| prop_value.clone());
            let existing = new_required.clone().or_else(|| default.clone());
            if let Some(flevel) = feature_level
                && existing
                    .and_then(|value| flags::level(standards, &value))
                    .is_none_or(|level| level < flevel)
            {
                new_required = Some(standards[flevel].to_owned());
            }
            if let Some(required) = new_required
                && !required.is_empty()
            {
                if mapped.as_deref() != Some(required.as_str()) {
                    bts.clear();
                }
                mapped = Some(required);
                bts.push(bt);
            }
        }
    }
    if let Some(standard) = mapped {
        return Some((standard, bts));
    }

    // LanguageStandardProperties[prop]: the value of the last
    // `<LANG>_STANDARD` write plus that write's backtrace.
    prop_value.as_ref()?;
    let entries = gt::evaluated_entries(ctx, target, target, &prop);
    let standard = prop_value?;
    if standard.is_empty() {
        return None;
    }
    let bts = entries
        .last()
        .map(|(_, bt)| vec![bt.clone()])
        .filter(|bts: &Vec<Vec<Frame>>| !bts[0].is_empty())
        .unwrap_or_default();
    Some((standard, bts))
}

/// `(dir, backtrace)` for the ordered link directories
/// (`GetDirectoriesWithBacktraces`).
fn link_dir_backtraces(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    language: &str,
) -> Vec<(String, Vec<Frame>)> {
    flags::link_dir_items(ctx, target, config, language)
}

fn result_index(arr: &Json) -> u32 {
    match arr {
        Json::Array(items) => items.len() as u32,
        _ => 0,
    }
}

/// `DumpInstallerPath`: a plain string when `to` is `from`'s basename.
fn installer_path(from: &str, to: &str) -> Json {
    if !to.contains('/')
        && from.ends_with(to)
        && (from.len() == to.len() || from.as_bytes()[from.len() - to.len() - 1] == b'/')
    {
        jstr(from)
    } else {
        let mut path = jobj();
        path.set("from", jstr(from));
        path.set("to", jstr(to));
        path
    }
}

/// The set name for `runtimeDependencySetName` (anonymous sets are empty).
fn set_name(set: &str) -> &str {
    if set.starts_with('#') { "" } else { set }
}

fn file_set_name(name: &str) -> &str {
    name
}

/// Split a `SHELL:`-prefixed or plain option into its fragment parts.
fn target_options_split(option: &str) -> Vec<String> {
    match option.strip_prefix("SHELL:") {
        Some(shell) => crate::util::expand_list(shell, true),
        None => vec![option.to_owned()],
    }
}

/// Entry point: build the codemodel object for the current build tree.
/// `Codemodel::Dump`: one `CodemodelConfig` per
/// `GetGeneratorConfigs(IncludeEmptyConfig)` entry.
pub(super) fn build(api: &mut FileApi, interp: &Interp) -> Json {
    let ctx = GenCtx::new(interp);
    let configs = if ctx.multi_config {
        if ctx.configs.is_empty() {
            vec![String::new()]
        } else {
            ctx.configs.clone()
        }
    } else {
        vec![ctx.config.clone()]
    };
    let mut result: Option<Json> = None;
    let mut configurations = Vec::new();
    for config in configs {
        let config_ctx = ctx.for_config(&config);
        let mut model = Codemodel {
            api: &mut *api,
            ctx: &config_ctx,
            config,
            top_source: interp.home_source.clone(),
            top_build: interp.home_binary.clone(),
            dirs: Vec::new(),
            dir_index: HashMap::new(),
            projects: Vec::new(),
            target_index: HashMap::new(),
            target_ids: HashMap::new(),
        };
        let dumped = model.dump();
        if let Some(Json::Array(mut one)) = dumped.get("configurations").cloned() {
            configurations.append(&mut one);
        }
        result.get_or_insert(dumped);
    }
    let mut codemodel = result.unwrap_or_else(jobj);
    codemodel.set("configurations", Json::Array(configurations));
    codemodel
}
