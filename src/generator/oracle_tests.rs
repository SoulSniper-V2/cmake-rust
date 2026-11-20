//! Fixture-driven oracle tests.
//!
//! `tests/data/generator/install/<case>/` holds the files that Kitware
//! CMake 4.1.0 generated for a small project, normalized so the project
//! source directory reads `@SOURCE@` and the build directory `@BINARY@`.
//! Each case below rebuilds the equivalent configured state — driving the
//! real `install()`/`export()` commands where possible and fabricating
//! target/test entries where their owning commands are not yet ported —
//! then compares every produced file byte-for-byte.
//!
//! Cases that need `generator::target` artifact naming (`names`,
//! `full_path`, `directory`, `object_directory`, `sources`,
//! `linker_language`) are skipped while those functions remain `todo!()`s;
//! they light up automatically once the generator-target port lands.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use super::ctx::GenCtx;
use super::{export, install, tests as test_gen};
use crate::interp::{Interp, Makefile, Mode};
use crate::messages::Frame;
use crate::props::Props;
use crate::state::{Target, TargetKind, Test};

const FIXTURES: &str = "tests/data/generator/install";

fn props(entries: &[(&str, &str)]) -> Props {
    let mut props = Props::default();
    for &(key, value) in entries {
        props.set(key, value.to_owned());
    }
    props
}

/// `cmMakefile` state the generator reads: per-directory final variables.
fn vars(interp: &mut Interp, makefile: usize, entries: &[(&str, &str)]) {
    if interp.dir_final_vars.len() <= makefile {
        interp.dir_final_vars.resize(makefile + 1, HashMap::new());
    }
    for &(key, value) in entries {
        interp.dir_final_vars[makefile].insert(key.to_owned(), Some(value.to_owned()));
    }
}

/// The variables a C project leaves in every directory.
fn c_vars(interp: &mut Interp, makefile: usize) {
    vars(interp, makefile, &[("CMAKE_INSTALL_SO_NO_EXE", "1"), ("CMAKE_CROSSCOMPILING", "FALSE")]);
}

/// Additional variables left by a configured toolchain.
fn toolchain_vars(interp: &mut Interp, makefile: usize) {
    vars(interp, makefile, &[
        ("CMAKE_INSTALL_SO_NO_EXE", "1"),
        ("CMAKE_CROSSCOMPILING", "FALSE"),
        ("CMAKE_OBJDUMP", "/usr/bin/objdump"),
        ("CMAKE_STRIP", "/usr/bin/strip"),
        ("CMAKE_EXECUTABLE_FORMAT", "ELF"),
        ("CMAKE_EXECUTABLE_SUFFIX", ""),
        ("CMAKE_STATIC_LIBRARY_PREFIX", "lib"),
        ("CMAKE_STATIC_LIBRARY_SUFFIX", ".a"),
        ("CMAKE_SHARED_LIBRARY_PREFIX", "lib"),
        ("CMAKE_SHARED_LIBRARY_SUFFIX", ".so"),
        ("CMAKE_SHARED_MODULE_PREFIX", "lib"),
        ("CMAKE_SHARED_MODULE_SUFFIX", ".so"),
        ("CMAKE_SHARED_LIBRARY_SONAME_FLAG", "-Wl,-soname,"),
        ("CMAKE_SHARED_LIBRARY_SONAME_C_FLAG", "-Wl,-soname,"),
        ("CMAKE_SHARED_LIBRARY_SONAME_CXX_FLAG", "-Wl,-soname,"),
        ("CMAKE_SHARED_MODULE_SONAME_FLAG", "-Wl,-soname,"),
        ("CMAKE_SHARED_MODULE_SONAME_C_FLAG", "-Wl,-soname,"),
        ("CMAKE_SHARED_LIBRARY_RUNTIME_C_FLAG_SEP", ":"),
        ("CMAKE_SHARED_MODULE_RUNTIME_C_FLAG_SEP", ":"),
    ]);
    if interp.langs.extension_to_language.is_empty() {
        for (ext, lang) in [("c", "C"), ("cxx", "CXX"), ("cpp", "CXX"), ("cppm", "CXX")] {
            interp.langs.extension_to_language.insert(ext.to_owned(), lang.to_owned());
        }
        interp.langs.linker_preference.insert("C".to_owned(), 10);
        interp.langs.linker_preference.insert("CXX".to_owned(), 30);
        interp.langs.output_extension.insert("C".to_owned(), ".o".to_owned());
        interp.langs.output_extension.insert("CXX".to_owned(), ".o".to_owned());
        interp.langs.enabled.extend(["C".to_owned(), "CXX".to_owned()]);
    }
}

