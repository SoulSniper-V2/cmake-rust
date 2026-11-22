//! Single-configuration CTestTestfile.cmake generation.
use std::fmt::Write;
use std::path::Path;

use super::ctx::GenCtx;
use super::install::{config_test, directory_policy};
use super::target::{ArtifactType, full_path};
use crate::commands::install::unix_slashes;
use crate::policy::PolicyStatus;
use crate::state::{TargetKind, Test};
use crate::util::{escape_for_cmake, expand_list, is_on_opt};

pub fn write_test_files(ctx: &GenCtx) -> std::io::Result<()> {
    for (index, mf) in ctx.interp.makefiles.iter().enumerate() {
        if is_on_opt(ctx.var(index, "CMAKE_TESTING_ENABLED").as_deref()) {
            let script = test_script(ctx, index);
            // cmGeneratedFileStream without SetCopyIfDifferent.
            crate::fsutil::write_generated(
                &Path::new(&mf.binary_dir).join("CTestTestfile.cmake"),
                script.as_bytes(),
            )?;
        }
    }
    Ok(())
}

pub fn test_script(ctx: &GenCtx, index: usize) -> String {
    let mf = &ctx.interp.makefiles[index];
    let mut out = format!(
        "# CMake generated Testfile for \n# Source directory: {}\n# Build directory: {}\n# \n\
         # This file includes the relevant testing commands required for \n\
         # testing this directory and lists subdirectories to be tested as well.\n",
        mf.source_dir, mf.binary_dir
    );
    if let Some(path) = ctx
        .var(index, "CTEST_RESOURCE_SPEC_FILE")
        .filter(|s| !s.is_empty())
    {
        writeln!(out, "set(CTEST_RESOURCE_SPEC_FILE \"{path}\")").unwrap();
    }
    if let Some(path) = mf.props.get("TEST_INCLUDE_FILE") {
        writeln!(out, "include(\"{path}\")").unwrap();
    }
    for path in expand_list(mf.props.get("TEST_INCLUDE_FILES").unwrap_or(""), false) {
        writeln!(out, "include(\"{path}\")").unwrap();
    }
    for test in ctx
        .interp
        .ext
        .tests
        .iter()
        .filter(|test| test.makefile == index)
    {
        write_test(ctx, &mut out, index, test);
    }
    for &child in &mf.children {
        let child_dir = &ctx.interp.makefiles[child].binary_dir;
        let relative = child_dir
            .strip_prefix(&format!("{}/", mf.binary_dir))
            .unwrap_or(child_dir);
        writeln!(out, "subdirs({})", escape_for_cmake(relative)).unwrap();
    }
    let labels = [
        mf.props.get("LABELS").map(str::to_owned),
        ctx.var(index, "CMAKE_DIRECTORY_LABELS"),
    ];
    let labels: Vec<_> = labels
        .into_iter()
        .flatten()
        .map(|label| escape_for_cmake(&label))
        .collect();
    if !labels.is_empty() {
        writeln!(
            out,
            "set_directory_properties(PROPERTIES LABELS {})",
            labels.join(";")
        )
        .unwrap();
    }
    out
}

fn quote_name(name: &str) -> String {
    let n = name.split(|c| c != '=').map(str::len).max().unwrap_or(0);
    let sign = "=".repeat(n + 1);
    format!("[{sign}[{name}]{sign}]")
}

