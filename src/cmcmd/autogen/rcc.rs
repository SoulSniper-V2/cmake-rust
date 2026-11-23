//! `cmQtAutoRccT`: the AUTORCC build-time generator behind
//! `cmake -E cmake_autorcc`.

use super::filetime::FileTime;
use super::generator::{
    FileLock, GeneratorBase, Info, file_read, file_write, make_parent_directory,
    run_single_command, settings_find, touch,
};
use super::mocuic::settings_hash;
use super::{GenT, RccLister, append_filename_suffix, quoted_command};

/// `cmQtAutoRccT`.
pub(crate) struct AutoRcc {
    base: GeneratorBase,
    // -- Config settings
    multi_config: bool,
    cross_config: bool,
    use_better_graph: bool,
    generator: String,
    // -- Directories
    autogen_build_dir: String,
    include_dir: String,
    // -- Qt environment
    rcc_executable: String,
    rcc_executable_time: FileTime,
    rcc_list_options: Vec<String>,
    // -- Job
    lock_file: String,
    lock_file_lock: FileLock,
    qrc_file: String,
    #[allow(dead_code)]
    qrc_file_name: String,
    #[allow(dead_code)]
    qrc_file_dir: String,
    qrc_file_time: FileTime,
    rcc_path_checksum: String,
    rcc_file_name: String,
    rcc_file_output: String,
    rcc_file_public: String,
    rcc_file_time: FileTime,
    reason: String,
    options: Vec<String>,
    inputs: Vec<String>,
    // -- Settings file
    settings_file: String,
    settings_string: String,
    settings_changed: bool,
    build_file_changed: bool,
}

impl AutoRcc {
    fn is_xcode(&self) -> bool {
        self.generator.contains("Xcode")
    }

    /// `MultiConfigOutput`.
    fn multi_config_output(&self) -> String {
        format!(
            "{}/{}",
            self.rcc_path_checksum,
            append_filename_suffix(&self.rcc_file_name, "_CMAKE_")
        )
    }

    fn message_path(&self, path: &str) -> String {
        self.base.message_path(path)
    }
}

/// `cmQtAutoRcc` entry point.
pub(crate) fn run(info_file: &str, config: &str, executable_config: &str) -> bool {
    let mut generator = AutoRcc {
        base: GeneratorBase::new(GenT::Rcc),
        multi_config: false,
        cross_config: false,
        use_better_graph: false,
        generator: String::new(),
        autogen_build_dir: String::new(),
        include_dir: String::new(),
        rcc_executable: String::new(),
        rcc_executable_time: FileTime::default(),
        rcc_list_options: Vec::new(),
        lock_file: String::new(),
        lock_file_lock: FileLock::new(),
        qrc_file: String::new(),
        qrc_file_name: String::new(),
        qrc_file_dir: String::new(),
        qrc_file_time: FileTime::default(),
        rcc_path_checksum: String::new(),
        rcc_file_name: String::new(),
        rcc_file_output: String::new(),
        rcc_file_public: String::new(),
        rcc_file_time: FileTime::default(),
        reason: String::new(),
        options: Vec::new(),
        inputs: Vec::new(),
        settings_file: String::new(),
        settings_string: String::new(),
        settings_changed: false,
        build_file_changed: false,
    };
    let Some(json) = generator
        .base
        .run_init(info_file, config, executable_config)
    else {
        return false;
    };
    if !generator.init_from_info(&json) {
        return false;
    }
    generator.process()
}

