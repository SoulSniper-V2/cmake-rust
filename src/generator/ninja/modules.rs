//! C++20 module dependency scanning, collation, and BMI generation.
//!
//! Port of the C++ module support in `cmNinjaTargetGenerator`,
//! `cmCommonTargetGenerator::GetLinkedTargetDirectories`,
//! `cmGeneratorTarget::{ApplyCXXStdTargets,DiscoverSyntheticTargets}`, and
//! the `CollationInformation*` blocks of `cmDyndepCollation`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use serde_json::{Value, json};
use sha3::{Digest, Sha3_512};

use super::*;
use crate::commands::string::json::stream_value;
use crate::generator::ctx::{self, Diagnostic, GenCtx};
use crate::install_model::InstallAction;
use crate::interp::Interp;
use crate::messages::MessageType;
use crate::state::{FileSet, PropertyEntry};

/// `cmGlobalGenerator::Compute` ordering: `ApplyCXXStdTargets` for every
/// target, then `DiscoverSyntheticTargets` — both while targets are still
/// mutable, before build files are generated.  Returns diagnostics raised
/// by the evaluations.
pub fn finalize_cxx_module_targets(interp: &mut Interp) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let keys: Vec<String> = interp.ext.target_order.clone();
    for key in &keys {
        apply_std_to_target(interp, key, &mut diagnostics);
    }
    // cmGeneratorTarget::DiscoverSyntheticTargets: worklist over target
    // keys; synthetic targets enqueue themselves since their own link
    // implementations may import further module targets.
    let mut queue = keys;
    let mut done: HashSet<String> = HashSet::new();
    // `(imported_key, synthetic_key)` — the `CxxModuleTargets` cache.
    let mut cache: Vec<(String, String)> = Vec::new();
    while let Some(key) = queue.pop() {
        if !done.insert(key.clone()) {
            continue;
        }
        let imported_keys = {
            let ctx = ctx::GenCtx::new(interp);
            diagnostics.extend(ctx.errors.take());
            let Some(target) = ctx.interp.ext.targets.get(&key) else {
                continue;
            };
            if target.imported {
                continue;
            }
            let config = ctx.config.clone();
            let lang = gt::linker_language(&ctx, target, &config);
            let mut imported = Vec::new();
            for (dep, _objects_item) in link::module_link_targets(&ctx, target, &lang) {
                if dep.imported && gt::have_cxx20_module_sources(&ctx, dep, &config) {
                    imported.push(ctx.target_key(dep));
                }
            }
            imported
        };
        for dep_key in imported_keys {
            let Some(synth_key) =
                synthesize_cxx_module_target(interp, &dep_key, &mut cache, &mut diagnostics)
            else {
                continue;
            };
            interp
                .ext
                .synthetic_deps
                .entry(dep_key.clone())
                .or_default()
                .push(synth_key.clone());
            // gtp->ApplyCXXStdTargets() + the recursive
            // gtp->DiscoverSyntheticTargets on the new target.
            apply_std_to_target(interp, &synth_key, &mut diagnostics);
            queue.push(synth_key);
        }
    }
    diagnostics
}

/// `cmGeneratorTarget::ApplyCXXStdTargets` for one target: diagnose
/// `CXX_MODULE_STD` and inject the `__CMAKE::CXX<level>` link item when the
/// toolchain provides it and `import std` support is enabled.
fn apply_std_to_target(interp: &mut Interp, target_key: &str, diagnostics: &mut Vec<Diagnostic>) {
    enum Action {
        Inject(String),
    }
    let action = {
        let ctx = ctx::GenCtx::new(interp);
        let result = (|| {
            let target = ctx.interp.ext.targets.get(target_key)?;
            let std_prop = target.props.get("CXX_MODULE_STD")?;
            let compiled = crate::genex::parse(std_prop);
            let mut ectx = crate::genex::EvalContext {
                config: "",
                ..crate::genex::EvalContext::default()
            };
            let mut host = crate::generator::genex_host::CtxHost::new(&ctx, target.makefile);
            let std_prop_value = match crate::genex::evaluate(&compiled, &mut ectx, &mut host) {
                Ok(value) => value,
                Err(error) => {
                    ctx.genex_error(Vec::new(), &error);
                    return None;
                }
            };
            if ectx.context_sensitive {
                ctx.error(
                    ctx.dir_backtrace(target.makefile),
                    format!(
                        "The \"CXX_MODULE_STD\" property on the target \"{}\" contains a context-sensitive condition that is not supported.",
                        target.name
                    ),
                );
                return None;
            }
            if !util::is_on(&std_prop_value) {
                return None;
            }
            if cxx_module_support_level(&ctx, target) != CxxSupportLevel::Supported {
                return None;
            }
            let level = explicit_cxx_standard(&ctx, target)?;
            // `standardResolver.LanguageStandardLevel("CXX", "23")`: the
            // smallest C++ standard level that can `import std`.
            if level < 23 {
                return None;
            }
            let target_name = format!("__CMAKE::CXX{level}");
            if find_target_to_use(&ctx, target.makefile, &target_name).is_none() {
                let reason = ctx
                    .var(
                        target.makefile,
                        &format!("CMAKE_CXX{level}_COMPILER_IMPORT_STD_NOT_FOUND_MESSAGE"),
                    )
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!("  Reason:\n  {reason}"))
                    .unwrap_or_default();
                ctx.error(
                    ctx.dir_backtrace(target.makefile),
                    format!(
                        "The \"CXX_MODULE_STD\" property on the target \"{}\" requires that the \"{target_name}\" target exist, but it was not provided by the toolchain.{reason}",
                        target.name
                    ),
                );
                return None;
            }
            // `cmExperimental::HasSupportEnabled(CxxImportStd)`: a toolchain
            // may provide the target while `import std` is disabled.
            if !experimental_enabled_ctx(&ctx, target.makefile, "CxxImportStd") {
                return None;
            }
            Some(Action::Inject(target_name))
        })();
        diagnostics.extend(ctx.errors.take());
        result
    };
    let Some(Action::Inject(target_name)) = action else {
        return;
    };
    let Some(target) = interp.ext.targets.get_mut(target_key) else {
        return;
    };
    // `$<$<CONFIG:cfg>:...>` guards the item for multi-config generators;
    // under the single-config Ninja generator the one config is always
    // active.
    let item = format!("$<BUILD_LOCAL_INTERFACE:{target_name}>");
    target.props.append("LINK_LIBRARIES", &item, false);
}

