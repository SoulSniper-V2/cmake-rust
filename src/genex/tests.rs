use std::sync::OnceLock;

use serde::Deserialize;

use super::*;

#[derive(Clone, Deserialize)]
struct MockTarget {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    imported: bool,
    #[serde(default)]
    properties: BTreeMap<String, String>,
    /// Property entries with their CMakeLists.txt line as backtrace.
    #[serde(default)]
    entries: BTreeMap<String, Vec<(Backtrace, String)>>,
    /// target_include_directories(SYSTEM) entries.
    #[serde(default)]
    system: Vec<String>,
}

impl MockTarget {
    fn property(&self, prop: &str) -> Option<String> {
        if let Some(value) = self.properties.get(prop) {
            return Some(value.clone());
        }
        if let Some(entries) = self.entries.get(prop) {
            return Some(
                entries
                    .iter()
                    .map(|(_, value)| value.as_str())
                    .collect::<Vec<_>>()
                    .join(";"),
            );
        }
        // cmTarget: imported targets are created with SYSTEM=ON.
        (prop == "SYSTEM" && self.imported).then(|| "ON".to_owned())
    }
}

#[derive(Clone, Deserialize, Default)]
struct MockHost {
    variables: BTreeMap<String, String>,
    targets: BTreeMap<String, MockTarget>,
    aliases: BTreeMap<String, String>,
    #[serde(skip)]
    backtraces: BTreeMap<(String, String), Backtrace>,
    #[serde(skip)]
    policies: BTreeMap<String, String>,
    #[serde(default)]
    missing_path: String,
}

impl MockHost {
    fn imported_value(&self, target: &str, config: &str, prop: &str) -> Option<String> {
        let mock = self.targets.get(target)?;
        let suffix = self
            .mapped_imported_config(target, config)
            .unwrap_or_default();
        mock.property(&format!("{prop}{suffix}"))
            .or_else(|| mock.property(prop))
    }
}

impl GenexHost for MockHost {
    fn target_exists(&self, name: &str) -> bool {
        self.targets.contains_key(name)
    }

    fn target_type(&self, name: &str) -> Option<&str> {
        match self.targets.get(name)?.kind.as_str() {
            "INTERFACE" => Some("INTERFACE_LIBRARY"),
            "UNKNOWN" => Some("UNKNOWN_LIBRARY"),
            "OBJECT" => Some("OBJECT_LIBRARY"),
            "SHARED" => Some("SHARED_LIBRARY"),
            "STATIC" => Some("STATIC_LIBRARY"),
            "MODULE" => Some("MODULE_LIBRARY"),
            "EXECUTABLE" => Some("EXECUTABLE"),
            _ => unreachable!(),
        }
    }

    fn target_property_raw(&self, target: &str, prop: &str) -> Option<String> {
        match prop {
            "TYPE" => self.target_type(target).map(str::to_owned),
            "NAME" => self.target_exists(target).then(|| target.to_owned()),
            "IMPORTED" => self.target_exists(target).then(|| {
                if self.is_imported(target) {
                    "TRUE"
                } else {
                    "FALSE"
                }
                .to_owned()
            }),
            _ => self.targets.get(target)?.property(prop),
        }
    }

    fn is_imported(&self, target: &str) -> bool {
        self.targets.get(target).is_some_and(|value| value.imported)
    }

    fn resolve_alias(&self, name: &str) -> Option<String> {
        self.aliases.get(name).cloned()
    }

    fn target_artifact(
        &self,
        target: &str,
        kind: ArtifactKind,
        config: &str,
    ) -> Result<String, String> {
        let location = self
            .imported_value(target, config, "IMPORTED_LOCATION")
            .unwrap_or_else(|| format!("{target}-NOTFOUND"));
        let import = self.imported_value(target, config, "IMPORTED_IMPLIB");
        Ok(match kind {
            ArtifactKind::File | ArtifactKind::LinkerLibraryFile => location,
            ArtifactKind::LinkerFile => import.unwrap_or(location),
            ArtifactKind::ImportFile | ArtifactKind::LinkerImportFile => import.unwrap_or_default(),
            ArtifactKind::SonameFile => {
                let dir = util::filename_path(&location);
                let soname = self
                    .imported_value(target, config, "IMPORTED_SONAME")
                    .unwrap_or_default();
                format!("{dir}/{soname}")
            }
            ArtifactKind::SonameImportFile => import.unwrap_or_default(),
            ArtifactKind::PdbFile | ArtifactKind::BundleDir | ArtifactKind::BundleContentDir => {
                return Err("mock has no PDB or bundle".to_owned());
            }
        })
    }

