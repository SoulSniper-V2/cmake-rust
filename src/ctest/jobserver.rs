//! GNU make POSIX jobserver client.

#[cfg(unix)]
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(unix)]
pub(super) struct Jobserver {
    reader: OwnedFd,
    writer: OwnedFd,
    held: usize,
    implicit_available: bool,
    pending_writes: usize,
}

#[cfg(unix)]
impl Jobserver {
    pub(super) fn connect() -> Option<Self> {
        let flags = std::env::var("MAKEFLAGS").ok()?;
        let args = crate::util::parse_unix_command_line(&flags);
        let mut auth = None;
        for arg in args.iter().rev() {
            for prefix in ["--jobserver-auth=", "--jobserver-fds=", "-J"] {
                if let Some(value) = arg.strip_prefix(prefix) {
                    auth = Some(value.trim().to_owned());
                    break;
                }
            }
            if auth.is_some() {
                break;
            }
        }
        let auth = auth?;
        if let Some(path) = auth.strip_prefix("fifo:") {
            return Self::connect_fifo(path);
        }
        let (reader, writer) = parse_fd_pair(&auth)?;
        Self::connect_fds(reader, writer)
    }

    fn duplicate(fd: RawFd, write: bool) -> Option<OwnedFd> {
        // SAFETY: dup duplicates a live kernel file descriptor or returns -1.
        let copy = unsafe { libc::dup(fd) };
        if copy < 0 {
            return None;
        }
        // SAFETY: copy is a live descriptor owned by this process.
        let owned = unsafe { OwnedFd::from_raw_fd(copy) };
        // SAFETY: fcntl does not access memory outside the process.
        if unsafe { libc::fcntl(copy, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return None;
        }
        // SAFETY: F_GETFL queries the descriptor status flags.
        let flags = unsafe { libc::fcntl(copy, libc::F_GETFL) };
        if flags < 0 {
            return None;
        }
        let access = flags & libc::O_ACCMODE;
        let valid = if write {
            access == libc::O_WRONLY || access == libc::O_RDWR
        } else {
            access == libc::O_RDONLY || access == libc::O_RDWR
        };
        valid.then_some(owned)
    }

    fn connect_fds(reader: RawFd, writer: RawFd) -> Option<Self> {
        let reader = Self::duplicate(reader, false)?;
        let writer = Self::duplicate(writer, true)?;
        Self::make_nonblocking(&reader)?;
        Self::make_nonblocking(&writer)?;
        Some(Self {
            reader,
            writer,
            held: 0,
            implicit_available: true,
            pending_writes: 0,
        })
    }

    fn connect_fifo(path: &str) -> Option<Self> {
        let path = std::ffi::CString::new(std::ffi::OsStr::new(path).as_bytes()).ok()?;
        // SAFETY: path is a valid NUL-terminated string and flags contain no invalid bits.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: fd is a live descriptor owned by this process.
        let fifo = unsafe { OwnedFd::from_raw_fd(fd) };
        Self::make_nonblocking(&fifo)?;
        let writer = Self::duplicate(fd, true)?;
        Some(Self {
            reader: fifo,
            writer,
            held: 0,
            implicit_available: true,
            pending_writes: 0,
        })
    }

    fn make_nonblocking(fd: &OwnedFd) -> Option<()> {
        use std::os::fd::AsRawFd;
        let raw = fd.as_raw_fd();
        // SAFETY: fcntl operates on the live descriptor and no user memory.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
        if flags < 0
            // SAFETY: F_SETFL updates only descriptor status flags.
            || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return None;
        }
        Some(())
    }

    pub(super) fn try_acquire(&mut self) -> bool {
        self.flush_writes();
        if self.implicit_available {
            self.implicit_available = false;
            self.held += 1;
            return true;
        }
        use std::os::fd::AsRawFd;
        let mut byte = 0u8;
        loop {
            // SAFETY: the buffer is writable for one byte and fd is a live descriptor.
            let count = unsafe { libc::read(self.reader.as_raw_fd(), (&raw mut byte).cast(), 1) };
            if count == 1 {
                self.held += 1;
                return true;
            }
            if count < 0 {
                // SAFETY: reading the thread-local errno is safe.
                let error = unsafe { *libc::__errno_location() };
                if error == libc::EINTR {
                    continue;
                }
            }
            return false;
        }
    }

    pub(super) fn release(&mut self) {
        if self.held == 0 {
            return;
        }
        self.held -= 1;
        if self.held == 0 {
            self.implicit_available = true;
            return;
        }
        self.pending_writes += 1;
        self.flush_writes();
    }

    fn flush_writes(&mut self) {
        use std::os::fd::AsRawFd;
        while self.pending_writes > 0 {
            let byte = b'.';
            // SAFETY: the buffer is readable for one byte and fd is a live descriptor.
            let count =
                unsafe { libc::write(self.writer.as_raw_fd(), (&raw const byte).cast(), 1) };
            if count == 1 {
                self.pending_writes -= 1;
                continue;
            }
            if count < 0 {
                // SAFETY: reading the thread-local errno is safe.
                let error = unsafe { *libc::__errno_location() };
                if error == libc::EINTR {
                    continue;
                }
                if error == libc::EAGAIN || error == libc::EWOULDBLOCK {
                    break;
                }
                self.pending_writes = 0;
            }
            break;
        }
    }
}

#[cfg(unix)]
fn parse_fd_pair(auth: &str) -> Option<(RawFd, RawFd)> {
    fn integer(input: &str) -> Option<(RawFd, usize)> {
        let mut length = 0;
        let mut end = 0;
        for byte in input.bytes() {
            if end == 0 && (byte == b'+' || byte == b'-') {
                end += 1;
                length += 1;
                continue;
            }
            if byte.is_ascii_digit() {
                end += 1;
                length += 1;
            } else {
                break;
            }
        }
        if end == 0 || input[..end].ends_with(['+', '-']) {
            return None;
        }
        input[..end].parse().ok().map(|value| (value, length))
    }
    let input = auth.trim_start();
    let (reader, used) = integer(input)?;
    let input = input[used..].strip_prefix(',')?;
    let input = input.trim_start();
    let (writer, _) = integer(input)?;
    Some((reader, writer))
}

#[cfg(not(unix))]
pub(super) struct Jobserver;

#[cfg(not(unix))]
impl Jobserver {
    pub(super) fn connect() -> Option<Self> {
        None
    }

    pub(super) fn try_acquire(&mut self) -> bool {
        false
    }

    pub(super) fn release(&mut self) {}
}
