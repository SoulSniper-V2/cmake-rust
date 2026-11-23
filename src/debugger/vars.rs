//! `cmDebuggerVariablesHelper` / `cmDebuggerVariables` /
//! `cmDebuggerVariablesManager` port: the variable trees behind DAP
//! `scopes`/`variables` requests.
//!
//! Where the Rust interpreter's model differs from `cmMakefile`/`cmTarget`
//! the closest faithful data is exposed; fields the model does not carry
//! are omitted rather than fabricated.

use super::{DebugFrame, Inner, J, State, VarNode};

pub struct VarEntry {
    pub name: String,
    pub value: String,
    pub kind: &'static str,
}

impl VarEntry {
    fn str(name: impl Into<String>, value: impl Into<String>) -> Self {
        VarEntry {
            name: name.into(),
            value: value.into(),
            kind: "string",
        }
    }
    fn boolean(name: impl Into<String>, value: bool) -> Self {
        VarEntry {
            name: name.into(),
            value: if value { "TRUE" } else { "FALSE" }.to_owned(),
            kind: "bool",
        }
    }
    fn int(name: impl Into<String>, value: i64) -> Self {
        VarEntry {
            name: name.into(),
            value: value.to_string(),
            kind: "int",
        }
    }
}

fn interp_at(ip: usize) -> &'static crate::interp::Interp {
    // SAFETY: handlers run on the DAP session thread while the interpreter
    // is alive (it owns the adapter, which is joined before drop).
    unsafe { &*(ip as *const crate::interp::Interp) }
}

/// `cmMakefile::GetDefinition` (normal definitions only).
pub fn get_definition(ip: usize, _mf: usize, name: &str) -> Option<String> {
    interp_at(ip).get_normal_def(name).map(str::to_owned)
}

fn register(state: &mut State, inner: &Inner, node: VarNode) -> i64 {
    let id = inner.next_var_id();
    state.variables.insert(id, node);
    id
}

fn presentation_hint(kind: &str) -> J {
    J::Obj(vec![
        ("kind".into(), J::str(kind)),
        ("visibility".into(), J::str("private")),
    ])
}

fn variable_json(name: &str, value: &str, kind: Option<&str>, reference: i64, hint: &str) -> J {
    let mut fields = vec![
        ("name".into(), J::str(name)),
        ("presentationHint".into(), presentation_hint(hint)),
    ];
    if let Some(kind) = kind {
        fields.push(("type".into(), J::str(kind)));
    }
    fields.push(("value".into(), J::str(value)));
    fields.push(("variablesReference".into(), J::Int(reference)));
    J::Obj(fields)
}

