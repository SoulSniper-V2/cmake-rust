//! Prefix collection and ordered config-file search.

use std::cmp::Ordering;
use std::fs;
use std::io::{BufRead, BufReader};

use super::*;

const FRAMEWORK_AND_APP_PATHS: [&str; 2] = ["CMAKE_FRAMEWORK_PATH", "CMAKE_APPBUNDLE_PATH"];

#[derive(Clone, Copy)]
enum Segment<'a> {
    Exact(&'a str),
    Common,
    Package,
    /// Case-insensitive whole-name match (CPS package directories).
    PackageExact,
    CaseInsensitive(&'a str),
    Bundle(&'a str),
    Any,
}

struct Layouts<'a> {
    names: &'a [String],
    common: Vec<String>,
    order: String,
    descending: bool,
}

impl Layouts<'_> {
    fn candidates(&self, parent: &str, segment: Segment<'_>) -> Vec<String> {
        let entries = match segment {
            Segment::Exact(name) => return vec![format!("{parent}{name}/")],
            Segment::Common => {
                return self
                    .common
                    .iter()
                    .map(|name| format!("{parent}{name}/"))
                    .collect();
            }
            _ => directory_names(parent).filter(|entry| {
                let matches = match segment {
                    Segment::CaseInsensitive(name) => entry.eq_ignore_ascii_case(name),
                    Segment::Package => {
                        let entry = entry.to_ascii_lowercase();
                        self.names
                            .iter()
                            .any(|name| entry.starts_with(&name.to_ascii_lowercase()))
                    }
                    Segment::PackageExact => self
                        .names
                        .iter()
                        .any(|name| entry.eq_ignore_ascii_case(name)),
                    Segment::Bundle(extension) => self
                        .names
                        .iter()
                        .any(|name| entry.eq_ignore_ascii_case(&format!("{name}{extension}"))),
                    _ => true,
                };
                matches && Path::new(&format!("{parent}{entry}")).is_dir()
            }),
        };
        let mut entries: Vec<_> = entries.collect();
        if matches!(segment, Segment::Package | Segment::PackageExact | Segment::Any) {
            match self.order.as_str() {
                "NAME" => entries.sort_unstable(),
                "NATURAL" => entries
                    .sort_unstable_by(|left, right| natural(left.as_bytes(), right.as_bytes())),
                _ => {
                    return entries
                        .into_iter()
                        .map(|name| format!("{parent}{name}/"))
                        .collect();
                }
            }
            if self.descending {
                entries.reverse();
            }
        }
        entries
            .into_iter()
            .map(|name| format!("{parent}{name}/"))
            .collect()
    }
}

