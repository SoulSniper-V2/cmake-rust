//! `cmExportPackageInfoGenerator` + `cmExportBuildPackageInfoGenerator` +
//! `cmExportInstallPackageInfoGenerator` +
//! `cmInstallPackageInfoExportGenerator`: CPS (`.cps`) JSON generation for
//! `export(PACKAGE_INFO)` and `install(PACKAGE_INFO)`.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use md5::{Digest, Md5};

use super::ctx::GenCtx;
use super::export::{
    Exporter, Record, config_suffix, export_name, replacement_check, write_config_copy,
};
use super::install::{CopyRule, absolute_dest, write_copy};
use crate::commands::string::json::{Json, write_json_indent2};
use crate::fsutil::write_if_different;
use crate::genex::{self, PreprocessContext};
use crate::install_model::{BuildExport, CopyKind, InstallOptions, InstallRule};
use crate::package_info::PackageInfoArguments;
use crate::state::{Target, TargetKind};
use crate::util::{expand_list, is_full_path};

const CPS_VERSION: &str = "0.13.0";

type Obj = BTreeMap<Vec<u8>, Json>;

fn skey(k: &str) -> Vec<u8> {
    k.as_bytes().to_vec()
}

fn jstr(v: &str) -> Json {
    Json::Str(v.as_bytes().to_vec())
}

fn jarr<'a>(items: impl Iterator<Item = &'a String>) -> Json {
    Json::Array(items.map(|v| jstr(v)).collect())
}

fn component_type(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Executable => "executable",
        TargetKind::StaticLibrary => "archive",
        TargetKind::SharedLibrary => "dylib",
        TargetKind::ModuleLibrary => "module",
        TargetKind::InterfaceLibrary => "interface",
        _ => "unknown",
    }
}

/// `GeneratePackageInfo`: the shared metadata object for a CPS file.
fn package_root(package: &PackageInfoArguments) -> Obj {
    let mut root = Obj::new();
    root.insert(skey("name"), jstr(&package.package_name));
    root.insert(skey("cps_version"), jstr(CPS_VERSION));
    if !package.version.is_empty() {
        root.insert(skey("version"), jstr(&package.version));
        if !package.compat_version.is_empty() {
            root.insert(skey("compat_version"), jstr(&package.compat_version));
        }
        if !package.version_schema.is_empty() {
            root.insert(skey("version_schema"), jstr(&package.version_schema));
        }
    }
    if !package.default_targets.is_empty() {
        root.insert(
            skey("default_components"),
            jarr(package.default_targets.iter()),
        );
    }
    if !package.default_configs.is_empty() {
        root.insert(skey("configurations"), jarr(package.default_configs.iter()));
    }
    if !package.description.is_empty() {
        root.insert(skey("description"), jstr(&package.description));
    }
    if !package.website.is_empty() {
        root.insert(skey("website"), jstr(&package.website));
    }
    root
}

/// `GeneratePackageRequires`.
fn package_requires(ex: &Exporter<'_, '_>, root: &mut Obj) {
    if ex.requirements.is_empty() {
        return;
    }
    let mut requires = Obj::new();
    for (pkg, components) in &ex.requirements {
        let mut data = Obj::new();
        if !components.is_empty() {
            data.insert(skey("components"), jarr(components.iter()));
        }
        requires.insert(skey(pkg), Json::Object(data));
    }
    root.insert(skey("requires"), Json::Object(requires));
}

