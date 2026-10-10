use crate::error::Result;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseReport {
    pub closed: usize,
    pub remaining: usize,
}

pub fn running_in(dir: &Path) -> Vec<u32> {
    imp::running_in(dir)
}

pub fn instances_of(exe: &Path) -> Vec<u32> {
    imp::instances_of(exe)
}

pub fn force_close(exe: &Path, wait: Duration) -> Result<CloseReport> {
    imp::force_close(exe, wait)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::CloseReport;
    use crate::error::{Error, Result};
    use std::fs;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    fn exe_of(pid: u32) -> Option<PathBuf> {
        let target = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        let s = target.to_string_lossy();
        Some(PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s)))
    }

    fn pids() -> Vec<u32> {
        let me = std::process::id();
        let Ok(entries) = fs::read_dir("/proc") else { return Vec::new() };
        entries.flatten().filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok())).filter(|p| *p != me).collect()
    }

    pub fn instances_of(exe: &Path) -> Vec<u32> {
        let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
        pids().into_iter().filter(|p| exe_of(*p).as_deref() == Some(exe.as_path())).collect()
    }

    fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0 as libc::c_uint) };
        if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) }) }
    }

    fn pidfd_signal(fd: &OwnedFd, sig: libc::c_int) -> io::Result<()> {
        let r = unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, std::ptr::null::<libc::siginfo_t>(), 0 as libc::c_uint) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn exited(fd: &OwnedFd, timeout: Duration) -> bool {
        let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        unsafe { libc::poll(&mut p, 1, ms) > 0 }
    }

    pub fn force_close(exe: &Path, wait: Duration) -> Result<CloseReport> {
        let exe = fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
        let mut targets = Vec::new();
        for pid in instances_of(&exe) {
            let fd = match pidfd_open(pid) {
                Ok(fd) => fd,
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => continue,
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
                    return Err(Error::Install("This system cannot close SVX safely from the patcher. Close it from your system monitor.".into()));
                }
                Err(e) => return Err(Error::io(format!("open SVX process {pid}"), e)),
            };
            if exe_of(pid).as_deref() != Some(exe.as_path()) || pidfd_signal(&fd, 0).is_err() {
                continue;
            }
            targets.push((pid, fd));
        }
        for (pid, fd) in &targets {
            match pidfd_signal(fd, libc::SIGKILL) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(Error::io(format!("close SVX process {pid}"), e)),
            }
        }
        let deadline = Instant::now() + wait;
        let mut report = CloseReport::default();
        for (_, fd) in &targets {
            if exited(fd, deadline.saturating_duration_since(Instant::now())) {
                report.closed += 1;
            } else {
                report.remaining += 1;
            }
        }
        Ok(report)
    }

    pub fn running_in(dir: &Path) -> Vec<u32> {
        let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        pids().into_iter().filter(|p| exe_of(*p).is_some_and(|exe| exe.starts_with(&dir) && !exe.starts_with(dir.join(crate::paths::STATE_DIR)))).collect()
    }
}

#[cfg(windows)]
mod imp {
    use super::CloseReport;
    use crate::error::Result;
    use std::path::Path;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
        WaitForSingleObject,
    };

    fn normalize(s: &str) -> String {
        let s = s.strip_prefix(r"\\?\").unwrap_or(s);
        let mut s = s.replace('/', "\\").to_lowercase();
        while s.ends_with('\\') {
            s.pop();
        }
        s
    }

    fn canonical(p: &Path) -> String {
        normalize(&std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy())
    }

    unsafe fn image_of(h: HANDLE) -> Option<String> {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        if unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) } == 0 {
            return None;
        }
        Some(normalize(&String::from_utf16_lossy(&buf[..len as usize])))
    }

    fn pids() -> Vec<u32> {
        let me = std::process::id();
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut entry) != 0;
            while ok {
                let pid = entry.th32ProcessID;
                if pid != me && pid != 0 {
                    out.push(pid);
                }
                ok = Process32NextW(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
        }
        out
    }

    fn matching(matches: impl Fn(&str) -> bool) -> Vec<u32> {
        let mut out = Vec::new();
        for pid in pids() {
            unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if h.is_null() {
                    continue;
                }
                if image_of(h).is_some_and(|p| matches(&p)) {
                    out.push(pid);
                }
                CloseHandle(h);
            }
        }
        out
    }

    pub fn running_in(dir: &Path) -> Vec<u32> {
        let prefix = format!("{}\\", canonical(dir));
        let state = format!("{prefix}{}\\", crate::paths::STATE_DIR);
        matching(|p| p.starts_with(&prefix) && !p.starts_with(&state))
    }

    pub fn instances_of(exe: &Path) -> Vec<u32> {
        let exe = canonical(exe);
        matching(|p| p == exe)
    }

    pub fn force_close(exe: &Path, wait: Duration) -> Result<CloseReport> {
        let exe = canonical(exe);
        let mut handles = Vec::new();
        for pid in matching(|p| p == exe) {
            unsafe {
                let h = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid);
                if h.is_null() {
                    continue;
                }
                if image_of(h).as_deref() == Some(exe.as_str()) && TerminateProcess(h, 1) != 0 {
                    handles.push(h);
                } else {
                    CloseHandle(h);
                }
            }
        }
        let deadline = Instant::now() + wait;
        let mut report = CloseReport::default();
        for h in handles {
            let ms = deadline.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128 - 1) as u32;
            unsafe {
                if WaitForSingleObject(h, ms) == WAIT_OBJECT_0 {
                    report.closed += 1;
                } else {
                    report.remaining += 1;
                }
                CloseHandle(h);
            }
        }
        Ok(report)
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn running_in(_dir: &std::path::Path) -> Vec<u32> {
        Vec::new()
    }

    pub fn instances_of(_exe: &std::path::Path) -> Vec<u32> {
        Vec::new()
    }

    pub fn force_close(_exe: &std::path::Path, _wait: std::time::Duration) -> crate::error::Result<super::CloseReport> {
        Err(crate::error::Error::Install("Closing SVX is not supported on this system.".into()))
    }
}
