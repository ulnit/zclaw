//! Cross-platform process control — liveness probes, termination and
//! file locking.
//!
//! Unix maps to signals (`kill(pid, 0)` / SIGTERM / SIGKILL) and
//! `flock`; Windows maps to `OpenProcess` / `TerminateProcess` and
//! `LockFileEx`. A terminated process surfaces as
//! `ErrorKind::NotFound` so callers can treat "already gone"
//! uniformly.

/// True iff `pid` refers to a live process. `EPERM` / access-denied
/// counts as alive (the process exists but belongs to someone else).
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as i32, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// True iff `pid` refers to a live process (Windows `OpenProcess`
/// probe — `ERROR_INVALID_PARAMETER` means the pid is gone).
#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        CloseHandle(handle);
        true
    }
}

/// Request termination (SIGTERM on unix).
#[cfg(unix)]
pub fn terminate(pid: u32) -> std::io::Result<()> {
    signal(pid, libc::SIGTERM)
}

/// Force-kill (SIGKILL on unix).
#[cfg(unix)]
pub fn kill_hard(pid: u32) -> std::io::Result<()> {
    signal(pid, libc::SIGKILL)
}

#[cfg(unix)]
fn signal(pid: u32, sig: i32) -> std::io::Result<()> {
    if unsafe { libc::kill(pid as i32, sig) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, err));
    }
    Err(err)
}

/// Request termination (Windows `TerminateProcess`).
#[cfg(windows)]
pub fn terminate(pid: u32) -> std::io::Result<()> {
    terminate_impl(pid)
}

/// Force-kill — identical to [`terminate`] on Windows.
#[cfg(windows)]
pub fn kill_hard(pid: u32) -> std::io::Result<()> {
    terminate_impl(pid)
}

#[cfg(windows)]
fn terminate_impl(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    if pid == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "pid 0"));
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle == 0 {
            let code = GetLastError();
            let err = std::io::Error::from_raw_os_error(code as i32);
            return Err(if code == ERROR_INVALID_PARAMETER {
                std::io::Error::new(std::io::ErrorKind::NotFound, err)
            } else {
                err
            });
        }
        let ok = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Advisory file locks (hermes singleton/dispatch-tick lock semantics).
// ---------------------------------------------------------------------------

/// Blocking exclusive lock on an open file.
#[cfg(unix)]
pub fn lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Non-blocking exclusive lock; `Err(WouldBlock)` when already held.
#[cfg(unix)]
pub fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, err));
        }
        Err(err)
    }
}

/// Release a lock taken via [`lock_exclusive`] / [`try_lock_exclusive`].
#[cfg(unix)]
pub fn unlock(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn lock_file_ex(file: &std::fs::File, fail_immediately: bool) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    let mut flags = LOCKFILE_EXCLUSIVE_LOCK;
    if fail_immediately {
        flags |= LOCKFILE_FAIL_IMMEDIATELY;
    }
    // OVERLAPPED lives on this stack frame; LockFileEx with an event
    // unset completes synchronously.
    let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as isize,
            flags,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        // ERROR_LOCK_VIOLATION (33) is the WouldBlock of LockFileEx.
        if err.raw_os_error() == Some(33) {
            return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, err));
        }
        return Err(err);
    }
    Ok(())
}

/// Blocking exclusive lock on an open file (Windows `LockFileEx`).
#[cfg(windows)]
pub fn lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    lock_file_ex(file, false)
}

/// Non-blocking exclusive lock (Windows `LockFileEx` with
/// `LOCKFILE_FAIL_IMMEDIATELY`).
#[cfg(windows)]
pub fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    lock_file_ex(file, true)
}

/// Release a lock taken via [`lock_exclusive`] / [`try_lock_exclusive`]
/// (Windows `UnlockFileEx`).
#[cfg(windows)]
pub fn unlock(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    let mut overlapped: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as isize,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
