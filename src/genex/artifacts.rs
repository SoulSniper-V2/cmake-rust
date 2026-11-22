use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactKind {
    File,
    ImportFile,
    LinkerFile,
    LinkerLibraryFile,
    LinkerImportFile,
    SonameFile,
    SonameImportFile,
    PdbFile,
    BundleDir,
    BundleContentDir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactComponent {
    Path,
    Name,
    Directory,
    BaseName,
    Prefix,
    Suffix,
}

pub(super) fn artifact_node(id: &str) -> Option<(ArtifactKind, ArtifactComponent)> {
    use ArtifactComponent::*;
    use ArtifactKind::*;
    let id = id.strip_prefix("TARGET_")?;
    match id {
        "BUNDLE_DIR" => return Some((BundleDir, Path)),
        "BUNDLE_DIR_NAME" => return Some((BundleDir, Name)),
        "BUNDLE_CONTENT_DIR" => return Some((BundleContentDir, Path)),
        _ => {}
    }
    let (stem, component) = if let Some(stem) = id.strip_suffix("_BASE_NAME") {
        (stem, BaseName)
    } else if let Some(stem) = id.strip_suffix("_NAME") {
        (stem, Name)
    } else if let Some(stem) = id.strip_suffix("_DIR") {
        (stem, Directory)
    } else if let Some(stem) = id.strip_suffix("_PREFIX") {
        (stem, Prefix)
    } else if let Some(stem) = id.strip_suffix("_SUFFIX") {
        (stem, Suffix)
    } else {
        (id, Path)
    };
    let kind = match stem {
        "FILE" => File,
        "IMPORT_FILE" => ImportFile,
        "LINKER_FILE" => LinkerFile,
        "LINKER_LIBRARY_FILE" => LinkerLibraryFile,
        "LINKER_IMPORT_FILE" => LinkerImportFile,
        "SONAME_FILE" => SonameFile,
        "SONAME_IMPORT_FILE" => SonameImportFile,
        "PDB_FILE" => PdbFile,
        _ => return None,
    };
    match (kind, component) {
        (SonameFile | SonameImportFile, BaseName | Prefix | Suffix)
        | (PdbFile, Prefix | Suffix) => None,
        _ => Some((kind, component)),
    }
}

impl Evaluator<'_, '_> {
    pub(super) fn artifact(
        &mut self,
        id: &str,
        name: &str,
        expr: &str,
        dag: Option<&DagChecker>,
    ) -> Result<String, GenexError> {
        use ArtifactComponent::*;
        use ArtifactKind::*;
        let (kind, component) = artifact_node(id).unwrap();
        if !is_valid_target_name(name) {
            return Err(self.error(expr, "Expression syntax not recognized."));
        }
        let target = self
            .host
            .resolve_target(name, None)
            .ok_or_else(|| self.error(expr, format!("No target \"{name}\"")))?;
        let ty = self.host.target_type(&target).unwrap_or_default();
        if !matches!(
            ty,
            "EXECUTABLE"
                | "STATIC_LIBRARY"
                | "SHARED_LIBRARY"
                | "MODULE_LIBRARY"
                | "UNKNOWN_LIBRARY"
        ) {
            return Err(self.error(
                expr,
                format!("Target \"{name}\" is not an executable or library."),
            ));
        }
        if dag.is_some_and(|dag| {
            dag.root().target == target
                && (dag.root().property == "LINK_LIBRARIES" || dag.sources())
        }) {
            return Err(self.error(expr, "Expressions which require the linker language may not be used while evaluating link libraries"));
        }

        let config = self.ctx.config;
        let dll = self.host.dll_platform(&target);
        let imported = self.host.is_imported(&target);
        let static_library = ty == "STATIC_LIBRARY";
        let library = matches!(
            ty,
            "STATIC_LIBRARY" | "SHARED_LIBRARY" | "MODULE_LIBRARY" | "UNKNOWN_LIBRARY"
        );
        let exports = ty == "EXECUTABLE"
            && self
                .host
                .target_property_raw(&target, "ENABLE_EXPORTS")
                .as_deref()
                .is_some_and(util::is_on);
        match kind {
            SonameFile | SonameImportFile => {
                let base = if kind == SonameFile {
                    "TARGET_SONAME_FILE"
                } else {
                    "TARGET_SONAME_IMPORT_FILE"
                };
                let reason = if dll {
                    "is not allowed for DLL target platforms."
                } else if ty != "SHARED_LIBRARY" {
                    "is allowed only for SHARED libraries."
                } else if self.host.archived_aix_shared_library(&target) {
                    "is not allowed for AIX_SHARED_LIBRARY_ARCHIVE libraries."
                } else {
                    ""
                };
                if !reason.is_empty() {
                    return Err(self.error(expr, format!("{base} {reason}")));
                }
            }
            PdbFile => {
                let base = if component == BaseName {
                    id
                } else {
                    "TARGET_PDB_FILE"
                };
                let reason = if imported {
                    "not allowed for IMPORTED targets."
                } else if !self
                    .host
                    .variable(&format!(
                        "CMAKE_{}_LINKER_SUPPORTS_PDB",
                        self.host.linker_language(&target, config)
                    ))
                    .as_deref()
                    .is_some_and(util::is_on)
                {
                    "is not supported by the target linker."
                } else if !matches!(ty, "SHARED_LIBRARY" | "MODULE_LIBRARY" | "EXECUTABLE") {
                    "is allowed only for targets with linker created artifacts."
                } else {
                    ""
                };
                if !reason.is_empty() {
                    return Err(self.error(expr, format!("{base} {reason}")));
                }
            }
            BundleDir | BundleContentDir => {
                let reason = if imported {
                    "not allowed for IMPORTED targets."
                } else if !self.host.apple_bundle(&target) {
                    "is allowed only for Bundle targets."
                } else {
                    ""
                };
                if !reason.is_empty() {
                    return Err(self.error(expr, format!("{id} {reason}")));
                }
            }
            LinkerFile | LinkerImportFile | LinkerLibraryFile => {
                let library_only = kind == LinkerLibraryFile;
                let disallowed = !(library || exports)
                    || (library_only
                        && if component == Suffix {
                            ty == "STATIC_LIBRARY"
                        } else {
                            ty == "EXECUTABLE"
                        });
                let base_id = match (kind, component) {
                    (LinkerFile, Name | Directory) => "TARGET_LINKER_FILE",
                    (LinkerLibraryFile, Name | Directory) => "TARGET_LINKER_LIBRARY_FILE",
                    (LinkerImportFile, Name | Directory) => "TARGET_LINKER_IMPORT_FILE",
                    _ => id,
                };
                if disallowed {
                    let allowed = if library_only {
                        "libraries with ENABLE_EXPORTS."
                    } else {
                        "libraries and executables with ENABLE_EXPORTS."
                    };
                    return Err(
                        self.error(expr, format!("{base_id} is allowed only for {allowed}"))
                    );
                }
            }
            _ => {}
        }

        if matches!(component, BaseName | Prefix | Suffix) {
            return self
                .host
                .target_artifact_component(&target, kind, component, config)
                .map_err(|message| self.error(expr, message));
        }
        self.ctx.referenced_targets.insert(target.clone());
        let policy = self.host.policy_status(&target, "CMP0112");
        let bundle = matches!(kind, BundleDir | BundleContentDir);
        if (component == Path && !bundle) || policy != PolicyStatus::New {
            self.ctx.dependency_targets.insert(target.clone());
            if (component != Path || bundle)
                && policy == PolicyStatus::Warn
                && self.host.policy_optional_warning("CMAKE_POLICY_WARNING_CMP0112")
            {
                self.policy_warning(
                    "CMP0112",
                    &format!("\nDependency being added to target:\n  \"{target}\"\n"),
                );
            }
        }
        let value = if kind == BundleDir && component == Name {
            self.host
                .target_artifact_component(&target, kind, component, config)
        } else if kind == LinkerLibraryFile && dll && !static_library {
            Ok(String::new())
        } else {
            self.host
                .target_artifact(&target, kind, config)
                .map(|value| match component {
                    Name => path::filename_name(&value).to_owned(),
                    Directory => path::filename_path(&value),
                    _ => value,
                })
        };
        value.map_err(|message| self.error(expr, message))
    }

    pub(super) fn target_objects_or_dlls(
        &mut self,
        node: &str,
        name: &str,
        expr: &str,
    ) -> Result<String, GenexError> {
        let target = self.host.resolve_target(name, None).ok_or_else(|| {
            self.error(
                expr,
                format!("Objects of target \"{name}\" referenced but no such target exists."),
            )
        })?;
        let ty = self.host.target_type(&target).unwrap_or_default();
        let objects = node == "TARGET_OBJECTS";
        if !(matches!(ty, "EXECUTABLE" | "SHARED_LIBRARY" | "MODULE_LIBRARY")
            || (objects && matches!(ty, "STATIC_LIBRARY" | "OBJECT_LIBRARY")))
        {
            let allowed = if objects {
                "EXECUTABLE, STATIC, SHARED, MODULE, OBJECT"
            } else {
                "EXECUTABLE, SHARED, MODULE"
            };
            return Err(self.error(expr, format!("Objects of target \"{name}\" referenced but is not one of the allowed target types ({allowed}).")));
        }
        let config = self.ctx.config;
        let mut paths = if objects {
            if !self.ctx.evaluate_for_buildsystem {
                self.host.known_object_location(&target).map_err(|reason|
                    self.error(expr, format!("The evaluation of the TARGET_OBJECTS generator expression is only suitable for consumption by CMake (limited{reason}).  It is not suitable for writing out elsewhere.")))?;
            }
            self.ctx.context_sensitive = true;
            if self.ctx.evaluate_for_buildsystem { self.host.target_objects_for_buildsystem(&target, config) }
            else { self.host.target_objects(&target, config) }
        } else {
            self.host.runtime_dlls(&target, config)
        }.map_err(|message| self.error(expr, message))?;
        if objects {
            for path in &paths {
                self.host.record_target_object(name, path);
            }
        }
        if node == "TARGET_RUNTIME_DLL_DIRS" {
            paths = list::dedup(paths.iter().map(|path| path::filename_path(path)).collect());
        }
        Ok(paths.join(";"))
    }
}
