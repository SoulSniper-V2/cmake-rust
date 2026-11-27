//! C++20 module dependency scanning, collation, and BMI generation.
//!
//! Port of the C++ module support in `cmNinjaTargetGenerator`,
//! `cmCommonTargetGenerator::GetLinkedTargetDirectories`,
//! `cmGeneratorTarget::{ApplyCXXStdTargets,DiscoverSyntheticTargets}`, and
//! the `CollationInformation*` blocks of `cmDyndepCollation`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde_json::{Value, json};
use sha3::{Digest, Sha3_512};

use super::*;
use crate::commands::string::json::stream_value;
use crate::generator::ctx::{Diagnostic, GenCtx};
use crate::install_model::InstallAction;
use crate::interp::Interp;
use crate::messages::MessageType;
use crate::state::{FileSet, PropertyEntry};

/// `cmGlobalGenerator::Compute`: `ApplyCXXStdTargets` for every target, then
/// `DiscoverSyntheticTargets`, while targets are still mutable. Returns the
/// diagnostics of the former.
pub fn finalize_cxx_module_targets(interp: &mut Interp) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let keys: Vec<String> = interp
        .makefiles
        .iter()
        .flat_map(|makefile| &makefile.targets)
        .filter(|key| {
            interp
                .ext
                .targets
                .get(*key)
                .is_some_and(|target| !target.imported)
        })
        .cloned()
        .collect();
    // Compile features are computed before any standard library target is
    // linked, so every target is inspected before the first is changed.
    let std_items: Vec<(String, Vec<String>)> = {
        let ctx = GenCtx::new(interp);
        keys.iter()
            .map(|key| (key.clone(), cxx_std_items(&ctx, key, &mut diagnostics)))
            .filter(|(_, items)| !items.is_empty())
            .collect()
    };
    for (key, items) in std_items {
        append_link_items(interp, &key, items);
    }
    // `cmSyntheticTargetCache`: the keys of the targets by name.  Discovery
    // evaluates every target's link implementation even when no imported
    // module target exists; that evaluation is observable through its
    // diagnostics.
    let mut cache = HashMap::new();
    let configs = GenCtx::new(interp).configs;
    for key in &keys {
        for config in &configs {
            discover_synthetic_targets(interp, &mut cache, key, config, &mut diagnostics);
        }
    }
    diagnostics
}

/// `cmGeneratorTarget::DiscoverSyntheticTargets`.
fn discover_synthetic_targets(
    interp: &mut Interp,
    cache: &mut HashMap<String, String>,
    key: &str,
    config: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let imported: Vec<String> = {
        // The link implementation is evaluated (and diagnosed) again when
        // the target is generated.
        let ctx = GenCtx::new(interp).for_config(config);
        let Some(target) = ctx.interp.ext.targets.get(key) else {
            return;
        };
        // DiscoverSyntheticTargets itself does not compute the linker
        // language; the language is only input to generator expressions in
        // the link implementation here.  Keep its diagnostics with the
        // upstream computation that performs it.
        let diagnostics_before_language = ctx.errors.borrow().len();
        let language = gt::linker_language(&ctx, target, config);
        ctx.errors
            .borrow_mut()
            .truncate(diagnostics_before_language);
        let items = link::implementation_targets(&ctx, target, config, &language);
        let imported = items
            .into_iter()
            .filter(|dep| dep.imported && gt::have_cxx20_module_sources(dep))
            .map(|dep| ctx.target_key(dep))
            .collect();
        diagnostics.extend(ctx.errors.take());
        imported
    };
    for imported_key in imported {
        let name = synthetic_target_name(interp, &imported_key);
        let synthetic = match cache.get(&name) {
            Some(synthetic) => synthetic.clone(),
            None => {
                let synthetic = synthesize_cxx_module_target(interp, &imported_key, &name);
                cache.insert(name, synthetic.clone());
                let items = cxx_std_items(&GenCtx::new(interp), &synthetic, diagnostics);
                append_link_items(interp, &synthetic, items);
                discover_synthetic_targets(interp, cache, &synthetic, config, diagnostics);
                synthetic
            }
        };
        interp
            .ext
            .synthetic_deps
            .entry((key.to_owned(), config.to_owned()))
            .or_default()
            .entry(imported_key)
            .or_default()
            .push(synthetic);
    }
}

