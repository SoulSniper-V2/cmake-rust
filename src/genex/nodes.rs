use super::*;

impl Evaluator<'_, '_> {
    pub(super) fn node(
        &mut self,
        id: &str,
        args: &[String],
        expr: &str,
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
    ) -> Result<String, GenexError> {
        let fail = |message: &str| Err(self.error(expr, message));
        let input = args.first().map_or("", String::as_str);
        let lang = self.language.clone();
        let lang = lang.as_str();
        let value = match id {
            "1" | "BUILD_INTERFACE" | "BUILD_LOCAL_INTERFACE" | "TARGET_NAME" => input.to_owned(),
            "AND" | "OR" => {
                let (stop, keep) = if id == "AND" { ("0", "1") } else { ("1", "0") };
                for arg in args {
                    if arg == stop {
                        return Ok(stop.to_owned());
                    }
                    if arg != keep {
                        return fail(&format!(
                            "Parameters to $<{id}> must resolve to either '0' or '1'."
                        ));
                    }
                }
                keep.to_owned()
            }
            "NOT" => match input {
                "0" => "1".to_owned(),
                "1" => "0".to_owned(),
                _ => return fail("$<NOT> parameter must resolve to exactly one '0' or '1' value."),
            },
            "BOOL" => boolean(!util::is_off(input)),
            "IF" => match input {
                "1" => args[1].clone(),
                "0" => args[2].clone(),
                _ => {
                    return fail(
                        "First parameter to $<IF> must resolve to exactly one '0' or '1' value.",
                    );
                }
            },
            "STREQUAL" => boolean(input == args[1]),
            "EQUAL" => {
                let number = |arg: &str| {
                    integer(arg).ok_or_else(|| {
                        self.error(
                            expr,
                            format!("$<EQUAL> parameter {arg} is not a valid integer."),
                        )
                    })
                };
                boolean(number(input)? == number(&args[1])?)
            }
            "VERSION_EQUAL"
            | "VERSION_GREATER"
            | "VERSION_GREATER_EQUAL"
            | "VERSION_LESS"
            | "VERSION_LESS_EQUAL" => {
                let kind = match id {
                    "VERSION_GREATER" => CompareOp::Greater,
                    "VERSION_GREATER_EQUAL" => CompareOp::GreaterEqual,
                    "VERSION_LESS" => CompareOp::Less,
                    "VERSION_LESS_EQUAL" => CompareOp::LessEqual,
                    _ => CompareOp::Equal,
                };
                boolean(util::version_compare(kind, input, &args[1]))
            }
            "ANGLE-R" => ">".to_owned(),
            "COMMA" => ",".to_owned(),
            "SEMICOLON" => ";".to_owned(),
            "QUOTE" => "\"".to_owned(),
            "LOWER_CASE" => input.to_ascii_lowercase(),
            "UPPER_CASE" => input.to_ascii_uppercase(),
            "MAKE_C_IDENTIFIER" => crate::util::make_c_identifier(input),
            "JOIN" => cm_list(input, false).join(&args[1]),
            "IN_LIST" => {
                let policy = self.host.local_policy_status("CMP0085");
                let values = util::expand_list(&args[1], policy == PolicyStatus::New);
                if policy == PolicyStatus::Warn
                    && input.is_empty()
                    && values != util::expand_list(&args[1], true)
                {
                    self.policy_warning(
                        "CMP0085",
                        &format!("\nSearch Item:\n  \"{input}\"\nList:\n  \"{}\"\n", args[1]),
                    );
                }
                boolean(values.iter().any(|arg| arg == input))
            }
            "REMOVE_DUPLICATES" => list::dedup(cm_list(input, true)).join(";"),
            "FILTER" => {
                if !matches!(args[1].as_str(), "INCLUDE" | "EXCLUDE") {
                    return fail(
                        "$<FILTER:...> second parameter must be either INCLUDE or EXCLUDE",
                    );
                }
                list::filter(cm_list(input, true), &args[1], &args[2])
                    .map_err(|_| self.error(expr, "$<FILTER:...> failed to compile regex"))?
                    .join(";")
            }
            "LIST" => list::evaluate(args, self.host.local_policy_status("CMP0186"), self.host)
                .map_err(|error| self.error(expr, error))?,
            "PATH" => path::evaluate(args).map_err(|error| self.error(expr, error))?,
            "PATH_EQUAL" => boolean(path::equal(input, &args[1])),
            "CONFIG" => return self.config(args, expr, scope),
            "CONFIGURATION" => return self.config(&[], expr, scope),
            "PLATFORM_ID" => {
                let platform = self.host.variable("CMAKE_SYSTEM_NAME").unwrap_or_default();
                match args {
                    [] => platform,
                    [first, ..] if platform.is_empty() => boolean(first.is_empty()),
                    _ => boolean(args.contains(&platform)),
                }
            }
            "INSTALL_PREFIX" => {
                return fail(
                    "INSTALL_PREFIX is a marker for install(EXPORT) only.  It should never be evaluated.",
                );
            }
            "SHELL_PATH" => {
                let windows = self.host.windows_shell();
                let mut paths = cm_list(input, false);
                if paths.is_empty() {
                    return fail("\"\" is not an absolute path.");
                }
                for path in &mut paths {
                    if !path::file_is_full_path(path) {
                        return fail(&format!("\"{path}\" is not an absolute path."));
                    }
                    if self.host.msys_shell()
                        && path.as_bytes().get(1) == Some(&b':')
                        && path.len() > 2
                    {
                        let drive = path[..1].to_owned();
                        path.replace_range(..2, &format!("/{drive}"));
                    }
                    if windows {
                        *path = path.replace('/', "\\");
                    }
                }
                paths.join(if windows { ";" } else { ":" })
            }
            "TARGET_PROPERTY" => return self.target_property(args, expr, scope, dag),
            "TARGET_EXISTS" | "TARGET_NAME_IF_EXISTS" => {
                if !is_valid_target_name(input) {
                    return fail(&format!(
                        "$<{id}:tgt> expression requires a non-empty valid target name."
                    ));
                }
                let exists = self.host.resolve_target(input, None).is_some();
                if id == "TARGET_EXISTS" {
                    boolean(exists)
                } else if exists {
                    input.to_owned()
                } else {
                    String::new()
                }
            }
            "GENEX_EVAL" | "TARGET_GENEX_EVAL" => {
                return self.genex_eval(id, args, expr, scope, dag);
            }
            "TARGET_POLICY" => return self.target_policy(input, expr, scope),
            "TARGET_OBJECTS" | "TARGET_RUNTIME_DLLS" | "TARGET_RUNTIME_DLL_DIRS" => {
                return self.target_objects_or_dlls(id, input, expr);
            }
            "LINK_ONLY" | "COMPILE_ONLY" => {
                let Some(root) = dag.map(DagChecker::root) else {
                    return fail(&format!("$<{id}:...> may only be used for linking"));
                };
                let enabled = if id == "LINK_ONLY" {
                    !root.transitive_properties_only_cmp0131
                } else {
                    root.transitive_properties_only
                };
                if enabled {
                    input.to_owned()
                } else {
                    String::new()
                }
            }
            "HOST_LINK" | "DEVICE_LINK" => {
                if scope.head.is_none() || !dag.is_some_and(DagChecker::link_options) {
                    return fail(&format!(
                        "$<{id}:...> may only be used with binary targets to specify link options."
                    ));
                }
                match (id, self.ctx.device_link) {
                    ("HOST_LINK", false) => args.join(";"),
                    ("DEVICE_LINK", true) => {
                        let mut values = vec!["<DEVICE_LINK>".to_owned()];
                        values.extend(args.iter().flat_map(|arg| cm_list(arg, false)).filter(
                            |arg| !matches!(arg.as_str(), "<DEVICE_LINK>" | "</DEVICE_LINK>"),
                        ));
                        values.push("</DEVICE_LINK>".to_owned());
                        values.join(";")
                    }
                    _ => String::new(),
                }
            }
            "LINK_LIBRARY" | "LINK_GROUP" => return self.link_group(id, args, expr, scope, dag),
            "COMPILE_FEATURES" => return self.compile_features(args, expr, scope, dag),
            "COMPILE_LANGUAGE" | "COMPILE_LANG_AND_ID" | "LINK_LANGUAGE" | "LINK_LANG_AND_ID" => {
                self.check_language_node(id, args, expr, scope, dag)?;
                if id.ends_with("_AND_ID") {
                    if input == lang {
                        return self.compiler_value(lang, "ID", &args[1..], expr);
                    }
                    boolean(false)
                } else if args.is_empty() {
                    lang.to_owned()
                } else {
                    boolean(args.iter().any(|arg| arg == lang))
                }
            }
            _ if compiler_node(id).is_some() => {
                if scope.head.is_none() {
                    return fail(&format!(
                        "$<{id}> may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target."
                    ));
                }
                let (lang, kind) = compiler_node(id).unwrap();
                return self.compiler_value(lang, kind, args, expr);
            }
            _ => return self.artifact(id, input, expr, dag),
        };
        Ok(value)
    }

