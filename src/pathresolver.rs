//! `cm::PathResolver` (cmPathResolver.cxx) for POSIX hosts, and the
//! `cmSystemTools` entry points built on it (`ToNormalizedPathOnDisk`
//! with `ResolveTildePath`).

use std::ffi::CString;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Symlinks {
    None,
    /// Resolve a symlink only when a following `..` would step out of it.
    Lazy,
    Eager,
}

#[derive(Clone, Copy)]
pub struct Policy {
    pub symlinks: Symlinks,
    pub existence_required: bool,
}

/// `Policies::LogicalPath`.
pub const LOGICAL_PATH: Policy = Policy { symlinks: Symlinks::Lazy, existence_required: false };
/// `Policies::RealPath`.
pub const REAL_PATH: Policy = Policy { symlinks: Symlinks::Eager, existence_required: true };
/// `Policies::NaivePath` (and `CasePath` off Windows and macOS).
pub const NAIVE_PATH: Policy = Policy { symlinks: Symlinks::None, existence_required: false };

const MAX_SYMBOLIC_LINKS: usize = 32;

enum Control {
    Continue(usize),
    Restart,
    Error(i32),
}

struct Impl {
    policy: Policy,
    p: String,
    symlink_depth: usize,
}

fn read_link(path: &str) -> Result<Option<String>, i32> {
    let Ok(c_path) = CString::new(path) else {
        return Err(libc::EINVAL);
    };
    let mut buffer = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: buffer is writable for its length.
    let n = unsafe { libc::readlink(c_path.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len()) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EINVAL));
    }
    buffer.truncate(n as usize);
    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

fn path_exists(path: &str) -> bool {
    let Ok(c_path) = CString::new(path) else {
        return false;
    };
    // SAFETY: valid C string; lstat-like existence check (kwsys PathExists
    // uses lstat).
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::lstat(c_path.as_ptr(), &mut st) == 0 }
}

impl Impl {
    /// `ImplBase::ReadSymlink`.
    fn read_symlink(&mut self, path: &str) -> Result<Option<String>, i32> {
        match read_link(path) {
            Ok(target) => {
                self.symlink_depth += 1;
                if self.symlink_depth >= MAX_SYMBOLIC_LINKS {
                    return Err(libc::ELOOP);
                }
                Ok(target.filter(|target| !target.is_empty()))
            }
            // The path was not a symlink.
            Err(libc::EINVAL) => Ok(None),
            Err(code) => Err(code),
        }
    }

    /// `ImplBase::ResolveSymlink`.
    fn resolve_symlink(&mut self, slash: usize, next_slash: usize, target: String) -> Control {
        let target = target.replace('\\', "/");
        if !target.starts_with('/') {
            // A symlink to a relative path: "*/link/" => "*/dest/".
            self.p.replace_range(slash + 1..next_slash, &target);
            return Control::Continue(slash);
        }
        // A symlink to an absolute path: "*/link/" => "/dest/", restart.
        self.p.replace_range(..next_slash, &target);
        Control::Restart
    }

    /// `Impl::ResolveRoot` (and `ResolveRelativePath`).
    fn resolve_root(&mut self) -> Control {
        if !self.p.starts_with('/') {
            let mut cwd = logical_working_directory().replace('\\', "/");
            if !cwd.starts_with('/') {
                cwd.insert(0, '/');
            }
            if !cwd.ends_with('/') {
                cwd.push('/');
            }
            self.p.insert_str(0, &cwd);
            return Control::Restart;
        }
        if self.policy.existence_required && !path_exists("/") {
            self.p = "/".to_owned();
            return Control::Error(libc::ENOENT);
        }
        Control::Continue(0)
    }