/// `cmExperimental::HasSupportEnabled` in a generation context.
fn experimental_enabled_ctx(ctx: &GenCtx, makefile: usize, feature: &str) -> bool {
    ctx.experimental_enabled(makefile, feature)
}

/// `cmSyntheticTargetCache::GetSyntheticTarget` + the copy logic in
/// `cmGeneratorTarget::DiscoverSyntheticTargets`.  Returns the `ext.targets`
/// key of the synthetic BMI-only interface target.
fn synthesize_cxx_module_target(
    interp: &mut Interp,
    imported_key: &str,
    cache: &mut Vec<(String, String)>,
    _diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    if let Some((_, synth)) = cache.iter().find(|(imp, _)| imp == imported_key) {
        return Some(synth.clone());
    }
    // Compute the name against an immutable view.
    let (name, makefile, file_sets, entries_props, entries_prov, policies) = {
        let imported = interp.ext.targets.get(imported_key)?;
        let imported_policies = imported.policies.clone();
        let safe_name = imported.name.replace(':', "_");
        // `cmCxxModuleUsageEffects` is a fixed placeholder upstream.
        let usage_hash = "0000000000000000000000000000000000000000";
        let dir_hash = hex(&Sha3_512::digest(
            interp.makefiles[imported.makefile].binary_dir.as_bytes(),
        ));
        let ident = hex(&Sha3_512::digest(
            format!("@d_{dir_hash}@u_{usage_hash}").as_bytes(),
        ));
        let name = format!("{safe_name}@synth_{}", &ident[..12]);
        // cmTarget::CopyImportedCxxModulesEntries: imported C++ module usage
        // requirements become private properties of the synthetic target.
        let mut entries_props: Vec<(String, String)> = Vec::new();
        let mut entries_prov: Vec<(String, Vec<PropertyEntry>)> = Vec::new();
        for (from, into) in [
            (
                "IMPORTED_CXX_MODULES_INCLUDE_DIRECTORIES",
                "INCLUDE_DIRECTORIES",
            ),
            (
                "IMPORTED_CXX_MODULES_COMPILE_DEFINITIONS",
                "COMPILE_DEFINITIONS",
            ),
            ("IMPORTED_CXX_MODULES_COMPILE_FEATURES", "COMPILE_FEATURES"),
            ("IMPORTED_CXX_MODULES_COMPILE_OPTIONS", "COMPILE_OPTIONS"),
            ("IMPORTED_CXX_MODULES_LINK_LIBRARIES", "LINK_LIBRARIES"),
        ] {
            if let Some(value) = imported.props.get(from) {
                entries_props.push((into.into(), value.to_owned()));
            }
            if let Some(entries) = imported.provenance.get(from) {
                entries_prov.push((into.into(), entries.clone()));
            }
        }
        if let Some(value) = imported.props.get("INTERFACE_CXX_MODULE_SETS") {
            entries_props.push(("CXX_MODULE_SETS".into(), value.to_owned()));
        }
        if let Some(entries) = imported.provenance.get("INTERFACE_CXX_MODULE_SETS") {
            entries_prov.push(("CXX_MODULE_SETS".into(), entries.clone()));
        }
        // cmTarget::CopyImportedCxxModulesProperties.
        for prop in CXX_MODULE_COPY_PROPERTIES {
            if let Some(value) = imported.props.get(prop) {
                entries_props.push(((*prop).into(), value.to_owned()));
            }
        }
        // Per-config copies (GetGeneratorConfigs(ExcludeEmptyConfig) is
        // empty for the single-config Ninja generator without a build type,
        // but an explicitly set CMAKE_BUILD_TYPE produces one config name).
        let config = interp
            .get_def("CMAKE_BUILD_TYPE")
            .unwrap_or_default()
            .to_ascii_uppercase();
        if !config.is_empty() {
            for prefix in [
                "EXCLUDE_FROM_DEFAULT_BUILD_",
                "IMPORTED_CXX_MODULES_",
                "MAP_IMPORTED_CONFIG_",
                "OSX_ARCHITECTURES_",
            ] {
                let prop = format!("{prefix}{config}");
                if let Some(value) = imported.props.get(&prop) {
                    entries_props.push((prop, value.to_owned()));
                }
            }
        }
        // FileSet is Clone-able field-by-field.
        let file_sets: BTreeMap<String, FileSet> = imported
            .file_sets
            .iter()
            .map(|(name, set)| {
                (
                    name.clone(),
                    FileSet {
                        kind: set.kind.clone(),
                        scope: set.scope,
                        base_dirs: set.base_dirs.clone(),
                        files: set.files.clone(),
                    },
                )
            })
            .collect();
        (
            name,
            imported.makefile,
            file_sets,
            entries_props,
            entries_prov,
            imported_policies,
        )
    };
    if let Some((_, synth)) = cache
        .iter()
        .find(|(imp, s)| imp == imported_key || s == &name)
    {
        return Some(synth.clone());
    }

    // `cmMakefile::AddSynthesizedTarget(INTERFACE_LIBRARY, name)`.
    let mut target = Target::new(&name, crate::state::TargetKind::InterfaceLibrary, makefile);
    target.synthetic = true;
    target.template_name = Some(
        interp
            .ext
            .targets
            .get(imported_key)
            .map(|t| t.name.clone())
            .unwrap_or_default(),
    );
    target.policies = policies;
    target.file_sets = file_sets;
    for (prop, value) in entries_props {
        target.props.set(&prop, value);
    }
    for (prop, entries) in entries_prov {
        target.provenance.insert(prop, entries);
    }
    // `$<COMPILE_ONLY:model>` gives the synthetic target the imported
    // target's usage requirements for its own compilations.
    let model = target.template_name.clone().unwrap_or_default();
    target
        .props
        .append("LINK_LIBRARIES", &format!("$<COMPILE_ONLY:{model}>"), false);

    let key = crate::state::register_target(interp, target);
    cache.push((imported_key.to_owned(), key.clone()));
    Some(key)
}

