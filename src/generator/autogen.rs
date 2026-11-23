//! The target scan of `cmQtAutoGenGlobalInitializer` (AUTOMOC, AUTOUIC and
//! AUTORCC) with `cmQtAutoGenInitializer::GetQtVersion`.
//!
//! Targets whose tools cannot run get CMake's "No valid Qt version found"
//! warning. The AUTOGEN machinery itself (cmQtAutoGenInitializer's
//! targets, rules and the cmake_autogen/cmake_autorcc steps) is not ported:
//! a target that would be initialized gets an explicit error instead of a
//! build that silently skips moc, uic and rcc.

use super::ctx::GenCtx;
use super::target as gt;
use crate::genex::GenexHost;
use crate::state::{Target, TargetKind};
use crate::util::is_on_opt;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct IntegerVersion {
    major: u32,
    minor: u32,
}

/// cmStrToULong for the version variables.
fn to_uint(value: Option<&str>) -> u32 {
    value
        .map(str::trim_start)
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(0, |value| value as u32)
}

/// `parseMocVersion`: the "major.minor" after "moc ".
fn parse_moc_version(output: &str) -> IntegerVersion {
    let Some(pos) = output.find("moc ") else {
        return IntegerVersion::default();
    };
    let mut parts = output[pos + 4..].splitn(3, '.');
    let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
        return IntegerVersion::default();
    };
    // std::stoi: leading digits.
    let leading = |text: &str| {
        let text = text.trim_start();
        let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().unwrap_or(0)
    };
    IntegerVersion {
        major: leading(major),
        minor: leading(minor),
    }
}

/// `GetMocVersion`: `moc --version`.
fn moc_version(executable: &str) -> IntegerVersion {
    match std::process::Command::new(executable)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(output) if output.status.code() == Some(0) => {
            parse_moc_version(&String::from_utf8_lossy(&output.stdout))
        }
        _ => IntegerVersion::default(),
    }
}

/// `cmQtAutoGenInitializer::GetQtVersion`: the version found and the
/// version the target requested through QT_MAJOR_VERSION.
fn qt_version(ctx: &GenCtx, target: &Target, moc_executable: &str) -> (IntegerVersion, u32) {
    let host = super::genex_host::CtxHost::new(ctx, target.makefile);
    let requested = host
        .compatible_interface_property(
            &ctx.target_key(target),
            "QT_MAJOR_VERSION",
            &ctx.config,
            None,
            false,
        )
        .ok()
        .flatten();
    let requested = to_uint(requested.as_deref());
    let keys = [
        ("Qt6Core_VERSION_MAJOR", "Qt6Core_VERSION_MINOR"),
        ("Qt5Core_VERSION_MAJOR", "Qt5Core_VERSION_MINOR"),
        ("QT_VERSION_MAJOR", "QT_VERSION_MINOR"),
    ];
    let mut known = Vec::new();
    for (major, minor) in keys {
        let version = IntegerVersion {
            major: to_uint(ctx.var(target.makefile, major).as_deref()),
            minor: to_uint(ctx.var(target.makefile, minor).as_deref()),
        };
        if version.major != 0 {
            known.push(version);
        }
    }
    let props = &ctx.interp.makefiles[target.makefile].props;
    for (major, minor) in keys {
        let version = IntegerVersion {
            major: to_uint(props.get(major)),
            minor: to_uint(props.get(minor)),
        };
        if version.major != 0 {
            known.push(version);
        }
    }
    let mut found = IntegerVersion::default();
    if !known.is_empty() {
        if requested == 0 {
            found = known[0];
        } else if let Some(version) = known.iter().find(|version| version.major == requested) {
            found = *version;
        }
    }
    if found.major == 0 {
        let mut executable = moc_executable.to_owned();
        if executable.is_empty()
            && let Some(moc) = ctx.target_at(target.makefile, &format!("Qt{requested}::moc"))
        {
            executable = moc
                .props
                .get("IMPORTED_LOCATION")
                .unwrap_or_default()
                .to_owned();
        }
        if !executable.is_empty() {
            found = moc_version(&executable);
        }
    }
    (found, requested)
}

/// `cmQtAutoGen::Tools`.
fn tools(moc: bool, uic: bool, rcc: bool) -> String {
    let list: Vec<&str> = [(moc, "AUTOMOC"), (uic, "AUTOUIC"), (rcc, "AUTORCC")]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
    match list.as_slice() {
        [one] => (*one).to_owned(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        _ => String::new(),
    }
}

/// The target loop of the `cmQtAutoGenGlobalInitializer` constructor
/// (C# targets, which it skips, are not supported by cmake-rust at all).
pub fn initialize(ctx: &GenCtx) {
    for dir in 0..ctx.interp.makefiles.len() {
        for target in ctx.targets_in_order() {
            if target.makefile != dir
                || target.imported
                || target.alias_of.is_some()
                || !matches!(
                    target.kind,
                    TargetKind::Executable
                        | TargetKind::StaticLibrary
                        | TargetKind::SharedLibrary
                        | TargetKind::ModuleLibrary
                        | TargetKind::ObjectLibrary
                )
            {
                continue;
            }
            let on = |name: &str| is_on_opt(target.props.get(name));
            let (moc, uic, rcc) = (on("AUTOMOC"), on("AUTOUIC"), on("AUTORCC"));
            if !(moc || uic || rcc) {
                continue;
            }
            let exec = |name: &str| target.props.get(name).unwrap_or_default().to_owned();
            let (moc_exec, uic_exec, rcc_exec) = (
                exec("AUTOMOC_EXECUTABLE"),
                exec("AUTOUIC_EXECUTABLE"),
                exec("AUTORCC_EXECUTABLE"),
            );
            let (version, requested) = qt_version(ctx, target, &moc_exec);
            let valid = matches!(version.major, 4..=6);
            let moc_valid = moc && (valid || !moc_exec.is_empty());
            let uic_valid = uic && (valid || !uic_exec.is_empty());
            let rcc_valid = rcc && (valid || !rcc_exec.is_empty());
            let (moc_off, uic_off, rcc_off) =
                (moc && !moc_valid, uic && !uic_valid, rcc && !rcc_valid);
            if moc_off || uic_off || rcc_off {
                let version = if requested == 0 {
                    "<QTVERSION>".to_owned()
                } else {
                    requested.to_string()
                };
                let component = if uic_off { "Widgets" } else { "Core" };
                ctx.warning(
                    gt::directory_backtrace(ctx, target.makefile),
                    format!(
                        "AUTOGEN: No valid Qt version found for target {}.  {} disabled.  Consider adding:\n  find_package(Qt{version} COMPONENTS {component})\nto your CMakeLists.txt file.",
                        target.name,
                        tools(moc_off, uic_off, rcc_off)
                    ),
                );
            }
            if moc_valid || uic_valid || rcc_valid {
                ctx.error(
                    target.backtrace.clone(),
                    format!(
                        "{} for target \"{}\" (Qt AUTOGEN) is not supported by cmake-rust.",
                        tools(moc_valid, uic_valid, rcc_valid),
                        target.name
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moc_versions() {
        assert!(
            parse_moc_version("moc 5.15.2\n")
                == IntegerVersion {
                    major: 5,
                    minor: 15
                }
        );
        assert!(parse_moc_version("moc-qt5 5.12.8") == IntegerVersion::default());
        assert!(parse_moc_version("nothing") == IntegerVersion::default());
    }
}