impl Package {
    fn search_prefixes(&mut self, interp: &Interp) -> Vec<String> {
        let mut paths = SearchPaths::default();
        paths.add(interp.get_safe("CMAKE_FIND_PACKAGE_REDIRECTS_DIR"), "");
        let mut debug_start = self.prefix_debug(
            &paths,
            0,
            "The internally managed CMAKE_FIND_PACKAGE_REDIRECTS_DIR.",
        );
        if self.options.uses(ROOT) {
            paths.extend(root_paths(interp).cloned());
            debug_start = self.prefix_debug(
                &paths,
                debug_start,
                "<PackageName>_ROOT CMake variable [CMAKE_FIND_USE_PACKAGE_ROOT_PATH].",
            );
        }
        if self.options.uses(CMAKE) {
            paths.extend(cmake_paths(interp, "CMAKE_PREFIX_PATH"));
            let group_empty = paths.len() == debug_start;
            let next = self.prefix_debug(
                &paths,
                debug_start,
                "CMAKE_PREFIX_PATH variable [CMAKE_FIND_USE_CMAKE_PATH].",
            );
            paths.extend(
                FRAMEWORK_AND_APP_PATHS
                    .iter()
                    .flat_map(|name| cmake_paths(interp, name)),
            );
            debug_start = self.prefix_debug_group(&paths, next, group_empty, "CMAKE_FRAMEWORK_PATH and CMAKE_APPBUNDLE_PATH variables [CMAKE_FIND_USE_CMAKE_PATH].");
        }
        if self.options.uses(CMAKE_ENV) {
            let group_start = debug_start;
            paths.extend(env_paths(&self.dir));
            let next = self.prefix_debug(
                &paths,
                debug_start,
                &format!(
                    "Env variable {} [CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH].",
                    self.dir
                ),
            );
            paths.extend(env_paths("CMAKE_PREFIX_PATH"));
            let next = self.prefix_debug_group(
                &paths,
                next,
                paths.len() == group_start,
                "CMAKE_PREFIX_PATH env variable [CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH].",
            );
            let empty = paths.len() == group_start;
            paths.extend(
                FRAMEWORK_AND_APP_PATHS
                    .iter()
                    .flat_map(|name| env_paths(name)),
            );
            debug_start = self.prefix_debug_group(&paths, next, empty, "CMAKE_FRAMEWORK_PATH and CMAKE_APPBUNDLE_PATH env variables [CMAKE_FIND_USE_CMAKE_ENVIRONMENT_PATH].");
        }
        paths.extend(self.hints.iter().flat_map(|path| user_paths(interp, path)));
        debug_start = self.prefix_debug(
            &paths,
            debug_start,
            "Paths specified by the find_package HINTS option.",
        );
        if self.options.uses(SYSTEM_ENV) {
            paths.extend(env_paths("PATH").into_iter().map(|path| {
                if path.ends_with("/bin") || path.ends_with("/sbin") {
                    filename_path(&path)
                } else {
                    path
                }
            }));
            debug_start = self.prefix_debug(
                &paths,
                debug_start,
                "Standard system environment variables [CMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH].",
            );
        }
        if !self.options.no_default && !self.no_user_registry {
            paths.extend(user_registry(&self.name));
            debug_start = self.prefix_debug(
                &paths,
                debug_start,
                "CMake User Package Registry [CMAKE_FIND_USE_PACKAGE_REGISTRY].",
            );
        }
        if self.options.uses(SYSTEM) {
            paths.extend(self.options.system_prefixes(interp, true));
            for name in ["CMAKE_SYSTEM_FRAMEWORK_PATH", "CMAKE_SYSTEM_APPBUNDLE_PATH"] {
                paths.extend(cmake_paths(interp, name));
            }
            debug_start = self.prefix_debug(
                &paths,
                debug_start,
                "CMake variables defined in the Platform file [CMAKE_FIND_USE_CMAKE_SYSTEM_PATH].",
            );
        }
        if !self.options.no_default && !self.no_system_registry {
            debug_start = self.prefix_debug(
                &paths,
                debug_start,
                "CMake System Package Registry [CMAKE_FIND_PACKAGE_NO_SYSTEM_PACKAGE_REGISTRY].",
            );
        }
        paths.extend(self.paths.iter().flat_map(|path| user_paths(interp, path)));
        self.prefix_debug(
            &paths,
            debug_start,
            "Paths specified by the find_package PATHS option.",
        );
        self.options
            .reroot(interp, paths.into_paths(&[], &[]), self.debug.as_mut())
            .into_iter()
            .filter(|path| !path.is_empty())
            .map(trailing_slash)
            .collect()
    }

    fn prefix_debug(&mut self, paths: &SearchPaths, start: usize, label: &str) -> usize {
        self.prefix_debug_group(paths, start, true, label)
    }

    fn prefix_debug_group(
        &mut self,
        paths: &SearchPaths,
        start: usize,
        show_none: bool,
        label: &str,
    ) -> usize {
        if let Some(debug) = &mut self.debug {
            if paths.len() == start && !show_none {
                let _ = writeln!(debug, "{label}");
            } else {
                debug_paths(debug, label, &paths.since(start));
            }
        }
        paths.len()
    }