impl AutoRcc {
    /// `cmQtAutoRccT::InitFromInfo`.
    fn init_from_info(&mut self, json: &serde_json::Value) -> bool {
        let info = Info::new(
            json,
            &self.base.info_config,
            &self.base.info_file,
            self.base.gen_type,
            &self.base.log,
        );
        // -- Required settings
        if !info.get_bool("MULTI_CONFIG", &mut self.multi_config, true)
            || !info.get_string("GENERATOR", &mut self.generator, true)
            || !info.get_bool("CROSS_CONFIG", &mut self.cross_config, true)
            || !info.get_bool("USE_BETTER_GRAPH", &mut self.use_better_graph, true)
            || !info.get_string("BUILD_DIR", &mut self.autogen_build_dir, true)
            || !info.get_string_config("INCLUDE_DIR", &mut self.include_dir, true)
            || !info.get_array_config("RCC_LIST_OPTIONS", &mut self.rcc_list_options, false)
            || !info.get_string("LOCK_FILE", &mut self.lock_file, true)
            || !info.get_string_config("SETTINGS_FILE", &mut self.settings_file, true)
            || !info.get_string("SOURCE", &mut self.qrc_file, true)
            || !info.get_string("OUTPUT_CHECKSUM", &mut self.rcc_path_checksum, true)
            || !info.get_string("OUTPUT_NAME", &mut self.rcc_file_name, true)
            || !info.get_array("OPTIONS", &mut self.options, false)
        {
            return false;
        }
        if self.use_better_graph {
            if !info.get_array_config("INPUTS", &mut self.inputs, false) {
                return false;
            }
            if self.cross_config {
                let rcc_executable_with_config =
                    format!("RCC_EXECUTABLE_{}", self.base.executable_config);
                if !info.get_string(&rcc_executable_with_config, &mut self.rcc_executable, true) {
                    return false;
                }
            } else if !info.get_string_config("RCC_EXECUTABLE", &mut self.rcc_executable, true) {
                return false;
            }
        } else if !info.get_string("RCC_EXECUTABLE", &mut self.rcc_executable, true)
            || !info.get_array("RCC_LIST_OPTIONS", &mut self.rcc_list_options, false)
            || !info.get_array("INPUTS", &mut self.inputs, false)
        {
            return false;
        }

        // -- Derive information
        self.qrc_file_name = crate::util::filename_name(&self.qrc_file);
        self.qrc_file_dir = crate::util::filename_path(&self.qrc_file);
        if self.multi_config && !self.is_xcode() && self.use_better_graph {
            self.rcc_file_public = format!(
                "{}/{}_{}/{}",
                self.autogen_build_dir,
                self.rcc_path_checksum,
                self.base.info_config,
                self.rcc_file_name
            );
        } else {
            self.rcc_file_public = format!(
                "{}/{}/{}",
                self.autogen_build_dir, self.rcc_path_checksum, self.rcc_file_name
            );
        }

        // rcc output file name
        if self.multi_config {
            self.rcc_file_output = format!("{}/{}", self.include_dir, self.multi_config_output());
        } else {
            self.rcc_file_output = self.rcc_file_public.clone();
        }

        // -- Checks
        self.rcc_executable_time = FileTime::load(&self.rcc_executable);
        if !self.rcc_executable_time.exists() {
            return info.log_error(format!(
                "The rcc executable {} does not exist.",
                self.message_path(&self.rcc_executable)
            ));
        }

        true
    }

    /// `cmQtAutoRccT::Process`.
    fn process(&mut self) -> bool {
        if !self.settings_file_read() {
            return false;
        }

        // Test if the rcc output needs to be regenerated
        let mut generate = false;
        if !self.test_qrc_rcc_files(&mut generate) {
            return false;
        }
        if !generate && !self.test_resources(&mut generate) {
            return false;
        }
        // Generate on demand
        if generate {
            if !self.generate_rcc() {
                return false;
            }
        } else {
            // Test if the info file is newer than the output file
            if !self.test_info_file() {
                return false;
            }
        }

        if !self.generate_wrapper() {
            return false;
        }

        self.settings_file_write()
    }

    /// `SettingsFileRead`.
    fn settings_file_read(&mut self) -> bool {
        // Compose current settings strings
        {
            let mut items: Vec<String> = vec![self.rcc_executable.clone()];
            items.extend(self.rcc_list_options.iter().cloned());
            items.push(self.qrc_file.clone());
            items.push(self.rcc_path_checksum.clone());
            items.push(self.rcc_file_name.clone());
            items.extend(self.options.iter().cloned());
            items.extend(self.inputs.iter().cloned());
            self.settings_string = settings_hash(items.iter().map(String::as_str));
        }

        // Make sure the settings file exists
        if !std::path::Path::new(&self.settings_file).is_file() {
            // Touch the settings file to make sure it exists
            if !touch(&self.settings_file, true) {
                self.base.log.error(
                    GenT::Rcc,
                    &format!(
                        "Touching the settings file {} failed.",
                        self.message_path(&self.settings_file)
                    ),
                );
                return false;
            }
        }

        // Lock the lock file
        {
            // Make sure the lock file exists
            if !std::path::Path::new(&self.lock_file).is_file() && !touch(&self.lock_file, true) {
                self.base.log.error(
                    GenT::Rcc,
                    &format!(
                        "Touching the lock file {} failed.",
                        self.message_path(&self.lock_file)
                    ),
                );
                return false;
            }
            // Lock the lock file
            if let Err(error) = self.lock_file_lock.lock(&self.lock_file) {
                self.base.log.error(
                    GenT::Rcc,
                    &format!(
                        "Locking of the lock file {} failed.\n{error}",
                        self.message_path(&self.lock_file)
                    ),
                );
                return false;
            }
        }

        // Read old settings
        {
            if let Some(content) = file_read(&self.settings_file, None) {
                let content = String::from_utf8_lossy(&content);
                self.settings_changed = self.settings_string != settings_find(&content, "rcc");
                // In case any setting changed clear the old settings file.
                // This triggers a full rebuild on the next run if the
                // current build is aborted before writing the current
                // settings in the end.
                if self.settings_changed {
                    let mut error = String::new();
                    if !file_write(&self.settings_file, b"", Some(&mut error)) {
                        self.base.log.error(
                            GenT::Rcc,
                            &format!(
                                "Clearing of the settings file {} failed.\n{error}",
                                self.message_path(&self.settings_file)
                            ),
                        );
                        return false;
                    }
                }
            } else {
                self.settings_changed = true;
            }
        }

        true
    }