/// `CheckDefaultTargets`.
fn check_default_targets(
    ex: &Exporter<'_, '_>,
    package: &PackageInfoArguments,
    records: &[Record<'_>],
    set_name: &str,
) -> bool {
    let names: BTreeSet<&str> = records.iter().map(|r| export_name(r.target)).collect();
    let mut result = true;
    for name in &package.default_targets {
        if !names.contains(name.as_str()) {
            ex.report_error(format!(
                "Package \"{}\" specifies DEFAULT_TARGETS \"{name}\", which is not a target in the export set \"{set_name}\".",
                package.package_name
            ));
            result = false;
        }
    }
    result
}

/// `ForbidGeneratorExpressions`: returns the collected (stripped) value, or
/// `None` after reporting the error.
fn forbid_genex(
    ex: &Exporter<'_, '_>,
    tgt: &Target,
    prop_name: &str,
    value: &str,
    allow: &mut BTreeMap<String, Vec<String>>,
) -> Option<String> {
    let allowed = allow.len();
    let evaluated = genex::collect(value, allow);
    if evaluated != *value && allow.len() > allowed {
        ex.ctx.error(
            ex.depender_backtrace(tgt),
            format!(
                "Property \"{prop_name}\" of target \"{}\" contains a generator expression. This is not allowed.",
                tgt.name
            ),
        );
        return None;
    }
    // Forbid nested generator expressions.
    for (name, values) in allow.iter() {
        for value in values {
            if value.contains("$<") {
                ex.ctx.error(
                    ex.depender_backtrace(tgt),
                    format!(
                        "$<{name}:...> expression in \"{prop_name}\" of target \"{}\" contains a generator expression. This is not allowed.",
                        tgt.name
                    ),
                );
                return None;
            }
        }
    }
    Some(evaluated)
}

fn forbid_genex_simple(
    ex: &Exporter<'_, '_>,
    tgt: &Target,
    prop_name: &str,
    value: &str,
) -> Option<String> {
    let mut allow = BTreeMap::new();
    forbid_genex(ex, tgt, prop_name, value, &mut allow)
}

/// `GenerateInterfaceLinkProperties`.
fn interface_link_properties(
    ex: &Exporter<'_, '_>,
    component: &mut Obj,
    tgt: &Target,
    props: &BTreeMap<String, String>,
) -> bool {
    let Some(value) = props.get("INTERFACE_LINK_LIBRARIES") else {
        return true;
    };

    // Extract any $<LINK_ONLY:...> from the link libraries, and assert that
    // no other generator expressions are present.
    let mut allow: BTreeMap<String, Vec<String>> =
        BTreeMap::from([("LINK_ONLY".to_owned(), Vec::new())]);
    let Some(cleaned) = forbid_genex(ex, tgt, "INTERFACE_LINK_LIBRARIES", value, &mut allow) else {
        return false;
    };

    let mut link_libraries = Vec::new();
    let mut link_requires = Vec::new();
    let mut build_requires = Vec::new();
    fn add_libraries(
        ex: &Exporter<'_, '_>,
        names: &[String],
        output: &mut Vec<String>,
        link_libraries: &mut Vec<String>,
    ) -> bool {
        for name in names {
            match ex.link_targets.get(name) {
                Some(mapped) => {
                    if mapped.is_empty() {
                        return false;
                    }
                    output.push(mapped.clone());
                }
                None => link_libraries.push(name.clone()),
            }
        }
        true
    }
    if !add_libraries(
        ex,
        allow.get("LINK_ONLY").map(Vec::as_slice).unwrap_or(&[]),
        &mut link_requires,
        &mut link_libraries,
    ) {
        return false;
    }
    if !add_libraries(
        ex,
        &expand_list(&cleaned, false),
        &mut build_requires,
        &mut link_libraries,
    ) {
        return false;
    }
    if !build_requires.is_empty() {
        component.insert(skey("requires"), jarr(build_requires.iter()));
    }
    if !link_requires.is_empty() {
        component.insert(skey("link_requires"), jarr(link_requires.iter()));
    }
    if !link_libraries.is_empty() {
        component.insert(skey("link_libraries"), jarr(link_libraries.iter()));
    }
    true
}

/// `GenerateInterfaceCompileFeatures`.
fn interface_compile_features(
    ex: &Exporter<'_, '_>,
    component: &mut Obj,
    tgt: &Target,
    props: &BTreeMap<String, String>,
) -> bool {
    let Some(value) = props.get("INTERFACE_COMPILE_FEATURES") else {
        return true;
    };
    if forbid_genex_simple(ex, tgt, "INTERFACE_COMPILE_FEATURES", value).is_none() {
        return false;
    }
    let mut features = BTreeSet::new();
    for value in expand_list(value, false) {
        if let Some(rest) = value.strip_prefix("c_std_") {
            let suffix: String = rest.chars().take(2).collect();
            features.insert(format!("cxx{suffix}"));
        } else if let Some(rest) = value.strip_prefix("cxx_std_") {
            let suffix: String = rest.chars().take(2).collect();
            features.insert(format!("c++{suffix}"));
        }
    }
    if !features.is_empty() {
        component.insert(skey("compile_features"), jarr(features.iter()));
    }
    true
}

/// `GenerateInterfaceCompileDefines`.
fn interface_compile_defines(
    ex: &Exporter<'_, '_>,
    component: &mut Obj,
    tgt: &Target,
    props: &BTreeMap<String, String>,
) -> bool {
    let Some(value) = props.get("INTERFACE_COMPILE_DEFINITIONS") else {
        return true;
    };
    if forbid_genex_simple(ex, tgt, "INTERFACE_COMPILE_DEFINITIONS", value).is_none() {
        return false;
    }
    let mut defines = Obj::new();
    for def in expand_list(value, false) {
        match def.find('=') {
            None => {
                defines.insert(def.as_bytes().to_vec(), Json::Null);
            }
            Some(n) => {
                defines.insert(def.as_bytes()[..n].to_vec(), jstr(&def[n + 1..]));
            }
        }
    }
    if !defines.is_empty() {
        let mut defs = Obj::new();
        defs.insert(skey("*"), Json::Object(defines));
        component.insert(skey("compile_definitions"), Json::Object(defs));
    }
    true
}

/// `GenerateInterfaceListProperty`.
fn interface_list_property(
    ex: &Exporter<'_, '_>,
    component: &mut Obj,
    tgt: &Target,
    out_name: &str,
    in_key: &str,
    props: &BTreeMap<String, String>,
) -> bool {
    let prop = format!("INTERFACE_{in_key}");
    let Some(value) = props.get(&prop) else {
        return true;
    };
    if forbid_genex_simple(ex, tgt, &prop, value).is_none() {
        return false;
    }
    // `Json::Value& array = component[outName]` then append per value: a
    // present-but-empty property serializes as `null`, not `[]`.
    let items: Vec<Json> = expand_list(value, false).iter().map(|v| jstr(v)).collect();
    component.insert(
        skey(out_name),
        if items.is_empty() {
            Json::Null
        } else {
            Json::Array(items)
        },
    );
    true
}

/// `GenerateProperty` (scalar).
fn interface_scalar_property(
    ex: &Exporter<'_, '_>,
    component: &mut Obj,
    tgt: &Target,
    out_name: &str,
    in_key: &str,
    props: &BTreeMap<String, String>,
) -> bool {
    let Some(value) = props.get(in_key) else {
        return true;
    };
    if forbid_genex_simple(ex, tgt, in_key, value).is_none() {
        return false;
    }
    component.insert(skey(out_name), jstr(value));
    true
}

/// `GenerateInterfaceProperties`.
fn generate_interface_properties(
    ex: &Exporter<'_, '_>,
    component: &mut Obj,
    tgt: &Target,
    props: &BTreeMap<String, String>,
) {
    interface_link_properties(ex, component, tgt, props);
    interface_compile_features(ex, component, tgt, props);
    interface_compile_defines(ex, component, tgt, props);
    interface_list_property(
        ex,
        component,
        tgt,
        "compile_flags",
        "COMPILE_OPTIONS",
        props,
    );
    interface_list_property(ex, component, tgt, "link_flags", "LINK_OPTIONS", props);
    interface_list_property(
        ex,
        component,
        tgt,
        "link_directories",
        "LINK_DIRECTORIES",
        props,
    );
    interface_list_property(ex, component, tgt, "includes", "INCLUDE_DIRECTORIES", props);
    interface_scalar_property(ex, component, tgt, "license", "SPDX_LICENSE", props);
}

/// `GenerateInterfaceConfigProperties`: extract the location properties for
/// `suffix` (`_RELEASE`, `_NOCONFIG`, ...) into a per-config component.
fn interface_config_props(suffix: &str, props: &BTreeMap<String, String>) -> Obj {
    let mut component = Obj::new();
    for (key, value) in props {
        if !key.ends_with(suffix) {
            continue;
        }
        // `p.first.length() - suffixLength - 9`; keys shorter than
        // 9 + suffix can never yield one of the property names below.
        if key.len() < suffix.len() + 9 {
            continue;
        }
        let prop = &key[9..key.len() - suffix.len()];
        match prop {
            "LOCATION" => {
                component.insert(skey("location"), jstr(value));
            }
            "IMPLIB" => {
                component.insert(skey("link_location"), jstr(value));
            }
            "LINK_INTERFACE_LANGUAGES" => {
                let languages: Vec<String> = expand_list(value, false)
                    .iter()
                    .map(|lang| {
                        let ll = lang.to_lowercase();
                        if ll == "cxx" { "cpp".to_owned() } else { ll }
                    })
                    .collect();
                if !languages.is_empty() {
                    component.insert(skey("link_languages"), jarr(languages.iter()));
                }
            }
            _ => {}
        }
    }
    component
}

/// `PropertyConfigSuffix`.
fn property_config_suffix(config: &str) -> String {
    if config.is_empty() {
        "_NOCONFIG".to_owned()
    } else {
        format!("_{}", config.to_uppercase())
    }
}

/// `GenerateInterfacePropertiesConfig` (build-tree per-config data).
fn interface_properties_config(
    ex: &mut Exporter<'_, '_>,
    configurations: &mut Obj,
    record: &Record<'_>,
    config: &str,
) {
    let suffix = property_config_suffix(config);
    let Some((props, _)) = ex.location_properties(record, config, &suffix) else {
        return;
    };
    let component = interface_config_props(&suffix, &props);
    if !component.is_empty() {
        let key = if config.is_empty() {
            "noconfig"
        } else {
            config
        };
        configurations.insert(skey(key), Json::Object(component));
    }
}

/// `cmExportBuildPackageInfoGenerator::GenerateMainFile`.
pub(super) fn write_build(
    ctx: &GenCtx,
    file: &str,
    export: &BuildExport,
    package: &PackageInfoArguments,
) -> std::io::Result<()> {
    if ctx.multi_config {
        ctx.error(
            export.backtrace.clone(),
            "cmake-rust does not implement per-configuration CPS package information export (export(PACKAGE_INFO)) for the \"Ninja Multi-Config\" generator.",
        );
        return Ok(());
    }
    let inst = &ctx.interp.ext.install;
    let set = export
        .set
        .as_ref()
        .and_then(|name| inst.export_sets.get(name));
    if export.set.is_some() && set.is_none() {
        ctx.error(export.backtrace.clone(), "Could not write export file.");
        return Ok(());
    }
    let mut ex = Exporter {
        ctx,
        options: &export.options,
        installed: false,
        append: export.append,
        error_dir: export.makefile,
        export_label: String::new(),
        required: Cell::new((2, 8, 3)),
        exported: HashSet::new(),
        external: Vec::new(),
        missing: Vec::new(),
        namespace: package.namespace(),
        package: Some(package),
        link_targets: HashMap::new(),
        requirements: BTreeMap::new(),
    };
    let records = ex.collect(set, &export.targets, export.makefile);
    if records.is_empty() && !export.targets.is_empty() {
        return Ok(());
    }
    let set_name = export.set.as_deref().unwrap_or_default();
    if !check_default_targets(&ex, package, &records, set_name) {
        return Ok(());
    }

    let mut root = package_root(package);
    root.insert(skey("cps_path"), jstr("@prefix@"));
    let mut components = Obj::new();
    for record in &records {
        let name = export_name(record.target);
        if name.is_empty() {
            return Ok(());
        }
        let props = ex.populate_interface_props_rule(record, PreprocessContext::InstallInterface);
        let mut component = Obj::new();
        component.insert(skey("type"), jstr(component_type(record.kind)));
        if record.kind != TargetKind::InterfaceLibrary {
            let mut configurations = Obj::new();
            {
                let c = ctx.config.clone();
                interface_properties_config(&mut ex, &mut configurations, record, &c);
            }
            if !configurations.is_empty() {
                component.insert(skey("configurations"), Json::Object(configurations));
            }
        }
        generate_interface_properties(&ex, &mut component, record.target, &props);
        components.insert(skey(name), Json::Object(component));
    }
    root.insert(skey("components"), Json::Object(components));
    package_requires(&ex, &mut root);
    write_if_different(
        Path::new(file),
        write_json_indent2(&Json::Object(root)).as_bytes(),
    )?;
    Ok(())
}

/// `cmExportInstallPackageInfoGenerator::GenerateImportPrefix`.
fn import_prefix_path(
    ex: &Exporter<'_, '_>,
    ctx: &GenCtx,
    index: usize,
    dest: &str,
    package: &PackageInfoArguments,
) -> Option<String> {
    let mut exp_dest = dest.to_owned();
    if is_full_path(&exp_dest) {
        let install_prefix = ctx.var_safe(index, "CMAKE_INSTALL_PREFIX");
        if exp_dest.starts_with(&install_prefix) {
            let mut n = install_prefix.len();
            let bytes = exp_dest.as_bytes();
            while n < bytes.len() && bytes[n] == b'/' {
                n += 1;
            }
            exp_dest = exp_dest[n..].to_owned();
        } else {
            ex.report_error(format!(
                "install(PACKAGE_INFO \"{}\" ...) specifies DESTINATION \"{exp_dest}\" which is not a subdirectory of the install prefix.",
                package.package_name
            ));
            return None;
        }
    }
    if exp_dest.is_empty() {
        Some("@prefix@".to_owned())
    } else {
        Some(format!("@prefix@/{exp_dest}"))
    }
}

/// `cmInstallPackageInfoExportGenerator` + the
/// `cmExportInstallPackageInfoGenerator` file generation: emit the staged
/// main `.cps` file, its install rules, and per-config `<stem>@<cfg>.cps`.
pub(super) fn write_install(
    ctx: &GenCtx,
    out: &mut String,
    index: usize,
    rule: &InstallRule,
    set_name: &str,
    package: &PackageInfoArguments,
    indent: &str,
) -> std::io::Result<()> {
    if ctx.multi_config {
        ctx.error(
            rule.backtrace.clone(),
            "cmake-rust does not implement per-configuration CPS package information export (install(PACKAGE_INFO)) for the \"Ninja Multi-Config\" generator.",
        );
        return Ok(());
    }
    let set = ctx.interp.ext.install.export_sets.get(set_name);
    if set.is_none_or(|set| set.targets.is_empty()) {
        ctx.error(
            rule.backtrace.clone(),
            format!("INSTALL(PACKAGE_INFO) given unknown export \"{set_name}\""),
        );
        return Ok(());
    }
    let set = set.unwrap();
    // Package-info installs carry no `ExportOptions`; the exporter only uses
    // these for `.cmake` output details that do not apply to CPS.
    static EMPTY_EXPORT_OPTIONS: crate::install_model::ExportOptions =
        crate::install_model::ExportOptions {
            namespace: String::new(),
            modules_directory: String::new(),
            old_link_interface: false,
            package_dependencies: false,
            android: false,
        };
    let mut ex = Exporter {
        ctx,
        options: &EMPTY_EXPORT_OPTIONS,
        installed: true,
        append: false,
        error_dir: index,
        export_label: set_name.to_owned(),
        required: Cell::new((2, 8, 3)),
        exported: HashSet::new(),
        external: Vec::new(),
        missing: Vec::new(),
        namespace: package.namespace(),
        package: Some(package),
        link_targets: HashMap::new(),
        requirements: BTreeMap::new(),
    };
    let records = ex.collect(Some(set), &[], index);
    // `collect` yields no records on duplicate-target failure; an export set
    // of only namelink entries legitimately produces an empty record list.
    if records.is_empty() && set.targets.iter().any(|t| !t.namelink_only) {
        return Ok(());
    }
    let dest = &rule.options.destination;
    let binary = &ctx.interp.makefiles[index].binary_dir;
    let temp = format!(
        "{binary}/CMakeFiles/Export/{:x}",
        Md5::digest(dest.as_bytes())
    );
    let filename = package.package_file_name();
    let file = format!("{temp}/{filename}");
    let stem = filename
        .strip_suffix(".cps")
        .unwrap_or(&filename)
        .to_owned();
    let glob = format!("{stem}@*.cps");

    // `GenerateMainFile`.
    if !check_default_targets(&ex, package, &records, set_name) {
        return Ok(());
    }
    let mut root = package_root(package);
    let Some(cps_path) = import_prefix_path(&ex, ctx, index, dest, package) else {
        return Ok(());
    };
    root.insert(skey("cps_path"), jstr(&cps_path));
    let mut components = Obj::new();
    for record in &records {
        let name = export_name(record.target);
        if name.is_empty() {
            return Ok(());
        }
        let props = ex.populate_interface_props_rule(record, PreprocessContext::InstallInterface);
        let mut component = Obj::new();
        component.insert(skey("type"), jstr(component_type(record.kind)));
        generate_interface_properties(&ex, &mut component, record.target, &props);
        components.insert(skey(name), Json::Object(component));
    }
    root.insert(skey("components"), Json::Object(components));
    package_requires(&ex, &mut root);
    write_if_different(
        Path::new(&file),
        write_json_indent2(&Json::Object(root)).as_bytes(),
    )?;

    // Install rules for the main file (`GenerateScriptActions`).
    let opts = InstallOptions {
        optional: false,
        ..rule.options.clone()
    };
    let installed_dir = format!("$ENV{{DESTDIR}}{}/", absolute_dest(dest));
    let installed_file = format!("{installed_dir}{filename}");
    replacement_check(
        out,
        indent,
        &installed_file,
        &file,
        &format!("{installed_dir}{glob}"),
        "export",
    );
    write_copy(
        out,
        indent,
        &opts,
        CopyRule::new(dest, CopyKind::File, std::slice::from_ref(&file)),
    );

    // Per-config file (`GenerateImportFileConfig` for the enabled config).
    let has_config = records
        .iter()
        .any(|r| r.kind != TargetKind::InterfaceLibrary);
    let installs = opts.configurations.is_empty()
        || opts
            .configurations
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&ctx.config));
    if has_config && installs {
        let config_file = format!("{temp}/{stem}@{}.cps", config_suffix(&ctx.config));
        let config = &ctx.config;
        let suffix = property_config_suffix(config);
        let mut cfg_root = Obj::new();
        cfg_root.insert(skey("name"), jstr(&package.package_name));
        cfg_root.insert(
            skey("configuration"),
            jstr(if config.is_empty() {
                "noconfig"
            } else {
                config
            }),
        );
        // `GenerateImportTargetsConfig` iterates all exports including
        // namelink-only entries.
        let all: Vec<Record<'_>> = set
            .targets
            .iter()
            .filter_map(|item| {
                let tgt = ex.find(index, &item.name)?;
                let kind = if tgt.kind == TargetKind::ObjectLibrary && item.artifact.is_none() {
                    TargetKind::InterfaceLibrary
                } else {
                    tgt.kind
                };
                Some(Record {
                    target: tgt,
                    installed: Some(item),
                    kind,
                    name: format!("{}{}", package.namespace(), export_name(tgt)),
                })
            })
            .collect();
        let mut cfg_components = Obj::new();
        for record in &all {
            if record.kind == TargetKind::InterfaceLibrary {
                continue;
            }
            let mut props = BTreeMap::new();
            if let Some((p, _)) = ex.location_properties(record, config, &suffix) {
                props = p;
            }
            let component = interface_config_props(&suffix, &props);
            if !component.is_empty() {
                cfg_components.insert(skey(export_name(record.target)), Json::Object(component));
            }
        }
        cfg_root.insert(skey("components"), Json::Object(cfg_components));
        write_if_different(
            Path::new(&config_file),
            write_json_indent2(&Json::Object(cfg_root)).as_bytes(),
        )?;
        write_config_copy(&ctx.config, out, indent, &opts, dest, config_file);
    }
    Ok(())
}