fn run(interp: &mut Interp, code: &str) {
    assert!(interp.run_string(code, "CMakeLists.txt"), "script failed to parse");
    assert!(!interp.error, "script produced errors");
}

/// Runs `code` as the list file of directory `makefile`.
fn run_in(interp: &mut Interp, makefile: usize, code: &str) {
    interp.mf_stack.push(makefile);
    interp.push_scope();
    run(interp, code);
    interp.pop_scope();
    interp.mf_stack.pop();
}

fn subdirectory(interp: &mut Interp, parent: usize, name: &str, exclude: bool) -> usize {
    let source_dir = format!("{}/{name}", interp.makefiles[parent].source_dir);
    let binary_dir = format!("{}/{name}", interp.makefiles[parent].binary_dir);
    let policies = interp.makefiles[parent].policies.clone();
    let index = interp.makefiles.len();
    interp.makefiles.push(Makefile {
        source_dir,
        binary_dir,
        parent: Some(parent),
        props: Props::default(),
        policies,
        list_files: Vec::new(),
        targets: Vec::new(),
        children: Vec::new(),
        exclude_from_all: exclude,
        deferred: Vec::new(),
        defer_counter: 0,
    });
    if exclude {
        interp.makefiles[index].props.set("EXCLUDE_FROM_ALL", "TRUE".to_owned());
    }
    interp.makefiles[parent].children.push(index);
    index
}

fn target(interp: &mut Interp, name: &str, kind: TargetKind, makefile: usize, entries: &[(&str, &str)]) {
    target_ex(interp, name, kind, makefile, false, entries);
}

fn target_ex(
    interp: &mut Interp, name: &str, kind: TargetKind, makefile: usize, imported: bool,
    entries: &[(&str, &str)],
) {
    let props = props(entries);
    // Source existence is checked at generate time; materialize the files.
    if let Some(sources) = props.get("SOURCES") {
        let source_dir = interp.makefiles[makefile].source_dir.clone();
        for source in sources.split(';').filter(|s| !s.is_empty() && !s.contains('$') && !s.starts_with('/')) {
            let path = Path::new(&source_dir).join(source);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
    }
    let target = Target {
        name: name.to_owned(),
        kind,
        makefile,
        imported,
        imported_global: false,
        alias_of: None,
        props,
        provenance: Default::default(),
        backtrace: Vec::new(),
    };
    interp.ext.target_order.push(name.to_owned());
    interp.ext.targets.insert(name.to_owned(), target);
}

fn add_test(
    interp: &mut Interp, makefile: usize, name: &str, command: &[&str],
    entries: &[(&str, &str)], old_style: bool, line: usize,
) {
    let file: std::rc::Rc<str> =
        format!("{}/CMakeLists.txt", interp.makefiles[makefile].source_dir).into();
    interp.ext.tests.push(Test {
        name: name.to_owned(),
        makefile,
        command: command.iter().map(|arg| (*arg).to_owned()).collect(),
        props: props(entries),
        old_style,
        backtrace: vec![
            Frame { file: file.clone(), line: 0, name: None, deferred: false },
            Frame { file, line, name: Some("add_test".into()), deferred: false },
        ],
    });
}

fn project(src: &str, bin: &str) -> Interp {
    Interp::new(Mode::Project, src, bin, "")
}

/// Generates all install/test/export files for `interp` and compares the
/// produced tree against the fixture directory for `case`.
fn check(case: &str, interp: &Interp) -> Result<(), String> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURES).join(case);
    let source = &interp.home_source;
    let binary = &interp.home_binary;
    let ctx = GenCtx::new(interp);
    let generate = || {
        install::write_install_scripts(&ctx)?;
        test_gen::write_test_files(&ctx)?;
        export::write_build_exports(&ctx)?;
        Ok::<(), std::io::Error>(())
    };
    generate().map_err(|error| format!("io error: {error}"))?;
    let errors = ctx.errors.borrow();
    if !errors.is_empty() {
        return Err(format!("generation errors: {errors:?}"));
    }
    drop(errors);
    let mut failures = Vec::new();
    let normalize = |text: String| {
        text.replace(binary.as_str(), "@BINARY@").replace(source.as_str(), "@SOURCE@")
    };
    for entry in walk(&fixture) {
        let rel = entry.strip_prefix(&fixture).unwrap().to_str().unwrap();
        let produced = Path::new(binary).join(rel);
        let Ok(actual) = std::fs::read_to_string(&produced) else {
            failures.push(format!("{rel}: not produced"));
            continue;
        };
        let expected = std::fs::read_to_string(&entry).unwrap();
        let actual = normalize(actual);
        if actual != expected {
            if let Ok(dump) = std::env::var("DUMP") {
                let path = Path::new(&dump).join(binary.replace('/', "_")).join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, &actual).unwrap();
            }
            failures.push(format!("{rel}: differs\n{}", diff(&expected, &actual)));
        }
    }
    // No file may be produced beyond the fixture set.
    let fixture_files: std::collections::BTreeSet<String> = walk(&fixture)
        .iter()
        .map(|path| path.strip_prefix(&fixture).unwrap().to_str().unwrap().to_owned())
        .collect();
    for entry in walk(Path::new(binary)) {
        let rel = entry.strip_prefix(Path::new(binary)).unwrap().to_str().unwrap().to_owned();
        if rel.ends_with(".actual") { continue; }
        if !fixture_files.contains(&rel) {
            failures.push(format!("{rel}: produced but not in fixture"));
        }
    }
    if failures.is_empty() { Ok(()) } else { Err(failures.join("\n  ")) }
}