fn append_link_items(interp: &mut Interp, key: &str, items: Vec<String>) {
    if let Some(target) = interp.ext.targets.get_mut(key) {
        for item in items {
            target.props.append("LINK_LIBRARIES", &item, false);
        }
    }
}

/// `cmGeneratorTarget::ApplyCXXStdTargets`: diagnose `CXX_MODULE_STD` and
/// compute the `LINK_LIBRARIES` entries of the `__CMAKE::CXX<level>` target.
fn cxx_std_items(ctx: &GenCtx, key: &str, diagnostics: &mut Vec<Diagnostic>) -> Vec<String> {
    let mut items = Vec::new();
    let Some(target) = ctx.interp.ext.targets.get(key) else {
        return items;
    };
    let Some(std_prop) = target.props.get("CXX_MODULE_STD") else {
        return items;
    };
    let compiled = crate::genex::parse(std_prop);
    let mut evaluation = crate::genex::EvalContext {
        config: "",
        ..crate::genex::EvalContext::default()
    };
    let mut host = crate::generator::genex_host::CtxHost::new(ctx, target.makefile);
    let value = match crate::genex::evaluate(&compiled, &mut evaluation, &mut host) {
        Ok(value) => value,
        Err(error) => {
            ctx.genex_error(Vec::new(), &error);
            diagnostics.extend(ctx.errors.take());
            return items;
        }
    };
    diagnostics.extend(ctx.errors.take());
    let error = |text: String| {
        Diagnostic::Message(
            MessageType::FatalError,
            ctx.dir_backtrace(target.makefile),
            text,
        )
    };
    if evaluation.context_sensitive {
        diagnostics.push(error(format!(
            "The \"CXX_MODULE_STD\" property on the target \"{}\" contains a context-sensitive condition that is not supported.",
            target.name
        )));
        return items;
    }
    if !util::is_on(&value) {
        return items;
    }
    for config in &ctx.configs {
        let ctx = ctx.for_config(config);
        let supported = module_support::support_level(&ctx, target, config);
        let level = flags::standard::explicit_level(&ctx, target, config, "CXX");
        // The usage requirements are diagnosed when the target is generated.
        ctx.errors.take();
        let Some(level) = level.filter(|level| *level >= module_support::cxx_level("23")) else {
            continue;
        };
        if supported != CxxSupportLevel::Supported {
            continue;
        }
        let level = flags::standard::levels("CXX")[level];
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
            diagnostics.push(error(format!(
                "The \"CXX_MODULE_STD\" property on the target \"{}\" requires that the \"{target_name}\" target exist, but it was not provided by the toolchain.{reason}",
                target.name
            )));
            break;
        }
        // A toolchain may provide the target while `import std` is disabled.
        if !experimental_enabled_ctx(&ctx, target.makefile, "CxxImportStd") {
            break;
        }
        items.push(format!(
            "$<BUILD_LOCAL_INTERFACE:$<$<CONFIG:{config}>:{target_name}>>"
        ));
    }
    items
}

/// `cmExperimental::HasSupportEnabled` in a generation context.
fn experimental_enabled_ctx(ctx: &GenCtx, makefile: usize, feature: &str) -> bool {
    ctx.experimental_enabled(makefile, feature)
}

/// The name of the synthetic target of an imported module target.
fn synthetic_target_name(interp: &Interp, imported_key: &str) -> String {
    let imported = &interp.ext.targets[imported_key];
    let safe_name = imported.name.replace(':', "_");
    // `cmCxxModuleUsageEffects` is a fixed placeholder upstream.
    let usage_hash = "0000000000000000000000000000000000000000";
    let dir_hash = hex(&Sha3_512::digest(
        interp.makefiles[imported.makefile].binary_dir.as_bytes(),
    ));
    let ident = hex(&Sha3_512::digest(
        format!("@d_{dir_hash}@u_{usage_hash}").as_bytes(),
    ));
    format!("{safe_name}@synth_{}", &ident[..12])
}