    fn target_artifact_component(
        &self,
        target: &str,
        kind: ArtifactKind,
        component: ArtifactComponent,
        config: &str,
    ) -> Result<String, String> {
        if matches!(
            kind,
            ArtifactKind::ImportFile | ArtifactKind::LinkerImportFile
        ) && self
            .imported_value(target, config, "IMPORTED_IMPLIB")
            .is_none()
        {
            return Ok(String::new());
        }
        Ok(match component {
            ArtifactComponent::BaseName => self
                .target_property_raw(target, "OUTPUT_NAME")
                .unwrap_or_else(|| target.to_owned()),
            ArtifactComponent::Prefix => self
                .target_property_raw(target, "PREFIX")
                .unwrap_or_default(),
            ArtifactComponent::Suffix => self
                .target_property_raw(target, "SUFFIX")
                .unwrap_or_default(),
            _ => unreachable!(),
        })
    }

    fn target_objects(&self, target: &str, config: &str) -> Result<Vec<String>, String> {
        Ok(cm_list(
            &self
                .imported_value(target, config, "IMPORTED_OBJECTS")
                .unwrap_or_default(),
            false,
        ))
    }

    fn variable(&self, name: &str) -> Option<String> {
        self.variables.get(name).cloned()
    }

    fn policy_status(&self, _target: &str, policy: &str) -> PolicyStatus {
        match self.policies.get(policy).map(String::as_str) {
            Some("OLD") => PolicyStatus::Old,
            Some("WARN") => PolicyStatus::Warn,
            _ => PolicyStatus::New,
        }
    }

    fn system_include_directories(&self, target: &str) -> Vec<String> {
        self.targets
            .get(target)
            .map(|t| t.system.clone())
            .unwrap_or_default()
    }

    fn property_entries(&self, target: &str, prop: &str) -> Vec<PropertyEntry> {
        if let Some(entries) = self.targets.get(target).and_then(|t| t.entries.get(prop)) {
            return entries
                .iter()
                .map(|(line, value)| PropertyEntry {
                    value: value.clone(),
                    backtrace: Some(*line),
                })
                .collect();
        }
        self.target_property_raw(target, prop)
            .into_iter()
            .map(|value| PropertyEntry {
                value,
                backtrace: self
                    .backtraces
                    .get(&(target.to_owned(), prop.to_owned()))
                    .copied(),
            })
            .collect()
    }

    fn mapped_imported_config(&self, target: &str, config: &str) -> Option<String> {
        let mock = self.targets.get(target)?;
        let has = |name: &str| mock.property(name).is_some();
        let prefix = if self.target_type(target) == Some("OBJECT_LIBRARY") {
            "IMPORTED_OBJECTS"
        } else {
            "IMPORTED_LOCATION"
        };
        let config = config.to_ascii_uppercase();
        if let Some(map) = mock.property(&format!("MAP_IMPORTED_CONFIG_{config}")) {
            return cm_list(&map, true).into_iter().find_map(|name| {
                let suffix = if name.is_empty() {
                    name
                } else {
                    format!("_{}", name.to_ascii_uppercase())
                };
                has(&format!("{prefix}{suffix}")).then_some(suffix)
            });
        }
        let suffix = if config.is_empty() {
            "_NOCONFIG".to_owned()
        } else {
            format!("_{config}")
        };
        if has(&format!("{prefix}{suffix}")) {
            return Some(suffix);
        }
        has(prefix).then(String::new)
    }

    fn path_exists(&self, path: &str) -> bool {
        path != self.missing_path
    }
}

