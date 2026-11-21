use std::collections::BTreeMap;

use serde::Deserialize;

use super::*;
use crate::cache::{CacheEntry, CacheType};
use crate::generator::target::{SourceInfo, SourceKind};
use crate::interp::{Interp, Mode};
use crate::props::Props;
use crate::state::TargetKind;

type Strings = BTreeMap<String, String>;

#[derive(Deserialize)]
struct Corpus {
    common: BTreeMap<String, SharedInputs>,
    projects: Vec<Fixture>,
}

#[derive(Deserialize)]
struct SharedInputs {
    cache: Strings,
    variables: Strings,
}

#[derive(Deserialize)]
struct Fixture {
    name: String,
    language: String,
    kind: String,
    config: String,
    target_name: String,
    source: String,
    binary: String,
    source_file: String,
    policies: Strings,
    variables: Strings,
    cache: Strings,
    target_props: Strings,
    source_props: Strings,
    directory_props: Strings,
    object_edge: Strings,
    link_edge: Strings,
    compile_rule: Strings,
    link_rule: Strings,
}

fn properties(values: Strings) -> Props {
    let mut props = Props::default();
    for (name, value) in values {
        props.set(&name, value);
    }
    props
}

fn target(name: &str, kind: TargetKind, props: Props) -> Target {
    Target {
        name: name.to_owned(),
        kind,
        makefile: 0,
        imported: false,
        imported_global: false,
        alias_of: None,
        props,
        provenance: Default::default(),
        backtrace: Vec::new(),
        ..crate::state::Target::new("", crate::state::TargetKind::Executable, 0)
    }
}

fn value<'a>(items: &'a Strings, key: &str) -> &'a str {
    items.get(key).map(String::as_str).unwrap_or_default()
}

