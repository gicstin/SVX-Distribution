use crate::changelog;
use crate::error::{Error, Result};
use crate::paths;
use crate::platform::Platform;
use crate::sign::{self, Envelope};
use crate::time::parse_utc;
use crate::util::is_sha256_hex;
use crate::version::Version;
use ed25519_dalek::VerifyingKey;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

pub const SPEC: u32 = 1;
pub const MAX_ROOT_BYTES: u64 = 256 * 1024;
pub const MAX_INDEX_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_FILES: usize = 200_000;
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_PACK_BYTES: u64 = 2 * 1024 * 1024 * 1024 - 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyDef {
    pub scheme: String,
    pub public: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Role {
    pub keyids: Vec<String>,
    pub threshold: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Roles {
    pub root: Role,
    pub release: Role,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Root {
    #[serde(rename = "type")]
    pub kind: String,
    pub spec: u32,
    pub version: u64,
    pub expires: String,
    pub keys: BTreeMap<String, KeyDef>,
    pub roles: Roles,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseRef {
    pub version: String,
    pub build: u64,
    pub date: String,
    pub manifest: Artifact,
    pub download_size: u64,
    pub install_size: u64,
    pub min_patcher: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub withdrawn: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlatformIndex {
    pub latest: String,
    pub releases: Vec<ReleaseRef>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatcherInfo {
    pub version: String,
    pub builds: BTreeMap<String, Artifact>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Index {
    #[serde(rename = "type")]
    pub kind: String,
    pub spec: u32,
    pub version: u64,
    pub channel: String,
    pub platforms: BTreeMap<String, PlatformIndex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patcher: Option<PatcherInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changelog: Vec<changelog::Entry>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Encoding {
    #[serde(rename = "deflate")]
    Deflate,
    #[serde(rename = "store")]
    Store,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default)]
    pub exec: bool,
    pub pack: usize,
    pub offset: u64,
    pub length: u64,
    pub encoding: Encoding,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pack {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Launch {
    pub exe: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    #[serde(rename = "type")]
    pub kind: String,
    pub spec: u32,
    pub version: String,
    pub build: u64,
    pub commit: String,
    pub platform: String,
    pub min_patcher: String,
    pub date: String,
    pub launch: Launch,
    pub files: Vec<FileEntry>,
    pub packs: Vec<Pack>,
}

fn meta_err(what: &str, why: impl std::fmt::Display) -> Error {
    Error::Metadata(format!("{what}: {why}"))
}

pub fn decode<T: DeserializeOwned>(env: &Envelope, what: &str) -> Result<T> {
    serde_json::from_value(env.signed.clone()).map_err(|e| meta_err(what, e))
}

fn check_sha(s: &str, what: &str) -> Result<()> {
    if is_sha256_hex(s) { Ok(()) } else { Err(meta_err(what, "sha256 must be 64 lowercase hex characters")) }
}

impl Root {
    pub fn validate(&self) -> Result<()> {
        if self.kind != "root" || self.spec != SPEC {
            return Err(meta_err("root", "wrong type or spec"));
        }
        if self.version == 0 {
            return Err(meta_err("root", "version must start at 1"));
        }
        parse_utc(&self.expires)?;
        for (id, k) in &self.keys {
            if k.scheme != "ed25519" {
                return Err(meta_err("root", format!("unsupported key scheme {}", k.scheme)));
            }
            let key = sign::parse_public(&k.public)?;
            if &sign::key_id(&key) != id {
                return Err(meta_err("root", format!("key id {id} does not match its key")));
            }
        }
        for (name, role) in [("root", &self.roles.root), ("release", &self.roles.release)] {
            let unique: HashSet<&String> = role.keyids.iter().collect();
            if role.threshold == 0 || role.threshold as usize > unique.len() || unique.len() != role.keyids.len() {
                return Err(meta_err("root", format!("{name} role threshold or key list is invalid")));
            }
            if role.keyids.iter().any(|k| !self.keys.contains_key(k)) {
                return Err(meta_err("root", format!("{name} role names an unknown key")));
            }
        }
        Ok(())
    }

    pub fn key_map(&self) -> BTreeMap<String, VerifyingKey> {
        self.keys.iter().filter_map(|(id, k)| sign::parse_public(&k.public).ok().map(|v| (id.clone(), v))).collect()
    }
}

impl Index {
    pub fn validate(&self) -> Result<()> {
        if self.kind != "index" || self.spec != SPEC {
            return Err(meta_err("index", "wrong type or spec"));
        }
        for (pid, p) in &self.platforms {
            if Platform::parse(pid).is_none() {
                continue;
            }
            let mut seen = HashSet::new();
            for r in &p.releases {
                Version::parse(&r.version)?;
                Version::parse(&r.min_patcher)?;
                check_sha(&r.manifest.sha256, "index manifest reference")?;
                if r.manifest.size == 0 || r.manifest.size > MAX_MANIFEST_BYTES {
                    return Err(meta_err("index", format!("manifest size for {} out of range", r.version)));
                }
                if !seen.insert(r.version.clone()) {
                    return Err(meta_err("index", format!("{pid} lists {} twice", r.version)));
                }
            }
            match p.releases.iter().find(|r| r.version == p.latest) {
                Some(r) if !r.withdrawn => {}
                _ => return Err(meta_err("index", format!("{pid} latest release {} is not listed or withdrawn", p.latest))),
            }
        }
        if let Some(pi) = &self.patcher {
            Version::parse(&pi.version)?;
            for a in pi.builds.values() {
                check_sha(&a.sha256, "patcher build")?;
            }
        }
        changelog::validate(&self.changelog)
    }

    pub fn platform(&self, platform: Platform) -> Option<&PlatformIndex> {
        self.platforms.get(platform.id())
    }
}

impl PlatformIndex {
    pub fn latest_ref(&self) -> Option<&ReleaseRef> {
        self.releases.iter().find(|r| r.version == self.latest && !r.withdrawn)
    }

    pub fn find(&self, version: &str) -> Option<&ReleaseRef> {
        self.releases.iter().find(|r| r.version == version && !r.withdrawn)
    }

    pub fn offered(&self) -> Vec<&ReleaseRef> {
        let mut v: Vec<&ReleaseRef> = self.releases.iter().filter(|r| !r.withdrawn).collect();
        v.sort_by(|a, b| Version::parse(&b.version).ok().cmp(&Version::parse(&a.version).ok()));
        v
    }
}

impl Manifest {
    pub fn validate(&self, platform: Platform) -> Result<()> {
        if self.kind != "release" || self.spec != SPEC {
            return Err(meta_err("manifest", "wrong type or spec"));
        }
        if self.platform != platform.id() {
            return Err(Error::Platform(format!("release is for {}, this system is {}", self.platform, platform.id())));
        }
        Version::parse(&self.version)?;
        Version::parse(&self.min_patcher)?;
        if self.files.is_empty() || self.files.len() > MAX_FILES {
            return Err(meta_err("manifest", "file count out of range"));
        }
        paths::check_set(self.files.iter().map(|f| f.path.as_str()), platform)?;
        if self.launch.exe != platform.exe_name() || !self.files.iter().any(|f| f.path == self.launch.exe) {
            return Err(meta_err("manifest", "launch executable missing or wrong for this platform"));
        }
        if !self.files.iter().any(|f| paths::is_native_library(&f.path, platform)) {
            return Err(meta_err("manifest", "native library missing"));
        }
        let mut pack_names = HashSet::new();
        for p in &self.packs {
            check_sha(&p.sha256, "pack")?;
            paths::validate_component(&p.name).map_err(|e| meta_err("pack name", e))?;
            if p.size == 0 || p.size > MAX_PACK_BYTES {
                return Err(meta_err("manifest", format!("pack {} size out of range", p.name)));
            }
            if !pack_names.insert(p.url.clone()) {
                return Err(meta_err("manifest", format!("pack {} listed twice", p.name)));
            }
        }
        for f in &self.files {
            check_sha(&f.sha256, &f.path)?;
            let pack = self.packs.get(f.pack).ok_or_else(|| meta_err(&f.path, "pack index out of range"))?;
            if f.size > MAX_FILE_BYTES {
                return Err(meta_err(&f.path, "file too large"));
            }
            let end = f.offset.checked_add(f.length).ok_or_else(|| meta_err(&f.path, "range overflow"))?;
            if end > pack.size {
                return Err(meta_err(&f.path, "range beyond pack end"));
            }
            match f.encoding {
                Encoding::Store if f.length != f.size => return Err(meta_err(&f.path, "stored length differs from size")),
                Encoding::Deflate if f.length > f.size + f.size / 64 + 4096 => return Err(meta_err(&f.path, "compressed length implausible")),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn file(&self, path: &str) -> Option<&FileEntry> {
        self.files.iter().find(|f| f.path == path)
    }

    pub fn install_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    pub fn download_size(&self) -> u64 {
        self.files.iter().map(|f| f.length).sum()
    }
}
