//! Explicit preprocessing, dependency scanning, and module collation edges
//! from cmNinjaTargetGenerator.

use std::path::Path;

use md5::{Digest, Md5};
use serde_json::{Value, json};

use super::*;
use crate::commands::string::json::stream_value;
use crate::generator::export::{export_name, filesystem_export_name};
use crate::install_model::InstallAction;
use crate::props::Props;

impl<'a> NinjaGenerator<'a> {
    fn fortran_rule_name(&self, target: &Target, step: &str) -> String {
        format!(
            "Fortran_{step}__{}_{}",
            encode_rule_name(&target.name),
            self.config
        )
    }

    pub(super) fn fortran_compile_with_defines(&self, target: &Target) -> bool {
        self.ctx
            .var(target.makefile, "CMAKE_Fortran_COMPILE_WITH_DEFINES")
            .is_some_and(|s| util::is_on(&s))
    }

    pub(super) fn fortran_rsp_flag(&self, target: &Target) -> Option<String> {
        let key = "CMAKE_NINJA_FORCE_RESPONSE_FILE";
        (self.ctx.var(target.makefile, key).is_some() || std::env::var_os(key).is_some()).then(
            || {
                self.ctx
                    .var(target.makefile, "CMAKE_Fortran_RESPONSE_FILE_FLAG")
                    .filter(|flag| !flag.is_empty())
                    .unwrap_or_else(|| "@".into())
            },
        )
    }

    pub(super) fn write_fortran_scan_rules(&mut self, target: &Target) {
        let tdi = self.shell(&self.ninja_path(&format!(
            "{}/FortranDependInfo.json",
            self.object_dir(target)
        )));
        let cmake = self.cmake_cmd.clone();
        let scan = format!("{cmake} -E cmake_ninja_depends --tdi={tdi} --lang=Fortran");
        let scan_deps = self
            .ctx
            .var_safe(target.makefile, "CMAKE_Fortran_SCANDEP_DEPFILE_FORMAT");
        let depfile = if scan_deps == "msvc" { "" } else { "$DEP_FILE" };
        let deptype = if scan_deps == "msvc" { "msvc" } else { "" };
        let launcher = rule_launcher(&self.ctx, target, "RULE_LAUNCH_COMPILE");
        let pp = self
            .ctx
            .var_safe(target.makefile, "CMAKE_Fortran_PREPROCESS_SOURCE");
        if pp.is_empty() {
            self.ctx.error(
                target.backtrace.clone(),
                "Missing CMAKE_Fortran_PREPROCESS_SOURCE rule for Ninja".to_owned(),
            );
            return;
        }
        let mut rule = Rule {
            name: self.fortran_rule_name(target, "PREPROCESS_SCAN"),
            comment: "Rule for generating Fortran dependencies.".into(),
            description: "Building Fortran preprocessed $out".into(),
            depfile: depfile.into(),
            deptype: deptype.into(),
            ..Rule::default()
        };
        let (mut defines, mut includes, mut flags) = ("$DEFINES", "$INCLUDES", "$FLAGS".to_owned());
        if let Some(flag) = self.fortran_rsp_flag(target) {
            rule.rspfile = "$RSP_FILE".into();
            rule.rspcontent = format!(" {defines} {includes} {flags}");
            flags = format!("{flag}$RSP_FILE");
            defines = "";
            includes = "";
        }
        let vars = [
            ("TARGET_NAME", target.name.clone()),
            ("TARGET_TYPE", target.kind.type_name().into()),
            ("LANGUAGE", "Fortran".into()),
            ("SOURCE", "$in".into()),
            ("OBJECT", "$OBJ_FILE".into()),
            ("PREPROCESSED_SOURCE", "$out".into()),
            ("DEP_FILE", depfile.into()),
            ("DEP_TARGET", "$out".into()),
            ("CONFIG", "$CONFIG".into()),
            ("DEFINES", defines.into()),
            ("INCLUDES", includes.into()),
            ("FLAGS", flags),
        ];
        let mut cmds: Vec<_> = util::expand_list(&pp, false)
            .iter()
            .map(|cmd| flags::expand_rule(&self.ctx, target, &format!("{launcher}{cmd}"), &vars))
            .collect();
        cmds.push(format!("{scan} --src=$out --out=$out --dep=$DEP_FILE --obj=$OBJ_FILE --ddi=$DYNDEP_INTERMEDIATE_FILE --src-orig=$in"));
        rule.command = self.build_command_line(&cmds);
        self.add_rule(&rule);
        self.add_rule(&Rule {
            name: self.fortran_rule_name(target, "SCAN"),
            comment: "Rule for generating Fortran dependencies on non-preprocessed files.".into(),
            description: "Generating Fortran dependencies for $in".into(),
            command: format!(
                "{scan} --src=$in --out=$out --dep=$DEP_FILE --obj=$OBJ_FILE --ddi=$out"
            ),
            depfile: depfile.into(),
            deptype: deptype.into(),
            ..Rule::default()
        });
        self.add_rule(&Rule {
            name: self.fortran_rule_name(target, "DYNDEP"),
            comment: "Rule to generate ninja dyndep files for Fortran.".into(),
            description: "Generating Fortran dyndep file $out".into(),
            command: format!(
                "{cmake} -E cmake_ninja_dyndep --tdi={tdi} --lang=Fortran --dd=$out @$out.rsp"
            ),
            rspfile: "$out.rsp".into(),
            rspcontent: "$in".into(),
            restat: "1".into(),
            ..Rule::default()
        });
    }

