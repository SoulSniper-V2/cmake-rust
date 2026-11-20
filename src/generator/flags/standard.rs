use super::{Flags, append_options};
use crate::policy::PolicyStatus;
use crate::util::{expand_list, is_on_opt, strtol};

fn levels(language: &str) -> &'static [&'static str] {
    match language {
        "C" | "OBJC" => &["90", "99", "11", "17", "23"],
        "CXX" | "OBJCXX" | "HIP" => &["98", "11", "14", "17", "20", "23", "26"],
        "CUDA" => &["03", "11", "14", "17", "20", "23", "26"],
        _ => &[],
    }
}

fn level(standards: &[&str], value: &str) -> Option<usize> {
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
    let standards = levels(inputs.lang);
    let mut requested = inputs
        .prop(&format!("{}_STANDARD", inputs.lang))
        .map(str::to_owned);
    let features = inputs.usage("COMPILE_FEATURES");
    let raise = |language: &str, current: Option<String>, default: &str| {
        let standards = levels(language);
        let highest = standards.iter().rposition(|standard| {
            let items = inputs
                .var(&format!("CMAKE_{language}{standard}_COMPILE_FEATURES"))
                .unwrap_or_default();
            expand_list(&items, false)
                .iter()
                .any(|item| features.contains(item))
        });
        if let Some(highest) = highest
            && level(standards, current.as_deref().unwrap_or(default))
                .is_none_or(|idx| idx < highest)
        {
            return Some(standards[highest].to_owned());
        }
        current
    };
    if let Some(paired) = paired_language(inputs.lang)
        && requested.is_none()
        && inputs
            .ctx
            .interp
            .ext
            .languages
            .iter()
            .any(|lang| lang == paired)
        && let Some(paired_default) = inputs.var(&format!("CMAKE_{paired}_STANDARD_DEFAULT"))
    {
        requested = raise(
            paired,
            inputs
                .prop(&format!("{paired}_STANDARD"))
                .map(str::to_owned),
            &paired_default,
        )
        .or(Some(paired_default));
        if inputs.lang == "CUDA" && requested.as_deref() == Some("98") {
            requested = Some("03".to_owned());
        }
    }
    if standards.is_empty() {
        requested
    } else {
        raise(inputs.lang, requested, default)
    }
}

pub(super) fn append_standard(inputs: &Flags<'_, '_>, flags: &mut String) {
    if let Some(option) = compile_option(inputs) {
        append_options(flags, expand_list(&option, false));
    }
}

fn compile_option(inputs: &Flags<'_, '_>) -> Option<String> {
    let lang = inputs.lang;
    let standards = levels(lang);
    let default = inputs
        .lang_var("STANDARD_DEFAULT")
        .filter(|v| !v.is_empty())?;
    if standards.is_empty() {
        return None;
    }
    let new = inputs.policy(128) == PolicyStatus::New;
    let default_ext = is_on_opt(inputs.lang_var("EXTENSIONS_DEFAULT").as_deref());
    let extensions =
        paired_prop(inputs, "EXTENSIONS").map_or(!new || default_ext, crate::util::is_on);
    let kind = if extensions { "EXTENSION" } else { "STANDARD" };
    let option = |standard| inputs.var(&format!("CMAKE_{lang}{standard}_{kind}_COMPILE_OPTION"));

    let Some(standard) = requested_standard(inputs, &default) else {
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

    if new && standard == default && extensions == default_ext {
        return None;
    }
    let standard = if lang == "CUDA" && standard == "98" {
        "03"
    } else {
        &standard
    };
    let Some(requested) = level(standards, standard) else {
        inputs.error(format!(
            "{lang}_STANDARD is set to invalid value '{standard}'"
        ));
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
