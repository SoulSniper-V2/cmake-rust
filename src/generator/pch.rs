//! Precompiled-header paths and options of `cmGeneratorTarget`
//! (GetPrecompileHeaders, GetPchHeader, GetPchSource, GetPchFileObject,
//! GetPchFile, GetPchCreateCompileOptions, GetPchUseCompileOptions) for a
//! single architecture (`GetPchArchs` yields one empty arch off Apple).

use super::ctx::GenCtx;
use super::target as gt;
use crate::state::Target;
use crate::util::is_on_opt;

/// The languages with precompiled-header support.
pub const PCH_LANGUAGES: [&str; 4] = ["C", "CXX", "OBJC", "OBJCXX"];

fn supported(language: &str) -> bool {
    PCH_LANGUAGES.contains(&language)
}

/// `cmGeneratorTarget::GetPrecompileHeaders` (values only).
pub fn precompile_headers(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> Vec<String> {
    let key = ctx.target_key(target);
    let mut host = super::genex_host::CtxHost::new(ctx, target.makefile);
    match crate::genex::target_usage(&mut host, &key, "PRECOMPILE_HEADERS", config, language) {
        Ok(items) => items.into_iter().map(|item| item.value).collect(),
        Err(error) => {
            if !error.quiet {
                let backtrace = ctx.backtrace(error.backtrace);
                ctx.error(backtrace, error.to_string());
            }
            Vec::new()
        }
    }
}

/// The target whose PCH a target uses (`PRECOMPILE_HEADERS_REUSE_FROM`).
pub fn reuse_target<'a>(ctx: &GenCtx<'a>, target: &'a Target) -> Option<&'a Target> {
    let name = target.props.get("PRECOMPILE_HEADERS_REUSE_FROM")?;
    ctx.target(name)
}

/// `GetPchHeader` without the file writes: the header path, or empty.
pub fn pch_header(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> String {
    if !supported(language) || is_on_opt(target.props.get("DISABLE_PRECOMPILE_HEADERS")) {
        return String::new();
    }
    // cmGeneratorTarget::PchHeaders caches per language and config.
    let key = format!("pch:header:{}:{config}:{language}", ctx.target_key(target));
    if let Some(cached) = ctx.memo.borrow().get(&key) {
        return cached.clone();
    }
    let header = compute_pch_header(ctx, target, config, language);
    ctx.memo.borrow_mut().insert(key, header.clone());
    header
}

fn compute_pch_header(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> String {
    let reuse = target.props.get("PRECOMPILE_HEADERS_REUSE_FROM");
    if reuse.is_none() && precompile_headers(ctx, target, config, language).is_empty() {
        return String::new();
    }
    let owner = match reuse {
        Some(_) => match reuse_target(ctx, target) {
            Some(owner) => owner,
            None => return String::new(),
        },
        None => target,
    };
    let extension = match language {
        "C" => ".h",
        "CXX" => ".hxx",
        "OBJC" => ".objc.h",
        _ => ".objcxx.hxx",
    };
    format!("{}/cmake_pch{extension}", gt::support_directory(ctx, owner))
}

/// `GetPchSource` without the file writes.
pub fn pch_source(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> String {
    if !supported(language) || pch_header(ctx, target, config, language).is_empty() {
        return String::new();
    }
    let owner = reuse_target(ctx, target).unwrap_or(target);
    let link_pch = is_on_opt(ctx.var(target.makefile, "CMAKE_LINK_PCH").as_deref());
    let extension = match (link_pch, language) {
        (false, "C") => ".h.c",
        (false, "CXX") => ".hxx.cxx",
        (false, "OBJC") => ".objc.h.m",
        (false, _) => ".objcxx.hxx.mm",
        (true, "C") => ".c",
        (true, "CXX") => ".cxx",
        (true, "OBJC") => ".m",
        (true, _) => ".mm",
    };
    format!("{}/cmake_pch{extension}", gt::support_directory(ctx, owner))
}

/// `GetPchFileObject`: the object file compiled from the PCH source.
pub fn pch_file_object(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> String {
    let source = pch_source(ctx, target, config, language);
    if source.is_empty() {
        return String::new();
    }
    let sources = gt::sources(ctx, target, config);
    match sources.iter().find(|info| info.path == source) {
        Some(info) if !info.object_name.is_empty() => {
            format!("{}{}", gt::object_directory(ctx, target, config), info.object_name)
        }
        _ => String::new(),
    }
}

/// `GetPchFile`.
pub fn pch_file(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> String {
    let extension = ctx.var_safe(target.makefile, "CMAKE_PCH_EXTENSION");
    if is_on_opt(ctx.var(target.makefile, "CMAKE_LINK_PCH").as_deref()) {
        let owner = reuse_target(ctx, target).unwrap_or(target);
        let object = pch_file_object(ctx, owner, config, language);
        if extension.is_empty() {
            return String::new();
        }
        let stem = object.rfind('.').map_or("", |dot| &object[..dot]);
        format!("{stem}{extension}")
    } else {
        format!("{}{extension}", pch_header(ctx, target, config, language))
    }
}

fn substitute(options: String, header: &str, file: &str) -> String {
    options.replace("<PCH_HEADER>", header).replace("<PCH_FILE>", file)
}

/// `GetPchCreateCompileOptions`.
pub fn pch_create_options(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> String {
    let dir = target.makefile;
    let mut options = String::new();
    if is_on_opt(target.props.get("PCH_WARN_INVALID")) {
        options = ctx.var_safe(dir, &format!("CMAKE_{language}_COMPILE_OPTIONS_INVALID_PCH"));
    }
    if is_on_opt(target.props.get("PCH_INSTANTIATE_TEMPLATES")) {
        let instantiate = ctx.var_safe(
            dir,
            &format!("CMAKE_{language}_COMPILE_OPTIONS_INSTANTIATE_TEMPLATES_PCH"),
        );
        if !instantiate.is_empty() {
            options = format!("{options};{instantiate}");
        }
    }
    let create = ctx.var_safe(dir, &format!("CMAKE_{language}_COMPILE_OPTIONS_CREATE_PCH"));
    options = format!("{options};{create}");
    let header = pch_header(ctx, target, config, language);
    let file = pch_file(ctx, target, config, language);
    substitute(options, &header, &file)
}

/// `GetPchUseCompileOptions`.
pub fn pch_use_options(ctx: &GenCtx, target: &Target, config: &str, language: &str) -> String {
    let dir = target.makefile;
    let mut options = String::new();
    if is_on_opt(target.props.get("PCH_WARN_INVALID")) {
        options = ctx.var_safe(dir, &format!("CMAKE_{language}_COMPILE_OPTIONS_INVALID_PCH"));
    }
    let property = format!("{language}_COMPILE_OPTIONS_USE_PCH");
    let value = target.props.get(&property).unwrap_or_default();
    let value = if value.is_empty() {
        ctx.var_safe(dir, &format!("CMAKE_{property}"))
    } else {
        value.to_owned()
    };
    options = format!("{options};{value}");
    let header = pch_header(ctx, target, config, language);
    let file = pch_file(ctx, target, config, language);
    substitute(options, &header, &file)
}