/// `cmTarget::CopyImportedCxxModulesProperties` list.
const CXX_MODULE_COPY_PROPERTIES: &[&str] = &[
    "DEFINE_SYMBOL",
    "DEPRECATION",
    "NO_SYSTEM_FROM_IMPORTED",
    "POSITION_INDEPENDENT_CODE",
    "VISIBILITY_INLINES_HIDDEN",
    "ANDROID_API",
    "ANDROID_API_MIN",
    "ANDROID_ARCH",
    "ANDROID_STL_TYPE",
    "OSX_ARCHITECTURES",
    "MSVC_DEBUG_INFORMATION_FORMAT",
    "MSVC_RUNTIME_CHECKS",
    "MSVC_RUNTIME_LIBRARY",
    "VS_PLATFORM_TOOLSET",
    "WATCOM_RUNTIME_LIBRARY",
    "CXX_COMPILER_LAUNCHER",
    "CXX_STANDARD",
    "CXX_STANDARD_REQUIRED",
    "CXX_EXTENSIONS",
    "CXX_VISIBILITY_PRESET",
    "CXX_MODULE_STD",
    "CXX_CLANG_TIDY",
    "CXX_CLANG_TIDY_EXPORT_FIXES_DIR",
    "CXX_CPPLINT",
    "CXX_CPPCHECK",
    "CXX_ICSTAT",
    "CXX_INCLUDE_WHAT_YOU_USE",
    "EXCLUDE_FROM_ALL",
    "EXCLUDE_FROM_DEFAULT_BUILD",
    "OPTIMIZE_DEPENDENCIES",
    "JOB_POOL_COMPILE",
    "VS_NO_COMPILE_BATCHING",
    "VS_PROJECT_IMPORT",
    "EchoString",
    "EXPORT_COMPILE_COMMANDS",
    "FOLDER",
    "LABELS",
    "PROJECT_LABEL",
    "SYSTEM",
];

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `cmMakefile::FindTargetToUse` from a directory's scope.
fn find_target_to_use<'i>(ctx: &GenCtx<'i>, dir: usize, name: &str) -> Option<&'i Target> {
    crate::state::find_target_key_at(ctx.interp, dir, name)
        .and_then(|key| ctx.interp.ext.targets.get(&key))
}

/// `cmGeneratorTarget::HaveCxxModuleSupport` (free-standing variant for
/// the mutable finalization phase; `NinjaGenerator` has the same logic on
/// `&self` for generation).
fn cxx_module_support_level(ctx: &GenCtx, target: &Target) -> CxxSupportLevel {
    if !ctx.interp.langs.enabled.iter().any(|l| l == "CXX") {
        return CxxSupportLevel::MissingCxx;
    }
    if ctx
        .var(target.makefile, "CMAKE_CXX_STANDARD_DEFAULT")
        .is_none_or(|default| default.is_empty())
    {
        return CxxSupportLevel::NoCxx20;
    }
    if explicit_cxx_standard(ctx, target).is_none_or(|level| level < 20) {
        return CxxSupportLevel::NoCxx20;
    }
    if ctx
        .var(target.makefile, "CMAKE_CXX_SCANDEP_SOURCE")
        .is_none()
    {
        return CxxSupportLevel::MissingRule;
    }
    CxxSupportLevel::Supported
}

/// `cmStandardLevelResolver::GetExplicitStandardLevel` (duplicated from
/// `NinjaGenerator::explicit_cxx_standard` for the mutable phase).
fn explicit_cxx_standard(ctx: &GenCtx, target: &Target) -> Option<u32> {
    let mut level: Option<u32> = None;
    if let Some(standard) = target.props.get("CXX_STANDARD") {
        let standard = ctx.genex(standard, Some(target), None);
        let standard = standard
            .trim()
            .strip_suffix("-FORCED")
            .unwrap_or(standard.trim());
        if let Ok(requested) = standard.parse::<u32>() {
            level = Some(level.unwrap_or(0).max(requested));
        }
    }
    let features = target
        .props
        .get("COMPILE_FEATURES")
        .map(|features| ctx.genex(features, Some(target), None))
        .unwrap_or_default();
    for feature in util::expand_list(&features, false) {
        if let Some(requested) = feature
            .strip_prefix("cxx_std_")
            .and_then(|requested| requested.parse::<u32>().ok())
        {
            level = Some(level.unwrap_or(0).max(requested));
        }
    }
    level
}

impl<'a> NinjaGenerator<'a> {
    /// `ImportedCxxModuleLookup::BmiNameForSource` for the target's
    /// `IMPORTED_CXX_MODULES_<CONFIG>` table.
    fn cxx_bmi_name(&self, target: &Target, source_path: &str) -> String {
        let config_upper = self.config.to_ascii_uppercase();
        let prop = format!("IMPORTED_CXX_MODULES_{config_upper}");
        if let Some(modules) = target
            .props
            .get(&prop)
            .or_else(|| target.props.get("IMPORTED_CXX_MODULES_NOCONFIG"))
        {
            for entry in util::expand_list(modules, false) {
                let Some(sep) = entry.find('=') else {
                    continue;
                };
                let name = &entry[..sep];
                let rest = &entry[sep + 1..];
                let source = rest.split(',').next().unwrap_or(rest);
                if source == source_path {
                    let digest = hex(&Sha3_512::digest(name.as_bytes()));
                    return format!("{}.bmi", &digest[..12]);
                }
            }
        }
        let digest = hex(&Sha3_512::digest(source_path.as_bytes()));
        format!("{}.bmi", &digest[..12])
    }

    /// `cmNinjaTargetGenerator::GetBmiFilePath`.
    fn cxx_bmi_path(&self, target: &Target, source: &gt::SourceInfo) -> String {
        format!(
            "{}/{}",
            self.object_dir(target),
            self.cxx_bmi_name(target, &source.path)
        )
    }

    /// `cmNinjaTargetGenerator::GetDyndepFilePath` for CXX.
    fn cxx_dyndep_path(&self, target: &Target) -> String {
        format!("{}/CXX.dd", self.object_dir(target))
    }

