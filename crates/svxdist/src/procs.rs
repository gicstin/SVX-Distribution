use std::path::Path;

pub fn running_in(dir: &Path) -> Vec<u32> {
    imp::running_in(dir)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};

    pub fn running_in(dir: &Path) -> Vec<u32> {
        let me = std::process::id();
        let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir("/proc") else { return out };
        for e in entries.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            if pid == me {
                continue;
            }
            let Ok(target) = fs::read_link(e.path().join("exe")) else { continue };
            let s = target.to_string_lossy();
            let exe = PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s));
            if exe.starts_with(&dir) && !exe.starts_with(dir.join(crate::paths::STATE_DIR)) {
                out.push(pid);
            }
        }
        out
    }
}

#[cfg(windows)]
mod imp {
    use std::path::Path;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};

    fn normalize(s: &str) -> String {
        let s = s.strip_prefix(r"\\?\").unwrap_or(s);
        let mut s = s.replace('/', "\\").to_lowercase();
        while s.ends_with('\\') {
            s.pop();
        }
        s
    }

    pub fn running_in(dir: &Path) -> Vec<u32> {
        let me = std::process::id();
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let prefix = format!("{}\\", normalize(&dir.to_string_lossy()));
        let state = format!("{prefix}{}\\", crate::paths::STATE_DIR);
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
                    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    if !h.is_null() {
                        let mut buf = [0u16; 1024];
                        let mut len = buf.len() as u32;
                        if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) != 0 {
                            let path = normalize(&String::from_utf16_lossy(&buf[..len as usize]));
                            if path.starts_with(&prefix) && !path.starts_with(&state) {
                                out.push(pid);
                            }
                        }
                        CloseHandle(h);
                    }
                }
                ok = Process32NextW(snap, &mut entry) != 0;
            }
            CloseHandle(snap);
        }
        out
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod imp {
    pub fn running_in(_dir: &std::path::Path) -> Vec<u32> {
        Vec::new()
    }
}
