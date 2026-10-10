use crate::error::{Error, Result};
use crate::platform::Platform;
use crate::util::is_link;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const STATE_DIR: &str = ".svx-patcher";
pub const CORE_DIR: &str = "Core";
pub const README: &str = "README.txt";
pub const PROTECTED: [&str; 6] = ["content", "user", "cache", "logs", "bench", STATE_DIR];
const MAX_PATH_LEN: usize = 200;
const MAX_COMPONENT_LEN: usize = 120;
const RESERVED_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7",
    "lpt8", "lpt9",
];

pub fn is_native_library(name: &str, platform: Platform) -> bool {
    name.starts_with("libsvx") && name.ends_with(platform.native_ext()) && name.len() > 6 + platform.native_ext().len()
}

pub fn fold(path: &str) -> String {
    path.to_lowercase()
}

pub fn validate_component(c: &str) -> std::result::Result<(), String> {
    if c.is_empty() {
        return Err("empty path component".into());
    }
    if c == "." || c == ".." {
        return Err("relative path component".into());
    }
    if c.len() > MAX_COMPONENT_LEN {
        return Err("path component too long".into());
    }
    if c.ends_with('.') || c.ends_with(' ') || c.starts_with(' ') {
        return Err("component starts or ends with a dot or space".into());
    }
    if let Some(bad) = c.chars().find(|ch| ch.is_control() || matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')) {
        return Err(format!("forbidden character {bad:?}"));
    }
    let stem = c.split('.').next().unwrap_or("").to_ascii_lowercase();
    if RESERVED_NAMES.contains(&stem.trim_end()) {
        return Err("reserved device name".into());
    }
    Ok(())
}

pub fn validate_relative(path: &str) -> std::result::Result<Vec<&str>, String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if path.len() > MAX_PATH_LEN {
        return Err("path too long".into());
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return Err("absolute or non-portable path".into());
    }
    let parts: Vec<&str> = path.split('/').collect();
    for c in &parts {
        validate_component(c)?;
    }
    Ok(parts)
}

pub fn check_managed(path: &str, platform: Platform) -> Result<()> {
    let fail = |why: String| Error::UnsafePath(format!("{path:?}: {why}"));
    let parts = validate_relative(path).map_err(fail)?;
    let first = parts[0];
    if PROTECTED.contains(&first.to_ascii_lowercase().as_str()) {
        return Err(fail("outside the patcher-managed files".into()));
    }
    if parts.len() == 1 {
        if first == platform.exe_name() || first == README || is_native_library(first, platform) {
            return Ok(());
        }
        return Err(fail(format!("not an allowed top-level file for {}", platform.id())));
    }
    if first == CORE_DIR {
        return Ok(());
    }
    Err(fail("not under Core/".into()))
}

pub fn check_set<'a>(paths: impl IntoIterator<Item = &'a str>, platform: Platform) -> Result<()> {
    let mut folded: HashSet<String> = HashSet::new();
    let mut dirs: HashSet<String> = HashSet::new();
    let mut files: Vec<String> = Vec::new();
    for p in paths {
        check_managed(p, platform)?;
        let f = fold(p);
        if !folded.insert(f.clone()) {
            return Err(Error::UnsafePath(format!("{p:?}: duplicate or case-colliding path")));
        }
        let mut acc = String::new();
        let comps: Vec<&str> = f.split('/').collect();
        for c in &comps[..comps.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(c);
            dirs.insert(acc.clone());
        }
        files.push(f);
    }
    for f in &files {
        if dirs.contains(f) {
            return Err(Error::UnsafePath(format!("{f:?}: used as both file and folder")));
        }
    }
    Ok(())
}

pub fn join(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    p
}

pub fn ensure_real_parents(root: &Path, rel: &str) -> Result<()> {
    let comps: Vec<&str> = rel.split('/').collect();
    let mut p = root.to_path_buf();
    for c in &comps[..comps.len() - 1] {
        p.push(c);
        match fs::symlink_metadata(&p) {
            Ok(m) if is_link(&m) => return Err(Error::UnsafePath(format!("{} is a link; refusing to follow it", p.display()))),
            Ok(m) if !m.is_dir() => return Err(Error::UnsafePath(format!("{} is not a folder", p.display()))),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::io_path("inspect", &p, e)),
        }
    }
    Ok(())
}

pub fn create_parents(root: &Path, rel: &str) -> Result<()> {
    ensure_real_parents(root, rel)?;
    if let Some(parent) = join(root, rel).parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io_path("create", parent, e))?;
    }
    ensure_real_parents(root, rel)
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: Platform = Platform::LinuxX86_64;
    const W: Platform = Platform::WindowsX86_64;

    #[test]
    fn allowlist() {
        assert!(check_managed("SVX.x86_64", L).is_ok());
        assert!(check_managed("SVX.exe", L).is_err());
        assert!(check_managed("SVX.exe", W).is_ok());
        assert!(check_managed("libsvx.linux.template_release.x86_64.so", L).is_ok());
        assert!(check_managed("libsvx.windows.template_release.x86_64.dll", W).is_ok());
        assert!(check_managed("libsvx.so.dll", L).is_err());
        assert!(check_managed("Core/textures/a.dds", L).is_ok());
        assert!(check_managed("README.txt", L).is_ok());
        for bad in [
            "../x",
            "/etc/passwd",
            "Core/../SVX.x86_64",
            "Core/./a",
            "Core//a",
            "C:/x",
            "Core\\a",
            "Content/a",
            "content/a",
            "User/settings.json",
            "Cache/x",
            "logs/latest.log",
            ".svx-patcher/installed.json",
            "core/a",
            "Core",
            "other.txt",
            "Core/con",
            "Core/aux.txt",
            "Core/a.",
            "Core/a ",
            "Core/a:b",
            "Core/a\u{0}",
            "Core/a?",
            "Bench/x",
            "",
        ] {
            assert!(check_managed(bad, L).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn collisions() {
        assert!(check_set(["Core/a", "Core/b"], L).is_ok());
        assert!(check_set(["Core/a", "Core/A"], L).is_err());
        assert!(check_set(["Core/a", "Core/a"], L).is_err());
        assert!(check_set(["Core/a", "Core/a/b"], L).is_err());
        assert!(check_set(["Core/A/b", "Core/a"], L).is_err());
    }
}
