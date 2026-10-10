use crate::error::{Error, Result};
use crate::meta::{Artifact, Index};
use crate::net::Fetcher;
use crate::platform::Platform;
use crate::util::{rename, set_executable, sha256_hex, write_atomic};
use crate::version::Version;
use std::path::{Path, PathBuf};
use url::Url;

pub const MAX_PATCHER_BYTES: u64 = 128 * 1024 * 1024;

pub fn available(index: &Index, current: Version, platform: Platform) -> Option<(Version, Artifact)> {
    let info = index.patcher.as_ref()?;
    let v = Version::parse(&info.version).ok()?;
    if v <= current {
        return None;
    }
    info.builds.get(platform.id()).map(|a| (v, a.clone()))
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    exe.with_file_name(name)
}

pub fn install(fetcher: &Fetcher, index_url: &Url, art: &Artifact, exe: &Path) -> Result<PathBuf> {
    if art.size == 0 || art.size > MAX_PATCHER_BYTES {
        return Err(Error::Metadata("patcher size out of range".into()));
    }
    let url = fetcher.policy.resolve(index_url, &art.url)?;
    let data = fetcher.get_small(&url, art.size)?;
    if data.len() as u64 != art.size || sha256_hex(&data) != art.sha256 {
        return Err(Error::Hash("downloaded patcher does not match the signed index".into()));
    }
    let staged = sibling(exe, ".new");
    write_atomic(&staged, &data).map_err(|e| match e {
        Error::Io { source, .. } => Error::NotWritable { path: exe.parent().unwrap_or(exe).to_path_buf(), source },
        other => other,
    })?;
    set_executable(&staged, true)?;
    if cfg!(windows) {
        let old = sibling(exe, ".old");
        let _ = std::fs::remove_file(&old);
        rename(exe, &old)?;
        if let Err(e) = rename(&staged, exe) {
            let _ = rename(&old, exe);
            return Err(e);
        }
    } else {
        rename(&staged, exe)?;
    }
    Ok(exe.to_path_buf())
}

pub fn cleanup(exe: &Path) {
    let _ = std::fs::remove_file(sibling(exe, ".old"));
    let _ = std::fs::remove_file(sibling(exe, ".new"));
}