    /// `SettingsFileWrite`.
    fn settings_file_write(&mut self) -> bool {
        // Only write if any setting changed
        if self.settings_changed {
            if self.base.log.verbose() {
                self.base.log.info(
                    GenT::Rcc,
                    &format!(
                        "Writing settings file {}",
                        self.message_path(&self.settings_file)
                    ),
                );
            }
            // Write settings file
            let content = format!("rcc:{}\n", self.settings_string);
            let mut error = String::new();
            if !file_write(&self.settings_file, content.as_bytes(), Some(&mut error)) {
                self.base.log.error(
                    GenT::Rcc,
                    &format!(
                        "Writing of the settings file {} failed.\n{error}",
                        self.message_path(&self.settings_file)
                    ),
                );
                // Remove old settings file to trigger a full rebuild on the
                // next run
                let _ = std::fs::remove_file(&self.settings_file);
                return false;
            }
        }

        // Unlock the lock file
        self.lock_file_lock.release();
        true
    }

    /// `TestQrcRccFiles`: basic checks if rcc generation is required.
    fn test_qrc_rcc_files(&mut self, generate: &mut bool) -> bool {
        // Test if the rcc input file exists
        self.qrc_file_time = FileTime::load(&self.qrc_file);
        if !self.qrc_file_time.exists() {
            self.base.log.error(
                GenT::Rcc,
                &format!(
                    "The resources file {} does not exist",
                    self.message_path(&self.qrc_file)
                ),
            );
            return false;
        }

        // Test if the rcc output file exists
        self.rcc_file_time = FileTime::load(&self.rcc_file_output);
        if !self.rcc_file_time.exists() {
            if self.base.log.verbose() {
                self.reason = format!(
                    "Generating {}, because it doesn't exist, from {}",
                    self.message_path(&self.rcc_file_output),
                    self.message_path(&self.qrc_file)
                );
            }
            *generate = true;
            return true;
        }

        // Test if the settings changed
        if self.settings_changed {
            if self.base.log.verbose() {
                self.reason = format!(
                    "Generating {}, because the rcc settings changed, from {}",
                    self.message_path(&self.rcc_file_output),
                    self.message_path(&self.qrc_file)
                );
            }
            *generate = true;
            return true;
        }

        // Test if the rcc output file is older than the .qrc file
        if self.rcc_file_time.older(&self.qrc_file_time) {
            if self.base.log.verbose() {
                self.reason = format!(
                    "Generating {}, because it is older than {}, from {}",
                    self.message_path(&self.rcc_file_output),
                    self.message_path(&self.qrc_file),
                    self.message_path(&self.qrc_file)
                );
            }
            *generate = true;
            return true;
        }

        // Test if the rcc output file is older than the rcc executable
        if self.rcc_file_time.older(&self.rcc_executable_time) {
            if self.base.log.verbose() {
                self.reason = format!(
                    "Generating {}, because it is older than the rcc executable, from {}",
                    self.message_path(&self.rcc_file_output),
                    self.message_path(&self.qrc_file)
                );
            }
            *generate = true;
            return true;
        }

        true
    }

    /// `TestResources`.
    fn test_resources(&mut self, generate: &mut bool) -> bool {
        // Read resource files list
        if self.inputs.is_empty() {
            let lister = RccLister {
                executable: self.rcc_executable.clone(),
                list_options: self.rcc_list_options.clone(),
            };
            let mut error = String::new();
            if !lister.list(
                &self.qrc_file,
                &mut self.inputs,
                &mut error,
                self.base.log.verbose(),
            ) {
                self.base.log.error(
                    GenT::Rcc,
                    &format!(
                        "Listing of {} failed.\n{error}",
                        self.message_path(&self.qrc_file)
                    ),
                );
                return false;
            }
        }

        // Check if any resource file is newer than the rcc output file
        for res_file in &self.inputs {
            // Check if the resource file exists
            let file_time = FileTime::load(res_file);
            if !file_time.exists() {
                self.base.log.error(
                    GenT::Rcc,
                    &format!(
                        "The resource file {} listed in {} does not exist.",
                        self.message_path(res_file),
                        self.message_path(&self.qrc_file)
                    ),
                );
                return false;
            }
            // Check if the resource file is newer than the rcc output file
            if self.rcc_file_time.older(&file_time) {
                if self.base.log.verbose() {
                    self.reason = format!(
                        "Generating {}, because it is older than {}, from {}",
                        self.message_path(&self.rcc_file_output),
                        self.message_path(res_file),
                        self.message_path(&self.qrc_file)
                    );
                }
                *generate = true;
                break;
            }
        }
        true
    }