    /// `cmNinjaTargetGenerator::LanguageScanRule` / `LanguageDyndepRule`.
    fn cxx_rule_name(&self, target: &Target, step: &str) -> String {
        format!(
            "CXX_{step}__{}_{}",
            encode_rule_name(&target.name),
            self.config
        )
    }

    /// `cmNinjaTargetGenerator::WriteCompileRule("CXX", WithScanning::Yes)`:
    /// the `CXX_SCAN__` and `CXX_DYNDEP__` rules (the scanned compile rule
    /// `CXX_COMPILER__` comes from `write_compile_rule(..., scanned=true)`).
    pub(super) fn write_cxx_scan_rules(&mut self, target: &Target) {
        let mf = target.makefile;
        let tdi = self
            .shell(&self.ninja_path(&format!("{}/CXXDependInfo.json", self.object_dir(target))));
        let cmake = self.cmake_cmd.clone();
        let modmap_format = self.ctx.var_safe(mf, "CMAKE_CXX_MODULE_MAP_FORMAT");

        // CXX compiles preprocess (`compilationPreprocesses`), so the scan
        // rule runs `CMAKE_CXX_SCANDEP_SOURCE` directly on the source.
        let scan_dep_type = self.ctx.var_safe(mf, "CMAKE_CXX_SCANDEP_DEPFILE_FORMAT");
        let depfile = if scan_dep_type == "msvc" {
            ""
        } else {
            "$DEP_FILE"
        };
        let deptype = if scan_dep_type == "msvc" { "msvc" } else { "" };
        let launcher = rule_launcher(&self.ctx, target, "RULE_LAUNCH_COMPILE");
        // cmMakefile::GetRequiredDefinition: only an unset variable is an
        // error; an empty value yields an empty command list.
        let scan_cmd = match self.ctx.var(mf, "CMAKE_CXX_SCANDEP_SOURCE") {
            Some(value) => value,
            None => {
                self.ctx.error(
                    target.backtrace.clone(),
                    "Error required internal CMake variable not set, cmake may not be built correctly.\nMissing variable is:\nCMAKE_CXX_SCANDEP_SOURCE".to_owned(),
                );
                String::new()
            }
        };
        let mut rule = Rule {
            name: self.cxx_rule_name(target, "SCAN"),
            comment: "Rule for generating CXX dependencies.".into(),
            description: "Scanning $in for CXX dependencies".into(),
            depfile: depfile.into(),
            deptype: deptype.into(),
            ..Rule::default()
        };
        let (mut defines, mut includes, mut flags) = ("$DEFINES", "$INCLUDES", "$FLAGS".to_owned());
        if self.cxx_force_rsp(target) {
            let flag = self
                .ctx
                .var(mf, "CMAKE_CXX_RESPONSE_FILE_FLAG")
                .filter(|flag| !flag.is_empty())
                .unwrap_or_else(|| "@".into());
            rule.rspfile = "$RSP_FILE".into();
            rule.rspcontent = format!(" {defines} {includes} {flags}");
            flags = format!("{flag}$RSP_FILE");
            defines = "";
            includes = "";
        }
        let vars: Vec<(&str, String)> = vec![
            ("TARGET_NAME", target.name.clone()),
            ("TARGET_TYPE", target.kind.type_name().into()),
            ("LANGUAGE", "CXX".into()),
            ("SOURCE", "$in".into()),
            ("OBJECT", "$OBJ_FILE".into()),
            ("PREPROCESSED_SOURCE", "$PREPROCESSED_OUTPUT_FILE".into()),
            ("DYNDEP_FILE", "$DYNDEP_INTERMEDIATE_FILE".into()),
            ("DEP_FILE", depfile.into()),
            ("DEP_TARGET", "$out".into()),
            ("CONFIG", "$CONFIG".into()),
            ("DEFINES", defines.into()),
            ("INCLUDES", includes.into()),
            ("FLAGS", flags),
        ];
        let cmds: Vec<String> = util::expand_list(&scan_cmd, false)
            .iter()
            .map(|cmd| flags::expand_rule(&self.ctx, target, &format!("{launcher}{cmd}"), &vars))
            .collect();
        rule.command = self.build_command_line(&cmds);
        self.add_rule(&rule);

        // Rule to collate scan results into the ninja dyndep file.
        let mut modmap_arg = String::new();
        if !modmap_format.is_empty() {
            modmap_arg = format!(" --modmapfmt={modmap_format}");
        }
        self.add_rule(&Rule {
            name: self.cxx_rule_name(target, "DYNDEP"),
            comment: "Rule to generate ninja dyndep files for CXX.".into(),
            description: "Generating CXX dyndep file $out".into(),
            command: self.build_command_line(&[format!(
                "{cmake} -E cmake_ninja_dyndep --tdi={tdi} --lang=CXX{modmap_arg} --dd=$out @$out.rsp"
            )]),
            rspfile: "$out.rsp".into(),
            rspcontent: "$in".into(),
            restat: "1".into(),
            ..Rule::default()
        });
    }