#[derive(Deserialize)]
struct Corpus {
    #[serde(flatten)]
    host: MockHost,
    c_variables: BTreeMap<String, String>,
    c_targets: BTreeMap<String, MockTarget>,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    expression: String,
    setup: Setup,
    output: Option<String>,
    error: Option<String>,
    #[serde(default)]
    loop_steps: Vec<String>,
}

#[derive(Deserialize)]
struct Setup {
    head: Option<String>,
    language: Option<String>,
    #[serde(default)]
    config: String,
    #[serde(default)]
    properties: BTreeMap<String, BTreeMap<String, String>>,
}

fn corpus() -> &'static Corpus {
    static CORPUS: OnceLock<Corpus> = OnceLock::new();
    CORPUS.get_or_init(|| {
        serde_json::from_str(include_str!("../../tests/data/genex_corpus.json")).unwrap()
    })
}

fn replay(case: &Case) {
    let corpus = corpus();
    let mut host = corpus.host.clone();
    if case.setup.language.as_deref() == Some("C") {
        host.variables.extend(corpus.c_variables.clone());
        host.targets.extend(corpus.c_targets.clone());
    }
    for (target, properties) in &case.setup.properties {
        host.targets
            .get_mut(target)
            .unwrap()
            .properties
            .extend(properties.clone());
    }
    let mut ctx = EvalContext {
        config: &case.setup.config,
        head_target: case.setup.head.as_deref(),
        language: case.setup.language.as_deref(),
        ..EvalContext::default()
    };
    let actual = evaluate(&parse(&case.expression), &mut ctx, &mut host);
    if let Some(expected) = &case.output {
        assert_eq!(actual, Ok(expected.clone()), "{}", case.expression);
    } else {
        let error = actual.expect_err(&case.expression);
        let body = crate::messages::format_text(&error.to_string(), 2);
        // The formatter leaves blank lines between prose and preformatted
        // blocks. Ignore only diagnostic block terminators.
        assert_eq!(
            body.trim_end(),
            case.error.as_deref().unwrap().trim_end(),
            "{}",
            case.expression
        );
        let actual_steps: Vec<_> = error
            .loop_steps
            .iter()
            .map(|(step, _)| crate::messages::format_text(step, 2))
            .collect();
        assert_eq!(
            actual_steps
                .iter()
                .map(|step| step.trim_end())
                .collect::<Vec<_>>(),
            case.loop_steps
                .iter()
                .map(|step| step.trim_end())
                .collect::<Vec<_>>(),
            "{}",
            case.expression
        );
    }
}

macro_rules! oracle_cases {
    ($($name:ident => $index:expr,)*) => {
        $(
            #[test]
            fn $name() {
                replay(&corpus().cases[$index]);
            }
        )*
        #[test]
        fn all_corpus_rows_are_tested() {
            assert_eq!(corpus().cases.len(), [$($index),*].len());
            assert!(corpus().cases.len() >= 250);
        }
    };
}

include!("corpus_tests.rs");

#[test]
fn strip_helpers() {
    assert_eq!(find("foo>$<bar"), None);
    assert_eq!(find("foo>$<bar>"), Some(4));
    assert!(contains_genex("$<bar>"));
    assert_eq!(strip(";$<1:a;$<JOIN:b;c,,>>;;d;"), "d");
    assert_eq!(strip("a;$<unclosed:x;;"), "a;$<unclosed:x");
    assert_eq!(strip_empty_list_elements(";;;foo;;bar;;;;"), "foo;bar");
    let interfaces = "x;$<BUILD_INTERFACE:a;$<BOOL:1>>;$<INSTALL_INTERFACE:incl;/abs;$<1:dir>>;$<BUILD_LOCAL_INTERFACE:z>";
    assert_eq!(
        preprocess(interfaces, PreprocessContext::BuildInterface, ""),
        "x;a;$<BOOL:1>"
    );
    assert_eq!(
        preprocess(
            interfaces,
            PreprocessContext::InstallInterface,
            "${_IMPORT_PREFIX}/"
        ),
        "x;${_IMPORT_PREFIX}/incl;/abs;$<1:dir>"
    );
    assert_eq!(
        replace_install_prefix("$<INSTALL_PREFIX>/dir", "/opt"),
        "/opt/dir"
    );
}

