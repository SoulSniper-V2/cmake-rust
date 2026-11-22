//! Generate translation units for public and interface header sets.

use std::collections::BTreeSet;
use std::path::Path;

use super::ctx::{Diagnostic, GenCtx};
use super::target::{
    directory_backtrace, evaluate_file_set, source_language, source_prop, sources,
};
use crate::commands::target::{directory_entries, get_or_create_source, register_target};
use crate::interp::Interp;
use crate::policy::{PolicyStatus, index_of};
use crate::state::{Target, TargetKind};
use crate::util::{expand_list, is_on_opt};

const AGGREGATE: &str = "all_verify_interface_header_sets";

/// `cmGlobalGenerator::AddHeaderSetVerification`.
pub(super) fn add_targets(interp: &mut Interp) -> bool {
    let (plans, diagnostics, failed) = {
        let ctx = GenCtx::new(interp);
        let mut plans = Vec::new();
        for key in interp.makefiles.iter().flat_map(|mf| &mf.targets) {
            let target = &interp.ext.targets[key];
            if !is_on_opt(target.props.get("VERIFY_INTERFACE_HEADER_SETS"))
                || !(matches!(
                    target.kind,
                    TargetKind::StaticLibrary
                        | TargetKind::SharedLibrary
                        | TargetKind::UnknownLibrary
                        | TargetKind::ObjectLibrary
                        | TargetKind::InterfaceLibrary
                ) || target.kind == TargetKind::Executable
                    && is_on_opt(target.props.get("ENABLE_EXPORTS")))
            {
                continue;
            }
            if let Some(headers) = verification_sources(&ctx, target) {
                if !headers.is_empty() {
                    plans.push((key.clone(), headers));
                }
            } else {
                break;
            }
        }
        let failed = ctx.has_error();
        (plans, ctx.errors.into_inner(), failed)
    };
    for diagnostic in diagnostics {
        match diagnostic {
            Diagnostic::Message(kind, backtrace, text) => interp.issue_at(kind, &text, &backtrace),
            Diagnostic::Bare(text) => crate::configure::system_error(Some(interp), &text),
        }
    }
    if failed {
        return false;
    }
    for (key, headers) in plans {
        let original = &interp.ext.targets[&key];
        let dir = original.makefile;
        let name = format!("{}_verify_interface_header_sets", original.name);
        let mut verify = Target::new(&name, TargetKind::ObjectLibrary, dir);
        verify
            .backtrace
            .clone_from(&interp.makefiles[dir].backtrace);
        verify.policies.clone_from(&original.policies);
        verify
            .policies
            .set(index_of("CMP0119").unwrap(), PolicyStatus::New);
        let backtrace = verify.backtrace.clone();
        verify.set_entry("LINK_LIBRARIES", Some(&original.name), &backtrace);
        for prop in [
            "AUTOMOC",
            "AUTOUIC",
            "AUTORCC",
            "UNITY_BUILD",
            "CXX_SCAN_FOR_MODULES",
        ] {
            verify.props.set(prop, "OFF".to_owned());
        }
        for prop in ["EXCLUDE_FROM_ALL", "DISABLE_PRECOMPILE_HEADERS"] {
            verify.props.set(prop, "ON".to_owned());
        }
        for prop in [
            "COMPILE_DEFINITIONS",
            "COMPILE_OPTIONS",
            "INCLUDE_DIRECTORIES",
            "LINK_DIRECTORIES",
            "LINK_OPTIONS",
        ] {
            for (value, trace) in directory_entries(interp, dir, prop) {
                verify.insert_entry(prop, &value, &trace, false);
            }
        }
        for (header, language) in headers {
            let extension = if language == "C" { "c" } else { "cxx" };
            let file = format!(
                "{}/{name}/{header}.{extension}",
                interp.makefiles[dir].binary_dir
            );
            let content = format!(
                "/* NOLINTNEXTLINE(misc-header-include-cycle,misc-include-cleaner) */\n#include <{header}> /* IWYU pragma: associated */\n"
            );
            if let Err(error) =
                crate::fsutil::write_if_different(Path::new(&file), content.as_bytes())
            {
                interp.issue_at(
                    crate::messages::MessageType::FatalError,
                    &format!("Cannot write header verification source \"{file}\": {error}"),
                    &verify.backtrace,
                );
                return false;
            }
            get_or_create_source(interp, dir, &file)
                .props
                .set("LANGUAGE", language);
            verify.insert_entry("SOURCES", &file, &backtrace, false);
        }
        register_target(interp, verify);
        if !interp.ext.targets.contains_key(AGGREGATE) {
            let mut aggregate = Target::new(AGGREGATE, TargetKind::Utility, 0);
            aggregate.props.set("EXCLUDE_FROM_ALL", "ON".to_owned());
            register_target(interp, aggregate);
        }
        interp
            .ext
            .targets
            .get_mut(AGGREGATE)
            .unwrap()
            .add_dependency(&name, &[]);
    }
    true
}

/// `cmGeneratorTarget::GenerateHeaderSetVerificationFile`: explicit header
/// LANGUAGE wins; otherwise use C++ over C, first from the target's sources
/// and then from the languages enabled in the project.
fn verification_sources(ctx: &GenCtx, target: &Target) -> Option<BTreeSet<(String, String)>> {
    let available = expand_list(
        target
            .props
            .get("INTERFACE_HEADER_SETS")
            .unwrap_or_default(),
        false,
    );
    let mut requested: BTreeSet<_> = expand_list(
        target
            .props
            .get("INTERFACE_HEADER_SETS_TO_VERIFY")
            .unwrap_or_default(),
        false,
    )
    .into_iter()
    .collect();
    let verify_all = requested.is_empty();
    let sets: Vec<_> = available
        .iter()
        .filter(|name| verify_all || requested.remove(*name))
        .collect();
    if !requested.is_empty() {
        ctx.error(
            directory_backtrace(ctx, target.makefile),
            format!(
                "Property INTERFACE_HEADER_SETS_TO_VERIFY of target \"{}\" contained the following header sets that are nonexistent or not INTERFACE:\n  {}",
                target.name,
                requested.into_iter().collect::<Vec<_>>().join("\n  ")
            ),
        );
        return None;
    }
    let dir = target.makefile;
    let property = |path: &str, prop: &str| source_prop(ctx, dir, path, prop);
    let mut fallback = None;
    let mut result = BTreeSet::new();
    for name in sets {
        let Some(set) = target.file_sets.get(name) else {
            continue;
        };
        for (path, relative, _) in evaluate_file_set(ctx, target, set) {
            if is_on_opt(property(&path, "SKIP_LINTING").as_deref()) {
                continue;
            }
            let mut language = source_language(ctx, &path, &property);
            if language.is_empty() {
                language = fallback
                    .get_or_insert_with(|| {
                        let mut languages: BTreeSet<_> = sources(ctx, target, &ctx.config)
                            .iter()
                            .map(|item| source_language(ctx, &item.path, &property))
                            .filter(|lang| matches!(lang.as_str(), "C" | "CXX"))
                            .collect();
                        if languages.is_empty() {
                            languages.extend(ctx.interp.langs.enabled.iter().cloned());
                        }
                        ["CXX", "C"]
                            .into_iter()
                            .find(|lang| languages.contains(*lang))
                            .unwrap_or_default()
                            .to_owned()
                    })
                    .clone();
            }
            if matches!(language.as_str(), "C" | "CXX") {
                result.insert((relative, language));
            }
        }
    }
    Some(result)
}