    /// `GetScanBuildStatement` for the `compilePP == false` /
    /// `compilationPreprocesses == true` case (CXX sources scan directly).
    /// Mutates `obj` to add the dyndep binding and modmap wiring; writes the
    /// scan build to `out` and returns `(ddi, Option<modmap>)`.
    pub(super) fn write_cxx_scan(
        &self,
        out: &mut String,
        target: &Target,
        obj: &mut Build,
    ) -> (String, Option<String>) {
        let object = obj.outputs[0].clone();
        let ddi = format!("{object}.ddi");
        let mut scan = Build::new(&self.cxx_rule_name(target, "SCAN"));
        scan.explicit_deps.clone_from(&obj.explicit_deps);
        scan.implicit_deps.clone_from(&obj.implicit_deps);
        scan.order_only_deps.clone_from(&obj.order_only_deps);
        if let Some(in_abs) = obj.variables.get("IN_ABS").cloned() {
            scan.variables.insert("IN_ABS".into(), in_abs);
        }
        for key in ["FLAGS", "DEFINES", "INCLUDES"] {
            if let Some(value) = obj.variables.get(key).cloned() {
                scan.variables.insert(key.into(), value);
            }
        }
        scan.variables.insert("OBJ_FILE".into(), object.clone());
        scan.variables
            .insert("DYNDEP_INTERMEDIATE_FILE".into(), ddi.clone());
        scan.rsp_file = format!("{ddi}.rsp");
        scan.outputs.push(ddi.clone());
        scan.variables
            .insert("PREPROCESSED_OUTPUT_FILE".into(), format!("{object}.ddi.i"));
        self.add_depfile_binding(&mut scan.variables, format!("{}.d", scan.outputs[0]));
        if let Some(pool) = target.props.get("JOB_POOL_COMPILE") {
            for build in [&mut scan, &mut *obj] {
                build.variables.insert("pool".into(), pool.into());
            }
        }
        write_build(out, &scan, if self.cxx_force_rsp(target) { -1 } else { 0 });
        let dyndep = self.ninja_path(&self.cxx_dyndep_path(target));
        obj.order_only_deps.push(dyndep.clone());
        obj.variables.insert("dyndep".into(), dyndep);
        let modmap_format = self
            .ctx
            .var_safe(target.makefile, "CMAKE_CXX_MODULE_MAP_FORMAT");
        let mut modmap = None;
        if !modmap_format.is_empty() {
            let file = format!("{object}.modmap");
            obj.variables
                .insert("DYNDEP_MODULE_MAP_FILE".into(), file.clone());
            obj.implicit_deps.push(file.clone());
            modmap = Some(file);
        }
        (ddi, modmap)
    }

    /// `cmNinjaTargetGenerator::WriteCxxModuleBmiBuildStatement`.  Emits the
    /// `.ddi` scan build and the BMI compile build; returns
    /// `(ddi, Option<modmap>)`.
    fn write_cxx_bmi_build(
        &mut self,
        out: &mut String,
        target: &Target,
        source: &gt::SourceInfo,
        order_target: &str,
    ) -> (String, Option<String>) {
        if source.language != "CXX" {
            self.ctx.error(
                target.backtrace.clone(),
                format!(
                    "Source file '{}' of target '{}' is a '{}' source but must be 'CXX' in order to have a BMI build statement generated.",
                    source.path, target.name, source.language
                ),
            );
        }
        let bmi_file = self.ninja_path(&self.cxx_bmi_path(target, source));
        let bmi_dir = self.ninja_path(&self.object_dir(target));
        let bmi_file_dir = util::filename_path(&bmi_file);
        let source_abs = self.ninja_abs_path(&source.path);
        let source_props = crate::commands::target::find_known_source(
            self.ctx.interp,
            target.makefile,
            &source.path,
        )
        .map(|index| {
            self.ctx.interp.ext.sources[&target.makefile][index]
                .props
                .clone()
        })
        .unwrap_or_default();
        let mut vars_map = flags::compile_vars_with_source_props(
            &self.ctx,
            target,
            &self.config,
            source,
            &source_props,
        );
        // cmGeneratorTarget::ComputeFlagsForObject: non-normal (synthetic)
        // targets compile BMIs with CMAKE_CXX_MODULE_BMI_ONLY_FLAG bound to
        // the BMI path.
        let bmi_only_flag = self
            .ctx
            .var_safe(target.makefile, "CMAKE_CXX_MODULE_BMI_ONLY_FLAG");
        if !bmi_only_flag.is_empty() {
            let flag = flags::expand_rule(
                &self.ctx,
                target,
                &bmi_only_flag,
                &[("OBJECT", bmi_file.clone())],
            );
            flags::append(&mut vars_map.flags, &flag);
        }

        let mut build = Build::new(&self.compiler_rule_name("CXX", target, true));
        build.outputs.push(bmi_file.clone());
        build.explicit_deps.push(source_abs);
        build.order_only_deps.push(order_target.to_owned());
        build
            .variables
            .insert("FLAGS".into(), vars_map.flags.clone());
        build
            .variables
            .insert("DEFINES".into(), vars_map.defines.clone());
        build
            .variables
            .insert("INCLUDES".into(), vars_map.includes.clone());
        build.variables.insert("CONFIG".into(), self.config.clone());
        if self
            .ctx
            .var_safe(target.makefile, "CMAKE_CXX_DEPFILE_FORMAT")
            != "msvc"
        {
            let replace_ext = self
                .ctx
                .var(target.makefile, "CMAKE_CXX_DEPFILE_EXTENSION_REPLACE")
                .is_some_and(|value| util::is_on(&value));
            let depfile = if replace_ext {
                let name = util::filename_name(&bmi_file);
                let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(&name);
                format!("{bmi_file_dir}/{stem}.d")
            } else {
                format!("{bmi_file}.d")
            };
            self.add_depfile_binding(&mut build.variables, depfile);
        }
        self.set_msvc_target_pdb_variable(target, &mut build.variables);
        let mut export_vars = build.variables.clone();
        export_vars.insert("FLAGS".into(), vars_map.flags_untrimmed.clone());
        self.export_object_compile_command_scanned(target, source, &bmi_file, &export_vars);
        let object_dir_rel = self.ninja_path(&self.object_dir(target));
        build
            .variables
            .insert("OBJECT_DIR".into(), self.shell(&object_dir_rel));
        build
            .variables
            .insert("OBJECT_FILE_DIR".into(), self.shell(&bmi_file_dir));
        let _ = bmi_dir;

        // Scan build (compilationPreprocesses): `.ddi` direct output.
        let ddi = format!("{bmi_file}.ddi");
        let mut scan = Build::new(&self.cxx_rule_name(target, "SCAN"));
        scan.explicit_deps.clone_from(&build.explicit_deps);
        scan.implicit_deps.clone_from(&build.implicit_deps);
        scan.order_only_deps.clone_from(&build.order_only_deps);
        if let Some(in_abs) = build.variables.get("IN_ABS").cloned() {
            scan.variables.insert("IN_ABS".into(), in_abs);
        }
        for key in ["FLAGS", "DEFINES", "INCLUDES"] {
            if let Some(value) = build.variables.get(key).cloned() {
                scan.variables.insert(key.into(), value);
            }
        }
        scan.variables.insert("OBJ_FILE".into(), bmi_file.clone());
        scan.variables
            .insert("DYNDEP_INTERMEDIATE_FILE".into(), ddi.clone());
        scan.rsp_file = format!("{ddi}.rsp");
        scan.outputs.push(ddi.clone());
        scan.variables.insert(
            "PREPROCESSED_OUTPUT_FILE".into(),
            format!("{bmi_file}.ddi.i"),
        );
        self.add_depfile_binding(&mut scan.variables, format!("{}.d", scan.outputs[0]));
        if let Some(pool) = target.props.get("JOB_POOL_COMPILE") {
            for b in [&mut scan, &mut build] {
                b.variables.insert("pool".into(), pool.into());
            }
        }
        write_build(out, &scan, if self.cxx_force_rsp(target) { -1 } else { 0 });

        let dyndep = self.ninja_path(&self.cxx_dyndep_path(target));
        build.order_only_deps.push(dyndep.clone());
        build.variables.insert("dyndep".into(), dyndep);
        let modmap_format = self
            .ctx
            .var_safe(target.makefile, "CMAKE_CXX_MODULE_MAP_FORMAT");
        let mut modmap = None;
        if !modmap_format.is_empty() {
            // For BMI builds the modmap is only an implicit output of the
            // dyndep collation (unlike object builds which also depend on it).
            let file = format!("{bmi_file}.modmap");
            build
                .variables
                .insert("DYNDEP_MODULE_MAP_FILE".into(), file.clone());
            modmap = Some(file);
        }
        build.rsp_file = format!("{bmi_file}.rsp");
        write_build(out, &build, if self.cxx_force_rsp(target) { -1 } else { 0 });
        (ddi, modmap)
    }

