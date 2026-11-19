use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cmake rust test {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create a fresh fixture directory");
        Self(path)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().expect("fixture file parent"))
            .expect("create fixture directory");
        fs::write(&path, contents).expect("write fixture contents");
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn cmake() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cmake"))
}

fn run(command: &mut Command) -> Output {
    let output = command.output().expect("spawn command");
    assert!(
        output.status.success(),
        "{command:?} exited {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn configure(project: &Path, build: &Path) -> Command {
    let mut command = cmake();
    command
        .arg("-S")
        .arg(project)
        .arg("-B")
        .arg(build)
        .args(["-G", "Ninja"]);
    command
}

#[test]
fn evaluates_cmake_syntax_and_function_scopes() {
    let fixture = Fixture::new();
    let script = fixture.write(
        "syntax.cmake",
        r###"#[=[
message(FATAL_ERROR "bracket comment must not execute")
]=]
set(input "a;b;c")
set(space "x y")
set(name space)
function(check result)
    set(local "hidden")
    set(items "")
    foreach(element IN LISTS input)
        if(NOT (element STREQUAL "b" OR element STREQUAL "x"))
            list(APPEND items "${element}")
        endif()
    endforeach()
    string(REGEX REPLACE "([a-z]);([a-z])" "\\2/\\1" transformed "${items}")
    set(${result} "${transformed}|${${name}}" PARENT_SCOPE)
endfunction()
check(answer)
if(DEFINED local OR NOT answer STREQUAL "c/a|x y")
    message(FATAL_ERROR "Unexpected function result ${answer}")
endif()
set(literal [==[${name} # no interpolation]==])
if(NOT literal STREQUAL [==[${name} # no interpolation]==])
    message(FATAL_ERROR "Bracket syntax was not preserved")
endif()
message(STATUS "syntax and scopes verified")
"###,
    );
    let output = run(cmake().current_dir(&fixture.0).arg("-P").arg(script));
    assert!(String::from_utf8_lossy(&output.stdout).contains("syntax and scopes verified"));
}

#[test]
fn fails_unsupported_commands_with_a_source_location() {
    let fixture = Fixture::new();
    let script = fixture.write("unsupported.cmake", "# first line\nunsupported_probe()\n");
    let output = cmake()
        .current_dir(&fixture.0)
        .arg("-P")
        .arg(script)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported.cmake:2"), "{stderr}");
    assert!(
        stderr.contains("unsupported command unsupported_probe"),
        "{stderr}"
    );
}

#[test]
fn builds_and_explains_transitive_compile_definitions() {
    let fixture = Fixture::new();
    fixture.write(
        "source/CMakeLists.txt",
        r#"cmake_minimum_required(VERSION 3.20)
project(Provenance C)
add_library(requirements INTERFACE)
target_compile_definitions(requirements INTERFACE ENABLE_FEATURE=17)
add_library(middle INTERFACE)
target_link_libraries(middle INTERFACE requirements)
add_subdirectory(app)
"#,
    );
    fixture.write(
        "source/app/CMakeLists.txt",
        "add_executable(example main.c)\ntarget_link_libraries(example PRIVATE middle)\n",
    );
    fixture.write(
        "source/app/main.c",
        "#if ENABLE_FEATURE != 17\n#error missing definition\n#endif\nint main(void) { return 0; }\n",
    );
    let source = fixture.0.join("source");
    let build = fixture.0.join("build");
    let output =
        run(configure(&source, &build).args(["--explain", "example", "-DENABLE_FEATURE=17"]));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("example -> middle -> requirements"),
        "{stdout}"
    );
    assert!(stdout.contains("CMakeLists.txt:4"), "{stdout}");

    // Rust's unit tests also run on hosts without Ninja. The external differential suite
    // requires Ninja and checks actual build and install behavior there.
    if Command::new("ninja")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        run(cmake().arg("--build").arg(&build));
        run(&mut Command::new(build.join("app/example")));
    }
    let compile_commands = fs::read_to_string(build.join("compile_commands.json")).unwrap();
    assert!(compile_commands.contains("-DENABLE_FEATURE=17"));
}

#[test]
fn reconfigure_preserves_cli_values_and_regenerates_after_input_changes() {
    let fixture = Fixture::new();
    let list = fixture.write(
        "source/CMakeLists.txt",
        r#"cmake_minimum_required(VERSION 3.20)
project(Cache C)
configure_file(value.h.in value.h @ONLY)
add_executable(print_value main.c)
target_include_directories(print_value PRIVATE "${CMAKE_CURRENT_BINARY_DIR}")
"#,
    );
    fixture.write("source/value.h.in", "#define VALUE @VALUE@\n");
    fixture.write(
        "source/main.c",
        "#include <stdio.h>\n#include \"value.h\"\nint main(void) { printf(\"%d\\n\", VALUE); }\n",
    );
    let source = fixture.0.join("source");
    let build = fixture.0.join("build");
    run(configure(&source, &build).arg("-DVALUE=17"));
    run(cmake().arg("-B").arg(&build));
    assert_eq!(
        fs::read_to_string(build.join("value.h")).unwrap(),
        "#define VALUE 17\n"
    );
    if Command::new("ninja")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        run(cmake().arg("--build").arg(&build));
        let file = build.join("print_value");
        assert_eq!(run(&mut Command::new(&file)).stdout, b"17\n");

        let previous = fs::read_to_string(&list).unwrap();
        fs::write(
            list,
            previous.replace("configure_file", "set(VALUE 23)\nconfigure_file"),
        )
        .unwrap();
        let rebuilt = run(cmake().arg("--build").arg(&build));
        assert!(String::from_utf8_lossy(&rebuilt.stdout).contains("Rerunning Rust CMake"));
        assert_eq!(run(&mut Command::new(&file)).stdout, b"23\n");
    }
}
