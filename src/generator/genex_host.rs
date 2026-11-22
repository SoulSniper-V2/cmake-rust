//! `GenexHost` implementation over the configured build system
//! (cmGeneratorTarget / cmLocalGenerator queries used by generator
//! expressions).

use crate::genex::{ArtifactComponent, ArtifactKind, GenexHost, PropertyEntry};
use crate::policy::{self, PolicyStatus};
use crate::state::{Target, TargetKind};

use super::ctx::GenCtx;
use super::target::{self as gt, ArtifactType, SourceKind};

pub struct CtxHost<'c, 'a> {
    pub ctx: &'c GenCtx<'a>,
    /// Directory whose variables and target visibility apply.
    pub dir: usize,
}

impl<'c, 'a> CtxHost<'c, 'a> {
    pub fn new(ctx: &'c GenCtx<'a>, dir: usize) -> Self {
        CtxHost { ctx, dir }
    }

    fn find(&self, name: &str) -> Option<&'a Target> {
        if let Some(target) = self.ctx.interp.ext.targets.get(name) {
            return Some(target);
        }
        self.ctx.target_at(self.dir, name)
    }

    fn artifact_path(&self, target: &Target, kind: ArtifactKind, config: &str) -> String {
        let ctx = self.ctx;
        let Some(artifact) = self.artifact_type(target, kind, config) else {
            return String::new();
        };
        match kind {
            ArtifactKind::SonameFile | ArtifactKind::SonameImportFile => {
                let dir = gt::directory(ctx, target, config, artifact);
                let soname = if target.imported {
                    gt::import_info(target, config)
                        .map(|info| {
                            if info.no_soname {
                                crate::util::filename_name(info.location)
                            } else {
                                info.soname.trim_start_matches("@rpath/").to_owned()
                            }
                        })
                        .unwrap_or_default()
                } else {
                    let names = gt::names(ctx, target, config);
                    if artifact == ArtifactType::Import {
                        names.import_library
                    } else {
                        names.shared_object
                    }
                };
                format!("{dir}/{soname}")
            }
            ArtifactKind::PdbFile => {
                let dir = gt::directory(ctx, target, config, artifact);
                format!("{dir}/{}", gt::names(ctx, target, config).pdb)
            }
            ArtifactKind::BundleDir | ArtifactKind::BundleContentDir => {
                gt::directory(ctx, target, config, artifact)
            }
            _ => gt::full_path(ctx, target, config, artifact, kind == ArtifactKind::File),
        }
    }

    fn artifact_type(
        &self,
        target: &Target,
        kind: ArtifactKind,
        config: &str,
    ) -> Option<ArtifactType> {
        match kind {
            ArtifactKind::ImportFile
            | ArtifactKind::LinkerImportFile
            | ArtifactKind::SonameImportFile => {
                gt::has_import_library(self.ctx, target, config).then_some(ArtifactType::Import)
            }
            ArtifactKind::LinkerFile if gt::has_import_library(self.ctx, target, config) => {
                Some(ArtifactType::Import)
            }
            ArtifactKind::LinkerLibraryFile
                if gt::dll_platform(self.ctx, target)
                    && target.kind != TargetKind::StaticLibrary =>
            {
                None
            }
            _ => Some(ArtifactType::Runtime),
        }
    }
}