    pub(super) fn write_fortran_scan(
        &self,
        out: &mut String,
        target: &Target,
        source: &gt::SourceInfo,
        props: &Props,
        obj: &mut Build,
    ) -> String {
        let object = &obj.outputs[0];
        let ddi = format!("{object}.ddi");
        let pp = flags::fortran_preprocess(target, props) != Some(false);
        let mut scan = Build::new(
            &self.fortran_rule_name(target, if pp { "PREPROCESS_SCAN" } else { "SCAN" }),
        );
        for key in ["FLAGS", "DEFINES", "INCLUDES"] {
            scan.variables
                .insert(key.into(), obj.variables[key].clone());
        }
        if pp {
            let mut ext = util::filename_last_extension(&source.path)
                .trim_start_matches('.')
                .to_owned();
            if ext.starts_with('F') {
                ext.make_ascii_lowercase();
            }
            if ext == "fpp" {
                ext = "f".into();
            }
            let suffix = self
                .ctx
                .var_safe(target.makefile, "CMAKE_Fortran_OUTPUT_EXTENSION");
            let pp_file = format!(
                "{}-pp.{ext}",
                object.strip_suffix(&suffix).unwrap_or(object)
            );
            scan.explicit_deps = std::mem::replace(&mut obj.explicit_deps, vec![pp_file.clone()]);
            scan.implicit_deps = std::mem::take(&mut obj.implicit_deps);
            scan.order_only_deps = std::mem::take(&mut obj.order_only_deps);
            scan.outputs.push(pp_file);
            scan.implicit_outs.push(ddi.clone());
            obj.variables.remove("DEP_FILE");
            obj.variables.remove("depfile");
            if !self.fortran_compile_with_defines(target) {
                obj.variables.remove("DEFINES");
            }
            let post = self
                .ctx
                .var_safe(target.makefile, "CMAKE_Fortran_POSTPROCESS_FLAG");
            flags::append(obj.variables.get_mut("FLAGS").unwrap(), &post);
            let src_inc =
                flags::fortran_source_include(&self.ctx, target, util::filename_path(&source.path));
            obj.variables.insert(
                "INCLUDES".into(),
                format!("{src_inc} {}", scan.variables["INCLUDES"]),
            );
        } else {
            scan.explicit_deps.clone_from(&obj.explicit_deps);
            scan.implicit_deps.clone_from(&obj.implicit_deps);
            scan.order_only_deps.clone_from(&obj.order_only_deps);
            scan.outputs.push(ddi.clone());
            scan.variables
                .insert("PREPROCESSED_OUTPUT_FILE".into(), format!("{ddi}.i"));
            obj.implicit_deps.push(ddi.clone());
        }
        scan.rsp_file = format!("{ddi}.rsp");
        scan.variables.insert("OBJ_FILE".into(), object.clone());
        scan.variables
            .insert("DYNDEP_INTERMEDIATE_FILE".into(), ddi.clone());
        self.add_depfile_binding(&mut scan.variables, format!("{}.d", scan.outputs[0]));
        if let Some(pool) = target.props.get("JOB_POOL_COMPILE") {
            for build in [&mut scan, &mut *obj] {
                build.variables.insert("pool".into(), pool.into());
            }
        }
        let dd = self.ninja_path(&format!("{}/Fortran.dd", self.object_dir(target)));
        obj.order_only_deps.push(dd.clone());
        obj.variables.insert("dyndep".into(), dd);
        write_build(
            out,
            &scan,
            if self.fortran_rsp_flag(target).is_some() {
                -1
            } else {
                0
            },
        );
        ddi
    }

