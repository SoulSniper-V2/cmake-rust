use super::{Flags, append, link::selected_linker, shell, shell_with_make_vars};
use crate::generator::ctx::GenCtx;
use crate::state::Target;
use crate::util::expand_list;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleStep {
    Compile,
    Link,
}

/// Ninja's per-object LAUNCHER binding, including the required trailing space.
pub fn compiler_launcher(ctx: &GenCtx, target: &Target, lang: &str) -> String {
    if !matches!(
        lang,
        "C" | "CXX" | "Fortran" | "CUDA" | "HIP" | "ISPC" | "OBJC" | "OBJCXX"
    ) {
        return String::new();
    }
    let inputs = Flags {
        ctx,
        target,
        lang,
        config: &ctx.config,
    };
    let mut launcher = launcher(&inputs, "COMPILER_LAUNCHER");
    if !launcher.is_empty() {
        launcher.push(' ');
    }
    launcher
}

fn launcher(inputs: &Flags<'_, '_>, suffix: &str) -> String {
    let name = format!("{}_{suffix}", inputs.lang);
    let Some(raw) = inputs.prop(&name).filter(|raw| !raw.is_empty()) else {
        return String::new();
    };
    let evaluated = inputs.eval(raw);
    if suffix == "COMPILER_LAUNCHER" && evaluated.is_empty() {
        return String::new();
    }
    let mut args = expand_list(&evaluated, true).into_iter();
    let Some(exe) = args.next() else {
        return String::new();
    };
    let mut command = shell_with_make_vars(&exe);
    append(
        &mut command,
        &args.map(|arg| shell(&arg)).collect::<Vec<_>>().join(" "),
    );
    command
}

/// Expand a compiler or linker rule with the supplied Ninja placeholders.
/// Call expand_rule_for_step when the language or build step is known.
pub fn expand_rule(
    ctx: &GenCtx,
    target: &Target,
    template: &str,
    vars: &[(&str, String)],
) -> String {
    let (step, lang) = if lookup(vars, "LINK_FLAGS").is_some()
        || lookup(vars, "LINK_LIBRARIES").is_some()
        || lookup(vars, "OBJECTS").is_some()
    {
        (
            RuleStep::Link,
            lookup(vars, "LANGUAGE").or_else(|| target.props.get("LINKER_LANGUAGE")),
        )
    } else {
        (RuleStep::Compile, lookup(vars, "LANGUAGE"))
    };
    let lang = lang
        .or_else(|| {
            ctx.interp
                .langs
                .enabled
                .iter()
                .find(|lang| template.contains(&format!("<CMAKE_{lang}_COMPILER>")))
                .map(String::as_str)
        })
        .or_else(|| ctx.interp.langs.enabled.first().map(String::as_str))
        .unwrap_or_default();
    expand_rule_for_step(ctx, target, lang, step, template, vars)
}

pub fn expand_rule_for_step(
    ctx: &GenCtx,
    target: &Target,
    lang: &str,
    step: RuleStep,
    template: &str,
    vars: &[(&str, String)],
) -> String {
    let inputs = Flags {
        ctx,
        target,
        lang,
        config: &ctx.config,
    };
    expand_placeholders(template, |name| replacement(&inputs, step, name, vars))
}

fn lookup<'a>(vars: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    vars.iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.as_str())
}

