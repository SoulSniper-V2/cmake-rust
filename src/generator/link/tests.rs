use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use super::*;
use crate::install_model::{InstallAction, InstallRule, Namelink};
use crate::interp::{Interp, Makefile, Mode};
use crate::props::Props;

const ROOT: &str = "/tmp/cmake-link-oracle-00000000";

#[derive(Deserialize)]
struct Fixture {
    config: String,
    policy: String,
    target_order: Vec<String>,
    targets: BTreeMap<String, Model>,
    variables: HashMap<String, String>,
    edges: BTreeMap<String, Edge>,
    failure: Option<String>,
}

#[derive(Deserialize)]
struct Model {
    kind: String,
    imported: bool,
    directory: String,
    properties: BTreeMap<String, String>,
    source_language: Option<String>,
}

#[derive(Deserialize)]
struct Edge {
    #[serde(rename = "LINK_LIBRARIES")]
    libraries: String,
    #[serde(rename = "LINK_PATH")]
    link_path: String,
    #[serde(rename = "LINK_FLAGS", default)]
    link_flags: String,
    explicit: Vec<String>,
    implicit: Vec<String>,
    order_only: Vec<String>,
}

struct FixtureGenerator;

impl GeneratorQueries for FixtureGenerator {
    fn artifact(&self, ctx: &GenCtx, target: &Target, config: &str, real: bool) -> String {
        if target.imported {
            return import_property(target, config, "IMPORTED_LOCATION")
                .unwrap_or_default()
                .to_owned();
        }
        let binary = &ctx.interp.makefiles[target.makefile].binary_dir;
        let (directory, prefix, suffix) = match target.kind {
            TargetKind::StaticLibrary => ("ARCHIVE_OUTPUT_DIRECTORY", "lib", ".a"),
            TargetKind::SharedLibrary | TargetKind::ModuleLibrary => {
                ("LIBRARY_OUTPUT_DIRECTORY", "lib", ".so")
            }
            _ => ("RUNTIME_OUTPUT_DIRECTORY", "", ""),
        };
        let directory = target.props.get(directory).unwrap_or(binary);
        let version = if real {
            target.props.get("VERSION")
        } else {
            None
        };
        let version = version.map_or_else(String::new, |version| format!(".{version}"));
        format!("{directory}/{prefix}{}{suffix}{version}", target.name)
    }

    fn soname(&self, _ctx: &GenCtx, target: &Target, config: &str) -> Option<String> {
        if target.imported {
            return import_property(target, config, "IMPORTED_SONAME").map(str::to_owned);
        }
        let version = target
            .props
            .get("SOVERSION")
            .map_or_else(String::new, |version| format!(".{version}"));
        Some(format!("lib{}.so{version}", target.name))
    }

    fn language(&self, _ctx: &GenCtx, _target: &Target, _config: &str) -> String {
        "C".into()
    }

    fn object_dir(&self, ctx: &GenCtx, target: &Target, _config: &str) -> String {
        format!(
            "{}/CMakeFiles/{}.dir/",
            ctx.interp.makefiles[target.makefile].binary_dir, target.name
        )
    }

    fn sources(&self, ctx: &GenCtx, target: &Target, _config: &str) -> Vec<SourceInfo> {
        let src = if target.kind == TargetKind::Executable {
            "main.c"
        } else {
            "empty.c"
        };
        let subdir = target.makefile != 0;
        vec![SourceInfo {
            path: format!("{}/{src}", ctx.interp.makefiles[0].source_dir),
            language: target
                .props
                .get("FIXTURE_SOURCE_LANGUAGE")
                .unwrap_or("C")
                .into(),
            kind: target::SourceKind::ObjectSource,
            object_name: format!("{}{src}.o", if subdir { "__/" } else { "" }),
            generated: false,
            object_library: None,
        }]
    }
}

