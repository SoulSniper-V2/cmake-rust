//! SARIF diagnostics (`cmSarif::ResultsLog` and `LogFileWriter`).

use std::path::Path;

use serde_json::{Value, json};

use crate::messages::{Frame, MessageType};

#[derive(Default, Debug)]
pub struct ResultsLog {
    rules: Vec<(MessageType, Value)>,
    results: Vec<Value>,
}

impl ResultsLog {
    pub fn message(&mut self, kind: MessageType, text: &str, backtrace: &[Frame]) {
        let (id, name, level) = match kind {
            MessageType::AuthorWarning => ("AuthorWarning", "Warning (dev)", "warning"),
            MessageType::Warning => ("Warning", "Warning", "warning"),
            MessageType::DeprecationWarning => {
                ("DeprecationWarning", "Deprecation Warning", "warning")
            }
            MessageType::AuthorError => ("AuthorError", "Error (dev)", "error"),
            MessageType::FatalError => ("FatalError", "Error", "error"),
            MessageType::InternalError => ("InternalError", "Internal Error", "error"),
            MessageType::DeprecationError => ("DeprecationError", "Deprecation Error", "error"),
            MessageType::Message => ("Message", "Message", "note"),
            MessageType::Log => ("Log", "Log", "note"),
        };
        let id = format!("CMake.{id}");
        let index = self
            .rules
            .iter()
            .position(|(key, _)| *key == kind)
            .unwrap_or_else(|| {
                let detail = if kind == MessageType::InternalError {
                    " (please report a bug)"
                } else {
                    ""
                };
                let index = self.rules.len();
                self.rules.push((kind, json!({
                "id": id,
                "name": format!("CMake {name}"),
                "messageStrings": {"default": {"text": format!("CMake {name}{detail}: {{0}}")}}
            })));
                index
            });
        let mut result = json!({
            "level": level,
            "message": {"text": text},
            "ruleId": id,
            "ruleIndex": index
        });
        if let Some(frame) = backtrace
            .last()
            .filter(|frame| frame.line > 0 && !frame.file.is_empty())
        {
            result["locations"] = json!([{
                "physicalLocation": {
                    "artifactLocation": {"uri": frame.file.as_ref()},
                    "region": {"startLine": frame.line}
                }
            }]);
        }
        self.results.push(result);
    }

    pub fn write(&self, path: &str, create_dirs: bool) {
        let write = || -> std::io::Result<()> {
            let path = Path::new(path);
            if create_dirs && let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let document = json!({
                "$schema": "https://schemastore.azurewebsites.net/schemas/json/sarif-2.1.0-rtm.4.json",
                "version": "2.1.0",
                "runs": [{
                    "tool": {"driver": {
                        "name": "CMake",
                        "version": crate::interp::VERSION_STRING,
                        "rules": self.rules.iter().map(|(_, rule)| rule).collect::<Vec<_>>()
                    }},
                    "results": self.results
                }]
            });
            serde_json::to_writer_pretty(std::fs::File::create(path)?, &document)?;
            Ok(())
        };
        if write().is_err() {
            crate::cmcmd::error(format!("Failed to write SARIF log to {path}"));
        }
    }
}