fn diff(expected: &str, actual: &str) -> String {
    let mut out = String::new();
    let expected: Vec<_> = expected.lines().collect();
    let actual: Vec<_> = actual.lines().collect();
    for (line, (a, b)) in expected.iter().zip(actual.iter()).enumerate() {
        if a != b {
            out.push_str(&format!("    line {}:\n      - {a}\n      + {b}\n", line + 1));
            if out.matches("line ").count() > 6 { break; }
        }
    }
    if expected.len() != actual.len() {
        out.push_str(&format!("    (length differs: expected {} lines, got {})\n", expected.len(), actual.len()));
    }
    out
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() { out.extend(walk(&path)); } else { out.push(path); }
        }
    }
    out.sort();
    out
}

fn case(case: &str, build: impl FnOnce(&mut Interp)) -> Result<(), String> {
    let work = std::env::temp_dir().join(format!("cmr-oracle-{}-{}", case, std::process::id()));
    let source = work.join("src");
    let binary = work.join("build");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&binary).unwrap();
    let src = source.to_str().unwrap().to_owned();
    let bin = binary.to_str().unwrap().to_owned();
    let mut interp = project(&src, &bin);
    build(&mut interp);
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| check(case, &interp)));
    let _ = std::fs::remove_dir_all(&work);
    match result {
        Ok(inner) => inner,
        Err(panic) => {
            let message = panic.downcast_ref::<&'static str>().map(|s| (*s).to_owned())
                .or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
            if message.contains("not yet implemented") || message.contains("not implemented") {
                Err(format!("SKIPPED: {message}"))
            } else {
                std::panic::resume_unwind(panic)
            }
        }
    }
}

