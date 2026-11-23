//! Apple platform queries shared by the configure and generate steps.

use crate::cmregex::Regex;
use crate::interp::Interp;
use crate::util::{filename_last_extension, filename_name, filename_path, is_on};

/// `cmMakefile::AppleSDK` as `GetAppleSDKType` classifies
/// `CMAKE_OSX_SYSROOT`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sdk {
    MacOs,
    AppleTvOs,
    AppleTvSimulator,
    IPhoneOs,
    IPhoneSimulator,
    WatchOs,
    WatchSimulator,
    XrOs,
    XrSimulator,
}

impl Sdk {
    pub fn of(sysroot: &str) -> Sdk {
        let root = sysroot.to_ascii_lowercase();
        for (name, sdk) in [
            ("appletvos", Sdk::AppleTvOs),
            ("appletvsimulator", Sdk::AppleTvSimulator),
            ("iphoneos", Sdk::IPhoneOs),
            ("iphonesimulator", Sdk::IPhoneSimulator),
            ("watchos", Sdk::WatchOs),
            ("watchsimulator", Sdk::WatchSimulator),
            ("xros", Sdk::XrOs),
            ("xrsimulator", Sdk::XrSimulator),
        ] {
            if root.starts_with(name) || root.contains(&format!("/{name}")) {
                return sdk;
            }
        }
        Sdk::MacOs
    }

    /// The debugger's `AppleSDKTypeString` (watch/xr SDKs fall through to
    /// "Unknown").
    pub fn label(self) -> &'static str {
        match self {
            Sdk::MacOs => "MacOS",
            Sdk::IPhoneOs => "IPhoneOS",
            Sdk::IPhoneSimulator => "IPhoneSimulator",
            Sdk::AppleTvOs => "AppleTVOS",
            Sdk::AppleTvSimulator => "AppleTVSimulator",
            _ => "Unknown",
        }
    }
}

/// `cmMakefile::PlatformIsAppleEmbedded` for the directory being configured.
pub fn is_embedded(interp: &Interp) -> bool {
    Sdk::of(&interp.get_safe("CMAKE_OSX_SYSROOT")) != Sdk::MacOs
}

/// `cmMakefile::PlatformSupportsAppleTextStubs`, given the values of `APPLE`
/// and `CMAKE_TAPI`.
pub fn supports_text_stubs(apple: Option<&str>, tapi: Option<&str>) -> bool {
    apple.is_some_and(is_on)
        // cmMakefile::IsSet: defined, non-empty and not NOTFOUND.
        && tapi.is_some_and(|value| {
            !value.is_empty() && !value.ends_with("-NOTFOUND") && value != "NOTFOUND"
        })
}

/// `cmGlobalGenerator::FrameworkFormat`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FrameworkFormat {
    Strict,
    Relaxed,
    Extended,
}

/// `cmGlobalGenerator::FrameworkDescriptor`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Framework {
    pub directory: String,
    pub version: String,
    pub name: String,
    pub suffix: String,
}

impl Framework {
    /// `GetFullName`.
    pub fn full_name(&self) -> String {
        format!("{0}.framework/{0}{1}", self.name, self.suffix)
    }

    /// `GetVersionedName`.
    pub fn versioned_name(&self) -> String {
        if self.version.is_empty() {
            self.full_name()
        } else {
            format!(
                "{0}.framework/Versions/{1}/{0}{2}",
                self.name, self.version, self.suffix
            )
        }
    }

    /// `GetFrameworkPath`.
    pub fn framework_path(&self) -> String {
        let name = format!("{}.framework", self.name);
        if self.directory.is_empty() {
            name
        } else {
            format!("{}/{name}", self.directory)
        }
    }

    /// `GetFullPath`.
    pub fn full_path(&self) -> String {
        let name = self.full_name();
        if self.directory.is_empty() {
            name
        } else {
            format!("{}/{name}", self.directory)
        }
    }

    /// `GetVersionedPath`.
    pub fn versioned_path(&self) -> String {
        let name = self.versioned_name();
        if self.directory.is_empty() {
            name
        } else {
            format!("{}/{name}", self.directory)
        }
    }

    /// `GetLinkName`.
    pub fn link_name(&self) -> String {
        if self.suffix.is_empty() {
            self.name.clone()
        } else {
            format!("{},{}", self.name, self.suffix)
        }
    }
}

/// `cmSystemTools::GetFilenameWithoutExtension` (the longest extension).
fn filename_without_extension(path: &str) -> String {
    let name = filename_name(path);
    match name.find('.') {
        Some(dot) => name[..dot].to_owned(),
        None => name,
    }
}

/// `cmSystemTools::IsPathToFramework`.
pub fn is_path_to_framework(path: &str) -> bool {
    path.starts_with('/') && path.ends_with(".framework")
}

/// `cmSystemTools::IsPathToXcFramework`.
pub fn is_path_to_xcframework(path: &str) -> bool {
    path.starts_with('/') && path.ends_with(".xcframework")
}

/// `cmGlobalGenerator::SplitFrameworkPath`.
pub fn split_framework_path(path: &str, format: FrameworkFormat) -> Option<Framework> {
    thread_local! {
        static FRAMEWORK: Regex =
            Regex::new("((.+)/)?([^/]+)\\.framework(/Versions/([^/]+))?(/(.+))?$").unwrap();
    }
    let extension = filename_last_extension(path);
    if matches!(extension.as_str(), "" | ".tbd" | ".framework")
        && let Some(found) = FRAMEWORK.with(|regex| regex.find(path))
    {
        let group = |index: usize| {
            found.groups[index]
                .map(|(start, end)| path[start..end].to_owned())
                .unwrap_or_default()
        };
        let name = group(3);
        let library = filename_without_extension(&group(7));
        if format == FrameworkFormat::Strict && library.is_empty() {
            return None;
        }
        if !library.is_empty() && !library.starts_with(&name) {
            return None;
        }
        let suffix = library.get(name.len()..).unwrap_or_default().to_owned();
        return Some(Framework {
            directory: group(2),
            version: group(5),
            name,
            suffix,
        });
    }
    if format == FrameworkFormat::Extended {
        // The path format can be more flexible: (/path/to/)?fwName(.framework)?
        return Some(Framework {
            directory: filename_path(path),
            name: if extension == ".framework" {
                filename_without_extension(path)
            } else {
                filename_name(path)
            },
            ..Framework::default()
        });
    }
    None
}