/// `cmDebuggerVariables::HandleVariablesRequest`: entries, then
/// collections, sorted by name unless disabled.
pub fn handle_variables_request(state: &mut State, inner: &Inner, reference: i64) -> Vec<J> {
    let supports_type = inner.supports_variable_type();
    let Some(node) = state.variables.get(&reference) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(handler) = node.handler.as_ref() {
        for entry in handler() {
            if node.ignore_empty && entry.kind == "string" && entry.value.is_empty() {
                continue;
            }
            let kind = supports_type.then_some(entry.kind);
            out.push(variable_json(&entry.name, &entry.value, kind, 0, "data"));
        }
    }
    // SubVariables are registered eagerly so ids exist before queries.
    // Collect child ids from the node — they were registered in creation
    // order and stored in `subvar_ids` by `register_tree`.
    if let Some(ids) = node_subvar_ids(state, reference) {
        for id in ids {
            let child = &state.variables[&id];
            let kind = supports_type.then_some("collection");
            out.push(variable_json(
                &child.name,
                &child.value,
                kind,
                id,
                "property",
            ));
        }
    }
    if state.variables[&reference].sort {
        out.sort_by(|a, b| {
            let name_of = |v: &J| match v {
                J::Obj(fields) => fields
                    .iter()
                    .find(|(k, _)| k == "name")
                    .map(|(_, v)| match v {
                        J::Str(s) => s.clone(),
                        _ => String::new(),
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            name_of(a).cmp(&name_of(b))
        });
    }
    out
}

/// Child ids of a `VarNode`, stored alongside registration.
fn node_subvar_ids(state: &State, id: i64) -> Option<Vec<i64>> {
    state.var_children.get(&id).cloned()
}

fn register_tree(state: &mut State, inner: &Inner, node: TreeNode) -> i64 {
    let TreeNode {
        name,
        value,
        ignore_empty,
        sort,
        subvars,
        handler,
    } = node;
    // Every `cmDebuggerVariablesHelper` constructs the parent
    // `cmDebuggerVariables` (taking the next id) before creating its
    // children: ids are assigned in pre-order.
    let id = register(
        state,
        inner,
        VarNode {
            name,
            value,
            ignore_empty,
            sort,
            handler,
        },
    );
    let mut child_ids = Vec::with_capacity(subvars.len());
    for child in subvars {
        child_ids.push(register_tree(state, inner, child));
    }
    state.var_children.insert(id, child_ids);
    id
}

/// Build-time representation of a `cmDebuggerVariables` node.
pub struct TreeNode {
    pub name: String,
    pub value: String,
    pub ignore_empty: bool,
    pub sort: bool,
    pub subvars: Vec<TreeNode>,
    pub handler: Option<super::VarHandler>,
}

impl TreeNode {
    fn node(name: impl Into<String>) -> Self {
        TreeNode {
            name: name.into(),
            value: String::new(),
            ignore_empty: false,
            sort: true,
            subvars: Vec::new(),
            handler: None,
        }
    }
    fn leaf(name: impl Into<String>, handler: impl Fn() -> Vec<VarEntry> + Send + 'static) -> Self {
        TreeNode {
            name: name.into(),
            value: String::new(),
            ignore_empty: false,
            sort: true,
            subvars: Vec::new(),
            handler: Some(Box::new(handler)),
        }
    }
}

/// `dap::Scope` for a registered scope node (`Locals`).
pub fn scope_json(state: &State, var_id: i64) -> J {
    let node = &state.variables[&var_id];
    J::Obj(vec![
        ("expensive".into(), J::Bool(false)),
        ("name".into(), J::str(node.name.clone())),
        ("presentationHint".into(), J::str("locals")),
        ("source".into(), state.frame_source(var_id)),
        ("variablesReference".into(), J::Int(var_id)),
    ])
}

/// `cmDebuggerVariablesHelper::Create(manager, "Locals", type, frame)`.
pub fn create_locals(
    state: &mut State,
    inner: &Inner,
    frame: &DebugFrame,
    supports_type: bool,
) -> i64 {
    let ip = frame.interp;
    let mf = frame.mf;
    let frame_line = frame.line;
    let frame_file = frame.file.clone();
    let _ = supports_type;

    let mut root = TreeNode::node("Locals");
    root.handler = Some(Box::new(move || {
        vec![VarEntry::int("CurrentLine", frame_line)]
    }));

    // closureKeys: normal variable names visible to the frame's makefile.
    let keys: Vec<String> = {
        let interp = interp_at(ip);
        interp.normal_definition_names()
    };

    let local_keys = keys.clone();
    let mut locals = TreeNode::leaf("Locals", move || {
        let interp = interp_at(ip);
        local_keys
            .iter()
            .map(|key| {
                VarEntry::str(
                    key.clone(),
                    interp.get_normal_def(key).unwrap_or("").to_owned(),
                )
            })
            .collect()
    });
    locals.value = keys.len().to_string();
    root.subvars.push(locals);

    let is_directory = |key: &str| key.ends_with("_DIR") || key.ends_with("_DIRECTORY");
    let dir_keys: Vec<String> = keys
        .iter()
        .filter(|key| is_directory(key))
        .cloned()
        .collect();
    let dir_count = dir_keys.len();
    let mut directories = TreeNode::leaf("Directories", move || {
        let interp = interp_at(ip);
        dir_keys
            .iter()
            .map(|key| {
                VarEntry::str(
                    key.clone(),
                    interp.get_normal_def(key).unwrap_or("").to_owned(),
                )
            })
            .collect()
    });
    directories.value = dir_count.to_string();
    root.subvars.push(directories);

    // CacheVariables: one collection per cache entry named `key:TYPE`.
    let cache_keys: Vec<String> = {
        let interp = interp_at(ip);
        interp.cache.entries.keys().cloned().collect()
    };
    let mut cache = TreeNode::node("CacheVariables");
    cache.value = cache_keys.len().to_string();
    for key in &cache_keys {
        let (type_name, value) = {
            let interp = interp_at(ip);
            let entry = &interp.cache.entries[key];
            (entry.kind.as_str().to_owned(), entry.value.clone())
        };
        let key2 = key.clone();
        let mut entry_node = TreeNode::leaf(format!("{key}:{type_name}"), move || {
            let interp = interp_at(ip);
            let mut ret = Vec::new();
            if let Some(entry) = interp.cache.entries.get(&key2) {
                for (name, value) in &entry.properties {
                    ret.push(VarEntry::str(name.clone(), value.clone()));
                }
                ret.push(VarEntry::str("TYPE", entry.kind.as_str()));
                ret.push(VarEntry::str("VALUE", entry.value.clone()));
            }
            ret
        });
        entry_node.value = value;
        cache.subvars.push(entry_node);
    }
    root.subvars.push(cache);

    // Targets: `GetOrderedTargets` of the frame's makefile.
    let target_names: Vec<String> = {
        let interp = interp_at(ip);
        interp
            .makefiles
            .get(mf)
            .map(|m| m.targets.clone())
            .unwrap_or_default()
    };
    if !target_names.is_empty() {
        let mut targets = TreeNode::node("Targets");
        targets.value = target_names.len().to_string();
        for name in target_names {
            targets.subvars.push(target_node(ip, mf, &name));
        }
        root.subvars.push(targets);
    }

    // Tests: `GetTests` for the frame's directory.
    let test_names: Vec<String> = {
        let interp = interp_at(ip);
        interp
            .ext
            .tests
            .iter()
            .filter(|test| test.makefile == mf)
            .map(|test| test.name.clone())
            .collect()
    };
    if !test_names.is_empty() {
        let mut tests = TreeNode::node("Tests");
        tests.value = test_names.len().to_string();
        for name in test_names {
            let name2 = name.clone();
            let mut test = TreeNode::leaf(name.clone(), move || {
                let interp = interp_at(ip);
                let Some(test) = interp.ext.tests.iter().find(|t| t.name == name2) else {
                    return Vec::new();
                };
                vec![
                    VarEntry::boolean("CommandExpandLists", test.command_expand_lists),
                    VarEntry::str("Name", test.name.clone()),
                    VarEntry::boolean("OldStyle", test.old_style),
                ]
            });
            let name3 = name.clone();
            if let Some(node) = create_if_any_vec("Command", move || {
                let interp = interp_at(ip);
                interp
                    .ext
                    .tests
                    .iter()
                    .find(|t| t.name == name3)
                    .map(|t| t.command.clone())
                    .unwrap_or_default()
            }) {
                test.subvars.push(node);
            }
            if let Some(node) = create_if_any_props("Properties", move || {
                let interp = interp_at(ip);
                interp
                    .ext
                    .tests
                    .iter()
                    .find(|t| t.name == name)
                    .map(|t| props_list(&t.props))
                    .unwrap_or_default()
            }) {
                test.subvars.push(node);
            }
            tests.subvars.push(test);
        }
        root.subvars.push(tests);
    }

    let id = register_tree(state, inner, root);
    state.var_frame_source.insert(id, frame_file);
    id
}

fn props_list(props: &crate::props::Props) -> Vec<(String, String)> {
    props.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// `CreateIfAny` for `std::vector<std::string>` -> `[i]` entries.
fn create_if_any_vec<F>(name: &str, items: F) -> Option<TreeNode>
where
    F: Fn() -> Vec<String> + Send + 'static,
{
    let items = items();
    if items.is_empty() {
        return None;
    }
    let count = items.len();
    let mut node = TreeNode::leaf(name, move || {
        items
            .iter()
            .enumerate()
            .map(|(i, item)| VarEntry::str(format!("[{i}]"), item.clone()))
            .collect()
    });
    node.sort = false;
    node.value = count.to_string();
    Some(node)
}

/// `CreateIfAny` for `cmPropertyMap::GetList` (name/value pairs).
fn create_if_any_props<F>(name: &str, props: F) -> Option<TreeNode>
where
    F: Fn() -> Vec<(String, String)> + Send + 'static,
{
    let list = props();
    if list.is_empty() {
        return None;
    }
    let count = list.len();
    let mut node = TreeNode::leaf(name, move || {
        list.iter()
            .map(|(k, v)| VarEntry::str(k.clone(), v.clone()))
            .collect()
    });
    node.value = count.to_string();
    Some(node)
}

/// `CreateIfAny` for `cmBTStringRange`/`BT<std::string>` collections:
/// child collections named by each entry's value, each expanding the
/// entry as a `;`-list (`cmList`).
fn create_if_any_bt_range(name: &str, values: Vec<String>) -> Option<TreeNode> {
    if values.is_empty() {
        return None;
    }
    let mut node = TreeNode::node(name);
    node.value = values.len().to_string();
    for value in values {
        let mut array = TreeNode::leaf(value.clone(), move || {
            crate::util::expand_list(&value, false)
                .into_iter()
                .enumerate()
                .map(|(i, item)| VarEntry::str(format!("[{i}]"), item))
                .collect()
        });
        array.sort = false;
        node.subvars.push(array);
    }
    Some(node)
}

/// `CreateIfAny` for a `cmPolicies::PolicyMap` -> `CMPnnnn` entries.
fn policy_map_node(name: &str, ip: usize, map: crate::policy::PolicyMap) -> TreeNode {
    TreeNode::leaf(name, move || {
        let mut ret = Vec::new();
        for index in 0..crate::policy_table::POLICIES.len() {
            if let Some(status) = map.get(index) {
                let text = match status {
                    crate::policy::PolicyStatus::Old => "OLD",
                    crate::policy::PolicyStatus::Warn => "WARN",
                    crate::policy::PolicyStatus::New => "NEW",
                };
                ret.push(VarEntry::str(format!("CMP{index:04}"), text));
            }
        }
        let _ = ip;
        ret
    })
}

/// `CreateIfAny` for `cmTarget*` -> per-target tree.
fn target_node(ip: usize, _mf: usize, name: &str) -> TreeNode {
    let name_owned = name.to_owned();
    let type_name = {
        let interp = interp_at(ip);
        interp
            .ext
            .targets
            .get(&name_owned)
            .map(|t| t.kind.type_name())
            .unwrap_or("")
            .to_owned()
    };
    let name2 = name_owned.clone();
    let mut target = TreeNode::leaf(name_owned.clone(), move || {
        let interp = interp_at(ip);
        let Some(target) = interp.ext.targets.get(&name2) else {
            return Vec::new();
        };
        let kind = target.kind;
        let is_apple = interp
            .get_def("APPLE")
            .map(crate::util::is_on)
            .unwrap_or(false);
        let system_name = interp.get_def("CMAKE_SYSTEM_NAME").unwrap_or("");
        let is_aix = system_name == "AIX" || system_name == "OS400";
        let is_android = system_name == "Android";
        let is_dll_platform = !interp.get_safe("CMAKE_IMPORT_LIBRARY_SUFFIX").is_empty();
        let prop_bool = |name: &str| {
            target
                .props
                .get(name)
                .map(crate::util::is_on)
                .unwrap_or(false)
        };
        vec![
            VarEntry::str("InstallPath", target_install_path(interp, target)),
            VarEntry::boolean("IsAIX", is_aix),
            VarEntry::boolean(
                "IsAndroidGuiExecutable",
                kind == crate::state::TargetKind::Executable
                    && is_android
                    && prop_bool("ANDROID_GUI"),
            ),
            VarEntry::boolean(
                "IsAppBundleOnApple",
                kind == crate::state::TargetKind::Executable
                    && is_apple
                    && prop_bool("MACOSX_BUNDLE"),
            ),
            VarEntry::boolean("IsDLLPlatform", is_dll_platform),
            VarEntry::boolean(
                "IsExecutableWithExports",
                kind == crate::state::TargetKind::Executable && prop_bool("ENABLE_EXPORTS"),
            ),
            VarEntry::boolean(
                "IsFrameworkOnApple",
                (kind == crate::state::TargetKind::SharedLibrary
                    || kind == crate::state::TargetKind::StaticLibrary)
                    && is_apple
                    && prop_bool("FRAMEWORK"),
            ),
            VarEntry::boolean("IsImported", target.imported),
            VarEntry::boolean("IsImportedGloballyVisible", target.imported_global),
            // `PerConfig::No` is only used for the generator's global
            // targets, which are not in `GetOrderedTargets`.
            VarEntry::boolean("IsPerConfig", true),
            VarEntry::str("Name", target.name.clone()),
            VarEntry::str(
                "RuntimeInstallPath",
                target_runtime_install_path(interp, target),
            ),
            VarEntry::str("Type", kind.type_name()),
        ]
    });
    target.value = type_name;
    target.ignore_empty = true;

    let name3 = name_owned.clone();
    target.subvars.push(policy_map_node("PolicyMap", ip, {
        let interp = interp_at(ip);
        interp
            .ext
            .targets
            .get(&name3)
            .map(|t| t.policies.clone())
            .unwrap_or_default()
    }));
    if let Some(node) = create_if_any_props("Properties", move || {
        let interp = interp_at(ip);
        interp
            .ext
            .targets
            .get(&name3)
            .map(|t| target_property_map(&t.props))
            .unwrap_or_default()
    }) {
        target.subvars.push(node);
    }
    // The `cmTarget::Get*Entries` usage-requirement collections, in the
    // order `CreateIfAny(..., std::vector<cmTarget*>)` adds them.
    for (label, property) in [
        ("IncludeDirectories", "INCLUDE_DIRECTORIES"),
        ("Sources", "SOURCES"),
        ("CompileDefinitions", "COMPILE_DEFINITIONS"),
        ("CompileFeatures", "COMPILE_FEATURES"),
        ("CompileOptions", "COMPILE_OPTIONS"),
        ("CxxModuleSets", "CXX_MODULE_SETS"),
        ("HeaderSets", "HEADER_SETS"),
        ("InterfaceHeaderSets", "INTERFACE_HEADER_SETS"),
        ("LinkDirectories", "LINK_DIRECTORIES"),
        ("LinkImplementations", "LINK_LIBRARIES"),
        ("LinkInterfaceDirects", "INTERFACE_LINK_LIBRARIES_DIRECT"),
        (
            "LinkInterfaceDirectExcludes",
            "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
        ),
        ("LinkInterfaces", "INTERFACE_LINK_LIBRARIES"),
        ("LinkOptions", "LINK_OPTIONS"),
    ] {
        let entries = {
            let interp = interp_at(ip);
            interp
                .ext
                .targets
                .get(&name_owned)
                .map(|t| target_entries(t, property))
                .unwrap_or_default()
        };
        if let Some(node) = create_if_any_bt_range(label, entries) {
            target.subvars.push(node);
        }
    }
    if let Some(node) = create_if_any_vec("SystemIncludeDirectories", {
        let name = name_owned.clone();
        move || {
            let interp = interp_at(ip);
            interp
                .ext
                .targets
                .get(&name)
                .map(|t| t.system_include_directories.iter().cloned().collect())
                .unwrap_or_default()
        }
    }) {
        target.subvars.push(node);
    }
    let name4 = name_owned.clone();
    target.subvars.push(makefile_node(ip, move || {
        let interp = interp_at(ip);
        interp
            .ext
            .targets
            .get(&name4)
            .map(|t| t.makefile)
            .unwrap_or(0)
    }));
    target.subvars.push(global_generator_node(ip));

    // `GetAllFileSetNames` (the std::map of file sets: sorted by name) and
    // `GetAllInterfaceFileSets` (the expanded INTERFACE_HEADER_SETS then
    // INTERFACE_CXX_MODULE_SETS entries).
    let (all, interface) = {
        let interp = interp_at(ip);
        match interp.ext.targets.get(&name_owned) {
            Some(t) => {
                let all: Vec<String> = t.file_sets.keys().cloned().collect();
                let mut interface = Vec::new();
                for property in ["INTERFACE_HEADER_SETS", "INTERFACE_CXX_MODULE_SETS"] {
                    for entry in target_entries(t, property) {
                        interface.extend(crate::util::expand_list(&entry, false));
                    }
                }
                (all, interface)
            }
            None => (Vec::new(), Vec::new()),
        }
    };
    for (label, names) in [("AllFileSets", all), ("AllInterfaceFileSets", interface)] {
        if names.is_empty() {
            continue;
        }
        let mut sets = TreeNode::node(label);
        for set in names {
            sets.subvars.push(file_set_node(ip, &name_owned, &set));
        }
        target.subvars.push(sets);
    }
    target
}

fn target_install_path(interp: &crate::interp::Interp, target: &crate::state::Target) -> String {
    // `cmTarget::impl->InstallPath` is filled by install() rules; the model
    // stores per-directory install destinations in the install state.
    let _ = (interp, target);
    String::new()
}

fn target_runtime_install_path(
    interp: &crate::interp::Interp,
    target: &crate::state::Target,
) -> String {
    let _ = (interp, target);
    String::new()
}

/// `CreateIfAny` for `cmMakefile*`.
fn makefile_node(ip: usize, mf_of: impl Fn() -> usize + Send + Sync + 'static) -> TreeNode {
    let mf_of = std::sync::Arc::new(mf_of);
    let mf = mf_of.clone();
    let mut node = TreeNode::leaf("Makefile", move || {
        let interp = interp_at(ip);
        makefile_entries(interp, mf())
    });
    node.ignore_empty = true;
    node.value = {
        let interp = interp_at(ip);
        directory_id(interp, mf_of())
    };
    let mf = mf_of.clone();
    if let Some(list) = create_if_any_vec("ListFiles", move || {
        let interp = interp_at(ip);
        interp.makefiles[mf()].list_files.clone()
    }) {
        node.subvars.push(list);
    }
    let mf = mf_of.clone();
    if let Some(list) = create_if_any_vec("OutputFiles", move || {
        let interp = interp_at(ip);
        interp.makefiles[mf()].output_files.clone()
    }) {
        node.subvars.push(list);
    }
    node
}

/// `cmMakefile::AppleSDK` as `GetAppleSDKType` classifies
/// `CMAKE_OSX_SYSROOT`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AppleSdk {
    MacOS,
    AppleTvOs,
    AppleTvSimulator,
    IPhoneOs,
    IPhoneSimulator,
    WatchOs,
    WatchSimulator,
    XrOs,
    XrSimulator,
}

impl AppleSdk {
    fn of(sysroot: &str) -> AppleSdk {
        let root = sysroot.to_lowercase();
        for (name, sdk) in [
            ("appletvos", AppleSdk::AppleTvOs),
            ("appletvsimulator", AppleSdk::AppleTvSimulator),
            ("iphoneos", AppleSdk::IPhoneOs),
            ("iphonesimulator", AppleSdk::IPhoneSimulator),
            ("watchos", AppleSdk::WatchOs),
            ("watchsimulator", AppleSdk::WatchSimulator),
            ("xros", AppleSdk::XrOs),
            ("xrsimulator", AppleSdk::XrSimulator),
        ] {
            if root.starts_with(name) || root.contains(&format!("/{name}")) {
                return sdk;
            }
        }
        AppleSdk::MacOS
    }

    /// The debugger's `AppleSDKTypeString` (watch/xr SDKs fall through to
    /// "Unknown").
    fn label(self) -> &'static str {
        match self {
            AppleSdk::MacOS => "MacOS",
            AppleSdk::IPhoneOs => "IPhoneOS",
            AppleSdk::IPhoneSimulator => "IPhoneSimulator",
            AppleSdk::AppleTvOs => "AppleTVOS",
            AppleSdk::AppleTvSimulator => "AppleTVSimulator",
            _ => "Unknown",
        }
    }
}

/// Target property names that `cmTarget` stores outside its
/// `cmPropertyMap` (usage-requirement entries, file-set storage and
/// computed properties), so `GetProperties().GetList()` never lists them.
fn is_unmapped_target_property(name: &str) -> bool {
    const USAGE: &[&str] = &[
        "INCLUDE_DIRECTORIES",
        "COMPILE_OPTIONS",
        "COMPILE_FEATURES",
        "COMPILE_DEFINITIONS",
        "PRECOMPILE_HEADERS",
        "SOURCES",
        "LINK_OPTIONS",
        "LINK_DIRECTORIES",
        "LINK_LIBRARIES",
        "INTERFACE_LINK_LIBRARIES",
        "INTERFACE_LINK_LIBRARIES_DIRECT",
        "INTERFACE_LINK_LIBRARIES_DIRECT_EXCLUDE",
        "IMPORTED_CXX_MODULES_INCLUDE_DIRECTORIES",
        "IMPORTED_CXX_MODULES_COMPILE_DEFINITIONS",
        "IMPORTED_CXX_MODULES_COMPILE_FEATURES",
        "IMPORTED_CXX_MODULES_COMPILE_OPTIONS",
        "IMPORTED_CXX_MODULES_LINK_LIBRARIES",
        // FileSetType::WriteProperties / FileSetEntries.
        "HEADER_DIRS",
        "HEADER_SET",
        "HEADER_SETS",
        "INTERFACE_HEADER_SETS",
        "CXX_MODULE_DIRS",
        "CXX_MODULE_SET",
        "CXX_MODULE_SETS",
        "INTERFACE_CXX_MODULE_SETS",
        // cmTarget::GetComputedProperty / read-only specials.
        "MANUALLY_ADDED_DEPENDENCIES",
        "TYPE",
        "NAME",
        "IMPORTED",
        "IMPORTED_GLOBAL",
        "BINARY_DIR",
        "SOURCE_DIR",
    ];
    USAGE.contains(&name)
        || name.starts_with("HEADER_DIRS_")
        || name.starts_with("HEADER_SET_")
        || name.starts_with("CXX_MODULE_DIRS_")
        || name.starts_with("CXX_MODULE_SET_")
}

fn target_property_map(props: &crate::props::Props) -> Vec<(String, String)> {
    props
        .iter()
        .filter(|(k, _)| !is_unmapped_target_property(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// `cmTarget::Get*Entries` for one usage-requirement property: the
/// recorded per-call entries, or the whole value as a single entry.  The
/// file-set name lists (`FileSetEntries`) hold one entry per set.
fn target_entries(target: &crate::state::Target, property: &str) -> Vec<String> {
    if property.ends_with("_SETS") {
        return target
            .props
            .get(property)
            .map(|v| crate::util::expand_list(v, false))
            .unwrap_or_default();
    }
    match target.provenance.get(property) {
        Some(entries) => entries.iter().map(|(value, _)| value.clone()).collect(),
        None => target
            .props
            .get(property)
            .map(|v| vec![v.to_owned()])
            .unwrap_or_default(),
    }
}

/// `CreateIfAny(..., cmFileSet*)`.
fn file_set_node(ip: usize, target: &str, set: &str) -> TreeNode {
    let (kind, visibility, dirs, files) = {
        let interp = interp_at(ip);
        match interp
            .ext
            .targets
            .get(target)
            .and_then(|t| t.file_sets.get(set))
        {
            Some(fs) => (
                fs.kind.clone(),
                match fs.scope {
                    crate::state::Scope::Private => "Private",
                    crate::state::Scope::Public => "Public",
                    crate::state::Scope::Interface => "Interface",
                },
                fs.base_dirs
                    .iter()
                    .map(|(v, _)| v.clone())
                    .collect::<Vec<_>>(),
                fs.files.iter().map(|(v, _)| v.clone()).collect::<Vec<_>>(),
            ),
            None => (String::new(), "Unknown", Vec::new(), Vec::new()),
        }
    };
    let name = set.to_owned();
    let mut node = TreeNode::leaf(set, move || {
        vec![
            VarEntry::str("Name", name.clone()),
            VarEntry::str("Type", kind.clone()),
            VarEntry::str("Visibility", visibility),
        ]
    });
    if let Some(list) = create_if_any_vec("Directories", move || dirs.clone()) {
        node.subvars.push(list);
    }
    if let Some(list) = create_if_any_vec("Files", move || files.clone()) {
        node.subvars.push(list);
    }
    node
}

/// `CreateIfAny(..., cmGlobalGenerator*)` for the Ninja generators.
fn global_generator_node(ip: usize) -> TreeNode {
    let name = interp_at(ip).get_safe("CMAKE_GENERATOR").to_owned();
    let multi = name == "Ninja Multi-Config";
    let label = name.clone();
    let mut node = TreeNode::leaf("GlobalGenerator", move || {
        let interp = interp_at(ip);
        // cmGlobalCommonGenerator::GetEditCacheCommand: no curses or GUI
        // dialog ships with this implementation, so only a cached
        // CMAKE_EDIT_COMMAND can name one.
        let edit = interp
            .cache
            .entries
            .get("CMAKE_EDIT_COMMAND")
            .map(|e| e.value.clone())
            .unwrap_or_default();
        vec![
            VarEntry::str("AllTargetName", "all"),
            VarEntry::str("CleanTargetName", "clean"),
            VarEntry::str("EditCacheCommand", edit),
            VarEntry::str("EditCacheTargetName", "edit_cache"),
            VarEntry::str("ExtraGeneratorName", ""),
            VarEntry::boolean("ForceUnixPaths", false),
            VarEntry::str("InstallLocalTargetName", "install/local"),
            VarEntry::str("InstallStripTargetName", "install/strip"),
            VarEntry::str("InstallTargetName", "install"),
            VarEntry::boolean("IsMultiConfig", multi),
            VarEntry::str("Name", label.clone()),
            VarEntry::str("MakefileEncoding", "None"),
            VarEntry::str("PackageSourceTargetName", "package_source"),
            VarEntry::str("PackageTargetName", "package"),
            VarEntry::str("PreinstallTargetName", ""),
            VarEntry::boolean("NeedSymbolicMark", false),
            VarEntry::str("RebuildCacheTargetName", "rebuild_cache"),
            VarEntry::str("TestTargetName", "test"),
            VarEntry::boolean("UseLinkScript", false),
        ]
    });
    // `GetInstallComponents` is never null; CreateIfAny drops it when the
    // set is empty.
    let components: Vec<String> = {
        let interp = interp_at(ip);
        interp
            .ext
            .misc
            .get("install-components")
            .map(|v| crate::util::expand_list(v, false))
            .unwrap_or_default()
    };
    if let Some(list) = create_if_any_vec("InstallComponents", move || components.clone()) {
        node.subvars.push(list);
    }
    node.ignore_empty = true;
    node.value = name;
    node
}

fn directory_id(interp: &crate::interp::Interp, mf: usize) -> String {
    // `cmMakefile::GetDirectoryId().String` is the instance pointer in
    // `(%p)` form; use the address of the modelled makefile.
    format!("({:#x})", &interp.makefiles[mf] as *const _ as usize)
}

fn makefile_entries(interp: &crate::interp::Interp, mf: usize) -> Vec<VarEntry> {
    let makefile = &interp.makefiles[mf];
    let void_p: i64 = interp.get_safe("CMAKE_SIZEOF_VOID_P").parse().unwrap_or(0);
    let is_x32 = interp.get_safe("CMAKE_INTERNAL_PLATFORM_ABI") == "ELF X32";
    let apple_sdk = AppleSdk::of(&interp.get_safe("CMAKE_OSX_SYSROOT"));
    vec![
        // cmMakefile initializes DefineFlags to " ".
        VarEntry::str(
            "DefineFlags",
            interp
                .ext
                .directory_define_flags
                .get(&mf)
                .cloned()
                .unwrap_or_else(|| " ".to_owned()),
        ),
        VarEntry::str("DirectoryId", directory_id(interp, mf)),
        VarEntry::boolean("IsRootMakefile", makefile.parent.is_none()),
        VarEntry::str("HomeDirectory", interp.home_source.clone()),
        VarEntry::str("HomeOutputDirectory", interp.home_binary.clone()),
        VarEntry::str("CurrentSourceDirectory", makefile.source_dir.clone()),
        VarEntry::str("CurrentBinaryDirectory", makefile.binary_dir.clone()),
        VarEntry::boolean("PlatformIs32Bit", !is_x32 && void_p == 4),
        VarEntry::boolean("PlatformIs64Bit", void_p == 8),
        VarEntry::boolean("PlatformIsx32", is_x32),
        VarEntry::str("AppleSDKType", apple_sdk.label()),
        VarEntry::boolean("PlatformIsAppleEmbedded", apple_sdk != AppleSdk::MacOS),
    ]
}
