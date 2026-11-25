//! Deprecated "Extra Generators" (cmExternalMakefileProjectGenerator and
//! the five IDE project file generators), wrapping the Ninja generator.

mod codeblocks;
mod codelite;
mod eclipse;
mod kate;
mod sublime;
pub mod xml;

use std::collections::BTreeMap;
use std::rc::Rc;

use super::ctx::GenCtx;
use super::ninja::GlobalTarget;
use super::target::{self, SourceInfo};
use crate::state::Target;
use crate::util;

/// `cmExternalMakefileProjectGeneratorFactory` registry entry.
pub struct ExtraGeneratorInfo {
    pub name: &'static str,
    /// Factory documentation string (the `--help` text lives in
    /// `cmcmd/generators.txt`).
    #[allow(dead_code)]
    pub doc: &'static str,
    /// `GetSupportedGlobalGenerators` on this platform.
    pub supported: &'static [&'static str],
}

/// `cmake::AddDefaultExtraGenerators` order.
pub const EXTRA_GENERATORS: &[ExtraGeneratorInfo] = &[
    ExtraGeneratorInfo {
        name: "CodeBlocks",
        doc: "Generates CodeBlocks project files (deprecated).",
        supported: &["Ninja", "Unix Makefiles"],
    },
    ExtraGeneratorInfo {
        name: "CodeLite",
        doc: "Generates CodeLite project files (deprecated).",
        supported: &["Ninja", "Unix Makefiles"],
    },
    ExtraGeneratorInfo {
        name: "Eclipse CDT4",
        doc: "Generates Eclipse CDT 4.0 project files (deprecated).",
        supported: &["Ninja", "Unix Makefiles"],
    },
    ExtraGeneratorInfo {
        name: "Kate",
        doc: "Generates Kate project files (deprecated).",
        supported: &["Ninja", "Ninja Multi-Config", "Unix Makefiles"],
    },
    ExtraGeneratorInfo {
        name: "Sublime Text 2",
        doc: "Generates Sublime Text 2 project files (deprecated).",
        supported: &["Ninja", "Unix Makefiles"],
    },
];

/// `cmExternalMakefileProjectGenerator::CreateFullGeneratorName`.
pub fn create_full_generator_name(global: &str, extra: &str) -> String {
    if extra.is_empty() {
        global.to_owned()
    } else {
        format!("{extra} - {global}")
    }
}

/// `createExtraGenerator` (cmake.cxx): resolves an extra generator alias or
/// a full `"<extra> - <global>"` name into `(extra name, global name)`.
/// The bare alias picks the factory's first supported global generator.
pub fn split_generator_name(name: &str) -> Option<(String, String)> {
    for info in EXTRA_GENERATORS {
        if info.name == name {
            return Some((info.name.to_owned(), info.supported[0].to_owned()));
        }
        for global in info.supported {
            let full = create_full_generator_name(global, info.name);
            if full == name {
                return Some((info.name.to_owned(), (*global).to_owned()));
            }
        }
    }
    None
}

/// `cmGlobalGenerator::Generate`'s extra generator step: invoked after the
/// normal generator has written the build files.
pub fn generate(ctx: &GenCtx, globals: &[GlobalTarget]) {
    let Some(name) = ctx.interp.ext.extra_generator.clone() else {
        return;
    };
    match name.as_str() {
        "CodeBlocks" => codeblocks::generate(ctx, globals),
        "CodeLite" => codelite::generate(ctx, globals),
        "Eclipse CDT4" => eclipse::generate(ctx, globals),
        "Kate" => kate::generate(ctx, globals),
        "Sublime Text 2" => sublime::generate(ctx, globals),
        _ => {}
    }
}

/// `cmSystemTools::RelativePath` (full paths in, `..` segments out).
fn relative_path(local: &str, remote: &str) -> String {
    if local == remote {
        return String::new();
    }
    let local_parts: Vec<&str> = local.split('/').filter(|p| !p.is_empty()).collect();
    let remote_parts: Vec<&str> = remote.split('/').filter(|p| !p.is_empty()).collect();
    let mut common = 0;
    while common < local_parts.len()
        && common < remote_parts.len()
        && local_parts[common] == remote_parts[common]
    {
        common += 1;
    }
    let mut out: Vec<&str> = std::iter::repeat_n("..", local_parts.len() - common).collect();
    out.extend(&remote_parts[common..]);
    out.join("/")
}

/// `cmSystemTools::GetFilenamePath`.
fn filename_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// `cmSystemTools::GetFilenameName`.
fn filename_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `cmSystemTools::GetFilenameWithoutExtension` (last `.` removed).
fn filename_without_extension(path: &str) -> &str {
    let name = filename_name(path);
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map_or(name, |(stem, _)| stem)
}

fn file_exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

fn file_is_dir(path: &str) -> bool {
    std::path::Path::new(path).is_dir()
}

fn write_file(path: &str, content: &str) {
    let _ = std::fs::write(path, content);
}