    /// `cmCommonTargetGenerator::GetLinkedTargetDirectories` for CXX (the
    /// `$<TARGET_OBJECTS>` forwarding branch is Fortran-only upstream).
    /// Requires `module_order` to be initialized.
    fn cxx_linked_target_dirs(&self, target: &Target) -> (Vec<String>, Vec<String>) {
        let mut direct = Vec::new();
        let forward: Vec<String> = Vec::new();
        let lang = gt::linker_language(&self.ctx, target, &self.config);
        for (dep, _objects) in link::module_link_targets(&self.ctx, target, &lang) {
            if dep.imported
                || (dep.kind == TargetKind::InterfaceLibrary && !dep.synthetic)
                || self.module_order.get(&dep.name) >= self.module_order.get(&target.name)
                || !gt::have_cxx20_module_sources(&self.ctx, dep, &self.config)
            {
                continue;
            }
            let dir = self.object_dir(dep);
            if !direct.contains(&dir) {
                direct.push(dir);
            }
        }
        (direct, forward)
    }

    /// `cmGeneratorTarget::BuildDatabasePath` for CXX.
    fn build_database_path(&self, target: &Target) -> Option<String> {
        let enabled = target
            .props
            .get("EXPORT_BUILD_DATABASE")
            .is_some_and(util::is_on);
        if !enabled {
            return None;
        }
        let key = self.ctx.target_key(target);
        if !experimental_enabled_ctx_key(&self.ctx, &key, "ExportBuildDatabase") {
            return None;
        }
        Some(format!(
            "{}/CXX_build_database.json",
            self.object_dir(target)
        ))
    }

    /// `cmBuildDatabase::ForTarget` + `Write`: the `<path>.in` template the
    /// dyndep collator filters into the final build database.
    fn write_build_database(&mut self, target: &Target, path: &str) {
        let config = self.config.clone();
        let compiler = self.ctx.var_safe(target.makefile, "CMAKE_CXX_COMPILER");
        let mut translation_units = Vec::new();
        for source in gt::sources(&self.ctx, target, &config).iter() {
            if source.language != "CXX" || source.kind != SourceKind::ObjectSource {
                continue;
            }
            let source_props = crate::commands::target::find_known_source(
                self.ctx.interp,
                target.makefile,
                &source.path,
            )
            .map(|index| {
                self.ctx.interp.ext.sources[&target.makefile][index]
                    .props
                    .clone()
            })
            .unwrap_or_default();
            let vars = flags::compile_vars_with_source_props(
                &self.ctx,
                target,
                &config,
                source,
                &source_props,
            );
            let mut source_flags = String::new();
            if let Some(compile_flags) = source_props.get("COMPILE_FLAGS") {
                flags::append(
                    &mut source_flags,
                    &self.ctx.genex(compile_flags, Some(target), None),
                );
            }
            if let Some(options) = source_props.get("COMPILE_OPTIONS") {
                for option in util::expand_list(&self.ctx.genex(options, Some(target), None), false)
                {
                    flags::append(&mut source_flags, &option);
                }
            }
            let split = flags::unix_command_line;
            let baseline_args: Vec<String> = {
                let mut v = vec![compiler.clone()];
                v.extend(split(&vars.flags));
                v.extend(split(&vars.defines));
                v.extend(split(&vars.includes));
                v
            };
            let private_args = split(&source_flags);
            let mut local_args = baseline_args.clone();
            local_args.extend(private_args.iter().cloned());
            let object = format!("{}/{}", self.object_dir(target), source.object_name);
            let mut args = local_args.clone();
            args.push("-o".into());
            args.push(object.clone());
            args.push("-c".into());
            args.push(source.path.clone());
            let classified = |arg: &String| {
                if baseline_args.contains(arg) {
                    json!({"flag":arg,"classified":["BaselineFlag","LocationFlag","PrivateFlag"]})
                } else if private_args.contains(arg) {
                    json!({"flag":arg,"classified":["LocationFlag","PrivateFlag"]})
                } else {
                    json!({"flag":arg,"classified":["LocationFlag"]})
                }
            };
            translation_units.push(json!({
                "arguments": args.iter().map(classified).collect::<Vec<_>>(),
                "baseline-arguments": baseline_args.iter().map(|arg| json!({"flag":arg})).collect::<Vec<_>>(),
                "local-arguments": local_args.iter().map(|arg| json!({"flag":arg})).collect::<Vec<_>>(),
                "private-arguments": private_args.iter().map(|arg| json!({"flag":arg})).collect::<Vec<_>>(),
                "source": source.path,
                "work-directory": self.home_binary,
                "object": object,
            }));
        }
        let content = json!({"translation-units": translation_units});
        if let Err(err) =
            crate::fsutil::write_generated(Path::new(path), stream_value(&content).as_bytes())
        {
            self.ctx.error(
                target.backtrace.clone(),
                format!("Failed to write {path}: {err}"),
            );
        }
    }