/// The target creation of `cmGeneratorTarget::DiscoverSyntheticTargets`.
/// Returns the `ext.targets` key of the BMI-only interface target.
fn synthesize_cxx_module_target(interp: &mut Interp, imported_key: &str, name: &str) -> String {
    let configs = GenCtx::new(interp).configs;
    // Collect the copies against an immutable view.
    let (makefile, file_sets, entries_props, entries_prov, policies, platform) = {
        let imported = &interp.ext.targets[imported_key];
        let imported_policies = imported.policies.clone();
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
        for config in configs.iter().filter(|config| !config.is_empty()) {
            let config = config.to_ascii_uppercase();
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
            imported.makefile,
            file_sets,
            entries_props,
            entries_prov,
            imported_policies,
            imported.platform,
        )
    };
    // `cmMakefile::AddSynthesizedTarget(INTERFACE_LIBRARY, name)`.
    let mut target = Target::new(name, crate::state::TargetKind::InterfaceLibrary, makefile);
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
    target.platform = platform;
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

    crate::state::register_target(interp, target)
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
        let flag = self.response_file_flag(target, "CXX");
        if !flag.is_empty() {
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
        write_build(out, &scan, self.compile_command_limit(target, "CXX"));
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
    pub(super) fn write_cxx_bmi_build(
        &mut self,
        out: &mut String,
        target: &Target,
        source: &gt::SourceInfo,
        order_target: &str,
        first_for_config: bool,
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
        if first_for_config {
            self.export_object_compile_command_scanned(target, source, &bmi_file, &export_vars);
        }
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
        write_build(out, &scan, self.compile_command_limit(target, "CXX"));

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
        write_build(out, &build, self.compile_command_limit(target, "CXX"));
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
                || !gt::have_cxx20_module_sources(dep)
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

    /// `cmGeneratorTarget::GetFamilyName`.
    fn family_name(&self, target: &Target) -> String {
        let template = target
            .template_name
            .clone()
            .unwrap_or_else(|| target.name.clone());
        if !target.imported && !target.synthetic {
            return template;
        }
        let dir = &self.ctx.interp.makefiles[target.makefile].binary_dir;
        let hash = |input: &str| {
            let digest = <sha3::Sha3_512 as sha3::Digest>::digest(input.as_bytes());
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let dirhash = hash(dir);
        let ident = hash(&format!("@d_{dirhash}"));
        format!("{template}@{}", &ident[..12])
    }

    /// `cmBuildDatabase::ForTarget` + `Write`: the `<path>.in` template the
    /// dyndep collator filters into the final build database.
    fn write_build_database(&mut self, target: &Target, path: &str) {
        let config = self.config.clone();
        let mut db = crate::build_database::BuildDatabase::default();
        let mut set = crate::build_database::DatabaseSet {
            name: format!("{}@{config}", target.name),
            family_name: self.family_name(target),
            ..Default::default()
        };

        // Linked targets with C++20 module sources are visible sets.
        {
            let lang = gt::linker_language(&self.ctx, target, &config);
            let mut emitted: Vec<String> = Vec::new();
            for (dep, _objects) in link::module_link_targets(&self.ctx, target, &lang) {
                if gt::have_cxx20_module_sources(dep) && !dep.imported {
                    let name = format!("{}@{config}", dep.name);
                    if !emitted.contains(&name) {
                        emitted.push(name.clone());
                        set.visible_sets.push(name);
                    }
                }
            }
        }

        // cmGeneratorTarget::BuildFileSetInfoCache: full path -> file set.
        let mut file_set_map: HashMap<String, (&str, crate::state::Scope)> = HashMap::new();
        for (name, set) in &target.file_sets {
            for (file, _relative, _backtrace) in gt::evaluate_file_set(&self.ctx, target, set) {
                file_set_map
                    .entry(util::collapse_full_path(&file, ""))
                    .or_insert((set.kind.as_str(), set.scope));
            }
            let _ = name;
        }

        // objectDir is `gg->ConvertToOutputPath` — relative to the
        // top binary directory.
        let object_dir = self.ninja_path(&self.object_dir(target));
        let work_directory = self.ctx.interp.makefiles[target.makefile]
            .binary_dir
            .clone();
        for source in gt::sources(&self.ctx, target, &config).iter() {
            let (is_cxx_module, is_private) = if source.language == "CXX" {
                match file_set_map.get(&source.path) {
                    Some(("CXX_MODULES", scope)) => (true, !scope.interface()),
                    _ => (false, true),
                }
            } else {
                (false, true)
            };

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

            let mut tu = crate::build_database::TranslationUnit {
                work_directory: work_directory.clone(),
                source: source.path.clone(),
                ..Default::default()
            };
            if !target.synthetic {
                tu.object = Some(format!("{object_dir}/{}", source.object_name));
            }
            if is_cxx_module {
                tu.provides.insert(
                    crate::build_database::PLACEHOLDER_NAME.to_owned(),
                    crate::build_database::PLACEHOLDER_NAME.to_owned(),
                );
            }

            for (cls, flag) in flags::classified_flags_for_source(
                &self.ctx,
                target,
                &config,
                source,
                &source_props,
                &object_dir,
            ) {
                match cls {
                    flags::FlagCls::Baseline => {
                        tu.baseline_arguments.push(flag.clone());
                        tu.local_arguments.push(flag.clone());
                    }
                    flags::FlagCls::Private => {
                        tu.local_arguments.push(flag.clone());
                    }
                    _ => {}
                }
                tu.arguments.push(flag);
            }
            tu.private = is_private;

            set.translation_units.push(tu);
        }

        db.sets.push(set);
        if let Err(err) =
            crate::fsutil::write_generated(Path::new(path), &stream_value(&db.to_value()))
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
        let modules = ModuleSupport::new(&self.ctx, target, &self.config);
        for (path, (source, bmi_only)) in &sf_map {
            if *bmi_only {
                continue;
            }
            if !modules.need_dyndep_for_source(source) {
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
        if let Err(err) = crate::fsutil::write_generated(Path::new(tdi_path), &stream_value(&tdi)) {
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
        files: &[ScanningFiles],
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
        build.implicit_deps.push(tdi_path.clone());
        if let Some(bdb_path) = self.build_database_path(target) {
            // Upstream lists the build-database output twice: once in the
            // ImplicitOuts block preceding `cmBuildDatabase::Write` and once
            // right after it.
            build.implicit_outs.push(self.ninja_path(&bdb_path));
            let template = format!("{bdb_path}.in");
            self.write_build_database(target, &template);
            build.implicit_deps.push(template.clone());
            build.implicit_outs.push(bdb_path);
        }
        for files in files {
            if let Some(output) = &files.scanning_output {
                build.explicit_deps.push(output.clone());
            }
            if let Some(modmap) = &files.module_map_file {
                build.implicit_outs.push(modmap.clone());
            }
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
    /// library with C++ module sources: language rules, the per-fileConfig
    /// `WriteObjectBuildStatements` (order phony, BMI builds, dyndep), and
    /// per-fileConfig `WriteCxxModuleLibraryStatement` — no link statement.
    pub(super) fn generate_synthetic_target(&mut self, target: &'a Target) {
        self.write_cxx_scan_rules(target);
        self.write_compile_rule("CXX", target, true);
        self.write_compile_rule("CXX", target, false);
        let config = self.config.clone();
        let sources = gt::sources(&self.ctx, target, &config);
        let file_configs = self.file_configs_for(&config);
        self.scanning_info.clear();
        let saved_stream = self.stream.clone();
        let saved_file_config = self.file_config.clone();
        for (index, file_config) in file_configs.iter().enumerate() {
            self.file_config = file_config.clone();
            self.stream = Stream::Impl(file_config.clone());
            self.write_object_build_statements(target, &sources, index == 0);
        }
        self.file_config = saved_file_config;
        self.stream = saved_stream;
        // `WriteCxxModuleLibraryStatement(config, fileConfig,
        // firstForConfig)`: the phony goes to the common stream once per
        // file config (upstream ignores `fileConfig` otherwise).
        for (index, _file_config) in file_configs.iter().enumerate() {
            let mut build = Build::new("phony");
            build.comment = format!("Imported C++ module library {}", target.name);
            self.append_target_outputs(target, &mut build.outputs, DependOn::TargetArtifact);
            if index == 0 {
                self.config_byproducts_for_clean
                    .entry(config.clone())
                    .or_default()
                    .extend(build.outputs.iter().cloned());
            }
            build
                .explicit_deps
                .push(self.ninja_path(&self.cxx_dyndep_path(target)));
            self.write_build_to(&Stream::Common, &build);
        }
        self.add_target_alias(&target.name.clone(), target);
        if self.cross_enabled() {
            self.add_target_alias_cfg(&target.name.clone(), target, "all");
        }
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