#[test]
fn custom_command_config_switches_and_records_cross_dependencies() {
    let mut host = corpus().host.clone();
    let mut dependencies = BTreeSet::new();
    let mut ctx = EvalContext::default();
    let expression = "cmd:$<CONFIG> $<OUTPUT_CONFIG:$<CONFIG>=$<TARGET_FILE:imp>> $<CONFIG> $<COMMAND_CONFIG:$<CONFIG>=$<TARGET_FILE:imp>> $<CONFIG>";
    assert_eq!(
        evaluate_custom_command(
            expression,
            &mut ctx,
            &mut host,
            "Debug",
            "Release",
            false,
            &mut dependencies
        )
        .unwrap(),
        "cmd:Release Debug=/opt/libsample.so.1 Debug Release=/opt/libsample.so.1 Release"
    );
    assert_eq!(
        dependencies
            .into_iter()
            .map(|dep| (dep.target, dep.cross))
            .collect::<Vec<_>>(),
        vec![("imp".to_owned(), false), ("imp".to_owned(), true)]
    );
    assert_eq!(ctx.config, "");
}

#[test]
fn link_and_compile_dag_modes() {
    let mut host = corpus().host.clone();
    host.variables
        .insert("CMAKE_CXX_COMPILER_ID".to_owned(), "Clang".to_owned());
    let dag = DagChecker::new("a", "LINK_LIBRARIES");
    let mut ctx = EvalContext {
        head_target: Some("a"),
        language: Some("CXX"),
        dag: Some(&dag),
        ..EvalContext::default()
    };
    for (expr, expected) in [
        ("$<LINK_LANG_AND_ID:CXX,Clang,GNU>", "1"),
        ("$<LINK_LANGUAGE:CXX>", "1"),
        (
            "$<LINK_LIBRARY:WHOLE_ARCHIVE,a;b>",
            "<LINK_LIBRARY:WHOLE_ARCHIVE>;a;b;</LINK_LIBRARY:WHOLE_ARCHIVE>",
        ),
        (
            "$<LINK_GROUP:RESCAN,a,b>",
            "<LINK_GROUP:RESCAN:a|b>;a;b;</LINK_GROUP:RESCAN>",
        ),
        ("$<COMPILE_ONLY:x>$<LINK_ONLY:y>", "y"),
    ] {
        assert_eq!(
            evaluate(&parse(expr), &mut ctx, &mut host).unwrap(),
            expected
        );
    }
    assert!(ctx.link_language_sensitive);
    let dag = DagChecker {
        transitive_properties_only: true,
        transitive_properties_only_cmp0131: true,
        ..DagChecker::new("a", "LINK_LIBRARIES")
    };
    ctx.dag = Some(&dag);
    assert_eq!(
        evaluate(
            &parse("$<COMPILE_ONLY:x>$<LINK_ONLY:y>"),
            &mut ctx,
            &mut host
        )
        .unwrap(),
        "x"
    );
}