    /// `cmDyndepCollation::AddCollationInformation` (CXX) +
    /// `cmNinjaTargetGenerator::WriteTargetDependInfo`.
    fn write_cxx_depend_info(
        &self,
        target: &Target,
        sources: &[gt::SourceInfo],
        tdi_path: &str,
        direct: &[String],
        forward: &[String],
    ) {
        let mf = &self.ctx.interp.makefiles[target.makefile];
        let var = |suffix: &str| {
            self.ctx
                .var_safe(target.makefile, &format!("CMAKE_CXX_{suffix}"))
        };
        let includes: Vec<String> = flags::include_items(&self.ctx, target, &self.config, "CXX")
            .iter()
            .map(|(dir, _, _)| self.ninja_path(dir))
            .collect();

        // CollationInformationSources: object sources + BMI-only sources
        // keyed by full path; CXX_MODULES file-set members are consumed
        // first, the rest lands in `sources` when it needs dyndep.
        let mut sf_map: BTreeMap<String, (&gt::SourceInfo, bool)> = BTreeMap::new();
        for source in sources.iter() {
            match source.kind {
                SourceKind::ObjectSource => {
                    sf_map.insert(source.path.clone(), (source, false));
                }
                SourceKind::CxxModuleSource => {
                    sf_map.insert(source.path.clone(), (source, true));
                }
                _ => {}
            }
        }

        let mut tdi_modules = json!({});
        let mut visited: HashSet<String> = HashSet::new();
        // `GetAllFileSetNames` order (BTreeMap: sorted by set name); only
        // CXX_MODULES sets participate.
        for (set_name, set) in &target.file_sets {
            if set.kind != "CXX_MODULES" {
                continue;
            }
            let destination = self.cxx_file_set_destination(target, set_name);
            let mut files_per_dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (file, relative, _backtrace) in gt::evaluate_file_set(&self.ctx, target, set) {
                // cmFileSet::EvaluateFileEntry keys files by
                // GetParentDirectory(RelativePath(base, file)).
                let rel_dir = relative
                    .rsplit_once('/')
                    .map(|(dir, _)| dir.to_owned())
                    .unwrap_or_default();
                files_per_dirs.entry(rel_dir).or_default().push(file);
            }
            for (relative, files) in &files_per_dirs {
                for file in files {
                    let full = util::collapse_full_path(file, "");
                    let Some((source, bmi_only)) = sf_map.remove(&full) else {
                        if !visited.insert(full.clone()) {
                            self.ctx.message(
                                MessageType::AuthorWarning,
                                target.backtrace.clone(),
                                format!(
                                    "Target \"{}\" has source file\n  {file}\nin a \"FILE_SET TYPE CXX_MODULES\" multiple times.",
                                    target.name
                                ),
                            );
                            continue;
                        }
                        self.ctx.error(
                            self.ctx.dir_backtrace(target.makefile),
                            format!(
                                "Target \"{}\" has source file\n  {file}\nin a \"FILE_SET TYPE CXX_MODULES\" but it is not scheduled for compilation.",
                                target.name
                            ),
                        );
                        continue;
                    };
                    visited.insert(full.clone());
                    let obj_path = if bmi_only {
                        self.ninja_path(&self.cxx_bmi_path(target, source))
                    } else {
                        self.ninja_path(&format!(
                            "{}/{}",
                            self.object_dir(target),
                            source.object_name
                        ))
                    };
                    tdi_modules[obj_path] = json!({
                        "source": full,
                        "bmi-only": bmi_only,
                        "relative-directory": relative,
                        "name": set_name,
                        "type": "CXX_MODULES",
                        "visibility": set.scope.name(),
                        "destination": destination,
                    });
                }
            }
        }

        let mut tdi_sources = json!({});
        for (path, (source, bmi_only)) in &sf_map {
            if *bmi_only {
                continue;
            }
            if !self.need_dyndep_for_source(target, &[], source) {
                continue;
            }
            let obj_path = self.ninja_path(&format!(
                "{}/{}",
                self.object_dir(target),
                source.object_name
            ));
            let _ = path;
            tdi_sources[obj_path] = json!({"source": source.path, "language": source.language});
        }

        let (exports, bmi) = self.fortran_export_info(target);
        let database_info = self
            .build_database_path(target)
            .map(|path| json!({"template-path": format!("{path}.in"), "output": path}))
            .unwrap_or(Value::Null);
        // `ExpandCFGIntDir(CollapseFullPath(ObjectDirectory))` — the
        // absolute module dir for CXX.
        let module_dir = util::collapse_full_path(&self.object_dir(target), "");
        let tdi = json!({
            "language": "CXX",
            "compiler-id": var("COMPILER_ID"),
            "compiler-simulate-id": var("SIMULATE_ID"),
            "compiler-frontend-variant": var("COMPILER_FRONTEND_VARIANT"),
            "module-dir": module_dir,
            "dir-cur-src": mf.source_dir,
            "dir-cur-bld": mf.binary_dir,
            "dir-top-bld": self.home_binary,
            "dir-top-src": self.home_source,
            "include-dirs": includes,
            "linked-target-dirs": direct,
            "forward-modules-from-target-dirs": forward,
            "sources": tdi_sources,
            "cxx-modules": tdi_modules,
            "database-info": database_info,
            "bmi-installation": bmi,
            "exports": exports,
            "config": self.config,
        });
        if let Err(err) =
            crate::fsutil::write_generated(Path::new(tdi_path), stream_value(&tdi).as_bytes())
        {
            self.ctx.error(
                target.backtrace.clone(),
                format!("Failed to write {tdi_path}: {err}"),
            );
        }
    }

    /// `cmInstallFileSetGenerator::GetDestination` for this target's file
    /// set, or null.
    fn cxx_file_set_destination(&self, target: &Target, set_name: &str) -> Value {
        for rule in self
            .ctx
            .interp
            .ext
            .install
            .rules
            .get(&target.makefile)
            .into_iter()
            .flatten()
        {
            if let InstallAction::FileSet { name, set } = &rule.action
                && name == &target.name
                && set == set_name
            {
                return json!(
                    self.ctx
                        .genex(&rule.options.destination, Some(target), None)
                );
            }
        }
        Value::Null
    }