#[test]
fn ninja_flag_assembly_matches_kitware() {
    let fixtures: Corpus = serde_json::from_str(include_str!(
        "../../../tests/data/generator/flags/oracle.json"
    ))
    .unwrap();
    assert!(fixtures.projects.len() >= 60);
    for mut fixture in fixtures.projects {
        let shared = &fixtures.common[&fixture.language];
        fixture.cache.extend(shared.cache.clone());
        fixture.variables.extend(shared.variables.clone());
        let mut interp = Interp::new(Mode::Project, &fixture.source, &fixture.binary, "");
        interp.cmake_command = fixture.variables["CMAKE_COMMAND"].clone();
        interp.langs.enabled.push(fixture.language.clone());
        interp.set_def("CMAKE_BUILD_TYPE", &fixture.config);
        for (name, val) in &fixture.cache {
            interp
                .cache
                .entries
                .insert(name.clone(), CacheEntry::new(val, CacheType::String, ""));
        }
        for (name, val) in &fixture.variables {
            if fixture.cache.get(name) != Some(val) {
                interp.set_def(name, val);
            }
        }
        interp.record_directory_vars();
        let mut directory_props = fixture.directory_props.clone();
        // The capture records cmMakefile::GetDefineFlags under this key.
        if let Some(flags) = directory_props.remove("DEFINE_FLAGS") {
            interp.ext.directory_define_flags.insert(0, format!(" {flags}"));
        }
        interp.mf_mut().props = properties(directory_props);
        for index in [69, 83, 119, 128, 181] {
            let key = format!("CMP{index:04}");
            let status = if value(&fixture.policies, &key) == "OLD" {
                PolicyStatus::Old
            } else {
                PolicyStatus::New
            };
            interp
                .mf_mut()
                .policies
                .last_mut()
                .unwrap()
                .map
                .set(index, status);
        }
        let kind = match fixture.kind.as_str() {
            "EXECUTABLE" => TargetKind::Executable,
            "STATIC_LIBRARY" => TargetKind::StaticLibrary,
            "SHARED_LIBRARY" => TargetKind::SharedLibrary,
            "MODULE_LIBRARY" => TargetKind::ModuleLibrary,
            _ => panic!("unsupported fixture target {}", fixture.kind),
        };
        let owned = target(
            &fixture.target_name,
            kind,
            properties(fixture.target_props.clone()),
        );
        let key = crate::state::register_target(&mut interp, owned);
        let ctx = GenCtx::new(&interp);
        let target = interp.ext.targets.get(&key).expect("registered target");
        let source = SourceInfo {
            path: format!("{}/{}", fixture.source, fixture.source_file),
            language: fixture.language.clone(),
            kind: SourceKind::ObjectSource,
            object_name: format!("{}.o", fixture.source_file),
            generated: false,
            object_library: None,
        };
        let source_props = properties(fixture.source_props.clone());
        let compile = if fixture.source_props.is_empty() {
            compile_vars(&ctx, target, &fixture.config, &source)
        } else {
            compile_vars_with_source_props(&ctx, target, &fixture.config, &source, &source_props)
        };
        let link = link_flag_vars(&ctx, target, &fixture.config, &fixture.language);
        let launcher = compiler_launcher(&ctx, target, &fixture.language);
        for (edge, name, actual) in [
            (&fixture.object_edge, "FLAGS", compile.flags),
            (&fixture.object_edge, "DEFINES", compile.defines),
            (&fixture.object_edge, "INCLUDES", compile.includes),
            (&fixture.object_edge, "LAUNCHER", launcher),
            (&fixture.link_edge, "FLAGS", link.flags),
            (&fixture.link_edge, "LINK_FLAGS", link.link_flags),
            (&fixture.link_edge, "ARCH_FLAGS", link.arch_flags),
            (
                &fixture.link_edge,
                "LANGUAGE_COMPILE_FLAGS",
                link.language_compile_flags,
            ),
            (
                &fixture.link_edge,
                "DEP_FILE",
                link_dependency_file(&ctx, target, &fixture.language).unwrap_or_default(),
            ),
        ] {
            assert_eq!(
                actual,
                value(edge, name),
                "{} {} {name}",
                fixture.name,
                edge["statement"]
            );
        }
        let deps = fixture.object_edge["statement"]
            .split_once(" | ")
            .and_then(|(_, pending)| pending.split_once(" || "))
            .map_or_else(Vec::new, |(deps, _)| {
                deps.split_ascii_whitespace().collect()
            });
        assert_eq!(object_dependencies(&source_props), deps, "{}", fixture.name);
        check_rules(&fixture, &ctx, &target);
        assert!(
            ctx.errors.borrow().is_empty(),
            "{} {:?}",
            fixture.name,
            ctx.errors.borrow()
        );
    }
}