fn model(fixture: &Fixture) -> Interp {
    let mut interp = Interp::new(
        Mode::Project,
        &format!("{ROOT}/src"),
        &format!("{ROOT}/build"),
        "",
    );
    let mut dirs = HashMap::from([(String::new(), 0)]);
    for name in &fixture.target_order {
        let record = &fixture.targets[name];
        let next = dirs.len();
        let directory = *dirs.entry(record.directory.clone()).or_insert(next);
        if directory == interp.makefiles.len() {
            interp.makefiles.push(Makefile {
                source_dir: format!("{ROOT}/src/{}", record.directory),
                binary_dir: format!("{ROOT}/build/{}", record.directory),
                parent: Some(0),
                props: Props::default(),
                policies: interp.makefiles[0].policies.clone(),
                list_files: Vec::new(),
                targets: Vec::new(),
                children: Vec::new(),
                exclude_from_all: false,
                deferred: Vec::new(),
                defer_counter: 0,
            });
        }
        let kind = match record.kind.as_str() {
            "EXECUTABLE" => TargetKind::Executable,
            "STATIC" => TargetKind::StaticLibrary,
            "SHARED" => TargetKind::SharedLibrary,
            "MODULE" => TargetKind::ModuleLibrary,
            "OBJECT" => TargetKind::ObjectLibrary,
            "INTERFACE" => TargetKind::InterfaceLibrary,
            "UTILITY" => TargetKind::Utility,
            kind => panic!("unsupported fixture kind {kind}"),
        };
        let mut props = Props::default();
        for (key, value) in &record.properties {
            if key == "HAVE_INSTALL_RULE" {
                interp
                    .ext
                    .install
                    .rules
                    .entry(directory)
                    .or_default()
                    .push(InstallRule {
                        options: Default::default(),
                        action: InstallAction::Target {
                            name: name.clone(),
                            namelink: Namelink::None,
                            imported: false,
                        },
                        preceding_children: 0,
                        backtrace: Vec::new(),
                    });
            } else {
                props.set(key, value.replace("@ROOT@", ROOT));
            }
        }
        if let Some(language) = &record.source_language {
            props.set("FIXTURE_SOURCE_LANGUAGE", language.clone());
        }
        let target = Target {
            name: name.clone(),
            kind,
            makefile: directory,
            imported: record.imported,
            imported_global: false,
            alias_of: None,
            props,
            backtrace: Vec::new(),
            provenance: BTreeMap::new(),
            ..Target::new("", TargetKind::Executable, 0)
        };
        let mut target = target;
        // install(TARGETS) marks the target (cmTarget::SetHaveInstallRule).
        target.have_install_rule = record.properties.iter().any(|(key, _)| key == "HAVE_INSTALL_RULE");
        crate::state::register_target(&mut interp, target);
    }
    let active = |policy, old| {
        (
            index_of(policy).unwrap(),
            if old {
                PolicyStatus::Old
            } else {
                PolicyStatus::New
            },
        )
    };
    for mf in &mut interp.makefiles {
        let frame = mf.policies.last_mut().unwrap();
        for (index, status) in [
            active("CMP0156", fixture.policy == "CMP0156_OLD"),
            active("CMP0179", fixture.policy == "OLD"),
            active("CMP0081", false),
        ] {
            frame.map.set(index, status);
        }
    }
    // cmTarget records the directory's policies when the target is created.
    for target in interp.ext.targets.values_mut() {
        let mut map = crate::policy::PolicyMap::default();
        for frame in &interp.makefiles[target.makefile].policies {
            map.overlay(&frame.map);
        }
        target.policies = map;
    }
    let vars = fixture
        .variables
        .iter()
        .map(|(key, value)| (key.clone(), Some(value.replace("@ROOT@", ROOT))))
        .collect::<HashMap<_, _>>();
    interp.dir_final_vars = vec![vars; interp.makefiles.len()];
    interp
}

fn ninja_dep(query: &Query<'_, '_>, path: &str) -> String {
    query
        .ninja_path(path)
        .replace('$', "$$")
        .replace(' ', "$ ")
        .replace(':', "$:")
        .replace(ROOT, "@ROOT@")
}

