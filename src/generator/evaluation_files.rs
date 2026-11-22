//! file(GENERATE) evaluation (cmGeneratorExpressionEvaluationFile::Generate).

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use super::ctx::GenCtx;
use crate::util;

/// Generates every recorded evaluation file; returns the files written.
pub fn generate(ctx: &GenCtx) -> Vec<String> {
    let interp = ctx.interp;
    let mut written = Vec::new();
    let languages: Vec<String> = interp.langs.enabled.clone();
    for file in &interp.ext.evaluation_files {
        let makefile = &interp.makefiles[file.dir];
        let mut permissions = file.permissions;
        let content = if file.input_is_content {
            file.input.clone()
        } else {
            let path = if util::is_full_path(&file.input) || !file.cmp0070_new {
                util::collapse_full_path(&file.input, &makefile.source_dir)
            } else {
                util::collapse_full_path(&file.input, &makefile.source_dir)
            };
            if permissions == 0
                && let Ok(meta) = std::fs::metadata(&path)
            {
                permissions = meta.permissions().mode() & 0o7777;
            }
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    // GetLineFromStream joins lines with "\n" and keeps a
                    // trailing separator after the last line.
                    let mut out = String::new();
                    let mut sep = "";
                    let mut lines: Vec<&str> = text.split('\n').collect();
                    if lines.last() == Some(&"") {
                        lines.pop();
                    }
                    for line in lines {
                        out.push_str(sep);
                        out.push_str(line.strip_suffix('\r').unwrap_or(line));
                        sep = "\n";
                    }
                    out.push_str(sep);
                    out
                }
                Err(_) => {
                    ctx.error(
                        file.backtrace.clone(),
                        format!("Evaluation file \"{path}\" cannot be read."),
                    );
                    continue;
                }
            }
        };
        let target = if file.target.is_empty() {
            None
        } else {
            ctx.target_at(file.dir, &file.target)
                .map(|target| ctx.target_key(target))
        };
        let mut outputs: BTreeMap<String, String> = BTreeMap::new();
        for language in &languages {
            let language = Some(language.as_str());
            if !file.condition.is_empty() {
                let result = ctx.genex_full(
                    &file.condition,
                    file.dir,
                    target.as_deref(),
                    language,
                    false,
                );
                if result == "0" {
                    continue;
                }
                if result != "1" {
                    ctx.error(
                        file.backtrace.clone(),
                        format!(
                            "Evaluation file condition \"{}\" did not evaluate to valid content. Got \"{result}\".",
                            file.condition
                        ),
                    );
                    continue;
                }
            }
            let mut output =
                ctx.genex_full(&file.output, file.dir, target.as_deref(), language, false);
            output = if util::is_full_path(&output) {
                util::collapse_full_path(&output, "")
            } else if file.cmp0070_new {
                util::collapse_full_path(&output, &makefile.binary_dir)
            } else {
                util::collapse_full_path(&output, "")
            };
            let value = ctx.genex_full(&content, file.dir, target.as_deref(), language, false);
            if let Some(existing) = outputs.get(&output) {
                if *existing != value {
                    ctx.error(
                        file.backtrace.clone(),
                        format!(
                            "Evaluation file to be written multiple times with different content. This is generally caused by the content evaluating the configuration type, language, or location of object files:\n {output}"
                        ),
                    );
                }
                continue;
            }
            let mut text = String::new();
            let mut lines: Vec<&str> = value.split('\n').collect();
            let trailing = lines.last() == Some(&"");
            if trailing {
                lines.pop();
            }
            let count = lines.len();
            for (index, line) in lines.into_iter().enumerate() {
                text.push_str(line);
                let has_newline = index + 1 < count || trailing;
                if !file.newline.is_empty() {
                    text.push_str(&file.newline);
                } else if has_newline {
                    text.push('\n');
                }
            }
            let path = std::path::Path::new(&output);
            if crate::fsutil::write_if_different(path, text.as_bytes()).is_err() {
                ctx.error(
                    file.backtrace.clone(),
                    format!("Could not open file for write in copy operation {output}"),
                );
                continue;
            }
            if permissions != 0 {
                let _ =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(permissions));
            }
            outputs.insert(output.clone(), value);
            written.push(output);
        }
    }
    written
}