/// The shared "local generator" view of a configured directory.
pub(super) struct Shared<'a> {
    pub ctx: &'a GenCtx<'a>,
    /// The Ninja GLOBAL_TARGET copies (per directory).
    pub globals: &'a [GlobalTarget],
}

impl<'a> Shared<'a> {
    pub fn new(ctx: &'a GenCtx, globals: &'a [GlobalTarget]) -> Shared<'a> {
        Shared { ctx, globals }
    }

    pub fn makefile_count(&self) -> usize {
        self.ctx.interp.makefiles.len()
    }

    /// `cmStateSnapshot::GetProjectName`: the directory's own project name
    /// or the nearest ancestor's.
    pub fn project_name(&self, dir: usize) -> String {
        let mut current = Some(dir);
        while let Some(index) = current {
            if let Some(name) = self.ctx.interp.makefiles[index].props.get("PROJECT_NAME") {
                return name.to_owned();
            }
            current = self.ctx.interp.makefiles[index].parent;
        }
        String::new()
    }

    /// `cmGlobalGenerator::FillProjectMap`.
    pub fn project_map(&self) -> BTreeMap<String, Vec<usize>> {
        let mut map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for dir in 0..self.makefile_count() {
            let mut name = String::new();
            let mut current = Some(dir);
            while let Some(index) = current {
                let level_name = self.project_name(index);
                if name != level_name {
                    name = level_name;
                    map.entry(name.clone()).or_default().push(dir);
                }
                current = self.ctx.interp.makefiles[index].parent;
            }
        }
        map
    }

    pub fn source_dir(&self, dir: usize) -> &str {
        &self.ctx.interp.makefiles[dir].source_dir
    }

    pub fn binary_dir(&self, dir: usize) -> &str {
        &self.ctx.interp.makefiles[dir].binary_dir
    }

    pub fn home_source(&self) -> &str {
        &self.ctx.interp.home_source
    }

    pub fn home_binary(&self) -> &str {
        &self.ctx.interp.home_binary
    }

    pub fn var(&self, dir: usize, name: &str) -> Option<String> {
        self.ctx.var(dir, name)
    }

    pub fn var_safe(&self, dir: usize, name: &str) -> String {
        self.ctx.var_safe(dir, name)
    }

    pub fn build_type(&self, dir: usize) -> String {
        self.var_safe(dir, "CMAKE_BUILD_TYPE")
    }

    /// `cmGlobalGenerator::GetLanguageEnabled`.
    pub fn lang_enabled(&self, lang: &str) -> bool {
        self.ctx
            .interp
            .langs
            .enabled
            .iter()
            .any(|l| l.as_str() == lang)
    }

    /// `cmMakefile::GetListFiles`.
    pub fn list_files(&self, dir: usize) -> &[String] {
        &self.ctx.interp.makefiles[dir].list_files
    }

    /// `cmMakefile::GetProperty`.
    pub fn prop(&self, dir: usize, name: &str) -> Option<String> {
        self.ctx.interp.makefiles[dir]
            .props
            .get(name)
            .map(str::to_owned)
    }

    /// `lg->GetGeneratorTargets()`: the directory's real targets followed by
    /// its GLOBAL_TARGET copies (names only; they have no `Target` entry).
    pub fn targets(&self, dir: usize) -> Vec<&'a Target> {
        let mf = &self.ctx.interp.makefiles[dir];
        mf.targets
            .iter()
            .filter_map(|name| self.ctx.target(name))
            .collect()
    }

    /// Names of the global targets created for `dir`.
    pub fn global_target_names(&self, dir: usize) -> Vec<&'a str> {
        self.globals
            .iter()
            .filter(|g| g.makefile == dir)
            .map(|g| g.name.as_str())
            .collect()
    }

    /// `cmGeneratorTarget::GetSourceFiles(config)`.
    pub fn target_sources(&self, target: &Target, config: &str) -> Rc<Vec<SourceInfo>> {
        target::sources(self.ctx, target, config)
    }

    /// `cmGeneratorTarget::GetLocation(config)`.
    pub fn target_location(&self, target: &Target, config: &str) -> String {
        target::full_path(
            self.ctx,
            target,
            config,
            target::ArtifactType::Runtime,
            false,
        )
    }

    /// `lg->GetObjectOutputRoot()`.
    pub fn object_output_root(&self, dir: usize) -> String {
        format!("{}/CMakeFiles", self.binary_dir(dir))
    }

    /// `lg->GetTargetDirectory(target)` under the object output root.
    pub fn target_dir(&self, dir: usize, target: &Target) -> String {
        format!("{}/{}.dir", self.object_output_root(dir), target.name)
    }

    /// `lg->MaybeRelativeToTopBinDir(path)`: `path` relative to the top
    /// binary dir when it is underneath it.
    pub fn maybe_relative_to_top_bin_dir(&self, path: &str) -> String {
        let top = self.home_binary();
        if path == top {
            ".".to_owned()
        } else if util::is_subdirectory(path, top) {
            relative_path(top, path)
        } else {
            path.to_owned()
        }
    }
}