    fn config(
        &mut self,
        args: &[String],
        expr: &str,
        scope: Scope<'_>,
    ) -> Result<String, GenexError> {
        self.ctx.context_sensitive = true;
        let config = self.ctx.config;
        if args.is_empty() {
            return Ok(config.to_owned());
        }
        for (index, arg) in args.iter().enumerate() {
            if !identifier(arg) {
                if index == 0 {
                    return Err(self.error(expr, "Expression syntax not recognized."));
                }
                self.ctx.warnings.push(super::GenexWarning {
                    backtrace: self.ctx.backtrace,
                    dev: false,
                    text: format!("Warning evaluating generator expression:\n  {expr}\nThe config name of \"{arg}\" is invalid"),
                });
            }
            if config.eq_ignore_ascii_case(arg) {
                return Ok(boolean(true));
            }
        }
        if let Some(target) = scope.current.filter(|t| self.host.is_imported(t)) {
            if let Some(suffix) = self.host.mapped_imported_config(target, config) {
                if let Some(map) = self.host.target_property_raw(
                    target,
                    &format!("MAP_IMPORTED_CONFIG_{}", config.to_ascii_uppercase()),
                ) {
                    let mapped = cm_list(&map, false);
                    return Ok(boolean(args.iter().any(|arg| {
                        mapped.iter().any(|item| item.eq_ignore_ascii_case(arg))
                    })));
                }
                return Ok(boolean(suffix.strip_prefix('_').is_some_and(|mapped| {
                    args.iter().any(|arg| arg.eq_ignore_ascii_case(mapped))
                })));
            }
        }
        Ok(boolean(false))
    }