fn check_rules(fixture: &Fixture, ctx: &GenCtx, target: &Target) {
    let lang = &fixture.language;
    let var = |suffix| value(&fixture.variables, &format!("CMAKE_{lang}_{suffix}"));
    let depfile = value(&fixture.variables, &format!("CMAKE_DEPFILE_FLAGS_{lang}"));
    let depfile = expand_rule(
        ctx,
        target,
        depfile,
        &[
            ("DEP_TARGET", "$out".to_owned()),
            ("DEP_FILE", "$DEP_FILE".to_owned()),
        ],
    );
    let flags = format!("$FLAGS {depfile}");
    let template = format!("${{LAUNCHER}}${{CODE_CHECK}}{}", var("COMPILE_OBJECT"));
    let compile_vars = [
        ("FLAGS", flags.as_str()),
        ("OBJECT", "$out"),
        ("SOURCE", "$in"),
        ("DEFINES", "$DEFINES"),
        ("INCLUDES", "$INCLUDES"),
    ]
    .map(|(key, val)| (key, val.to_owned()));
    assert_eq!(
        expand_rule_for_step(
            ctx,
            target,
            lang,
            RuleStep::Compile,
            &template,
            &compile_vars
        ),
        fixture.compile_rule["command"],
        "{} compile rule",
        fixture.name
    );
    let link_vars = [
        ("FLAGS", "$FLAGS"),
        (
            "LANGUAGE_COMPILE_FLAGS",
            "$LANGUAGE_COMPILE_FLAGS $ARCH_FLAGS",
        ),
        ("LINK_FLAGS", "$LINK_FLAGS"),
        ("OBJECTS", "$in"),
        ("TARGET", "$TARGET_FILE"),
        ("SONAME_FLAG", "$SONAME_FLAG"),
        ("TARGET_SONAME", "$SONAME"),
        ("LINK_LIBRARIES", "$LINK_PATH $LINK_LIBRARIES"),
    ]
    .map(|(key, val)| (key, val.to_owned()));
    let mut commands = vec!["$PRE_LINK".to_owned()];
    let templates = match target.kind {
        TargetKind::StaticLibrary => vec![
            "<CMAKE_COMMAND> -E rm -f <TARGET>",
            var("ARCHIVE_CREATE"),
            var("ARCHIVE_FINISH"),
        ],
        TargetKind::Executable => vec![var("LINK_EXECUTABLE")],
        TargetKind::SharedLibrary => vec![var("CREATE_SHARED_LIBRARY")],
        TargetKind::ModuleLibrary => vec![var("CREATE_SHARED_MODULE")],
        _ => unreachable!(),
    };
    commands.extend(templates.iter().map(|template| {
        expand_rule_for_step(ctx, target, lang, RuleStep::Link, template, &link_vars)
    }));
    commands.push("$POST_BUILD".to_owned());
    assert_eq!(
        commands.join(" && "),
        fixture.link_rule["command"],
        "{} link rule",
        fixture.name
    );
    if link_dependency_file(ctx, target, lang).is_some() {
        assert_eq!(
            fixture.link_rule["depfile"], "$DEP_FILE",
            "{}",
            fixture.name
        );
        assert_eq!(
            fixture.link_rule["deps"],
            var("LINKER_DEPFILE_FORMAT"),
            "{}",
            fixture.name
        );
    }
}

#[test]
fn standards_inherit_from_paired_language() {
    // CMake's pairing pass uses the C/C++ target property even when only
    // the Objective-C, HIP, or CUDA language is enabled.
    for (lang, paired, default, inherited, effective) in [
        ("OBJC", "C", "17", "99", "99"),
        ("OBJCXX", "CXX", "17", "14", "14"),
        ("HIP", "CXX", "17", "14", "14"),
        ("CUDA", "CXX", "17", "98", "03"),
    ] {
        let mut interp = Interp::new(Mode::Project, "/source", "/build", "");
        interp.langs.enabled.push(lang.to_owned());
        interp.set_def(format!("CMAKE_{lang}_STANDARD_DEFAULT"), default);
        interp.set_def(
            format!("CMAKE_{lang}{effective}_STANDARD_COMPILE_OPTION"),
            "-dialect",
        );
        interp.record_directory_vars();
        let mut props = Props::default();
        props.set(&format!("{paired}_STANDARD"), inherited.to_owned());
        props.set(&format!("{paired}_EXTENSIONS"), "OFF".to_owned());
        props.set(&format!("{paired}_STANDARD_REQUIRED"), "ON".to_owned());
        let key = crate::state::register_target(&mut interp, target("x", TargetKind::Executable, props));
        let source = SourceInfo {
            path: "/source/example".to_owned(),
            language: lang.to_owned(),
            kind: SourceKind::ObjectSource,
            object_name: "example.o".to_owned(),
            generated: false,
            object_library: None,
        };
        let ctx = GenCtx::new(&interp);
        let target = interp.ext.targets.get(&key).expect("registered target");
        assert_eq!(
            compile_vars(&ctx, target, "", &source).flags,
            "-dialect",
            "{lang}"
        );
        assert!(ctx.errors.borrow().is_empty(), "{lang}");
    }
}