    pub(super) fn search_config(&mut self, interp: &mut Interp) -> Option<String> {
        let paths = self.search_prefixes(interp);
        let ignore = self
            .options
            .reroot(interp, ignored_paths(interp, false), None);
        let ignore_prefix = self
            .options
            .reroot(interp, ignored_paths(interp, true), None);
        self.append_debug(&format!(
            "find_package considered the following locations for {}'s Config module:\n",
            self.name
        ));

        let names = self.names.clone();
        let mut common = Vec::new();
        let arch = interp.get_safe("CMAKE_LIBRARY_ARCHITECTURE");
        if !arch.is_empty() {
            common.push(format!("lib/{arch}"));
        }
        common.extend(lib_path_variants(interp).map(|variant| format!("lib{variant}")));
        common.extend(["lib".into(), "share".into()]);
        let layouts = Layouts {
            names: &names,
            common,
            order: interp.get_safe("CMAKE_FIND_PACKAGE_SORT_ORDER"),
            descending: interp.get_def("CMAKE_FIND_PACKAGE_SORT_DIRECTION") == Some("DEC"),
        };

        use Segment::{Any, Bundle, CaseInsensitive, Common, Exact, Package as PackageDir};
        use Segment::PackageExact;
        let cmake = CaseInsensitive("cmake");
        let cps_ci = CaseInsensitive("cps");
        let lib_cmake = Exact("cmake");
        let cps_lit = Exact("cps");
        let framework = Bundle(".framework");
        let app = Bundle(".app");
        let resources = Exact("Resources");
        let versions = Exact("Versions");
        let app_resources = Exact("Contents/Resources");
        let normal: &[(&[Segment<'_>], Desc)] = &[
            // PREFIX/(Foo|foo|FOO)/(cps|CPS)/
            (&[PackageExact, cps_ci], Desc::Cps),
            // PREFIX/(Foo|foo|FOO)/(cps|CPS)/*/
            (&[PackageExact, cps_ci, Any], Desc::Cps),
            // PREFIX/(cps|CPS)/(Foo|foo|FOO)/
            (&[cps_ci, PackageExact], Desc::Cps),
            // PREFIX/(cps|CPS)/(Foo|foo|FOO)/*/
            (&[cps_ci, PackageExact, Any], Desc::Cps),
            // PREFIX/(cps|CPS)/ (useful on windows or in build trees)
            (&[cps_ci], Desc::Cps),
            // PREFIX/ (useful on windows or in build trees)
            (&[], Desc::CMake),
            // PREFIX/(cmake|CMake)/
            (&[cmake], Desc::CMake),
            // PREFIX/(Foo|foo|FOO).*/
            (&[PackageDir], Desc::CMake),
            // PREFIX/(Foo|foo|FOO).*/(cmake|CMake)/
            (&[PackageDir, cmake], Desc::CMake),
            // PREFIX/(Foo|foo|FOO).*/(cmake|CMake)/(Foo|foo|FOO).*/
            (&[PackageDir, cmake, PackageDir], Desc::CMake),
            // PREFIX/(lib/ARCH|lib*|share)/cps/(Foo|foo|FOO)/
            (&[Common, cps_lit, PackageExact], Desc::Cps),
            // PREFIX/(lib/ARCH|lib*|share)/cps/(Foo|foo|FOO)/*/
            (&[Common, cps_lit, PackageExact, Any], Desc::Cps),
            // PREFIX/(lib/ARCH|lib*|share)/cps/
            (&[Common, cps_lit], Desc::Cps),
            // PREFIX/(lib/ARCH|lib*|share)/cmake/(Foo|foo|FOO).*/
            (&[Common, lib_cmake, PackageDir], Desc::CMake),
            // PREFIX/(lib/ARCH|lib*|share)/(Foo|foo|FOO).*/
            (&[Common, PackageDir], Desc::CMake),
            // PREFIX/(lib/ARCH|lib*|share)/(Foo|foo|FOO).*/(cmake|CMake)/
            (&[Common, PackageDir, cmake], Desc::CMake),
            // PREFIX/(Foo|foo|FOO).*/(lib/ARCH|lib*|share)/cmake/(Foo|foo|FOO).*/
            (&[PackageDir, Common, lib_cmake, PackageDir], Desc::CMake),
            // PREFIX/(Foo|foo|FOO).*/(lib/ARCH|lib*|share)/(Foo|foo|FOO).*/
            (&[PackageDir, Common, PackageDir], Desc::CMake),
            // PREFIX/(Foo|foo|FOO).*/(lib/ARCH|lib*|share)/(Foo|foo|FOO).*/(cmake|CMake)/
            (&[PackageDir, Common, PackageDir, cmake], Desc::CMake),
        ];
        let frameworks: &[(&[Segment<'_>], Desc)] = &[
            // <prefix>/Foo.framework/Versions/*/Resources/CPS/
            (&[framework, versions, Any, resources, cps_ci], Desc::Cps),
            // <prefix>/Foo.framework/Resources/CPS/
            (&[framework, resources, cps_ci], Desc::Cps),
            (&[framework, resources], Desc::CMake),
            (&[framework, resources, cmake], Desc::CMake),
            (&[framework, versions, Any, resources], Desc::CMake),
            (&[framework, versions, Any, resources, cmake], Desc::CMake),
        ];
        let apps: &[(&[Segment<'_>], Desc)] = &[
            // <prefix>/Foo.app/Contents/Resources/CPS/
            (&[app, app_resources, cps_ci], Desc::Cps),
            (&[app, app_resources], Desc::CMake),
            (&[app, app_resources, cmake], Desc::CMake),
        ];

        // Look for CPS_PATH entries; these are effectively prefixes with an
        // implied trailing 'cps', that may specify additional directories to
        // directly search for CPS files.
        let mut file = None;
        if self.use_cps && let Some(cps_env) = env::var_os("CPS_PATH") {
            for path in env::split_paths(&cps_env) {
                let prefix = format!("{}/", path.to_string_lossy());
                if !Path::new(&prefix).is_dir() {
                    continue;
                }
                for pattern in [&[PackageExact, cps_lit][..], &[PackageExact][..]] {
                    file = walk_layout(&layouts, &prefix, pattern, &mut |dir| {
                        self.search_directory(interp, dir, &[], Desc::Cps)
                    });
                    if file.is_some() {
                        break;
                    }
                }
                if file.is_some() {
                    break;
                }
            }
        }

        let fw = self.options.framework;
        let bundle = self.options.bundle;
        for (enabled, check_prefix, patterns) in [
            (
                matches!(fw, MacMode::First | MacMode::Only),
                false,
                frameworks,
            ),
            (
                matches!(bundle, MacMode::First | MacMode::Only),
                false,
                apps,
            ),
            (fw != MacMode::Only && bundle != MacMode::Only, true, normal),
            (fw == MacMode::Last, false, frameworks),
            (bundle == MacMode::Last, false, apps),
        ] {
            if !enabled {
                continue;
            }
            for path in &paths {
                if file.is_some() {
                    break;
                }
                if check_prefix {
                    let no_slash = if path == "/" {
                        path
                    } else {
                        path.trim_end_matches('/')
                    };
                    if !Path::new(path).is_dir()
                        || ignore
                            .iter()
                            .chain(&ignore_prefix)
                            .any(|ignored| ignored == no_slash)
                    {
                        continue;
                    }
                }
                for &(pattern, kind) in patterns {
                    file = walk_layout(&layouts, path, pattern, &mut |dir| {
                        self.search_directory(interp, dir, &ignore, kind)
                    });
                    if file.is_some() {
                        break;
                    }
                }
            }
            if file.is_some() {
                break;
            }
        }
        if let Some(file) = &file {
            self.append_debug(&format!("The file was found at\n  {file}\n"));
        } else {
            self.append_debug("The file was not found.\n");
        }
        file
    }

    fn search_directory(
        &mut self,
        interp: &mut Interp,
        dir: &str,
        ignored: &[String],
        kind: Desc,
    ) -> Option<String> {
        let suffixes = self.suffixes.clone();
        for suffix in suffixes {
            let path = if suffix.is_empty() {
                dir.trim_end_matches('/').to_owned()
            } else {
                format!("{dir}{suffix}")
            };
            if !ignored.contains(&path)
                && let Some(config) = self.find_config_file(interp, &path, kind)
            {
                return Some(config);
            }
        }
        None
    }

    pub(super) fn find_config_file(&mut self, interp: &mut Interp, dir: &str, kind: Desc) -> Option<String> {
        for config in self.configs.clone() {
            if kind != Desc::Any && cps::config_desc(&config) != kind {
                continue;
            }
            let file = format!("{dir}/{config}");
            if let Some(debug) = &mut self.debug {
                let _ = writeln!(debug, "  {file}");
            }
            if readable_file(&file) && self.check_version(interp, &file) {
                if interp.is_on("CMAKE_FIND_PACKAGE_RESOLVE_SYMLINKS")
                    && let Ok(real) = fs::canonicalize(&file)
                {
                    return Some(real.to_string_lossy().into_owned());
                }
                return Some(normalized(&file));
            }
        }
        None
    }

    fn check_version(&mut self, interp: &mut Interp, config: &str) -> bool {
        if cps::is_cps(config) {
            // CPS file
            return cps::check_version(self, config);
        }
        let base = config.strip_suffix(".cmake").unwrap_or(config);
        let version_file = [
            format!("{base}-version.cmake"),
            format!("{base}Version.cmake"),
        ]
        .into_iter()
        .find(|file| readable_file(file));
        let Some(version_file) = version_file else {
            self.considered.push((config.to_owned(), "unknown".into()));
            return self.version.min.is_empty();
        };

        interp.push_scope();
        for suffix in ["", "_UNSUITABLE", "_COMPATIBLE", "_EXACT"] {
            interp.unset_def(&format!("PACKAGE_VERSION{suffix}"));
        }
        interp.set_def("PACKAGE_FIND_NAME", &self.name);
        interp.set_def("PACKAGE_FIND_VERSION_COMPLETE", &self.version.complete);
        for (key, value) in self.version.definitions("PACKAGE_FIND_VERSION") {
            interp.set_def(key, value);
        }
        let fatal_before_read = interp.fatal;
        let read = self.read(interp, &version_file, true);
        if read.is_err() {
            // A syntax error rejects the version file. The parser's error is
            // retained, and the search proceeds to the next configuration.
            interp.fatal = fatal_before_read;
        }
        let suitable = read.is_ok()
            && !interp.is_on("PACKAGE_VERSION_UNSUITABLE")
            && (self.version.min.is_empty()
                || interp.is_on("PACKAGE_VERSION_EXACT")
                || (!self.version.exact && interp.is_on("PACKAGE_VERSION_COMPATIBLE")));
        let version = interp.get_safe("PACKAGE_VERSION");
        interp.pop_scope();
        self.considered.push((
            config.to_owned(),
            if version.is_empty() {
                "unknown".into()
            } else {
                version.clone()
            },
        ));
        if suitable {
            self.selected_version = version;
        }
        suitable
    }
}

fn walk_layout(
    layouts: &Layouts<'_>,
    path: &str,
    segments: &[Segment<'_>],
    visit: &mut impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let Some((&first, rest)) = segments.split_first() else {
        return visit(path);
    };
    for path in layouts.candidates(path, first) {
        if let Some(found) = walk_layout(layouts, &path, rest, visit) {
            return Some(found);
        }
    }
    None
}

fn user_registry(name: &str) -> Vec<String> {
    let Ok(home) = env::var("HOME") else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in fs::read_dir(format!("{home}/.cmake/packages/{name}"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let file = entry.path();
        if file.is_dir() {
            continue;
        }
        let Some(line) = fs::File::open(&file)
            .ok()
            .and_then(|file| BufReader::new(file).lines().next())
            .and_then(Result::ok)
        else {
            let _ = fs::remove_file(file);
            continue;
        };
        if is_full_path(&line) {
            if !access(&line, libc::R_OK) {
                let _ = fs::remove_file(file);
            } else if Path::new(&line).is_dir() {
                paths.push(line);
            } else {
                paths.push(filename_path(&line));
            }
        }
    }
    paths
}

/// Directory sorting uses CMake's strverscmp, including fractional-style
/// treatment of digit runs with leading zeros.
fn natural(left: &[u8], right: &[u8]) -> Ordering {
    let i = left.iter().zip(right).take_while(|(l, r)| l == r).count();
    let l = left.get(i).copied().unwrap_or(0);
    let r = right.get(i).copied().unwrap_or(0);
    let begin = i - left[..i]
        .iter()
        .rev()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if l != r && (begin != i || (l.is_ascii_digit() && r.is_ascii_digit())) {
        let zeros = |bytes: &[u8]| {
            bytes[begin..]
                .windows(2)
                .take_while(|pair| pair[0] == b'0' && pair[1].is_ascii_digit())
                .count()
        };
        let lzeros = zeros(left);
        let rzeros = zeros(right);
        if lzeros != rzeros {
            return rzeros.cmp(&lzeros);
        }
        if lzeros == 0 {
            let digits = |bytes: &[u8]| {
                bytes[i..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_digit())
                    .count()
            };
            let order = digits(left).cmp(&digits(right));
            if order != Ordering::Equal {
                return order;
            }
        }
    }
    l.cmp(&r)
}
