//! Keep a protocol stream clean.
//!
//! When rust-bot speaks JSON-RPC on stdout (`rust-bot acp`), a single stray
//! byte from a `println!` anywhere in rust-bot or a dependency corrupts the
//! stream. Auditing every print is fragile, so the protocol writer takes the
//! *real* stdout for itself and the process-level stdout is pointed at stderr:
//! whatever prints afterwards lands on stderr.

use std::fs::File;
use std::io;

/// Claim the process's real standard output for protocol frames.
///
/// Returns a [`File`] for the original stdout and redirects the process-level
/// stdout to stderr. Call it exactly once, first thing at startup, before
/// anything prints. The redirect is process-global and cannot be undone, so it
/// is verified by running the real binary (`tests/acp_stdout_test.rs`), not by
/// in-process unit tests.
pub fn claim_protocol_stdout() -> io::Result<File> {
    imp::claim()
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::fd::FromRawFd;

    pub fn claim() -> io::Result<File> {
        // SAFETY: `dup`/`dup2`/`close` only touch this process's own standard
        // descriptors, and the duplicate is handed to exactly one owner.
        unsafe {
            let real_stdout = libc::dup(libc::STDOUT_FILENO);
            if real_stdout < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) < 0 {
                let error = io::Error::last_os_error();
                libc::close(real_stdout);
                return Err(error);
            }
            Ok(File::from_raw_fd(real_stdout))
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::{FromRawHandle, RawHandle};

    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    pub fn claim() -> io::Result<File> {
        // SAFETY: `GetStdHandle`/`SetStdHandle` only read and swap this
        // process's standard handle slots. Rust's `std::io::stdout()` looks the
        // slot up on every write, so swapping it redirects `println!`. The
        // original handle stays valid and is owned by the returned `File`.
        unsafe {
            let real_stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            let stderr = GetStdHandle(STD_ERROR_HANDLE);
            if real_stdout.is_null() || real_stdout == INVALID_HANDLE_VALUE {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "standard output is not available",
                ));
            }
            if stderr.is_null() || stderr == INVALID_HANDLE_VALUE {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "standard error is not available",
                ));
            }
            if SetStdHandle(STD_OUTPUT_HANDLE, stderr) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(File::from_raw_handle(real_stdout as RawHandle))
        }
    }
}