    /// `TestInfoFile`.
    fn test_info_file(&mut self) -> bool {
        // Test if the rcc output file is older than the info file
        if self.rcc_file_time.older(&self.base.info_file_time) {
            if self.base.log.verbose() {
                self.base.log.info(
                    GenT::Rcc,
                    &format!(
                        "Touching {} because it is older than {}",
                        self.message_path(&self.rcc_file_output),
                        self.message_path(&self.base.info_file)
                    ),
                );
            }
            // Touch build file
            if !touch(&self.rcc_file_output, false) {
                self.base.log.error(
                    GenT::Rcc,
                    &format!(
                        "Touching {} failed.",
                        self.message_path(&self.rcc_file_output)
                    ),
                );
                return false;
            }
            self.build_file_changed = true;
        }
        true
    }

    /// `GenerateRcc`.
    fn generate_rcc(&mut self) -> bool {
        // Make parent directory
        if !make_parent_directory(&self.rcc_file_output) {
            self.base.log.error(
                GenT::Rcc,
                &format!(
                    "Could not create parent directory of {}",
                    self.message_path(&self.rcc_file_output)
                ),
            );
            return false;
        }

        // Compose rcc command
        let mut cmd: Vec<String> = vec![self.rcc_executable.clone()];
        cmd.extend(self.options.iter().cloned());
        cmd.push("-o".to_owned());
        cmd.push(self.rcc_file_output.clone());
        cmd.push(self.qrc_file.clone());

        // Log reason and command
        if self.base.log.verbose() {
            self.base.log.info(
                GenT::Rcc,
                &format!(
                    "{}{}{}\n",
                    self.reason,
                    if self.reason.ends_with('\n') {
                        ""
                    } else {
                        "\n"
                    },
                    quoted_command(&cmd)
                ),
            );
        }

        let result = run_single_command(&cmd, &self.autogen_build_dir);
        if !result.ok || result.ret_val != 0 {
            // rcc process failed
            self.base.log.error_command(
                GenT::Rcc,
                &format!(
                    "The rcc process failed to compile\n  {}\ninto\n  {}",
                    self.message_path(&self.qrc_file),
                    self.message_path(&self.rcc_file_output)
                ),
                &cmd,
                &format!("{}{}", result.stdout, result.stderr),
            );
            let _ = std::fs::remove_file(&self.rcc_file_output);
            return false;
        }

        // rcc process success
        // Print rcc output
        if !result.stdout.is_empty() {
            self.base.log.info(GenT::Rcc, &result.stdout);
        }
        self.build_file_changed = true;

        true
    }

    /// `GenerateWrapper`.
    fn generate_wrapper(&mut self) -> bool {
        // Generate a wrapper source file on demand
        if self.multi_config {
            // Wrapper file content
            let content = format!(
                "// This is an autogenerated configuration wrapper file.\n\
                 // Changes will be overwritten.\n\
                 #include <{}>\n",
                self.multi_config_output()
            );

            // Compare with existing file content
            let file_differs = match file_read(&self.rcc_file_public, None) {
                Some(old_contents) => old_contents != content.as_bytes(),
                None => true,
            };
            if file_differs {
                // Write new wrapper file
                if self.base.log.verbose() {
                    self.base.log.info(
                        GenT::Rcc,
                        &format!(
                            "Generating RCC wrapper file {}",
                            self.message_path(&self.rcc_file_public)
                        ),
                    );
                }
                let mut error = String::new();
                if !file_write(&self.rcc_file_public, content.as_bytes(), Some(&mut error)) {
                    self.base.log.error(
                        GenT::Rcc,
                        &format!(
                            "Generating RCC wrapper file {} failed.\n{error}",
                            self.message_path(&self.rcc_file_public)
                        ),
                    );
                    return false;
                }
            } else if self.build_file_changed {
                // Just touch the wrapper file
                if self.base.log.verbose() {
                    self.base.log.info(
                        GenT::Rcc,
                        &format!(
                            "Touching RCC wrapper file {}",
                            self.message_path(&self.rcc_file_public)
                        ),
                    );
                }
                if !touch(&self.rcc_file_public, false) {
                    self.base.log.error(
                        GenT::Rcc,
                        &format!(
                            "Touching RCC wrapper file {} failed.",
                            self.message_path(&self.rcc_file_public)
                        ),
                    );
                    return false;
                }
            }
        }
        true
    }
}
