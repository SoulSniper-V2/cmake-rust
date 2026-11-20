use std::sync::OnceLock;

use serde::Deserialize;

use super::*;

#[derive(Clone, Deserialize)]
struct MockTarget {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    imported: bool,
    properties: BTreeMap<String, String>,
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
}

impl MockHost {
    fn imported_value(&self, target: &str, config: &str, prop: &str) -> Option<String> {
        let props = &self.targets.get(target)?.properties;
        let suffix = self
            .mapped_imported_config(target, config)
            .unwrap_or_default();
        props
            .get(&format!("{prop}{suffix}"))
            .or_else(|| props.get(prop))
            .cloned()
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
            _ => self.targets.get(target)?.properties.get(prop).cloned(),
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

    fn property_entries(&self, target: &str, prop: &str) -> Vec<PropertyEntry> {
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
        let props = &self.targets.get(target)?.properties;
        let prefix = if self.target_type(target) == Some("OBJECT_LIBRARY") {
            "IMPORTED_OBJECTS"
        } else {
            "IMPORTED_LOCATION"
        };
        let config = config.to_ascii_uppercase();
        if let Some(map) = props.get(&format!("MAP_IMPORTED_CONFIG_{config}")) {
            return cm_list(map, true).into_iter().find_map(|name| {
                let suffix = if name.is_empty() {
                    name
                } else {
                    format!("_{}", name.to_ascii_uppercase())
                };
                props
                    .contains_key(&format!("{prefix}{suffix}"))
                    .then_some(suffix)
            });
        }
        let suffix = if config.is_empty() {
            "_NOCONFIG".to_owned()
        } else {
            format!("_{config}")
        };
        if props.contains_key(&format!("{prefix}{suffix}")) {
            return Some(suffix);
        }
        props.contains_key(prefix).then(String::new)
    }

    fn path_exists(&self, _path: &str) -> bool {
        true
    }
}

#[derive(Deserialize)]
struct Corpus {
    #[serde(flatten)]
    host: MockHost,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    expression: String,
    setup: Setup,
    output: Option<String>,
    error: Option<String>,
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
    let mut host = corpus().host.clone();
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
        ..dag
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
fn usage_items_preserve_order_and_provenance() {
    let mut host = corpus().host.clone();
    let props = &mut host.targets.get_mut("a").unwrap().properties;
    props.insert("LINK_LIBRARIES".to_owned(), "b;imp".to_owned());
    props.insert(
        "COMPILE_OPTIONS".to_owned(),
        "A;SHELL:-I 'two words';A".to_owned(),
    );
    props.insert(
        "INCLUDE_DIRECTORIES".to_owned(),
        "/a;/opt/imported".to_owned(),
    );
    props.insert("SYSTEM_INCLUDE_DIRECTORIES".to_owned(), "/a".to_owned());
    host.targets.get_mut("b").unwrap().properties.insert(
        "INTERFACE_COMPILE_OPTIONS".to_owned(),
        "SHELL:-I 'two words';B;$<$<COMPILE_LANG_AND_ID:CXX,Clang>:CLANG>".to_owned(),
    );
    host.targets.get_mut("imp").unwrap().properties.insert(
        "INTERFACE_INCLUDE_DIRECTORIES".to_owned(),
        "/opt/imported".to_owned(),
    );
    host.targets.get_mut("imp").unwrap().properties.insert(
        "INTERFACE_SYSTEM_INCLUDE_DIRECTORIES".to_owned(),
        "/explicit".to_owned(),
    );
    host.targets
        .get_mut("imp")
        .unwrap()
        .properties
        .get_mut("INTERFACE_INCLUDE_DIRECTORIES")
        .unwrap()
        .push_str(";/explicit");
    host.variables
        .insert("CMAKE_CXX_COMPILER_ID".to_owned(), "Clang".to_owned());
    host.backtraces
        .insert(("a".to_owned(), "COMPILE_OPTIONS".to_owned()), 11);
    host.backtraces
        .insert(("b".to_owned(), "INTERFACE_COMPILE_OPTIONS".to_owned()), 12);
    let options = target_usage(&mut host, "a", "COMPILE_OPTIONS", "Debug", "CXX").unwrap();
    assert_eq!(
        options
            .iter()
            .map(|item| (item.value.as_str(), item.backtrace))
            .collect::<Vec<_>>(),
        [
            ("A", Some(11)),
            ("-I", Some(11)),
            ("two words", Some(11)),
            ("B", Some(12)),
            ("CLANG", Some(12))
        ]
    );
    let includes = target_usage(&mut host, "a", "INCLUDE_DIRECTORIES", "", "CXX").unwrap();
    assert_eq!(
        includes
            .iter()
            .map(|item| (item.value.as_str(), item.system))
            .collect::<Vec<_>>(),
        [
            ("/a", true),
            ("/opt/imported", true),
            ("/b", false),
            ("/d", false),
            ("/explicit", true)
        ]
    );
    host.targets
        .get_mut("a")
        .unwrap()
        .properties
        .insert("NO_SYSTEM_FROM_IMPORTED".to_owned(), "ON".to_owned());
    let includes = target_usage(&mut host, "a", "INCLUDE_DIRECTORIES", "", "CXX").unwrap();
    assert!(
        !includes
            .iter()
            .find(|item| item.value == "/opt/imported")
            .unwrap()
            .system
    );
    assert!(
        includes
            .iter()
            .find(|item| item.value == "/explicit")
            .unwrap()
            .system
    );
}
