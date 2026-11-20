//! In-process differential checks of the configure-time target model against
//! oracle captures.
//!
//! Each case under `tests/projects/targets/<case>/` carries
//! `expected-configure.stderr`, the stderr of Kitware CMake 4.1.0 configuring
//! that project (`project(x NONE)`, Ninja), with the source and binary
//! directories replaced by `<SOURCE_DIR>` / `<BINARY_DIR>`.  Until the
//! system-detection and property commands land, the case body is run here in a
//! project-mode interpreter with the `project()` line blanked (line numbers are
//! preserved).  Minimal test-only stand-ins for the property query commands
//! call the public model API (`read_target_property`, `write_target_property`,
//! `source_prop`), which is exactly what the real commands will use.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::interp::{CmdResult, Interp, Mode, err};
use crate::state::{
    PropertyWrite, find_target_to_use, read_target_property, source_prop, write_target_property,
};

fn arity() -> CmdResult {
    err("called with incorrect number of arguments")
}

fn get_target_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [var, target, prop] = args else {
        return arity();
    };
    if find_target_to_use(interp, target).is_none() {
        return err(format!(
            "could not find target \"{target}\" (test stand-in)"
        ));
    }
    let value = read_target_property(interp, target, prop);
    interp.set_def(
        var.as_str(),
        value.unwrap_or_else(|| format!("{var}-NOTFOUND")),
    );
    Ok(())
}

fn get_source_file_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    let [var, file, prop] = args else {
        return arity();
    };
    let dir = interp.mf_index();
    let value = source_prop(interp, dir, file, prop)
        .unwrap_or("NOTFOUND")
        .to_owned();
    interp.set_def(var.as_str(), value);
    Ok(())
}

fn get_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    let (var, scope, name, prop) = match args {
        [var, scope, keyword, prop] if keyword == "PROPERTY" => (var, scope, None, prop),
        [var, scope, name, keyword, prop] if keyword == "PROPERTY" => {
            (var, scope, Some(name), prop)
        }
        _ => return err("form not supported by the test stand-in"),
    };
    let dir = interp.mf_index();
    let value = match (scope.as_str(), name) {
        ("TARGET", Some(name)) => read_target_property(interp, name, prop).unwrap_or_default(),
        ("DIRECTORY", None) if prop == "TESTS" => {
            let mut names: Vec<&str> = interp
                .ext
                .tests
                .iter()
                .filter(|test| test.makefile == dir)
                .map(|test| test.name.as_str())
                .collect();
            names.sort_unstable();
            names.dedup();
            names.join(";")
        }
        ("DIRECTORY", None) => interp.mf().props.get(prop).unwrap_or_default().to_owned(),
        ("TEST", Some(name)) => interp
            .ext
            .tests
            .iter()
            .rev()
            .find(|test| test.makefile == dir && test.name == *name)
            .and_then(|test| test.props.get(prop))
            .unwrap_or_default()
            .to_owned(),
        _ => return err("scope not supported by the test stand-in"),
    };
    interp.set_def(var.as_str(), value);
    Ok(())
}

// The fixtures use plain message() results and FATAL_ERROR assertions.
fn message(interp: &mut Interp, args: &[String]) -> CmdResult {
    if let Some((mode, content)) = args.split_first() {
        if mode == "FATAL_ERROR" {
            return super::report(interp, &content.concat());
        }
        interp.stderr_line(&args.concat());
    }
    Ok(())
}

fn set_property(interp: &mut Interp, args: &[String]) -> CmdResult {
    let Some(at) = args.iter().position(|arg| arg == "PROPERTY") else {
        return err("form not supported by the test stand-in");
    };
    let (Some(scope), Some(prop)) = (args.first(), args.get(at + 1)) else {
        return arity();
    };
    let value = args[at + 2..].join(";");
    match scope.as_str() {
        "TARGET" => {
            for target in &args[1..at] {
                write_target_property(interp, target, prop, Some(&value), PropertyWrite::Set)?;
            }
            Ok(())
        }
        "GLOBAL" => Ok(()),
        _ => err("scope not supported by the test stand-in"),
    }
}

/// Test-only output-genex expander: `$<1:...>` and `$<CONFIG>` (empty for
/// single-config Ninja with no CMAKE_BUILD_TYPE); anything else fails.
fn test_genex(
    interp: &Interp,
    dir: usize,
    output: &str,
    _: &[crate::messages::Frame],
) -> Result<Vec<String>, String> {
    let mut text = output.replace("$<CONFIG>", "");
    while let Some(start) = text.find("$<1:") {
        let end = start + text[start..].find('>').ok_or("bad genex")?;
        text = format!(
            "{}{}{}",
            &text[..start],
            &text[start + 4..end],
            &text[end + 1..]
        );
    }
    if text.contains("$<") {
        return Err(format!("test expander cannot evaluate {output}"));
    }
    Ok(vec![crate::util::collapse_full_path(
        &text,
        &interp.makefiles[dir].binary_dir,
    )])
}

