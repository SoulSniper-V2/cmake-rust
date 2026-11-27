//! External generator (`cmCPackExternalGenerator`).

use std::fs;

use super::{
    Generator, Logger, internally_on, is_on, option, option_nonempty, read_module, set_option,
};
use crate::util;

pub(super) fn initialize(interp: &mut crate::interp::Interp, log: &mut Logger) -> bool {
    set_option(interp, "CPACK_EXTERNAL_KNOWN_VERSIONS", "1.0");
    if !read_module(interp, "Internal/CPack/CPackExternal.cmake") {
        log.error("Error while executing CPackExternal.cmake");
        return false;
    }
    option(interp, "CPACK_EXTERNAL_SELECTED_MAJOR").as_deref() == Some("1")
}

fn json_escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

enum Json {
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
    String(String),
    Bool(bool),
    Number(usize),
}

impl Json {
    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Self::Object(items) => {
                if items.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                let mut items = items.iter().collect::<Vec<_>>();
                items.sort_by(|a, b| a.0.cmp(&b.0));
                for (index, (key, value)) in items.iter().enumerate() {
                    out.push_str(&" ".repeat(indent + 2));
                    out.push('"');
                    out.push_str(&json_escape(key));
                    out.push_str("\" : ");
                    if matches!(value, Self::Array(v) if !v.is_empty()) {
                        out.push('\n');
                        out.push_str(&" ".repeat(indent + 2));
                    }
                    value.write(out, indent + 2);
                    if index + 1 != items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&" ".repeat(indent));
                out.push('}');
            }
            Self::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (index, value) in items.iter().enumerate() {
                    out.push_str(&" ".repeat(indent + 2));
                    value.write(out, indent + 2);
                    if index + 1 != items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&" ".repeat(indent));
                out.push(']');
            }
            Self::String(value) => {
                out.push('"');
                out.push_str(&json_escape(value));
                out.push('"');
            }
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => out.push_str(&value.to_string()),
        }
    }
}