    /// The dyndep collation build statement (`CXX.dd`) plus the
    /// `CXXDependInfo.json` it consumes — the `ScanningInfo` loop of
    /// `WriteObjectBuildStatements`.
    pub(super) fn write_cxx_dyndep(
        &mut self,
        out: &mut String,
        target: &Target,
        sources: &[gt::SourceInfo],
        ddis: Vec<String>,
        modmaps: Vec<String>,
    ) {
        if self.module_order.is_empty() {
            self.module_target_order();
        }
        let object_dir = self.object_dir(target);
        let tdi_path = format!("{object_dir}/CXXDependInfo.json");
        let (direct, forward) = self.cxx_linked_target_dirs(target);
        let mut build = Build::new(&self.cxx_rule_name(target, "DYNDEP"));
        build
            .outputs
            .push(self.ninja_path(&format!("{object_dir}/CXX.dd")));
        build
            .implicit_outs
            .push(self.ninja_path(&format!("{object_dir}/CXXModules.json")));
        build.explicit_deps = ddis;
        build.implicit_deps.push(self.ninja_path(&tdi_path));
        if let Some(bdb_path) = self.build_database_path(target) {
            let template = format!("{bdb_path}.in");
            self.write_build_database(target, &template);
            build.implicit_deps.push(template.clone());
            build.implicit_outs.push(self.ninja_path(&bdb_path));
        }
        for mm in &modmaps {
            build.implicit_outs.push(mm.clone());
        }
        build.implicit_deps.extend(
            direct
                .iter()
                .chain(&forward)
                .map(|dir| self.ninja_path(&format!("{dir}/CXXModules.json"))),
        );
        write_build(out, &build, 0);
        // WriteTargetDependInfo after the build statement (upstream writes
        // the TDI inside the same loop; ordering in the file does not
        // matter since it is produced during generation).
        self.write_cxx_depend_info(target, sources, &tdi_path, &direct, &forward);
    }

    /// `cmNinjaNormalTargetGenerator::Generate` for a synthetic interface
    /// library with C++ module sources: language rules, BMI builds, dyndep,
    /// and the module library phony — no link statement.
    pub(super) fn generate_synthetic_target(&mut self, target: &'a Target) {
        self.write_cxx_scan_rules(target);
        self.write_compile_rule("CXX", target, true);
        self.write_compile_rule("CXX", target, false);
        let mut out = String::new();
        write_divider(&mut out);
        let _ = write!(
            out,
            "# Object build statements for {} target {}\n\n",
            target.kind.type_name(),
            target.name
        );
        let order_target = format!("cmake_object_order_depends_target_{}", target.name);
        {
            let mut build = Build::new("phony");
            build.comment = format!("Order-only phony target for {}", target.name);
            build.outputs.push(order_target.clone());
            let mut order = Vec::new();
            self.append_target_depends(target, &mut order, DependOn::TargetOrdering);
            order.sort();
            order.dedup();
            if order.is_empty() {
                order.push(self.output_path("."));
            }
            build.order_only_deps = order;
            write_build(&mut out, &build, 0);
        }
        let sources = gt::cxx_module_sources(&self.ctx, target, &self.config);
        let mut ddis = Vec::new();
        let mut modmaps = Vec::new();
        for source in &sources {
            let (ddi, modmap) = self.write_cxx_bmi_build(&mut out, target, source, &order_target);
            ddis.push(ddi);
            if let Some(mm) = modmap {
                modmaps.push(mm);
            }
        }
        // Upstream emits the dyndep collation whenever scanning info exists.
        if !ddis.is_empty() {
            self.write_cxx_dyndep(&mut out, target, &sources, ddis, modmaps);
        } else {
            // The phony still needs its dyndep edge input to exist upstream;
            // with no BMI sources there is nothing to collate.
        }
        out.push('\n');
        self.build.push_str(&out);
        // WriteCxxModuleLibraryStatement.
        let mut build = Build::new("phony");
        build.comment = format!("Imported C++ module library {}", target.name);
        self.append_target_outputs(target, &mut build.outputs, DependOn::TargetArtifact);
        build
            .explicit_deps
            .push(self.ninja_path(&self.cxx_dyndep_path(target)));
        self.write_build(&build);
        self.add_target_alias(&target.name.clone(), target);
    }

    fn cxx_force_rsp(&self, target: &Target) -> bool {
        self.ctx
            .var(target.makefile, "CMAKE_NINJA_FORCE_RESPONSE_FILE")
            .is_some()
            || std::env::var_os("CMAKE_NINJA_FORCE_RESPONSE_FILE").is_some()
    }

    /// `cmNinjaTargetGenerator::ExportObjectCompileCommand` for a scanned
    /// source: appends the module-map flag with `<MODULE_MAP_FILE>` bound to
    /// `<object>.modmap` when a module map format is configured.
    pub(super) fn export_object_compile_command_scanned(
        &mut self,
        target: &Target,
        source: &gt::SourceInfo,
        object: &str,
        vars: &BTreeMap<String, String>,
    ) {
        let mut vars = vars.clone();
        let modmap_format = self
            .ctx
            .var_safe(target.makefile, "CMAKE_CXX_MODULE_MAP_FORMAT");
        if !modmap_format.is_empty() {
            let flag = self
                .ctx
                .var_safe(target.makefile, "CMAKE_CXX_MODULE_MAP_FLAG");
            let flag = flag.replace("<MODULE_MAP_FILE>", &format!("{object}.modmap"));
            if !flag.is_empty() {
                flags::append(vars.get_mut("FLAGS").unwrap(), &flag);
            }
        }
        self.export_object_compile_command(target, source, object, &vars);
    }
}

/// `experimental_enabled_ctx` keyed by target key (build-database path check
/// only has the key handy in some call sites).
fn experimental_enabled_ctx_key(ctx: &GenCtx, target_key: &str, feature: &str) -> bool {
    let Some(dir) = ctx.interp.ext.targets.get(target_key).map(|t| t.makefile) else {
        return false;
    };
    experimental_enabled_ctx(ctx, dir, feature)
}