fn replacement(
    inputs: &Flags<'_, '_>,
    step: RuleStep,
    name: &str,
    vars: &[(&str, String)],
) -> String {
    if let Some(value) = lookup(vars, name) {
        return value.to_owned();
    }
    if let Some(target) = lookup(vars, "TARGET") {
        match name {
            "TARGET_QUOTED" if !target.is_empty() && !target.starts_with('"') => {
                return format!("\"{target}\"");
            }
            "TARGET_QUOTED" => return target.to_owned(),
            "TARGET_UNQUOTED" => {
                return target
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .filter(|s| !s.is_empty())
                    .unwrap_or(target)
                    .to_owned();
            }
            "TARGET_BASE" => {
                return target
                    .rsplit_once('.')
                    .map_or(target, |(base, _)| base)
                    .to_owned();
            }
            "TARGET_VERSION_MAJOR" | "TARGET_VERSION_MINOR" => return "0".to_owned(),
            "TARGET_IMPLIB" => return "$TARGET_IMPLIB".to_owned(),
            _ => {}
        }
    }

    match name {
        "CMAKE_COMMAND" => return shell_with_make_vars(&inputs.ctx.interp.cmake_command),
        "CMAKE_LINKER" if step == RuleStep::Link => {
            // cmGeneratorTarget::GetLinkerTool.
            let linker = if inputs.lang_var("LINK_MODE").as_deref() == Some("LINKER") {
                selected_linker(inputs)
            } else {
                None
            }
            .or_else(|| inputs.var(name))
            .unwrap_or_default();
            // cmStrCat(Launcher, ' ', result) always inserts the separator.
            let launcher = launcher(inputs, "LINKER_LAUNCHER");
            let escaped = shell_with_make_vars(&linker);
            return if launcher.is_empty() {
                escaped
            } else {
                format!("{launcher} {escaped}")
            };
        }
        "CONFIG"
        | "ROLE"
        | "TARGET_LABELS"
        | "TARGET_SONAME"
        | "SONAME_FLAG"
        | "TARGET_INSTALLNAME_DIR" => return String::new(),
        "CMAKE_OSX_SYSROOT" => {
            let value = inputs
                .var(name)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "/".to_owned());
            return shell_with_make_vars(&shell_with_make_vars(&value));
        }
        _ => {}
    }

    let languages = inputs
        .ctx
        .interp
        .langs
        .enabled
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(inputs.lang))
        .filter(|lang| !lang.is_empty() && *lang != "NONE");
    for language in languages {
        if name == format!("CMAKE_{language}_COMPILER") {
            return compiler(
                &Flags {
                    lang: language,
                    ..*inputs
                },
                step,
                language == inputs.lang,
            );
        }
        if [
            format!("CMAKE_SHARED_LIBRARY_CREATE_{language}_FLAGS"),
            format!("CMAKE_SHARED_MODULE_CREATE_{language}_FLAGS"),
            format!("CMAKE_{language}_LINK_FLAGS"),
        ]
        .iter()
        .any(|key| name == key)
        {
            return String::new();
        }
        if [
            format!("CMAKE_SHARED_MODULE_{language}_FLAGS"),
            format!("CMAKE_SHARED_LIBRARY_{language}_FLAGS"),
            format!("CMAKE_SHARED_LIBRARY_SONAME_{language}_FLAG"),
            format!("CMAKE_{language}_ARCHIVE"),
            format!("CMAKE_{language}_COMPILER_ARG1"),
            format!("CMAKE_{language}_COMPILER_TARGET"),
            format!("CMAKE_{language}_COMPILER_EXTERNAL_TOOLCHAIN"),
            format!("CMAKE_{language}_COMPILE_OPTIONS_TARGET"),
            format!("CMAKE_{language}_COMPILE_OPTIONS_EXTERNAL_TOOLCHAIN"),
            format!("CMAKE_{language}_COMPILE_OPTIONS_SYSROOT"),
        ]
        .iter()
        .any(|key| name == key)
        {
            return mapped_variable(inputs, name);
        }
    }
    if matches!(
        name,
        "CMAKE_AR"
            | "CMAKE_CURRENT_SOURCE_DIR"
            | "CMAKE_CURRENT_BINARY_DIR"
            | "CMAKE_RANLIB"
            | "CMAKE_MT"
            | "CMAKE_TAPI"
            | "CMAKE_CUDA_HOST_COMPILER"
            | "CMAKE_CUDA_HOST_LINK_LAUNCHER"
            | "CMAKE_HIP_HOST_COMPILER"
            | "CMAKE_HIP_HOST_LINK_LAUNCHER"
            | "CMAKE_CL_SHOWINCLUDES_PREFIX"
    ) {
        return mapped_variable(inputs, name);
    }

    // An unavailable rule placeholder loses its angle brackets.
    name.to_owned()
}

