use super::{Flags, append_options};
use crate::generator::ctx::GenCtx;
use crate::messages::MessageType;
use crate::policy::PolicyStatus;
use crate::state::Target;
use crate::util::{expand_list, is_on_opt, strtol};

pub(crate) fn levels(language: &str) -> &'static [&'static str] {
    match language {
        "C" | "OBJC" => &["90", "99", "11", "17", "23"],
        "CXX" | "OBJCXX" | "HIP" => &["98", "11", "14", "17", "20", "23", "26"],
        "CUDA" => &["03", "11", "14", "17", "20", "23", "26"],
        _ => &[],
    }
}

pub(crate) fn level(standards: &[&str], value: &str) -> Option<usize> {
    let value = value.trim_start();
    // std::stoi accepts a decimal prefix.
    if !value
        .trim_start_matches(['+', '-'])
        .starts_with(|c: char| c.is_ascii_digit())
    {
        return None;
    }
    let number = strtol(value, 10).0;
    standards
        .iter()
        .position(|standard| strtol(standard, 10).0 == number)
}

/// `cmGeneratorTarget::GetExplicitStandardLevel`: the highest level among the
/// `<LANG>_STANDARD` property and the compile features.
pub(in crate::generator) fn explicit_level(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    lang: &str,
) -> Option<usize> {
    let inputs = Flags {
        ctx,
        target,
        config,
        lang,
    };
    let standards = levels(lang);
    let property = inputs
        .prop(&format!("{lang}_STANDARD"))
        .and_then(|value| level(standards, value));
    property.max(feature_level(&inputs, lang))
}

/// The highest level of `language` that the compile features require.
fn feature_level(inputs: &Flags<'_, '_>, language: &str) -> Option<usize> {
    let features = inputs.usage("COMPILE_FEATURES");
    if features.is_empty() {
        return None;
    }
    levels(language).iter().rposition(|standard| {
        let items = inputs
            .var(&format!("CMAKE_{language}{standard}_COMPILE_FEATURES"))
            .unwrap_or_default();
        expand_list(&items, false)
            .iter()
            .any(|item| features.contains(item))
    })
}

/// `cmStandardLevelResolver::GetEffectiveStandard`.
pub(in crate::generator) fn effective_standard(
    ctx: &GenCtx,
    target: &Target,
    config: &str,
    lang: &str,
) -> Option<String> {
    let inputs = &Flags {
        ctx,
        target,
        config,
        lang,
    };
    let standards = levels(lang);
    let default = inputs
        .lang_var("STANDARD_DEFAULT")
        .filter(|value| !value.is_empty())?;
    let new = ctx.interp.policy_status_at(target.makefile, "CMP0128") == PolicyStatus::New;
    let default_ext = is_on_opt(inputs.lang_var("EXTENSIONS_DEFAULT").as_deref());
    let extensions =
        paired_prop(inputs, "EXTENSIONS").map_or(!new || default_ext, crate::util::is_on);
    let Some(standard) = requested_standard(inputs, &default) else {
        let reported = if new {
            extensions != default_ext
        } else {
            extensions
        };
        return reported.then_some(default);
    };
    if is_on_opt(paired_prop(inputs, "STANDARD_REQUIRED")) {
        return Some(standard);
    }
    if new && standard == default && extensions == default_ext {
        return None;
    }
    let standard = if lang == "CUDA" && standard == "98" {
        "03"
    } else {
        &standard
    };
    let requested = level(standards, standard)?;
    let default_level = level(standards, &default)?;
    if (new && (requested < default_level || extensions != default_ext))
        || (!new && requested <= default_level)
    {
        return Some(standards[requested].to_owned());
    }
    let kind = if extensions { "EXTENSION" } else { "STANDARD" };
    (default_level + 1..=requested)
        .rev()
        .map(|index| standards[index])
        .find(|standard| {
            inputs
                .var(&format!("CMAKE_{lang}{standard}_{kind}_COMPILE_OPTION"))
                .is_some()
        })
        .map(str::to_owned)
}

fn paired_language(lang: &str) -> Option<&str> {
    match lang {
        "OBJC" => Some("C"),
        "OBJCXX" | "CUDA" | "HIP" => Some("CXX"),
        _ => None,
    }
}

fn paired_prop<'a>(inputs: &'a Flags<'_, '_>, suffix: &str) -> Option<&'a str> {
    inputs
        .prop(&format!("{}_{suffix}", inputs.lang))
        .or_else(|| inputs.prop(&format!("{}_{suffix}", paired_language(inputs.lang)?)))
}