    /// `Impl::ResolveComponent`.
    fn resolve_component(&mut self, root_slash: usize, mut slash: usize) -> Control {
        let mut next_slash = self.p[slash + 1..].find('/').map_or(self.p.len(), |i| slash + 1 + i);
        let component = self.p[slash + 1..next_slash].to_owned();
        if slash == root_slash {
            if component.is_empty() || component == "." || component == ".." {
                // "//", "/./", "/../" => "/"
                let end = (next_slash + 1).min(self.p.len());
                self.p.replace_range(slash + 1..end, "");
                return Control::Continue(slash);
            }
        } else {
            if component.is_empty() || component == "." {
                // "*//", "*/./" => "*/"
                self.p.replace_range(slash..next_slash, "");
                return Control::Continue(slash);
            }
            if component == ".." {
                // Rewind to the previous component.
                next_slash = slash;
                slash = self.p[..slash].rfind('/').unwrap_or(0);
                if self.policy.symlinks == Symlinks::Lazy {
                    let path = self.p[..next_slash].to_owned();
                    match self.read_symlink(&path) {
                        Ok(Some(target)) => return self.resolve_symlink(slash, next_slash, target),
                        Ok(None) => {}
                        Err(code) => {
                            if self.policy.existence_required {
                                self.p = path;
                                return Control::Error(code);
                            }
                        }
                    }
                }
                // "*/dir/../" => "*/"
                let end = (next_slash + 4).min(self.p.len());
                self.p.replace_range(slash + 1..end, "");
                return Control::Continue(slash);
            }
        }
        // A named component.
        if self.policy.symlinks == Symlinks::Eager {
            let path = self.p[..next_slash].to_owned();
            match self.read_symlink(&path) {
                Ok(Some(target)) => return self.resolve_symlink(slash, next_slash, target),
                Ok(None) => {}
                Err(code) => {
                    if self.policy.existence_required {
                        self.p = path;
                        return Control::Error(code);
                    }
                }
            }
        }
        if self.policy.existence_required {
            let path = self.p[..next_slash].to_owned();
            if !path_exists(&path) {
                self.p = path;
                return Control::Error(libc::ENOENT);
            }
        }
        Control::Continue(next_slash)
    }

    /// `Impl::ResolvePath`.
    fn resolve_path(&mut self) -> Control {
        let root_slash = match self.resolve_root() {
            Control::Continue(slash) => slash,
            other => return other,
        };
        let mut slash = root_slash;
        while self.p.len() > root_slash + 1 && slash < self.p.len() {
            match self.resolve_component(root_slash, slash) {
                Control::Continue(next) => slash = next,
                other => return other,
            }
        }
        Control::Continue(self.p.len())
    }
}

/// `cmSystemTools::GetLogicalWorkingDirectory`: PWD when it names the
/// current directory (preserving symlinks), else the physical directory.
pub fn logical_working_directory() -> String {
    let Ok(current) = std::env::current_dir() else {
        return String::new();
    };
    std::env::var("PWD")
        .ok()
        .filter(|pwd| {
            std::path::Path::new(pwd).is_absolute()
                && std::fs::canonicalize(pwd).ok().as_ref() == Some(&current)
        })
        .unwrap_or_else(|| current.to_string_lossy().into_owned())
}

/// `Resolver<Policy>::Resolve`: the resolved path, or the partial path and
/// the errno of the failure.
pub fn resolve(input: &str, policy: Policy) -> Result<String, (String, i32)> {
    let mut resolver = Impl { policy, p: input.replace('\\', "/"), symlink_depth: 0 };
    loop {
        match resolver.resolve_path() {
            Control::Continue(_) => return Ok(resolver.p),
            Control::Restart => continue,
            Control::Error(code) => return Err((resolver.p, code)),
        }
    }
}

/// cmSystemTools.cxx `ResolveTildePath`: `~` or `~/...` from HOME, and
/// `~user/...` from the password database.
pub fn resolve_tilde_path(p: &str) -> String {
    if !p.starts_with('~') {
        return p.to_owned();
    }
    let last = p.find(['/', '\\']).unwrap_or(p.len());
    let home = if last == 1 {
        std::env::var("HOME").ok()
    } else {
        let user = &p[1..last];
        CString::new(user).ok().and_then(|user| {
            // SAFETY: getpwnam with a valid C string; the result is read
            // before any other passwd call.
            let pw = unsafe { libc::getpwnam(user.as_ptr()) };
            if pw.is_null() {
                return None;
            }
            // SAFETY: pw is non-null; pw_dir is a C string or null.
            let dir = unsafe { (*pw).pw_dir };
            if dir.is_null() {
                return None;
            }
            // SAFETY: non-null C string.
            Some(unsafe { std::ffi::CStr::from_ptr(dir) }.to_string_lossy().into_owned())
        })
    };
    match home {
        Some(home) => format!("{home}{}", &p[last..]),
        None => p.to_owned(),
    }
}

/// `cmSystemTools::ToNormalizedPathOnDisk`: tilde expansion, then the
/// LogicalPath resolver relative to the working directory.
pub fn to_normalized_path_on_disk(p: &str) -> String {
    let p = resolve_tilde_path(p);
    match resolve(&p, LOGICAL_PATH) {
        Ok(path) | Err((path, _)) => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_path_normalizes_lexically_without_symlinks() {
        assert_eq!(resolve("/a/./b//c/../d/", NAIVE_PATH).unwrap(), "/a/b/d");
        assert_eq!(resolve("/../x", NAIVE_PATH).unwrap(), "/x");
        assert_eq!(resolve("/", NAIVE_PATH).unwrap(), "/");
        assert_eq!(resolve("//", NAIVE_PATH).unwrap(), "/");
        assert_eq!(resolve("/a/..", NAIVE_PATH).unwrap(), "/");
    }
}