fn object(items: Vec<(&str, Json)>) -> Json {
    Json::Object(items.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}
fn string(value: impl Into<String>) -> Json {
    Json::String(value.into())
}
fn array_strings(values: impl IntoIterator<Item = String>) -> Json {
    Json::Array(values.into_iter().map(Json::String).collect())
}

pub(super) fn package_files(
    generator: &mut Generator,
    interp: &mut crate::interp::Interp,
    _log: &mut Logger,
) -> bool {
    let filename = generator
        .package_file_names
        .first()
        .cloned()
        .unwrap_or_else(|| "package.json".into());
    let mut root_items = vec![
        ("formatVersionMajor", Json::Number(1)),
        ("formatVersionMinor", Json::Number(0)),
    ];
    for (var, key) in [
        ("CPACK_PACKAGE_NAME", "packageName"),
        ("CPACK_PACKAGE_VERSION", "packageVersion"),
        ("CPACK_PACKAGE_DESCRIPTION_FILE", "packageDescriptionFile"),
        (
            "CPACK_PACKAGE_DESCRIPTION_SUMMARY",
            "packageDescriptionSummary",
        ),
        ("CPACK_BUILD_CONFIG", "buildConfig"),
        (
            "CPACK_INSTALL_DEFAULT_DIRECTORY_PERMISSIONS",
            "defaultDirectoryPermissions",
        ),
    ] {
        if let Some(value) = option_nonempty(interp, var) {
            root_items.push((key, string(value)));
        }
    }
    let setdest = internally_on(option(interp, "CPACK_SET_DESTDIR").as_deref());
    root_items.push(("setDestdir", Json::Bool(setdest)));
    if setdest {
        root_items.push((
            "packagingInstallPrefix",
            string(option(interp, "CPACK_PACKAGING_INSTALL_PREFIX").unwrap_or_default()),
        ));
    }
    let strip = !util::is_off_opt(option(interp, "CPACK_STRIP_FILES").as_deref());
    root_items.push(("stripFiles", Json::Bool(strip)));
    root_items.push((
        "warnOnAbsoluteInstallDestination",
        Json::Bool(is_on(interp, "CPACK_WARN_ON_ABSOLUTE_INSTALL_DESTINATION")),
    ));
    root_items.push((
        "errorOnAbsoluteInstallDestination",
        Json::Bool(is_on(interp, "CPACK_ERROR_ON_ABSOLUTE_INSTALL_DESTINATION")),
    ));

    let projects = generator
        .cmake_projects
        .iter()
        .map(|project| {
            object(vec![
                ("projectName", string(project.project_name.clone())),
                ("component", string(project.component.clone())),
                ("directory", string(project.directory.clone())),
                ("subDirectory", string(project.sub_directory.clone())),
                (
                    "installationTypes",
                    array_strings(project.installation_types.clone()),
                ),
                ("components", array_strings(project.components.clone())),
            ])
        })
        .collect();
    root_items.push(("projects", Json::Array(projects)));

    let installation_types = generator
        .installation_types
        .values()
        .map(|kind| {
            (
                kind.name.clone(),
                object(vec![
                    ("name", string(kind.name.clone())),
                    ("displayName", string(kind.display_name.clone())),
                    ("index", Json::Number(kind.index)),
                ]),
            )
        })
        .collect();
    root_items.push(("installationTypes", Json::Object(installation_types)));

    let components = generator
        .components
        .values()
        .map(|component| {
            let mut fields = vec![
                ("name", string(component.name.clone())),
                ("displayName", string(component.display_name.clone())),
            ];
            if let Some(group) = &component.group {
                fields.push(("group", string(group.clone())));
            }
            fields.extend([
                ("isRequired", Json::Bool(component.required)),
                ("isHidden", Json::Bool(component.hidden)),
                (
                    "isDisabledByDefault",
                    Json::Bool(component.disabled_by_default),
                ),
                ("isDownloaded", Json::Bool(component.downloaded)),
                ("description", string(component.description.clone())),
                ("archiveFile", string(component.archive_file.clone())),
                (
                    "installationTypes",
                    array_strings(component.installation_types.clone()),
                ),
                (
                    "dependencies",
                    array_strings(component.dependencies.clone()),
                ),
            ]);
            (component.name.clone(), object(fields))
        })
        .collect();
    root_items.push(("components", Json::Object(components)));

    let groups = generator
        .groups
        .values()
        .map(|group| {
            let mut fields = vec![
                ("name", string(group.name.clone())),
                ("displayName", string(group.display_name.clone())),
                ("description", string(group.description.clone())),
                ("isBold", Json::Bool(group.bold)),
                ("isExpandedByDefault", Json::Bool(group.expanded_by_default)),
            ];
            if let Some(parent) = &group.parent_group {
                fields.push(("parentGroup", string(parent.clone())));
            }
            // The first CPack External format has no nested-group population in
            // the Linux component-install path; the key is still always present.
            fields.push(("subgroups", Json::Array(Vec::new())));
            fields.push(("components", array_strings(group.components.clone())));
            (group.name.clone(), object(fields))
        })
        .collect();
    root_items.push(("componentGroups", Json::Object(groups)));

    let root = Json::Object(
        root_items
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect(),
    );
    let mut text = String::new();
    root.write(&mut text, 0);
    text.push('\n');
    if let Some(parent) = std::path::Path::new(&filename).parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::write(&filename, text).is_err() {
        return false;
    }

    if let Some(script) = option_nonempty(interp, "CPACK_EXTERNAL_PACKAGE_SCRIPT") {
        if !std::path::Path::new(&script).is_absolute() {
            // Reported by the caller through CPack's normal error path in
            // upstream; use the same text here.
            eprintln!(
                "CPack Error: CPACK_EXTERNAL_PACKAGE_SCRIPT does not contain a full file path"
            );
            return false;
        }
        if !interp.read_list_file(&script) || interp.error || interp.fatal {
            return false;
        }
        if let Some(built) = option(interp, "CPACK_EXTERNAL_BUILT_PACKAGES") {
            generator.package_file_names = util::expand_list(&built, false);
        }
    }
    true
}