    pub(super) fn has_fortran_sources(&self, target: &Target) -> bool {
        let sources = gt::sources(&self.ctx, target, &self.config);
        sources.iter().any(|s| s.language == "Fortran")
            || sources
                .iter()
                .filter_map(|s| s.object_library.as_ref())
                .filter_map(|n| self.ctx.target(n))
                .any(|dep| self.has_fortran_sources(dep))
    }

    fn module_target_order(&mut self) {
        fn visit<'a>(
            ctx: &GenCtx<'a>,
            t: &'a Target,
            seen: &mut HashMap<String, usize>,
            index: &mut usize,
        ) {
            if seen.contains_key(&t.name) {
                return;
            }
            seen.insert(t.name.clone(), 0);
            for (dep, _) in link::target_graph::dep_edges(ctx, t, &ctx.config) {
                visit(ctx, dep, seen, index);
            }
            seen.insert(t.name.clone(), *index);
            *index += 1;
        }
        let mut targets = self.ctx.targets_in_order();
        targets.sort_by_key(|t| t.makefile);
        let mut index = 0;
        for target in targets {
            if !target.imported {
                visit(&self.ctx, target, &mut self.module_order, &mut index);
            }
        }
    }

    /// cmDyndepCollation includes export and installation information in
    /// every language's TDI, even when the build-time writer won't need it.
    fn fortran_export_info(&self, target: &Target) -> (Vec<Value>, Value) {
        let install = &self.ctx.interp.ext.install;
        let contains = |set: &str| {
            install
                .export_sets
                .get(set)
                .is_some_and(|s| s.targets.iter().any(|t| t.name == target.name))
        };
        let entry = |namespace: &str, modules: &str, dest: &str, prefix: &str, installed| {
            json!({"namespace":namespace, "export-name":export_name(target),
                "filesystem-export-name":filesystem_export_name(target), "destination":dest,
                "cxx-module-info-dir":modules, "export-prefix":prefix,"install":installed})
        };
        let mut exports = Vec::new();
        for name in install.export_sets.keys().filter(|key| contains(key)) {
            for (&dir, rules) in &install.rules {
                for rule in rules {
                    if let InstallAction::Export(e) = &rule.action
                        && e.set == *name
                    {
                        let dest = &rule.options.destination;
                        let binary = &self.ctx.interp.makefiles[dir].binary_dir;
                        let prefix = format!(
                            "{binary}/CMakeFiles/Export/{:x}",
                            Md5::digest(dest.as_bytes())
                        );
                        exports.push(entry(
                            &e.options.namespace,
                            &e.options.modules_directory,
                            dest,
                            &prefix,
                            true,
                        ));
                    }
                }
            }
        }
        for (path, e) in &install.build_exports {
            if e.set.as_deref().is_some_and(contains) || e.targets.contains(&target.name) {
                let dir = util::filename_path(path);
                exports.push(entry(
                    &e.options.namespace,
                    &e.options.modules_directory,
                    &dir,
                    &dir,
                    false,
                ));
            }
        }
        let mut bmi = Value::Null;
        for rule in install.rules.get(&target.makefile).into_iter().flatten() {
            if matches!(&rule.action, InstallAction::Bmi(name) if name == &target.name) {
                let config = if self.config.is_empty() {
                    "noconfig"
                } else {
                    &self.config
                };
                let permissions: String = rule
                    .options
                    .permissions
                    .iter()
                    .map(|p| format!(" {p}"))
                    .collect();
                bmi = json!({"permissions":permissions,"destination":self.ctx.genex(&rule.options.destination, Some(target), None),
                    "message-level":rule.options.message,"script-location":format!("{}/install-cxx-module-bmi-{config}.cmake",self.object_dir(target))});
            }
        }
        (exports, bmi)
    }

    pub(super) fn write_fortran_dyndep(
        &mut self,
        out: &mut String,
        target: &Target,
        sources: &[gt::SourceInfo],
        ddis: Vec<String>,
    ) {
        if self.module_order.is_empty() {
            self.module_target_order();
        }
        let object_dir = self.object_dir(target);
        let tdi_path = format!("{object_dir}/FortranDependInfo.json");
        let mf = &self.ctx.interp.makefiles[target.makefile];
        let mut direct = Vec::new();
        let mut forward = Vec::new();
        let lang = gt::linker_language(&self.ctx, target, &self.config);
        let linked = link::module_link_targets(&self.ctx, target, &lang);
        let objects = sources
            .iter()
            .filter_map(|s| s.object_library.as_ref())
            .filter_map(|n| self.ctx.target(n))
            .map(|t| (t, true));
        for (dep, fwd) in linked.into_iter().chain(objects) {
            if !dep.imported
                && dep.kind != TargetKind::InterfaceLibrary
                && self.module_order.get(&dep.name) < self.module_order.get(&target.name)
                && self.has_fortran_sources(dep)
            {
                let dir = self.object_dir(dep);
                if fwd && !forward.contains(&dir) {
                    forward.push(dir.clone());
                }
                if !direct.contains(&dir) {
                    direct.push(dir);
                }
            }
        }
        let var = |suffix| {
            self.ctx
                .var_safe(target.makefile, &format!("CMAKE_Fortran_{suffix}"))
        };
        let includes: Vec<_> = flags::include_items(&self.ctx, target, &self.config, "Fortran")
            .iter()
            .map(|(dir, _, _)| self.ninja_path(dir))
            .collect();
        let mut tdi_sources = json!({});
        for s in sources
            .iter()
            .filter(|s| s.kind == SourceKind::ObjectSource && s.language == "Fortran")
        {
            let object = self.ninja_path(&format!("{object_dir}/{}", s.object_name));
            tdi_sources[object] = json!({"language":"Fortran","source":s.path});
        }
        let (exports, bmi) = self.fortran_export_info(target);
        let tdi = json!({
            "language":"Fortran", "compiler-id":var("COMPILER_ID"), "compiler-simulate-id":var("SIMULATE_ID"),
            "compiler-frontend-variant":var("COMPILER_FRONTEND_VARIANT"), "submodule-sep":var("SUBMODULE_SEP"),
            "submodule-ext":var("SUBMODULE_EXT"), "module-dir":flags::fortran_module_dir(&self.ctx,target).unwrap_or_else(|| mf.binary_dir.clone()),
            "dir-cur-src":mf.source_dir,"dir-cur-bld":mf.binary_dir,"dir-top-bld":self.home_binary,"dir-top-src":self.home_source,
            "include-dirs":includes,"linked-target-dirs":direct,"forward-modules-from-target-dirs":forward,
            "sources":tdi_sources,"cxx-modules":{},"database-info":null,"bmi-installation":bmi,"exports":exports,"config":self.config
        });
        if let Err(err) =
            crate::fsutil::write_generated(Path::new(&tdi_path), stream_value(&tdi).as_bytes())
        {
            self.ctx.error(
                target.backtrace.clone(),
                format!("Failed to write {tdi_path}: {err}"),
            );
        }
        let mut build = Build::new(&self.fortran_rule_name(target, "DYNDEP"));
        build
            .outputs
            .push(self.ninja_path(&format!("{object_dir}/Fortran.dd")));
        build
            .implicit_outs
            .push(self.ninja_path(&format!("{object_dir}/FortranModules.json")));
        build.explicit_deps = ddis;
        build.implicit_deps.push(tdi_path);
        build.implicit_deps.extend(
            direct
                .iter()
                .chain(&forward)
                .map(|dir| self.ninja_path(&format!("{dir}/FortranModules.json"))),
        );
        write_build(out, &build, 0);
    }
}