fn requested_standard(inputs: &Flags<'_, '_>, default: &str) -> Option<String> {
    let raise = |language: &str, default: &str| {
        let current = inputs.prop(&format!("{language}_STANDARD"));
        let standards = levels(language);
        let highest = feature_level(inputs, language);
        if let Some(highest) = highest
            && level(standards, current.unwrap_or(default)).is_none_or(|idx| idx < highest)
        {
            return Some(standards[highest].to_owned());
        }
        current.map(str::to_owned)
    };
    raise(inputs.lang, default).or_else(|| {
        // The paired-language pass runs after feature-driven standards have
        // been resolved, even when the paired language was not enabled.
        let paired = paired_language(inputs.lang)?;
        let paired_default = inputs.var(&format!("CMAKE_{paired}_STANDARD_DEFAULT"));
        let inherited =
            raise(paired, paired_default.as_deref().unwrap_or_default()).or(paired_default)?;
        if inputs.lang == "CUDA" && inherited == "98" {
            Some("03".to_owned())
        } else {
            Some(inherited)
        }
    })
}

pub(super) fn append_standard(inputs: &Flags<'_, '_>, flags: &mut String) {
    if let Some(option) = compile_option(inputs) {
        append_options(flags, expand_list(&option, false));
    }
}

pub(super) fn compile_option(inputs: &Flags<'_, '_>) -> Option<String> {
    let lang = inputs.lang;
    let standards = levels(lang);
    let default = inputs
        .lang_var("STANDARD_DEFAULT")
        .filter(|v| !v.is_empty())?;
    if standards.is_empty() {
        return None;
    }
    let policy = inputs
        .ctx
        .interp
        .policy_status_at(inputs.target.makefile, "CMP0128");
    let new = policy == PolicyStatus::New;
    let warn = policy == PolicyStatus::Warn
        && inputs
            .ctx
            .interp
            .policy_optional_warning(inputs.var("CMAKE_POLICY_WARNING_CMP0128").as_deref());
    let default_ext = is_on_opt(inputs.lang_var("EXTENSIONS_DEFAULT").as_deref());
    let extensions =
        paired_prop(inputs, "EXTENSIONS").map_or(!new || default_ext, crate::util::is_on);
    let kind = if extensions { "EXTENSION" } else { "STANDARD" };
    let option = |standard| inputs.var(&format!("CMAKE_{lang}{standard}_{kind}_COMPILE_OPTION"));

    let Some(standard) = requested_standard(inputs, &default) else {
        if warn && extensions != default_ext {
            let state = if !extensions {
                Some("disabled")
            } else if inputs.lang_var("EXTENSION_COMPILE_OPTION").is_none() {
                Some("enabled")
            } else {
                None
            };
            if let Some(state) = state {
                warn_cmp0128(
                    inputs,
                    &format!(
                        "For compatibility with older versions of CMake, compiler extensions won't be {state}."
                    ),
                );
            }
        }
        return if new && extensions != default_ext {
            option(default.as_str())
        } else if !new && extensions {
            inputs.lang_var("EXTENSION_COMPILE_OPTION")
        } else {
            None
        };
    };

    if is_on_opt(paired_prop(inputs, "STANDARD_REQUIRED")) {
        let result = option(standard.as_str());
        if result.is_none() {
            let extensions = if extensions {
                "(with compiler extensions)"
            } else {
                ""
            };
            let id = inputs.lang_var("COMPILER_ID").unwrap_or_default();
            inputs.error(format!(
                "Target \"{}\" requires the language dialect \"{lang}{standard}\" {extensions}. But the current compiler \"{id}\" does not support this, or CMake does not know the flags to enable it.",
                inputs.target.name
            ));
        }
        return result;
    }

    if standard == default && extensions == default_ext {
        if new {
            return None;
        }
        if warn {
            warn_cmp0128(
                inputs,
                "For compatibility with older versions of CMake, unnecessary flags for language standard or compiler extensions may be added.",
            );
        }
    }
    let standard = if lang == "CUDA" && standard == "98" {
        "03"
    } else {
        &standard
    };
    let Some(requested) = level(standards, standard) else {
        inputs.ctx.error(
            inputs.target.backtrace.clone(),
            format!("{lang}_STANDARD is set to invalid value '{standard}'"),
        );
        return None;
    };
    let Some(default_level) = level(standards, &default) else {
        inputs.error(format!(
            "CMAKE_{lang}_STANDARD_DEFAULT is set to invalid value '{default}'"
        ));
        return None;
    };
    if (new && (requested < default_level || extensions != default_ext))
        || (!new && requested <= default_level)
    {
        return option(standards[requested]);
    }
    (default_level + 1..=requested)
        .rev()
        .find_map(|idx| option(standards[idx]))
}

fn warn_cmp0128(inputs: &Flags<'_, '_>, text: &str) {
    let memo = format!(
        "CMP0128:{}:{}",
        inputs.ctx.target_key(inputs.target),
        inputs.lang
    );
    if inputs
        .ctx
        .memo
        .borrow_mut()
        .insert(memo, String::new())
        .is_none()
    {
        let warning = crate::policy::policy_warning(crate::policy::index_of("CMP0128").unwrap());
        inputs.ctx.message(
            MessageType::AuthorWarning,
            super::super::target::directory_backtrace(inputs.ctx, inputs.target.makefile),
            format!("{warning}\n{text}"),
        );
    }
}