fn target_artifact(query: &Query<'_, '_>, target: &Target) -> String {
    match target.kind {
        TargetKind::ObjectLibrary | TargetKind::Utility | TargetKind::InterfaceLibrary => {
            format!(
                "{}/{}",
                query.ctx.interp.makefiles[target.makefile].binary_dir, target.name
            )
        }
        _ => query
            .generator
            .artifact(query.ctx, target, query.config, false),
    }
}

#[test]
fn cmake_4_1_ninja_edges() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/generator/link");
    let files = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|file| file.path())
        .filter(|file| file.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    assert!(files.len() >= 60);
    let mut failures = Vec::new();
    for file in files {
        let case = file.file_stem().unwrap().to_string_lossy();
        let fixture: Fixture = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        let interp = model(&fixture);
        let ctx = GenCtx::new(&interp);
        let query = Query {
            ctx: &ctx,
            config: &fixture.config,
            language: "C",
            generator: &FixtureGenerator,
            compile_usage: false,
        };
        let graph = target_graph::compute(&query);
        if let Some(failure) = fixture.failure {
            for target in ctx.targets_in_order() {
                information::compute(&query, target);
            }
            let diagnostics = ctx.errors.borrow();
            assert!(
                diagnostics.iter().any(|(_, message)| {
                    message
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .contains(&failure)
                }),
                "{case}: expected {failure:?}, got {diagnostics:?}"
            );
            continue;
        }
        for (name, expected) in fixture.edges {
            let target = ctx.target(&name).unwrap();
            let line = information::compute(&query, target);
            if on(target, "LINK_WHAT_YOU_USE") {
                let check = line
                    .link_what_you_use
                    .as_ref()
                    .expect("oracle enabled LINK_WHAT_YOU_USE");
                assert!(
                    expected
                        .link_flags
                        .split_whitespace()
                        .any(|flag| flag == check.flag)
                );
                assert_eq!(check.checker.as_deref(), Some("ldd;-u;-r"));
            }
            if on(target, "BUILD_WITH_INSTALL_RPATH") {
                assert!(line.rpath_change.is_none());
            }
            let mut order_only = graph
                .iter()
                .find(|(head, _)| head.name == name)
                .unwrap()
                .1
                .iter()
                .map(|dep| target_artifact(&query, ctx.target(dep).unwrap()))
                .collect::<Vec<_>>();
            order_only.sort();
            order_only.extend(line.shared_symlinks);
            let mut explicit = information::objects(&query, target);
            explicit.extend(
                query
                    .generator
                    .sources(&ctx, target, query.config)
                    .into_iter()
                    .map(|src| {
                        format!(
                            "{}{}",
                            query.generator.object_dir(&ctx, target, query.config),
                            src.object_name
                        )
                    }),
            );
            let mut check = |key, actual: Vec<String>, want: Vec<String>| {
                if actual != want {
                    failures.push(format!(
                        "{case}/{name} {key}\n  actual: {actual:?}\n    want: {want:?}"
                    ));
                }
            };
            check(
                "LINK_LIBRARIES",
                vec![line.libraries.replace(ROOT, "@ROOT@")],
                vec![expected.libraries],
            );
            check(
                "LINK_PATH",
                vec![line.link_path.replace(ROOT, "@ROOT@")],
                vec![expected.link_path],
            );
            check(
                "implicit deps",
                line.depends
                    .iter()
                    .map(|dep| ninja_dep(&query, dep))
                    .collect(),
                expected.implicit,
            );
            check(
                "order-only deps",
                order_only
                    .iter()
                    .map(|dep| ninja_dep(&query, dep))
                    .collect(),
                expected.order_only,
            );
            check(
                "explicit deps",
                explicit.iter().map(|dep| ninja_dep(&query, dep)).collect(),
                expected.explicit,
            );
        }
        if !ctx.errors.borrow().is_empty() {
            failures.push(format!("{case} diagnostics: {:?}", ctx.errors.borrow()));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