fn write_test(ctx: &GenCtx, out: &mut String, index: usize, test: &Test) {
    let configs = &test.configurations;
    let indent = if configs.is_empty() { "" } else { "  " };
    if !configs.is_empty() {
        writeln!(
            out,
            "if({})",
            config_test("CTEST_CONFIGURATION_TYPE", configs)
        )
        .unwrap();
    }
    let name = if directory_policy(ctx, index, "CMP0110") == PolicyStatus::New {
        quote_name(&test.name)
    } else {
        test.name.clone()
    };
    write!(out, "{indent}add_test({name} ").unwrap();
    if test.old_style {
        if let Some((exe, args)) = test.command.split_first() {
            write!(out, "\"{}\"", unix_slashes(exe)).unwrap();
            for arg in args {
                write!(out, " \"{}\"", arg.replace('"', "\\\"")).unwrap();
            }
        }
    } else {
        let expand = test.command_expand_lists;
        let mut args = Vec::new();
        for arg in &test.command {
            // cmGeneratorExpression::Evaluate(LG, config) in the test's directory.
            let value = ctx.genex_full(arg, test.makefile, None, None, false, &test.backtrace);
            if expand {
                args.extend(expand_list(&value, true));
            } else {
                args.push(value);
            }
        }
        let exe = args.first().map_or("", String::as_str);
        let exe = if let Some(target) = ctx
            .target(exe)
            .filter(|target| target.kind == TargetKind::Executable)
        {
            let keep_empty = test.props.get("_CMAKE_RUST_CMP0178").map_or_else(
                || directory_policy(ctx, index, "CMP0178") == PolicyStatus::New,
                |policy| policy == "NEW",
            );
            let emulator = test.props.get("_CMAKE_RUST_CMP0158").map_or_else(
                || directory_policy(ctx, index, "CMP0158") != PolicyStatus::New,
                |policy| policy != "NEW",
            ) || is_on_opt(ctx.var(index, "CMAKE_CROSSCOMPILING").as_deref());
            let properties = if emulator {
                &["TEST_LAUNCHER", "CROSSCOMPILING_EMULATOR"][..]
            } else {
                &["TEST_LAUNCHER"][..]
            };
            for key in properties {
                if let Some(value) = target.props.get(key).filter(|s| !s.is_empty()) {
                    // INTEGRATION: launcher arguments may have generator expressions.
                    let values = expand_list(&ctx.genex(value, None, None), keep_empty);
                    if let Some((launcher, args)) = values
                        .split_first()
                        .filter(|(launcher, _)| !launcher.is_empty())
                    {
                        write!(out, "{} ", escape_for_cmake(&unix_slashes(launcher))).unwrap();
                        for arg in args {
                            write!(out, "{} ", escape_for_cmake(arg)).unwrap();
                        }
                    }
                }
            }
            full_path(ctx, target, &ctx.config, ArtifactType::Runtime, false)
        } else {
            unix_slashes(exe)
        };
        out.push_str(&escape_for_cmake(&exe));
        for arg in args.iter().skip(1) {
            write!(out, " {}", escape_for_cmake(arg)).unwrap();
        }
    }
    writeln!(out, ")").unwrap();
    write!(out, "{indent}set_tests_properties({name} PROPERTIES ").unwrap();
    for (key, value) in test
        .props
        .iter()
        .filter(|(key, _)| !key.starts_with("_CMAKE_RUST_"))
    {
        // INTEGRATION: modern add_test properties evaluate generator expressions.
        let value = if test.old_style {
            value.clone()
        } else {
            ctx.genex(value, None, None)
        };
        write!(out, " {key} {}", escape_for_cmake(&value)).unwrap();
    }
    if !test.backtrace.is_empty() {
        let mut frames = Vec::new();
        for frame in test.backtrace.iter().rev() {
            frames.push(format!(
                "{};{};{}",
                frame.file,
                frame.line,
                frame.name.as_deref().unwrap_or("")
            ));
            if frame.line == 0
                && frame.file.as_ref()
                    == format!("{}/CMakeLists.txt", ctx.interp.makefiles[index].source_dir)
            {
                break;
            }
        }
        write!(out, " _BACKTRACE_TRIPLES \"{}\"", frames.join(";")).unwrap();
    }
    writeln!(out, ")").unwrap();
    if !configs.is_empty() {
        writeln!(out, "endif()").unwrap();
    }
}
