use crate::error::{Error, Result};
use crate::meta::Manifest;
use crate::paths::{self, CORE_DIR, STATE_DIR};
use crate::platform::Platform;
use crate::trust::parse_stored_manifest;
use crate::util::{is_executable, is_link, sha256_file, write_atomic};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::UNIX_EPOCH;

pub const RECORD_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledRecord {
    pub schema: u32,
    pub version: String,
    pub build: u64,
    pub platform: String,
    pub manifest: String,
    pub manifest_url: String,
    pub installed_at: String,
    pub origin: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileState {
    pub sha256: String,
    pub size: u64,
    pub exec: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Health {
    pub missing: Vec<String>,
    pub changed: Vec<String>,
}

impl Health {
    pub fn ok(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty()
    }

    pub fn summary(&self) -> String {
        let files = |n: usize| if n == 1 { "1 game file".to_string() } else { format!("{n} game files") };
        match (self.missing.len(), self.changed.len()) {
            (m, 0) => format!("{} missing", files(m)),
            (0, c) => format!("{} damaged", files(c)),
            (m, c) => format!("{} missing and {} damaged", files(m), c),
        }
    }
}

pub struct Installed {
    pub record: InstalledRecord,
    pub manifest: Manifest,
    pub health: Health,
}

pub enum Detected {
    Absent,
    Unmanaged,
    Managed(Box<Installed>),
    Damaged(String),
}

pub struct InstallLock {
    _file: File,
}

#[derive(Clone, Debug)]
pub struct Install {
    pub root: PathBuf,
    pub platform: Platform,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CacheEntry {
    size: u64,
    mtime: u128,
    sha256: String,
}

impl Install {
    pub fn new(root: &Path, platform: Platform) -> Install {
        let root = fs::canonicalize(root).map(|p| plain(&p)).unwrap_or_else(|_| absolute(root));
        Install { root, platform }
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    pub fn record_path(&self) -> PathBuf {
        self.state_dir().join("installed.json")
    }

    pub fn txn_dir(&self) -> PathBuf {
        self.state_dir().join("txn")
    }

    pub fn rollback_dir(&self) -> PathBuf {
        self.state_dir().join("rollback")
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.state_dir().join("downloads")
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        paths::join(&self.root, rel)
    }

    pub fn exe(&self) -> PathBuf {
        self.root.join(self.platform.exe_name())
    }

    pub fn check_writable(&self) -> Result<()> {
        let not_writable = |source: io::Error| Error::NotWritable { path: self.root.clone(), source };
        if let Ok(m) = fs::symlink_metadata(&self.root)
            && !m.is_dir()
        {
            return Err(not_writable(io::Error::other("not a folder")));
        }
        fs::create_dir_all(&self.root).map_err(not_writable)?;
        let state = self.state_dir();
        if let Ok(m) = fs::symlink_metadata(&state)
            && (is_link(&m) || !m.is_dir())
        {
            return Err(Error::UnsafePath(format!("{} is not a plain folder", state.display())));
        }
        fs::create_dir_all(&state).map_err(not_writable)?;
        let probe = state.join(format!("write-probe-{}", std::process::id()));
        File::create(&probe).map_err(not_writable)?;
        let _ = fs::remove_file(&probe);
        let probe = self.root.join(format!(".svx-write-probe-{}", std::process::id()));
        File::create(&probe).map_err(not_writable)?;
        let _ = fs::remove_file(&probe);
        Ok(())
    }

    pub fn lock(&self) -> Result<InstallLock> {
        self.check_writable()?;
        let path = self.state_dir().join("lock");
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path).map_err(|e| Error::io_path("open", &path, e))?;
        match file.try_lock() {
            Ok(()) => Ok(InstallLock { _file: file }),
            Err(fs::TryLockError::WouldBlock) => Err(Error::Busy),
            Err(fs::TryLockError::Error(e)) => Err(Error::io_path("lock", &path, e)),
        }
    }

    pub fn read_record(&self) -> Result<Option<InstalledRecord>> {
        let path = self.record_path();
        match fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| Error::Metadata(format!("installed.json: {e}"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io_path("read", &path, e)),
        }
    }

    pub fn write_record(&self, record: Option<&InstalledRecord>) -> Result<()> {
        match record {
            Some(r) => write_atomic(&self.record_path(), &serde_json::to_vec_pretty(r).expect("serialize record")),
            None => crate::util::remove_file_if_exists(&self.record_path()),
        }
    }

    pub fn record_manifest(&self, r: &InstalledRecord) -> Result<Manifest> {
        if r.platform != self.platform.id() {
            return Err(Error::Platform(format!("installation is for {}, this system is {}", r.platform, self.platform.id())));
        }
        parse_stored_manifest(r.manifest.as_bytes(), self.platform)
    }

    pub fn looks_installed(&self) -> bool {
        self.exe().exists() || self.root.join(CORE_DIR).exists() || self.record_path().exists()
    }

    pub fn detect(&self) -> Detected {
        if !self.root.is_dir() || !self.looks_installed() {
            return Detected::Absent;
        }
        match self.read_record() {
            Ok(Some(record)) => match self.record_manifest(&record) {
                Ok(manifest) => {
                    let health = self.quick_health(&manifest);
                    Detected::Managed(Box::new(Installed { record, manifest, health }))
                }
                Err(e) => Detected::Damaged(e.to_string()),
            },
            Ok(None) => Detected::Unmanaged,
            Err(e) => Detected::Damaged(e.to_string()),
        }
    }

    pub fn quick_health(&self, m: &Manifest) -> Health {
        let mut h = Health::default();
        for f in &m.files {
            match fs::symlink_metadata(self.path(&f.path)) {
                Ok(meta) if meta.is_file() && !is_link(&meta) && meta.len() == f.size && (!f.exec || is_executable(&meta)) => {}
                Ok(_) => h.changed.push(f.path.clone()),
                Err(_) => h.missing.push(f.path.clone()),
            }
        }
        h
    }

    fn cache_path(&self) -> PathBuf {
        self.state_dir().join("hashcache.json")
    }

    fn load_cache(&self) -> BTreeMap<String, CacheEntry> {
        fs::read(self.cache_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn current(&self, rel: &str, use_cache: bool, cache: &mut HashCache, cancel: &AtomicBool) -> Result<Option<FileState>> {
        let p = self.path(rel);
        paths::ensure_real_parents(&self.root, rel)?;
        let meta = match fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::io_path("inspect", &p, e)),
        };
        if is_link(&meta) || !meta.is_file() {
            return Ok(Some(FileState { sha256: String::new(), size: 0, exec: false }));
        }
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        let exec = is_executable(&meta);
        if use_cache
            && let Some(c) = cache.entries.get(rel)
            && c.size == meta.len()
            && c.mtime == mtime
            && mtime != 0
        {
            return Ok(Some(FileState { sha256: c.sha256.clone(), size: c.size, exec }));
        }
        let (sha, size) = sha256_file(&p, Some(cancel))?;
        cache.entries.insert(rel.to_string(), CacheEntry { size, mtime, sha256: sha.clone() });
        cache.dirty = true;
        Ok(Some(FileState { sha256: sha, size, exec }))
    }

    pub fn hash_cache(&self) -> HashCache {
        HashCache { entries: self.load_cache(), dirty: false }
    }

    pub fn save_cache(&self, cache: &mut HashCache) {
        if cache.dirty {
            if let Ok(b) = serde_json::to_vec(&cache.entries) {
                let _ = write_atomic(&self.cache_path(), &b);
            }
            cache.dirty = false;
        }
    }

    pub fn remember(&self, cache: &mut HashCache, rel: &str, state: &FileState) {
        let Ok(meta) = fs::symlink_metadata(self.path(rel)) else { return };
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        if meta.is_file() && meta.len() == state.size && mtime != 0 {
            cache.entries.insert(rel.to_string(), CacheEntry { size: state.size, mtime, sha256: state.sha256.clone() });
            cache.dirty = true;
        }
    }

    pub fn forget_cached(&self, cache: &mut HashCache, rel: &str) {
        if cache.entries.remove(rel).is_some() {
            cache.dirty = true;
        }
    }

    pub fn unknown_managed_files(&self, known: &dyn Fn(&str) -> bool) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else { return Ok(out) };
        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(|s| s.to_string()) else { continue };
            let Ok(meta) = fs::symlink_metadata(e.path()) else { continue };
            if !meta.is_dir() && paths::check_managed(&name, self.platform).is_ok() && !known(&name) {
                out.push(name);
            }
        }
        let core = self.root.join(CORE_DIR);
        if let Ok(meta) = fs::symlink_metadata(&core) {
            if is_link(&meta) {
                return Err(Error::UnsafePath(format!("{} is a link; refusing to manage it", core.display())));
            }
            if meta.is_dir() {
                walk(&core, CORE_DIR, &mut |rel| {
                    if !known(rel) {
                        out.push(rel.to_string());
                    }
                })?;
            }
        }
        out.retain(|p| paths::validate_relative(p).is_ok());
        out.sort();
        Ok(out)
    }
}

pub struct HashCache {
    entries: BTreeMap<String, CacheEntry>,
    dirty: bool,
}

fn walk(dir: &Path, rel: &str, f: &mut dyn FnMut(&str)) -> Result<()> {
    let entries = fs::read_dir(dir).map_err(|e| Error::io_path("list", dir, e))?;
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(|s| s.to_string()) else { continue };
        let child = format!("{rel}/{name}");
        let Ok(meta) = fs::symlink_metadata(e.path()) else { continue };
        if meta.is_dir() && !is_link(&meta) {
            walk(&e.path(), &child, f)?;
        } else {
            f(&child);
        }
    }
    Ok(())
}

pub fn plain(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.len() >= 3 && rest.as_bytes()[1] == b':' && rest.as_bytes()[0].is_ascii_alphabetic() => PathBuf::from(rest),
        _ => p.to_path_buf(),
    }
}

pub fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_prefix_is_removed() {
        assert_eq!(plain(Path::new(r"\\?\C:\Users\a\Games\SVX")), PathBuf::from(r"C:\Users\a\Games\SVX"));
        assert_eq!(plain(Path::new(r"\\?\UNC\server\share")), PathBuf::from(r"\\?\UNC\server\share"));
        assert_eq!(plain(Path::new("/home/a/Games/SVX")), PathBuf::from("/home/a/Games/SVX"));
    }
}
