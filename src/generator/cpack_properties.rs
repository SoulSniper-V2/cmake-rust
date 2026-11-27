//! `cmCPackPropertiesGenerator`: writes INSTALL-scope properties for CPack.

use super::ctx::GenCtx;
use crate::util;

fn config_pattern(config: &str) -> String {
    let mut out = String::new();
    for ch in config.chars() {
        if ch.is_ascii_alphabetic() {
            out.push('[');
            out.push(ch.to_ascii_uppercase());
            out.push(ch.to_ascii_lowercase());
            out.push(']');
        } else {
            out.push(ch);
        }
    }
    out
}

fn property_lines(ctx: &GenCtx, indent: &str) -> String {
    let mut out = String::new();
    let mut files: Vec<_> = ctx.interp.ext.install_properties.iter().collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    let backtrace = Vec::new();
    for (filename, properties) in files {
        let expanded_file = ctx
            .genex_eval(filename, 0, None, None, false, &backtrace)
            .value;
        for (name, value) in properties.iter() {
            let expanded_value = ctx
                .genex_eval(value, 0, None, None, false, &backtrace)
                .value;
            out.push_str(&format!(
                "{indent}set_property(INSTALL {} PROPERTY {} {})\n",
                util::escape_for_cmake(&expanded_file),
                util::escape_for_cmake(name),
                util::escape_for_cmake(&expanded_value),
            ));
        }
    }
    out
}

/// `cmGlobalGenerator::GenerateCPackPropertiesFile`.
pub fn generate(ctx: &GenCtx) {
    if ctx.interp.ext.install_properties.is_empty() {
        return;
    }
    let path = format!("{}/CPackProperties.cmake", ctx.interp.home_binary);
    let mut text = String::from("# CPack properties\n");
    if ctx.multi_config {
        for (index, config) in ctx.configs.iter().enumerate() {
            let keyword = if index == 0 { "if" } else { "elseif" };
            text.push_str(&format!(
                "{keyword}(CPACK_BUILD_CONFIG MATCHES \"^({})$)\")\n",
                config_pattern(config)
            ));
            text.push_str(&property_lines(&ctx.for_config(config), "  "));
        }
        text.push_str("endif()\n");
    } else {
        text.push_str(&property_lines(ctx, ""));
    }
    if let Err(error) = std::fs::write(&path, text) {
        ctx.error(
            Vec::new(),
            format!("Could not write CPack properties file.\n{error}"),
        );
    }
}
