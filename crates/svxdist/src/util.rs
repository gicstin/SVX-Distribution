use crate::error::{Error, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn sha256_file(path: &Path, cancel: Option<&AtomicBool>) -> Result<(String, u64)> {
    let mut file = File::open(path).map_err(|e| Error::io_path("open", path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(Error::Cancelled);
        }
        let n = file.read(&mut buf).map_err(|e| Error::io_path("read", path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), total))
}

pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| Error::Install(format!("no parent for {}", path.display())))?;
    fs::create_dir_all(parent).map_err(|e| Error::io_path("create", parent, e))?;
    let tmp = tmp_sibling(path);
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp).map_err(|e| Error::io_path("create", &tmp, e))?;
        f.write_all(data).map_err(|e| Error::io_path("write", &tmp, e))?;
        f.sync_all().map_err(|e| Error::io_path("sync", &tmp, e))?;
    }
    rename(&tmp, path)?;
    sync_dir(parent);
    Ok(())
}

pub fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".tmp{}", std::process::id()));
    path.with_file_name(name)
}

pub fn rename(from: &Path, to: &Path) -> Result<()> {
    let mut last = None;
    for attempt in 0..5 {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied && attempt < 4 => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(200 * (attempt + 1)));
            }
            Err(e) => return Err(Error::Io { context: format!("move {} to {}", from.display(), to.display()), source: e }),
        }
    }
    Err(Error::Io { context: format!("move {} to {}", from.display(), to.display()), source: last.unwrap() })
}

pub fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() && !is_link(&m) => fs::remove_dir_all(path).map_err(|e| Error::io_path("remove", path, e)),
        Ok(_) => fs::remove_file(path).map_err(|e| Error::io_path("remove", path, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_path("inspect", path, e)),
    }
}

pub fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_path("remove", path, e)),
    }
}

pub fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

pub fn set_executable(path: &Path, exec: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if exec { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| Error::io_path("set permissions on", path, e))?;
    }
    #[cfg(not(unix))]
    let _ = (path, exec);
    Ok(())
}

pub fn is_executable(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        true
    }
}

pub fn random_id() -> String {
    let mut b = [0u8; 8];
    getrandom::getrandom(&mut b).expect("system random source");
    hex::encode(b)
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

pub fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) { Err(Error::Cancelled) } else { Ok(()) }
}