#[test]
fn unix_command_line_matches_kwsys() {
    use super::target::parse_unix_command_line as parse;
    assert_eq!(parse(r#"-x 'a b' "c d"e \ f"#), ["-x", "a b", "c de", " f"]);
    assert_eq!(parse("a\u{b}b\t c"), ["a", "b", "c"]);
    assert_eq!(parse("''"), [""]);
    assert_eq!(parse("'unterminated x"), ["unterminated x"]);
    assert_eq!(parse("  "), Vec::<String>::new());
    assert_eq!(parse("ä'ö ü'"), ["äö ü"]);
}

#[test]
fn device_link_options_wrap_host_flags() {
    let mut host = corpus().host.clone();
    let a = host.targets.get_mut("a").unwrap();
    a.kind = "EXECUTABLE".to_owned();
    a.properties.insert(
        "LINK_OPTIONS".to_owned(),
        "-host;$<DEVICE_LINK:-dev;SHELL:-x y>;LINKER:-z,defs;SHELL:-s t".to_owned(),
    );
    a.properties.remove("INTERFACE_LINK_LIBRARIES");
    for (name, value) in [
        ("CMAKE_CUDA_DEVICE_COMPILER_WRAPPER_FLAG", "-Xcompiler=;"),
        ("CMAKE_CUDA_DEVICE_COMPILER_WRAPPER_FLAG_SEP", ","),
        ("CMAKE_CUDA_DEVICE_LINKER_WRAPPER_FLAG", "-Xlinker; "),
        ("CMAKE_CUDA_LINKER_WRAPPER_FLAG", "-Wl,"),
        ("CMAKE_CUDA_LINKER_WRAPPER_FLAG_SEP", ","),
    ] {
        host.variables.insert(name.to_owned(), value.to_owned());
    }
    let values = |host: &mut MockHost, device| {
        let mut ctx = EvalContext {
            language: Some("CUDA"),
            device_link: device,
            ..EvalContext::default()
        };
        usage_requirements(host, "a", "LINK_OPTIONS", &mut ctx)
            .unwrap()
            .into_iter()
            .map(|item| item.value)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        values(&mut host, false),
        ["-host", "-Wl,-z,defs", "-s", "t"]
    );
    // Device sections split SHELL: options; host options stay whole until
    // they are parsed and wrapped for the device compiler. Nested linker
    // flags keep LINKER: and then use the device linker wrapper.
    assert_eq!(
        values(&mut host, true),
        [
            "-Xcompiler=-host",
            "-dev",
            "-x",
            "y",
            "-Xlinker",
            "-z",
            "-Xlinker",
            "defs",
            "-Xcompiler=-s,t"
        ]
    );
}

#[test]
fn per_root_already_seen() {
    // Two separate top-level evaluations must not share their Seen sets.
    let mut host = corpus().host.clone();
    let mut ctx = EvalContext::default();
    let expression = parse(
        "$<TARGET_PROPERTY:a,INTERFACE_LINK_OPTIONS>|$<TARGET_PROPERTY:a,INTERFACE_LINK_OPTIONS>",
    );
    assert_eq!(
        evaluate(&expression, &mut ctx, &mut host).unwrap(),
        "A_LINK;B_LINK;D_LINK;C_LINK|A_LINK;B_LINK;D_LINK;C_LINK"
    );
}

#[derive(Deserialize)]
struct UsageCorpus {
    variables: BTreeMap<String, String>,
    missing_path: String,
    cases: Vec<UsageCase>,
}

#[derive(Deserialize)]
struct UsageCase {
    name: String,
    targets: BTreeMap<String, MockTarget>,
    aliases: BTreeMap<String, String>,
    logs: BTreeMap<String, Vec<(Backtrace, String)>>,
    error: Option<String>,
    ninja: Option<BTreeMap<String, String>>,
}

fn usage_corpus() -> &'static UsageCorpus {
    static CORPUS: OnceLock<UsageCorpus> = OnceLock::new();
    CORPUS.get_or_init(|| {
        serde_json::from_str(include_str!("../../tests/data/genex_usage_corpus.json")).unwrap()
    })
}

const USAGE_LOGS: [(&str, &str); 6] = [
    ("compile options", "COMPILE_OPTIONS"),
    ("compile definitions", "COMPILE_DEFINITIONS"),
    ("includes", "INCLUDE_DIRECTORIES"),
    ("compile features", "COMPILE_FEATURES"),
    ("link options", "LINK_OPTIONS"),
    ("link directories", "LINK_DIRECTORIES"),
];

fn replay_usage(case: &UsageCase) {
    use super::target::parse_unix_command_line as parse;
    let corpus = usage_corpus();
    let mut host = MockHost {
        variables: corpus.variables.clone(),
        targets: case.targets.clone(),
        aliases: case.aliases.clone(),
        missing_path: corpus.missing_path.clone(),
        ..MockHost::default()
    };
    let results: Vec<_> = USAGE_LOGS
        .iter()
        .map(|(log, prop)| (*log, target_usage(&mut host, "main", prop, "", "C")))
        .collect();
    if let Some(expected) = &case.error {
        let errors: Vec<String> = results
            .iter()
            .filter_map(|(_, result)| result.as_ref().err())
            .map(|error| crate::messages::format_text(&error.to_string(), 2))
            .collect();
        assert!(
            errors
                .iter()
                .any(|error| error.trim_end() == expected.trim_end()),
            "{}: expected error\n{expected}\ngot {errors:#?}",
            case.name
        );
        return;
    }
    let ninja = case.ninja.as_ref().unwrap();
    for (log, result) in results {
        let items = result.unwrap_or_else(|error| panic!("{}: {log}: {error}", case.name));
        let expected = case.logs.get(log).cloned().unwrap_or_default();
        match log {
            "compile options" => {
                // The log shows values before SHELL: splitting.
                let expected: Vec<(Backtrace, String)> = expected
                    .into_iter()
                    .flat_map(|(line, value)| match value.strip_prefix("SHELL:") {
                        Some(rest) => parse(rest).into_iter().map(|v| (line, v)).collect(),
                        None => vec![(line, value)],
                    })
                    .collect();
                let actual: Vec<_> = items
                    .iter()
                    .map(|item| (item.backtrace.unwrap(), item.value.clone()))
                    .collect();
                assert_eq!(actual, expected, "{}: {log}", case.name);
                let values: Vec<_> = items.iter().map(|item| item.value.clone()).collect();
                assert_eq!(values, parse(&ninja["FLAGS"]), "{}: FLAGS", case.name);
            }
            "link options" => {
                // The log shows values before SHELL: splitting and LINKER:
                // wrapping; build.ninja shows the final flags.
                let mut flags = parse(&ninja["LINK_FLAGS"]);
                assert!(flags.pop().unwrap().starts_with("-Wl,--dependency-file="));
                let values: Vec<_> = items.iter().map(|item| item.value.clone()).collect();
                assert_eq!(values, flags, "{}: LINK_FLAGS", case.name);
                let lines: BTreeSet<_> = expected.iter().map(|(line, _)| *line).collect();
                assert!(
                    items
                        .iter()
                        .all(|item| lines.contains(&item.backtrace.unwrap())),
                    "{}: link option backtraces",
                    case.name
                );
            }
            _ => {
                let actual: Vec<_> = items
                    .iter()
                    .map(|item| (item.backtrace.unwrap(), item.value.clone()))
                    .collect();
                assert_eq!(actual, expected, "{}: {log}", case.name);
            }
        }
        if log == "includes" {
            // cmLocalGenerator moves system directories last, so compare
            // classification rather than order.
            let mut tokens = parse(&ninja["INCLUDES"]).into_iter();
            let mut system = BTreeMap::new();
            while let Some(token) = tokens.next() {
                if token == "-isystem" {
                    system.insert(tokens.next().unwrap(), true);
                } else {
                    system.insert(token.strip_prefix("-I").unwrap().to_owned(), false);
                }
            }
            for item in &items {
                assert_eq!(
                    system.get(&item.value),
                    Some(&item.system),
                    "{}: system flag of {}",
                    case.name,
                    item.value
                );
            }
        }
    }
}

macro_rules! usage_cases {
    ($($name:ident => $index:expr,)*) => {
        $(
            #[test]
            fn $name() {
                replay_usage(&usage_corpus().cases[$index]);
            }
        )*
        #[test]
        fn all_usage_rows_are_tested() {
            assert_eq!(usage_corpus().cases.len(), [$($index),*].len());
        }
    };
}

include!("usage_corpus_tests.rs");

#[test]
fn interface_usage_collects_transitive_includes_and_their_origins() {
    let mut host = corpus().host.clone();
    host.backtraces
        .insert(("a".into(), "INTERFACE_INCLUDE_DIRECTORIES".into()), 7);
    host.backtraces
        .insert(("a".into(), "INTERFACE_LINK_LIBRARIES".into()), 8);
    host.targets
        .get_mut("b")
        .unwrap()
        .properties
        .insert("SYSTEM".into(), "ON".into());
    let items = target_usage(
        &mut host,
        "a",
        "INTERFACE_INCLUDE_DIRECTORIES",
        "Debug",
        "C",
    )
    .unwrap();
    assert_eq!(
        items
            .iter()
            .map(|item| (item.value.as_str(), item.backtrace, item.system))
            .collect::<Vec<_>>(),
        [
            ("/a", Some(7), false),
            ("/b", Some(8), true),
            ("/d", Some(8), false)
        ]
    );
}