struct Case {
    root: PathBuf,
}

impl Case {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cmake-rust-target-cases-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let case = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/projects/targets")
            .join(name);
        copy_tree(&case, &root.join("src"));
        std::fs::create_dir_all(root.join("build")).unwrap();
        Self { root }
    }

    fn source(&self) -> String {
        self.root.join("src").to_str().unwrap().to_owned()
    }

    fn binary(&self) -> String {
        self.root.join("build").to_str().unwrap().to_owned()
    }

    /// Configures the case and returns (normalized stderr, error flag).
    fn configure(&self) -> (String, bool) {
        let source = self.source();
        let binary = self.binary();
        let list = format!("{source}/CMakeLists.txt");
        let text = std::fs::read_to_string(&list).unwrap();
        let blanked: Vec<&str> = text
            .split('\n')
            .map(|line| {
                if line.starts_with("project(") {
                    ""
                } else {
                    line
                }
            })
            .collect();
        std::fs::write(&list, blanked.join("\n")).unwrap();

        let mut interp = Interp::new(Mode::Project, &source, &binary, "");
        for (name, command) in [
            (
                "get_target_property",
                get_target_property as crate::interp::Builtin,
            ),
            ("get_source_file_property", get_source_file_property),
            ("get_property", get_property),
            ("set_property", set_property),
            ("message", message),
        ] {
            interp.register(name, command);
        }
        interp.stderr_capture = Some(String::new());
        for (name, value) in [
            ("CMAKE_SOURCE_DIR", &source),
            ("CMAKE_CURRENT_SOURCE_DIR", &source),
            ("CMAKE_BINARY_DIR", &binary),
            ("CMAKE_CURRENT_BINARY_DIR", &binary),
        ] {
            interp.set_def(name, value.as_str());
        }
        interp.set_def("CMAKE_COMMAND", "/cmake");
        interp.read_list_file_directory(&list);
        interp.record_directory_vars();
        if !interp.error && !interp.fatal {
            // cmake::Generate runs only after a successful configure.
            crate::commands::custom_actions::run_generator_actions(&mut interp, &mut test_genex);
        }
        let output = interp.stderr_capture.take().unwrap();
        let output = output
            .replace(&binary, "<BINARY_DIR>")
            .replace(&source, "<SOURCE_DIR>")
            .replace(self.root.to_str().unwrap(), "<WORK_DIR>");
        (output, interp.error)
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn check(name: &str) -> Option<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/projects/targets")
        .join(name);
    let expected = std::fs::read_to_string(dir.join("expected-configure.stderr")).ok()?;
    // The CLI reports its final status after the model has issued diagnostics.
    let expected = expected.replace(
        "CMake Generate step failed.  Build files cannot be regenerated correctly.\n",
        "",
    );
    let expect_error = std::fs::read_to_string(dir.join("expected-configure.exit"))
        .is_ok_and(|exit| exit.trim() != "0");
    let case = Case::new(name);
    let (actual, error) = case.configure();
    // Diagnostics are word-wrapped after the real paths are substituted, and
    // the temporary tree's path length differs from the capture's, so compare
    // with whitespace runs (where wrapping happens) collapsed.
    let unwrap = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    if unwrap(&actual) == unwrap(&expected) && error == expect_error {
        return None;
    }
    let diff = expected
        .lines()
        .zip(actual.lines().chain(std::iter::repeat("<missing>")))
        .enumerate()
        .find(|(_, (want, got))| want.split_whitespace().ne(got.split_whitespace()))
        .map(|(line, (want, got))| format!("line {}:\n  want: {want}\n  got:  {got}", line + 1))
        .unwrap_or_else(|| "difference after the expected output ends".to_owned());
    Some(format!(
        "case {name} (error want {expect_error}, got {error}):\n{diff}\n--- actual ---\n{actual}"
    ))
}

#[test]
fn oracle_cases() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/projects/targets");
    let mut names: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("expected-configure.stderr").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let failures: Vec<String> = names.iter().filter_map(|name| check(name)).collect();
    assert!(
        failures.is_empty(),
        "{} of {} oracle cases differ:\n\n{}",
        failures.len(),
        names.len(),
        failures.join("\n\n")
    );
}