    fn compiler_value(
        &self,
        lang: &str,
        kind: &str,
        args: &[String],
        expr: &str,
    ) -> Result<String, GenexError> {
        let value = self
            .host
            .variable(&format!("CMAKE_{lang}_COMPILER_{kind}"))
            .unwrap_or_default();
        if args.is_empty() {
            return Ok(value);
        }
        if kind == "VERSION" && !args[0].bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            return Err(self.error(expr, "Expression syntax not recognized."));
        }
        if value.is_empty() {
            return Ok(boolean(args[0].is_empty()));
        }
        if kind == "VERSION" {
            return Ok(boolean(util::version_compare(
                CompareOp::Equal,
                &args[0],
                &value,
            )));
        }
        for arg in args {
            if !identifier(arg) {
                return Err(self.error(expr, "Expression syntax not recognized."));
            }
            if *arg == value {
                return Ok(boolean(true));
            }
        }
        Ok(boolean(false))
    }

    fn check_language_node(
        &mut self,
        id: &str,
        args: &[String],
        expr: &str,
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
    ) -> Result<(), GenexError> {
        let missing_compile =
            self.language.is_empty() && !dag.is_some_and(DagChecker::compile_expression);
        let missing_link = scope.head.is_none() || !dag.is_some_and(DagChecker::link_language);
        let problem = match id {
            "COMPILE_LANGUAGE" if missing_compile => {
                "$<COMPILE_LANGUAGE:...> may only be used to specify include directories, compile definitions, compile options, and to evaluate components of the file(GENERATE) command."
            }
            "COMPILE_LANG_AND_ID" if scope.head.is_none() || missing_compile => {
                "$<COMPILE_LANG_AND_ID:lang,id> may only be used with binary targets to specify include directories, compile definitions, and compile options.  It may not be used with the add_custom_command, add_custom_target, or file(GENERATE) commands."
            }
            "LINK_LANGUAGE" if missing_link => {
                "$<LINK_LANGUAGE:...> may only be used with binary targets to specify link libraries, link directories, link options and link depends."
            }
            "LINK_LANG_AND_ID" if missing_link => {
                "$<LINK_LANG_AND_ID:lang,id> may only be used with binary targets to specify link libraries, link directories, link options, and link depends."
            }
            _ => "",
        };
        if !problem.is_empty() {
            return Err(self.error(expr, problem));
        }
        let linking = id.starts_with("LINK_") && dag.is_some_and(|d| d.link_libraries(false));
        if linking && args.is_empty() {
            return Err(self.error(
                expr,
                "$<LINK_LANGUAGE> is not supported in link libraries expression.",
            ));
        }
        let generator = self.host.generator_name();
        if ![
            "Makefiles",
            "Ninja",
            "Visual Studio",
            "Xcode",
            "Watcom WMake",
            "Green Hills MULTI",
        ]
        .iter()
        .any(|name| generator.contains(name))
        {
            let syntax = if id.ends_with("_AND_ID") {
                "lang,id"
            } else {
                "..."
            };
            return Err(self.error(
                expr,
                format!("$<{id}:{syntax}> not supported for this generator."),
            ));
        }
        if linking {
            self.ctx.head_sensitive = true;
            self.ctx.link_language_sensitive = true;
        }
        Ok(())
    }

    fn target_policy(
        &mut self,
        input: &str,
        expr: &str,
        scope: Scope<'_>,
    ) -> Result<String, GenexError> {
        let target = scope.head.ok_or_else(|| self.error(expr,
            "$<TARGET_POLICY:prop> may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target."))?;
        self.ctx.head_sensitive = true;
        self.ctx.context_sensitive = true;
        if !tables::TARGET_POLICIES.contains(&input) {
            let mut message = String::from(
                "$<TARGET_POLICY:prop> may only be used with a limited number of policies.  Currently it may be used with the following policies:\n",
            );
            for policy in tables::TARGET_POLICIES {
                message.push_str(&format!(" * {policy}\n"));
            }
            return Err(self.error(expr, message));
        }
        let policy = self.host.policy_status(target, input);
        if policy == PolicyStatus::Warn {
            self.policy_warning(input, "");
        }
        Ok(boolean(policy == PolicyStatus::New))
    }

    fn compile_features(
        &mut self,
        args: &[String],
        expr: &str,
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
    ) -> Result<String, GenexError> {
        let target = scope.head.ok_or_else(|| self.error(expr,
            "$<COMPILE_FEATURE> may only be used with binary targets.  It may not be used with add_custom_command or add_custom_target."))?;
        self.ctx.head_sensitive = true;
        let mut tested = BTreeMap::<&str, Vec<&str>>::new();
        let mut available = BTreeMap::new();
        for feature in args {
            let lang = [
                ("C", tables::C_FEATURES),
                ("CXX", tables::CXX_FEATURES),
                ("CUDA", tables::CUDA_FEATURES),
                ("HIP", tables::HIP_FEATURES),
            ]
            .into_iter()
            .find_map(|(lang, features)| features.contains(&feature.as_str()).then_some(lang))
            .ok_or_else(|| {
                self.error(
                    expr,
                    format!("specified unknown feature \"{feature}\" for target \"{target}\"."),
                )
            })?;
            tested.entry(lang).or_default().push(feature);
            if let std::collections::btree_map::Entry::Vacant(entry) = available.entry(lang) {
                let value = self
                    .host
                    .variable(&format!("CMAKE_{lang}_COMPILE_FEATURES"))
                    .unwrap_or_default();
                if !self.host.language_enabled(lang) {
                    return Err(self.error(
                        expr,
                        format!("cannot use features from non-enabled language {lang}"),
                    ));
                }
                if value.is_empty() {
                    let id = self
                        .host
                        .variable(&format!("CMAKE_{lang}_COMPILER_ID"))
                        .unwrap_or_default();
                    let version = self
                        .host
                        .variable(&format!("CMAKE_{lang}_COMPILER_VERSION"))
                        .unwrap_or_default();
                    return Err(self.error(
                        expr,
                        format!(
                            "no known features for {lang} compiler\n\"{id}\"\nversion {version}."
                        ),
                    ));
                }
                entry.insert(cm_list(&value, false));
            }
        }
        for (lang, features) in tested {
            for feature in features {
                if !available[lang].iter().any(|item| item == feature) {
                    return Ok(boolean(false));
                }
                if self
                    .host
                    .variable(&format!("CMAKE_{lang}_STANDARD_DEFAULT"))
                    .as_deref()
                    == Some("")
                {
                    continue;
                }
                if let Err(standard) =
                    self.host
                        .standard_for_feature(target, self.ctx.config, lang, feature)
                {
                    if !dag.is_some_and(|d| d.link_libraries(false)) {
                        return Ok(boolean(false));
                    }
                    self.ctx
                        .max_language_standard
                        .insert((target.to_owned(), lang.to_owned()), standard);
                }
            }
        }
        Ok(boolean(true))
    }

    fn link_group(
        &self,
        id: &str,
        args: &[String],
        expr: &str,
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
    ) -> Result<String, GenexError> {
        let group = id == "LINK_GROUP";
        if scope.head.is_none() || !dag.is_some_and(|d| d.link_libraries(true)) {
            let noun = if group {
                "group of link libraries"
            } else {
                "link libraries"
            };
            return Err(self.error(expr, format!("$<{id}:...> may only be used with binary targets to specify {noun} through 'LINK_LIBRARIES', 'INTERFACE_LINK_LIBRARIES', and 'INTERFACE_LINK_LIBRARIES_DIRECT' properties.")));
        }
        let mut list: Vec<_> = args.iter().flat_map(|arg| cm_list(arg, false)).collect();
        let feature = list
            .first()
            .ok_or_else(|| {
                self.error(
                    expr,
                    format!("$<{id}:...> expects a feature name as first argument."),
                )
            })?
            .clone();
        if group && list.iter().any(|item| item.starts_with("<LINK_GROUP")) {
            return Err(self.error(expr, "$<LINK_GROUP:...> cannot be nested."));
        }
        if list.len() == 1 {
            return Ok(String::new());
        }
        if !identifier(&feature) {
            return Err(self.error(
                expr,
                format!("The feature name '{feature}' contains invalid characters."),
            ));
        }
        let end = format!("</{id}:{feature}>");
        if group {
            list[0] = format!("<LINK_GROUP:{feature}:{}>", list[1..].join("|"));
        } else {
            let start = format!("<LINK_LIBRARY:{feature}>");
            list.retain(|item| item != &start && item != &end);
            if list[1..]
                .iter()
                .any(|item| item.starts_with("<LINK_LIBRARY:"))
            {
                return Err(self.error(
                    expr,
                    "$<LINK_LIBRARY:...> with different features cannot be nested.",
                ));
            }
            if list[1..]
                .iter()
                .any(|item| item.starts_with("<LINK_GROUP:"))
            {
                return Err(self.error(
                    expr,
                    "$<LINK_GROUP:...> cannot be nested inside a $<LINK_LIBRARY:...> expression.",
                ));
            }
            list[0] = start;
        }
        list.push(end);
        Ok(list.join(";"))
    }

    fn genex_eval(
        &mut self,
        id: &str,
        args: &[String],
        expr: &str,
        scope: Scope<'_>,
        dag: Option<&DagChecker>,
    ) -> Result<String, GenexError> {
        let target;
        let (scope, input) = if id == "TARGET_GENEX_EVAL" {
            let name = &args[0];
            if !is_valid_target_name(name) {
                return Err(self.error(expr, "$<TARGET_GENEX_EVAL:tgt, ...> expression requires a non-empty valid target name."));
            }
            target = self.host.resolve_target(name, None).ok_or_else(|| {
                self.error(
                    expr,
                    format!("$<TARGET_GENEX_EVAL:tgt, ...> target \"{name}\" not found."),
                )
            })?;
            (Scope::target(&target), args[1].as_str())
        } else {
            (scope, args[0].as_str())
        };
        let Some(target) = scope.head.filter(|_| !input.is_empty()) else {
            return self.dependent(input, scope, dag);
        };
        let property = format!("{id}:{input}");
        let frame = DagChecker {
            parent: dag,
            expression: Some(expr),
            backtrace: self.ctx.backtrace,
            ..DagChecker::new(target, &property)
        };
        if matches!(
            self.check(&frame),
            dag::Check::SelfReference | dag::Check::Cycle
        ) {
            return Err(self.cycle_error(&frame, expr, scope));
        }
        self.dependent(input, scope, Some(&frame))
    }
}

// strtol with CMake's extra binary prefix. A binary prefix is recognized only
// at the start of the argument (after an optional sign).
fn integer(input: &str) -> Option<i64> {
    let signed = input.strip_prefix(['-', '+']).unwrap_or(input);
    if let Some(binary) = signed
        .strip_prefix("0b")
        .or_else(|| signed.strip_prefix("0B"))
    {
        let binary = binary.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\u{b}');
        let number = i64::from_str_radix(binary, 2).ok()?;
        return Some(if input.starts_with('-') && number > 0 {
            -number
        } else {
            number
        });
    }
    let input = input.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '\u{b}');
    let digits = input.strip_prefix(['-', '+']).unwrap_or(input);
    let (digits, radix) = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (hex, 16)
    } else if digits.starts_with('0') {
        (digits, 8)
    } else {
        (digits, 10)
    };
    if digits.starts_with(['+', '-']) {
        return None;
    }
    let magnitude = u64::from_str_radix(digits, radix).ok()?;
    if input.starts_with('-') {
        i64::try_from(-i128::from(magnitude)).ok()
    } else {
        i64::try_from(magnitude).ok()
    }
}