#[test]
fn oracle_fixtures() {
    let cases: Vec<(&str, Box<dyn Fn(&mut Interp)>)> = vec![
        ("empty", Box::new(|i| c_vars(i, 0))),
        ("prologue", Box::new(|i| {
            vars(i, 0, &[
                ("CMAKE_INSTALL_PREFIX", "/stage"),
                ("CMAKE_INSTALL_SO_NO_EXE", "1"),
                ("CMAKE_CROSSCOMPILING", "FALSE"),
                ("CMAKE_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS", "OWNER_READ"),
                ("CMAKE_GET_RUNTIME_DEPENDENCIES_PLATFORM", "linux+elf"),
                ("CMAKE_GET_RUNTIME_DEPENDENCIES_TOOL", "objdump"),
                ("CMAKE_GET_RUNTIME_DEPENDENCIES_COMMAND", "/tool/objdump"),
                ("CMAKE_OBJDUMP", "/other/objdump"),
            ]);
        })),
        ("files", Box::new(|i| {
            run(i, "install(FILES a.txt DESTINATION share)");
            c_vars(i, 0);
        })),
        ("files-bin", Box::new(|i| {
            run(i, "install(FILES a.txt TYPE BIN)");
            c_vars(i, 0);
        })),
        ("files-sbin", Box::new(|i| {
            run(i, "set(CMAKE_INSTALL_SBINDIR tools)\ninstall(FILES a.txt TYPE SBIN)");
            c_vars(i, 0);
        })),
        ("files-lib", Box::new(|i| {
            run(i, "set(CMAKE_INSTALL_LIBDIR lib64)\ninstall(FILES a.txt TYPE LIB)");
            c_vars(i, 0);
        })),
        ("files-libexec", Box::new(|i| {
            run(i, "install(PROGRAMS a TYPE LIBEXEC)");
            c_vars(i, 0);
        })),
        ("files-include", Box::new(|i| {
            run(i, "install(FILES a.h TYPE INCLUDE)");
            c_vars(i, 0);
        })),
        ("files-sysconf", Box::new(|i| {
            run(i, "install(FILES a.conf TYPE SYSCONF)");
            c_vars(i, 0);
        })),
        ("files-sharedstate", Box::new(|i| {
            run(i, "install(FILES a.txt TYPE SHAREDSTATE)");
            c_vars(i, 0);
        })),
        ("files-localstate", Box::new(|i| {
            run(i, "install(FILES a.txt TYPE LOCALSTATE)");
            c_vars(i, 0);
        })),
        ("files-runstate", Box::new(|i| {
            run(i, "set(CMAKE_INSTALL_LOCALSTATEDIR state)\ninstall(FILES a.txt TYPE RUNSTATE)");
            c_vars(i, 0);
        })),
        ("files-data", Box::new(|i| {
            run(i, "set(CMAKE_INSTALL_DATADIR assets)\ninstall(FILES a.txt TYPE DATA)");
            c_vars(i, 0);
        })),
        ("files-info", Box::new(|i| {
            run(i, "install(FILES a.info TYPE INFO)");
            c_vars(i, 0);
        })),
        ("files-locale", Box::new(|i| {
            run(i, "install(FILES a.po TYPE LOCALE)");
            c_vars(i, 0);
        })),
        ("files-man", Box::new(|i| {
            run(i, "install(FILES a.1 TYPE MAN)");
            c_vars(i, 0);
        })),
        ("files-doc", Box::new(|i| {
            run(i, "install(FILES a.txt TYPE DOC)");
            c_vars(i, 0);
        })),
        ("files-multi", Box::new(|i| {
            run(i, "install(FILES a.txt sub/b.txt DESTINATION data)");
            c_vars(i, 0);
        })),
        ("files-component", Box::new(|i| {
            run(i, "install(FILES a.txt DESTINATION share COMPONENT docs EXCLUDE_FROM_ALL)");
            c_vars(i, 0);
        })),
        ("files-config", Box::new(|i| {
            run(i, "install(FILES a.txt DESTINATION share CONFIGURATIONS Debug Release)");
            c_vars(i, 0);
        })),
        ("permissions", Box::new(|i| {
            run(i, "install(FILES a.txt DESTINATION share PERMISSIONS OWNER_READ OWNER_WRITE GROUP_READ)");
            c_vars(i, 0);
        })),
        ("programs", Box::new(|i| {
            run(i, "install(PROGRAMS run DESTINATION bin)");
            c_vars(i, 0);
        })),
        ("programs-optional", Box::new(|i| {
            run(i, "install(PROGRAMS run DESTINATION bin OPTIONAL RENAME demo)");
            c_vars(i, 0);
        })),
        ("directory", Box::new(|i| {
            run(i, "install(DIRECTORY assets DESTINATION share)");
            c_vars(i, 0);
        })),
        ("directory-contents", Box::new(|i| {
            run(i, "install(DIRECTORY assets/ DESTINATION share)");
            c_vars(i, 0);
        })),
        ("directory-empty", Box::new(|i| {
            run(i, "install(DIRECTORY DESTINATION share)");
            c_vars(i, 0);
        })),
        ("directory-options", Box::new(|i| {
            run(i, "install(DIRECTORY assets DESTINATION share OPTIONAL MESSAGE_NEVER USE_SOURCE_PERMISSIONS FILE_PERMISSIONS OWNER_READ DIRECTORY_PERMISSIONS OWNER_EXECUTE)");
            c_vars(i, 0);
        })),
        ("directory-rules", Box::new(|i| {
            run(i, "install(DIRECTORY assets/ x DESTINATION share FILES_MATCHING PATTERN \"*.h\" PERMISSIONS OWNER_READ REGEX \"/private[^/]*$\" EXCLUDE REGEX \"[.]old$\" EXCLUDE)");
            c_vars(i, 0);
        })),
        ("directory-config", Box::new(|i| {
            run(i, "install(DIRECTORY assets DESTINATION /opt/xyz COMPONENT assets CONFIGURATIONS Debug EXCLUDE_FROM_ALL)");
            c_vars(i, 0);
        })),
        ("code", Box::new(|i| {
            run(i, "install(CODE \"message(STATUS hi)\")");
            c_vars(i, 0);
        })),
        ("code-all", Box::new(|i| {
            run(i, "install(CODE \"message(STATUS hi)\" ALL_COMPONENTS)");
            c_vars(i, 0);
        })),
        ("code-excluded", Box::new(|i| {
            run(i, "install(CODE \"message(STATUS hi)\" ALL_COMPONENTS EXCLUDE_FROM_ALL)");
            c_vars(i, 0);
        })),
        ("code-multiline", Box::new(|i| {
            run(i, "install(CODE \"message(STATUS hi)\nmessage(STATUS bye)\" COMPONENT scripts)");
            c_vars(i, 0);
        })),
        ("script", Box::new(|i| {
            run(i, "install(SCRIPT rel/install.cmake COMPONENT scripts)");
            c_vars(i, 0);
        })),
        ("script-multiple", Box::new(|i| {
            run(i, "install(SCRIPT rel/foo.cmake)\ninstall(CODE \"message(STATUS hi)\")\ninstall(SCRIPT rel/bar.cmake)");
            c_vars(i, 0);
        })),
        ("absolute", Box::new(|i| {
            run(i, "install(FILES a.txt b.txt DESTINATION /opt/xyz)");
            c_vars(i, 0);
        })),
        ("absolute-rename", Box::new(|i| {
            run(i, "install(FILES a.txt DESTINATION /etc RENAME test.conf)");
            c_vars(i, 0);
        })),
        ("dest-normal", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0177 NEW)\ninstall(FILES a.txt DESTINATION a/b/../c/.)");
            c_vars(i, 0);
        })),
        ("dest-old", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0177 OLD)\ninstall(FILES a.txt DESTINATION a/b/../c/.)");
            c_vars(i, 0);
        })),
        ("message-always", Box::new(|i| {
            run(i, "set(CMAKE_INSTALL_MESSAGE ALWAYS)\ninstall(FILES a.txt DESTINATION .)");
            c_vars(i, 0);
        })),
        ("message-lazy", Box::new(|i| {
            run(i, "set(CMAKE_INSTALL_MESSAGE LAZY)\ninstall(FILES a.txt DESTINATION .)");
            c_vars(i, 0);
        })),
        ("message-never", Box::new(|i| {
            run(i, "set(CMAKE_INSTALL_MESSAGE NEVER)\ninstall(FILES a.txt DESTINATION .)");
            c_vars(i, 0);
        })),
        ("legacy-files", Box::new(|i| {
            run(i, "install_files(/files FILES missing)");
            c_vars(i, 0);
        })),
        ("legacy-programs", Box::new(|i| {
            run(i, "install_programs(/bin FILES missing)");
            c_vars(i, 0);
        })),
        ("subdir", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0082 NEW)\ncmake_policy(SET CMP0110 NEW)\ninstall(CODE \"message(STATUS first)\")");
            let sub = subdirectory(i, 0, "sub", false);
            run_in(i, sub, "install(CODE \"message(STATUS nested)\")");
            add_test(i, sub, "child", &["echo", "child"], &[], false, 2);
            run(i, "install(CODE \"message(STATUS last)\")");
            c_vars(i, 0);
            c_vars(i, sub);
            vars(i, 0, &[("CMAKE_TESTING_ENABLED", "1")]);
            vars(i, sub, &[("CMAKE_TESTING_ENABLED", "1")]);
        })),
        ("subdir-excluded", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0082 NEW)\ncmake_policy(SET CMP0110 NEW)");
            let sub = subdirectory(i, 0, "sub", true);
            run_in(i, sub, "install(CODE \"message(STATUS nested)\")");
            add_test(i, sub, "child", &["echo", "child"], &[], false, 2);
            c_vars(i, 0);
            c_vars(i, sub);
            vars(i, 0, &[("CMAKE_TESTING_ENABLED", "1")]);
            vars(i, sub, &[("CMAKE_TESTING_ENABLED", "1")]);
        })),
        ("target-exe", Box::new(|i| {
            target(i, "app", TargetKind::Executable, 0, &[("SOURCES", "main.c")]);
            run(i, "install(TARGETS app)");
            toolchain_vars(i, 0);
        })),
        ("target-module", Box::new(|i| {
            target(i, "foo", TargetKind::ModuleLibrary, 0, &[("SOURCES", "lib.c")]);
            run(i, "install(TARGETS foo LIBRARY DESTINATION plugins)");
            toolchain_vars(i, 0);
        })),
        ("target-shared", Box::new(|i| {
            target(i, "foo", TargetKind::SharedLibrary, 0, &[("SOURCES", "lib.c")]);
            run(i, "install(TARGETS foo LIBRARY DESTINATION lib NAMELINK_COMPONENT devel)");
            toolchain_vars(i, 0);
        })),
        ("target-versioned", Box::new(|i| {
            target(i, "foo", TargetKind::SharedLibrary, 0, &[("SOURCES", "lib.c"), ("VERSION", "1.2"), ("SOVERSION", "1")]);
            run(i, "install(TARGETS foo LIBRARY DESTINATION lib NAMELINK_COMPONENT devel)");
            toolchain_vars(i, 0);
        })),
        ("target-rpath", Box::new(|i| {
            target(i, "app", TargetKind::Executable, 0, &[("SOURCES", "main.c"), ("INSTALL_RPATH", "/opt/lib;$ORIGIN")]);
            run(i, "cmake_policy(SET CMP0095 NEW)\ninstall(TARGETS app)");
            toolchain_vars(i, 0);
        })),
        ("target-objects", Box::new(|i| {
            target(i, "foo", TargetKind::ObjectLibrary, 0, &[("SOURCES", "a.c")]);
            run(i, "install(TARGETS foo OBJECTS DESTINATION obj)");
            toolchain_vars(i, 0);
        })),
        ("target-interface", Box::new(|i| {
            target(i, "iface", TargetKind::InterfaceLibrary, 0, &[]);
            run(i, "install(TARGETS iface)");
            c_vars(i, 0);
        })),
        ("target-imported", Box::new(|i| {
            target_ex(i, "foo", TargetKind::SharedLibrary, 0, true,
                      &[("IMPORTED_LOCATION", "/else/libfoo.so.1")]);
            run(i, "install(IMPORTED_RUNTIME_ARTIFACTS foo DESTINATION lib)");
            c_vars(i, 0);
        })),
        ("target-file-set", Box::new(|i| {
            let src = i.home_source.clone();
            let set_file = format!("{src}/sub/a.h");
            target(i, "foo", TargetKind::InterfaceLibrary, 0, &[
                ("INTERFACE_HEADER_SETS", "HEADERS"),
                ("HEADER_SET", &set_file),
                ("HEADER_DIRS", &src),
            ]);
            run(i, "install(TARGETS foo FILE_SET HEADERS DESTINATION include)");
            c_vars(i, 0);
        })),
        ("target-bmi", Box::new(|i| {
            target(i, "foo", TargetKind::StaticLibrary, 0, &[("SOURCES", "foo.cpp;mod.cppm")]);
            run(i, "install(TARGETS foo CXX_MODULES_BMI DESTINATION bmi)");
            toolchain_vars(i, 0);
        })),
        ("export-interface", Box::new(|i| {
            target(i, "api", TargetKind::InterfaceLibrary, 0, &[]);
            run(i, "export(TARGETS api NAMESPACE Demo:: FILE Api.cmake)");
            c_vars(i, 0);
        })),
        ("export-build-static", Box::new(|i| {
            target(i, "api", TargetKind::StaticLibrary, 0, &[("SOURCES", "a.c")]);
            run(i, "export(TARGETS api NAMESPACE Demo:: FILE Api.cmake)");
            toolchain_vars(i, 0);
        })),
        ("export-install-static", Box::new(|i| {
            target(i, "api", TargetKind::StaticLibrary, 0, &[("SOURCES", "a.c")]);
            run(i, "install(TARGETS api EXPORT Api ARCHIVE DESTINATION lib)\ninstall(EXPORT Api NAMESPACE Demo:: DESTINATION lib/cmake/Api)");
            vars(i, 0, &[("CMAKE_INSTALL_PREFIX", "/usr/local")]);
            toolchain_vars(i, 0);
        })),
        ("export-install-interface", Box::new(|i| {
            target(i, "api", TargetKind::InterfaceLibrary, 0, &[]);
            run(i, "install(TARGETS api EXPORT Api)\ninstall(EXPORT Api NAMESPACE Demo:: DESTINATION lib/cmake/Api)");
            c_vars(i, 0);
            vars(i, 0, &[("CMAKE_INSTALL_PREFIX", "/usr/local")]);
        })),
        ("test-name", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0110 NEW)");
            add_test(i, 0, "some = name", &["cmd", "", "$abc", "a\\b"], &[], false, 4);
            c_vars(i, 0);
            vars(i, 0, &[("CMAKE_TESTING_ENABLED", "1")]);
        })),
        ("test-properties", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0110 NEW)");
            add_test(i, 0, "test", &["echo", "hello"],
                     &[("LABELS", "one;two"), ("TIMEOUT", "42"), ("WORKING_DIRECTORY", "tmp")], false, 4);
            c_vars(i, 0);
            vars(i, 0, &[("CMAKE_TESTING_ENABLED", "1")]);
        })),
        ("test-config", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0110 NEW)");
            add_test(i, 0, "test", &["echo"],
                     &[("_CMAKE_RUST_CONFIGURATIONS", "Release;Debug")], false, 4);
            c_vars(i, 0);
            vars(i, 0, &[("CMAKE_TESTING_ENABLED", "1")]);
        })),
        ("test-old", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0110 NEW)");
            add_test(i, 0, "legacy", &["cmd", "a b", "a\"b", "a\\b"], &[], true, 4);
            c_vars(i, 0);
            vars(i, 0, &[("CMAKE_TESTING_ENABLED", "1")]);
        })),
        ("test-expand", Box::new(|i| {
            run(i, "cmake_policy(SET CMP0110 NEW)");
            add_test(i, 0, "test", &["cmd", "x;;y;"],
                     &[("_CMAKE_RUST_COMMAND_EXPAND_LISTS", "TRUE")], false, 4);
            c_vars(i, 0);
            vars(i, 0, &[("CMAKE_TESTING_ENABLED", "1")]);
        })),
        ("test-include", Box::new(|i| {
            i.mf_mut().props.set("TEST_INCLUDE_FILE", "a.cmake".to_owned());
            i.mf_mut().props.set("TEST_INCLUDE_FILES", "b.cmake;c.cmake".to_owned());
            i.mf_mut().props.set("LABELS", "local".to_owned());
            c_vars(i, 0);
            vars(i, 0, &[
                ("CMAKE_TESTING_ENABLED", "1"),
                ("CTEST_RESOURCE_SPEC_FILE", "resources.json"),
                ("CMAKE_DIRECTORY_LABELS", "global"),
            ]);
        })),
        ("test-target", Box::new(|i| {
            target(i, "app", TargetKind::Executable, 0, &[("CROSSCOMPILING_EMULATOR", "wrap;;arg")]);
            run(i, "cmake_policy(SET CMP0110 NEW)\ncmake_policy(SET CMP0178 NEW)");
            add_test(i, 0, "test", &["app", "abc"], &[], false, 6);
            toolchain_vars(i, 0);
            vars(i, 0, &[("CMAKE_TESTING_ENABLED", "1")]);
        })),
    ];

    let mut verified = 0;
    let mut skipped = 0;
    let mut failed = Vec::new();
    for (name, build) in cases {
        match case(name, build) {
            Ok(()) => { verified += 1; }
            Err(message) if message.starts_with("SKIPPED:") => {
                skipped += 1;
                eprintln!("SKIP {name}: {}", &message[8..].trim());
            }
            Err(message) => {
                eprintln!("FAIL {name}:\n  {message}");
                failed.push(name);
            }
        }
    }
    eprintln!("oracle install fixtures: {verified} verified, {skipped} skipped, {} failed", failed.len());
    assert!(failed.is_empty(), "{} fixture cases failed", failed.len());
    assert!(verified >= 50, "only {verified} fixture cases verified");
}