impl GenexHost for CtxHost<'_, '_> {
    fn target_exists(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    fn target_type(&self, name: &str) -> Option<&str> {
        self.find(name).map(|target| target.kind.type_name())
    }

    fn target_property_raw(&self, name: &str, prop: &str) -> Option<String> {
        let target = self.find(name)?;
        let interp = self.ctx.interp;
        let boolean = |value: bool| if value { "TRUE" } else { "FALSE" }.to_owned();
        match prop {
            "TYPE" => Some(target.kind.type_name().to_owned()),
            "NAME" => Some(target.name.clone()),
            "IMPORTED" => Some(boolean(target.imported)),
            "IMPORTED_GLOBAL" => Some(boolean(target.imported_global)),
            "BINARY_DIR" => Some(interp.makefiles[target.makefile].binary_dir.clone()),
            "SOURCE_DIR" => Some(interp.makefiles[target.makefile].source_dir.clone()),
            _ => target.props.get(prop).map(str::to_owned),
        }
    }

    fn property_entries(&self, name: &str, prop: &str) -> Vec<PropertyEntry> {
        let Some(target) = self.find(name) else {
            return Vec::new();
        };
        if let Some(entries) = target.provenance.get(prop) {
            return entries
                .iter()
                .map(|(value, backtrace)| PropertyEntry {
                    value: value.clone(),
                    backtrace: Some(self.ctx.intern_backtrace(backtrace.clone())),
                })
                .collect();
        }
        self.target_property_raw(name, prop)
            .into_iter()
            .map(|value| PropertyEntry {
                value,
                backtrace: None,
            })
            .collect()
    }

    /// `cmTarget::GetSystemIncludeDirectories`.
    fn system_include_directories(&self, name: &str) -> Vec<String> {
        self.find(name)
            .map(|target| target.system_include_directories.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn is_imported(&self, name: &str) -> bool {
        self.find(name).is_some_and(|target| target.imported)
    }

    fn resolve_alias(&self, name: &str) -> Option<String> {
        let interp = self.ctx.interp;
        let mut dir = Some(self.dir);
        while let Some(index) = dir {
            if let Some(key) = interp.ext.scoped_aliases.get(&(index, name.to_owned())) {
                return Some(key.clone());
            }
            dir = interp.makefiles.get(index).and_then(|mf| mf.parent);
        }
        interp.ext.aliases.get(name).cloned()
    }

    fn alias_global(&self, name: &str) -> bool {
        self.ctx.interp.ext.aliases.contains_key(name)
    }

    fn display_name(&self, target: &str) -> String {
        self.find(target)
            .map(|target| target.name.clone())
            .unwrap_or_else(|| target.to_owned())
    }

    fn resolve_target(&self, name: &str, current: Option<&str>) -> Option<String> {
        if self.ctx.interp.ext.targets.contains_key(name) {
            return Some(name.to_owned());
        }
        let dir = current
            .and_then(|name| self.find(name))
            .map_or(self.dir, |target| target.makefile);
        self.ctx
            .target_at(dir, name)
            .map(|target| self.ctx.target_key(target))
    }

    fn compatible_interface_property(
        &self,
        name: &str,
        prop: &str,
        config: &str,
        _raw: Option<&str>,
        evaluating_links: bool,
    ) -> Result<Option<String>, String> {
        Ok(self.find(name).and_then(|target| {
            super::compatible::property(self.ctx, target, config, prop, evaluating_links)
        }))
    }

    fn target_artifact(
        &self,
        name: &str,
        kind: ArtifactKind,
        config: &str,
    ) -> Result<String, String> {
        let target = self
            .find(name)
            .ok_or_else(|| format!("No target \"{name}\""))?;
        Ok(self.artifact_path(target, kind, config))
    }

    fn target_artifact_component(
        &self,
        name: &str,
        kind: ArtifactKind,
        component: ArtifactComponent,
        config: &str,
    ) -> Result<String, String> {
        let target = self
            .find(name)
            .ok_or_else(|| format!("No target \"{name}\""))?;
        let Some(artifact) = self.artifact_type(target, kind, config) else {
            return Ok(String::new());
        };
        Ok(match component {
            ArtifactComponent::Path => self.artifact_path(target, kind, config),
            ArtifactComponent::Name => {
                crate::util::filename_name(&self.artifact_path(target, kind, config))
            }
            ArtifactComponent::Directory => {
                crate::util::filename_path(&self.artifact_path(target, kind, config))
            }
            ArtifactComponent::BaseName if kind == ArtifactKind::PdbFile => {
                gt::pdb_base_name(self.ctx, target, config)
            }
            ArtifactComponent::BaseName => format!(
                "{}{}",
                gt::output_name(self.ctx, target, config, artifact),
                gt::postfix(target, config),
            ),
            ArtifactComponent::Prefix => gt::file_affixes(self.ctx, target, config, artifact).0,
            ArtifactComponent::Suffix => gt::file_affixes(self.ctx, target, config, artifact).1,
        })
    }

    fn dll_platform(&self, name: &str) -> bool {
        self.find(name)
            .is_some_and(|target| gt::dll_platform(self.ctx, target))
    }

    fn target_objects(&self, name: &str, config: &str) -> Result<Vec<String>, String> {
        let target = self
            .find(name)
            .ok_or_else(|| format!("No target \"{name}\""))?;
        let objects: Vec<String> = if target.imported {
            gt::mapped_import_config(target, config)
                .and_then(|info| info.location)
                .map(|list| crate::util::expand_list(list, false))
                .unwrap_or_default()
        } else {
            let dir = gt::object_directory(self.ctx, target, config);
            gt::sources(self.ctx, target, config)
                .iter()
                .filter(|source| source.kind == SourceKind::ObjectSource)
                .map(|source| format!("{}/{}", dir.trim_end_matches('/'), source.object_name))
                .collect()
        };
        // cmMakefile::AddTargetObject in the referencing directory.
        let mut registry = self.ctx.target_objects.borrow_mut();
        for object in &objects {
            registry.insert((self.dir, object.clone()), target.name.clone());
        }
        Ok(objects)
    }

    fn variable(&self, name: &str) -> Option<String> {
        self.ctx.var(self.dir, name)
    }

    fn policy_optional_warning(&self, var: &str) -> bool {
        self.ctx
            .interp
            .policy_optional_warning(self.ctx.var(self.dir, var).as_deref())
    }

    fn policy_status(&self, name: &str, id: &str) -> PolicyStatus {
        let Some(index) = policy::index_of(id) else {
            return PolicyStatus::Warn;
        };
        if crate::policy_table::POLICIES[index].removed {
            return PolicyStatus::New;
        }
        match self.find(name) {
            Some(target) => target.policies.get(index).unwrap_or(PolicyStatus::Warn),
            None => self.local_policy_status(id),
        }
    }

    fn local_policy_status(&self, id: &str) -> PolicyStatus {
        let Some(index) = policy::index_of(id) else {
            return PolicyStatus::Warn;
        };
        if crate::policy_table::POLICIES[index].removed {
            return PolicyStatus::New;
        }
        let frames = &self.ctx.interp.makefiles[self.dir].policies;
        for frame in frames.iter().rev() {
            if let Some(status) = frame.map.get(index) {
                return status;
            }
        }
        PolicyStatus::Warn
    }

    fn linker_language(&self, name: &str, config: &str) -> String {
        match self.find(name) {
            Some(target) => gt::linker_language(self.ctx, target, config),
            None => String::new(),
        }
    }

    fn object_directory(&self, name: &str, config: &str) -> String {
        self.find(name)
            .map(|target| gt::object_directory(self.ctx, target, config))
            .unwrap_or_default()
    }
}