fn mapped_variable(inputs: &Flags<'_, '_>, name: &str) -> String {
    let dir = &inputs.ctx.interp.makefiles[inputs.target.makefile];
    let value = inputs.var(name).unwrap_or_else(|| match name {
        "CMAKE_CURRENT_SOURCE_DIR" => dir.source_dir.clone(),
        "CMAKE_CURRENT_BINARY_DIR" => dir.binary_dir.clone(),
        _ => String::new(),
    });
    if name.contains("_FLAG") {
        value
    } else {
        shell_with_make_vars(&value)
    }
}

fn compiler(inputs: &Flags<'_, '_>, step: RuleStep, current_language: bool) -> String {
    let path = shell_with_make_vars(&inputs.lang_var("COMPILER").unwrap_or_default());
    // cmStrCat(Launcher, ' ', ret) always inserts the separator.
    let mut command = if step == RuleStep::Link && current_language {
        let launcher = launcher(inputs, "LINKER_LAUNCHER");
        if launcher.is_empty() {
            path
        } else {
            format!("{launcher} {path}")
        }
    } else {
        path
    };
    if let Some(arg) = inputs
        .lang_var("COMPILER_ARG1")
        .filter(|arg| !arg.is_empty())
    {
        command.push(' ');
        command.push_str(&arg);
    }

    let sysroot_name = if step == RuleStep::Compile {
        "CMAKE_SYSROOT_COMPILE"
    } else {
        "CMAKE_SYSROOT_LINK"
    };
    let sysroot = inputs
        .var(sysroot_name)
        .or_else(|| inputs.var("CMAKE_SYSROOT"));
    for (option, value, escape) in [
        ("TARGET", inputs.lang_var("COMPILER_TARGET"), false),
        (
            "EXTERNAL_TOOLCHAIN",
            inputs.lang_var("COMPILER_EXTERNAL_TOOLCHAIN"),
            true,
        ),
        ("SYSROOT", sysroot, true),
    ] {
        if let (Some(prefix), Some(value)) = (
            inputs
                .lang_var(&format!("COMPILE_OPTIONS_{option}"))
                .filter(|s| !s.is_empty()),
            value.filter(|s| !s.is_empty()),
        ) {
            command.push(' ');
            command.push_str(&prefix);
            if escape {
                command.push_str(&shell_with_make_vars(&value));
            } else {
                command.push_str(&value);
            }
        }
    }
    command
}

fn expand_placeholders(template: &str, mut replacement: impl FnMut(&str) -> String) -> String {
    let Some(mut start) = template.find('<') else {
        return template.to_owned();
    };
    let mut result = String::new();
    let mut pos = 0;
    // C++ compares `start < s.size() - 2` in size_t, which underflows for
    // inputs shorter than two characters and still enters the loop.
    while start + 2 < template.len() || template.len() < 2 {
        let Some(offset) = template[start..].find('>') else {
            return result;
        };
        let end = start + offset;
        let next = if template.as_bytes()[start + 1].is_ascii_alphabetic() {
            let value = replacement(&template[start + 1..end]);
            result.push_str(&template[pos..start]);
            if value.is_empty()
                && start > 0
                && template.as_bytes()[start - 1] == b' '
                && template.as_bytes().get(end + 1) == Some(&b' ')
            {
                result.pop();
            }
            result.push_str(&value);
            pos = end + 1;
            pos
        } else {
            start + 1
        };
        let Some(offset) = template[next..].find('<') else {
            break;
        };
        start = next + offset;
    }
    result.push_str(&template[pos..]);
    if result.ends_with(' ') {
        result.pop();
    }
    result
}
