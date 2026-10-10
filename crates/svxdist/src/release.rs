use crate::changelog;
use crate::error::{Error, Result};
use crate::meta::{self, Artifact, Encoding, FileEntry, Index, Launch, Manifest, Pack, PatcherInfo, PlatformIndex, ReleaseRef, SPEC};
use crate::paths::{self, PROTECTED};
use crate::platform::Platform;
use crate::sign::{self, envelope_bytes};
use crate::time::now_unix;
use crate::util::{is_executable, is_link, sha256_file, sha256_hex};
use crate::version::Version;
use ed25519_dalek::SigningKey;
use flate2::Compression;
use flate2::write::DeflateEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use url::Url;

pub const PAYLOAD_INFO: &str = "release.json";
pub const DEFAULT_PACK_LIMIT: u64 = 1536 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct PayloadFile {
    pub rel: String,
    pub abs: PathBuf,
    pub exec: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PayloadInfo {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub commit: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub date: String,
}

pub fn read_payload_info(dir: &Path) -> Option<PayloadInfo> {
    fs::read(dir.join(PAYLOAD_INFO)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

pub fn collect_payload(dir: &Path, platform: Platform) -> Result<(Vec<PayloadFile>, Vec<String>)> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| Error::io_path("list", dir, e))?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if PROTECTED.contains(&name.to_ascii_lowercase().as_str()) || name == PAYLOAD_INFO {
            skipped.push(name);
            continue;
        }
        walk(&e.path(), &name, platform, &mut files)?;
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    paths::check_set(files.iter().map(|f| f.rel.as_str()), platform)?;
    skipped.sort();
    Ok((files, skipped))
}

fn walk(abs: &Path, rel: &str, platform: Platform, out: &mut Vec<PayloadFile>) -> Result<()> {
    let meta = fs::symlink_metadata(abs).map_err(|e| Error::io_path("inspect", abs, e))?;
    if is_link(&meta) {
        return Err(Error::UnsafePath(format!("{rel}: links are not allowed in a payload")));
    }
    if meta.is_dir() {
        for e in fs::read_dir(abs).map_err(|e| Error::io_path("list", abs, e))?.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            walk(&e.path(), &format!("{rel}/{name}"), platform, out)?;
        }
        return Ok(());
    }
    paths::check_managed(rel, platform)?;
    let exec = platform == Platform::LinuxX86_64 && (rel == platform.exe_name() || is_executable(&meta));
    out.push(PayloadFile { rel: rel.to_string(), abs: abs.to_path_buf(), exec });
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub version: String,
    pub build: u64,
    pub commit: String,
    pub platform: Platform,
    pub min_patcher: String,
    pub date: String,
}

#[derive(Clone, Debug)]
pub struct Reused {
    pub pack: Pack,
    pub offset: u64,
    pub length: u64,
    pub encoding: Encoding,
}

#[derive(Default)]
pub struct ReuseIndex {
    pub blobs: HashMap<String, Reused>,
}

impl ReuseIndex {
    pub fn add(&mut self, m: &Manifest, manifest_url: &Url) -> Result<()> {
        for f in &m.files {
            let mut pack = m.packs[f.pack].clone();
            pack.url = manifest_url.join(&pack.url).map_err(|e| Error::UnsafeUrl(e.to_string()))?.to_string();
            self.blobs.entry(f.sha256.clone()).or_insert(Reused { pack, offset: f.offset, length: f.length, encoding: f.encoding });
        }
        Ok(())
    }
}

pub struct Built {
    pub manifest: Manifest,
    pub new_packs: Vec<PathBuf>,
    pub reused_bytes: u64,
}

struct PackWriter {
    file: File,
    path: PathBuf,
    name: String,
    size: u64,
    hasher: Sha256,
}

fn compress(path: &Path, level: u32) -> Result<(Vec<u8>, Encoding, String, u64)> {
    let mut input = File::open(path).map_err(|e| Error::io_path("open", path, e))?;
    let mut raw = Vec::new();
    input.read_to_end(&mut raw).map_err(|e| Error::io_path("read", path, e))?;
    let sha = sha256_hex(&raw);
    let size = raw.len() as u64;
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::new(level));
    enc.write_all(&raw).map_err(|e| Error::io("compress", e))?;
    let deflated = enc.finish().map_err(|e| Error::io("compress", e))?;
    if (deflated.len() as u64) < size - size / 50 { Ok((deflated, Encoding::Deflate, sha, size)) } else { Ok((raw, Encoding::Store, sha, size)) }
}

pub fn build_release(
    files: &[PayloadFile],
    info: &ReleaseInfo,
    asset_base: &str,
    out_dir: &Path,
    reuse: &ReuseIndex,
    pack_limit: u64,
    level: u32,
) -> Result<Built> {
    Version::parse(&info.version)?;
    Version::parse(&info.min_patcher)?;
    if pack_limit == 0 || pack_limit > meta::MAX_PACK_BYTES {
        return Err(Error::Install("pack limit must be between 1 byte and 2 GiB".into()));
    }
    fs::create_dir_all(out_dir).map_err(|e| Error::io_path("create", out_dir, e))?;
    let prefix = format!("svx-{}-{}", info.version, info.platform.id());
    let mut packs: Vec<Pack> = Vec::new();
    let mut pack_index: HashMap<String, usize> = HashMap::new();
    let mut written: HashMap<String, (usize, u64, u64, Encoding)> = HashMap::new();
    let mut entries = Vec::new();
    let mut current: Option<PackWriter> = None;
    let mut finished: Vec<PackWriter> = Vec::new();
    let mut new_packs = Vec::new();
    let mut reused_bytes = 0u64;
    let mut n = 0usize;
    for f in files {
        let (sha, size) = sha256_file(&f.abs, None)?;
        if let Some(r) = reuse.blobs.get(&sha) {
            let idx = *pack_index.entry(r.pack.url.clone()).or_insert_with(|| {
                packs.push(r.pack.clone());
                packs.len() - 1
            });
            reused_bytes += r.length;
            entries.push(FileEntry {
                path: f.rel.clone(),
                size,
                sha256: sha,
                exec: f.exec,
                pack: idx,
                offset: r.offset,
                length: r.length,
                encoding: r.encoding,
            });
            continue;
        }
        if let Some((idx, offset, length, encoding)) = written.get(&sha).cloned() {
            entries.push(FileEntry { path: f.rel.clone(), size, sha256: sha, exec: f.exec, pack: idx, offset, length, encoding });
            continue;
        }
        let (blob, encoding, sha2, size2) = compress(&f.abs, level)?;
        if sha2 != sha || size2 != size {
            return Err(Error::Install(format!("{} changed while packing", f.rel)));
        }
        let blen = blob.len() as u64;
        if blen > pack_limit {
            return Err(Error::Install(format!("{} is larger than the pack limit", f.rel)));
        }
        if current.as_ref().is_some_and(|c| c.size + blen > pack_limit) {
            finished.push(current.take().expect("pack"));
        }
        if current.is_none() {
            n += 1;
            let name = format!("{prefix}-{n}.pack");
            let path = out_dir.join(&name);
            let file = File::create(&path).map_err(|e| Error::io_path("create", &path, e))?;
            packs.push(Pack { name: name.clone(), url: format!("{asset_base}{name}"), size: 0, sha256: String::new() });
            pack_index.insert(format!("{asset_base}{name}"), packs.len() - 1);
            current = Some(PackWriter { file, path, name, size: 0, hasher: Sha256::new() });
        }
        let w = current.as_mut().expect("pack");
        let idx = pack_index[&format!("{asset_base}{}", w.name)];
        let offset = w.size;
        w.file.write_all(&blob).map_err(|e| Error::io_path("write", &w.path, e))?;
        w.hasher.update(&blob);
        w.size += blen;
        written.insert(sha.clone(), (idx, offset, blen, encoding));
        entries.push(FileEntry { path: f.rel.clone(), size, sha256: sha, exec: f.exec, pack: idx, offset, length: blen, encoding });
    }
    if let Some(c) = current.take() {
        finished.push(c);
    }
    for w in finished {
        let mut file = w.file;
        file.flush().map_err(|e| Error::io_path("write", &w.path, e))?;
        file.sync_all().map_err(|e| Error::io_path("sync", &w.path, e))?;
        let idx = pack_index[&format!("{asset_base}{}", w.name)];
        packs[idx].size = w.size;
        packs[idx].sha256 = hex::encode(w.hasher.finalize());
        new_packs.push(w.path);
    }
    let manifest = Manifest {
        kind: "release".into(),
        spec: SPEC,
        version: info.version.clone(),
        build: info.build,
        commit: info.commit.clone(),
        platform: info.platform.id().into(),
        min_patcher: info.min_patcher.clone(),
        date: info.date.clone(),
        launch: Launch { exe: info.platform.exe_name().into() },
        files: entries,
        packs,
    };
    manifest.validate(info.platform)?;
    Ok(Built { manifest, new_packs, reused_bytes })
}

pub fn sign_json<T: Serialize>(value: &T, keys: &[&SigningKey]) -> Result<Vec<u8>> {
    let v = serde_json::to_value(value).map_err(|e| Error::Metadata(e.to_string()))?;
    Ok(envelope_bytes(&sign::sign_value(&v, keys)?))
}

pub fn release_ref(m: &Manifest, signed: &[u8], manifest_url: &str, notes: &str) -> ReleaseRef {
    let mut seen = std::collections::HashSet::new();
    let download_size = m.files.iter().filter(|f| seen.insert((f.pack, f.offset))).map(|f| f.length).sum();
    ReleaseRef {
        version: m.version.clone(),
        build: m.build,
        date: m.date.clone(),
        manifest: Artifact { url: manifest_url.into(), sha256: sha256_hex(signed), size: signed.len() as u64 },
        download_size,
        install_size: m.install_size(),
        min_patcher: m.min_patcher.clone(),
        notes: notes.into(),
        withdrawn: false,
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub channel: String,
    pub platforms: BTreeMap<String, PlatformIndex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patcher: Option<PatcherInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changelog: Vec<changelog::Entry>,
}

impl Catalog {
    pub fn add(&mut self, platform: Platform, r: ReleaseRef, make_latest: bool) -> Result<()> {
        let v = Version::parse(&r.version)?;
        let p = self.platforms.entry(platform.id().into()).or_insert_with(|| PlatformIndex { latest: r.version.clone(), releases: Vec::new() });
        p.releases.retain(|x| x.version != r.version);
        let newest = p.releases.iter().filter(|x| !x.withdrawn).filter_map(|x| Version::parse(&x.version).ok()).max();
        if make_latest || newest.is_none_or(|n| v > n) {
            p.latest = r.version.clone();
        }
        p.releases.push(r);
        p.releases.sort_by(|a, b| Version::parse(&a.version).ok().cmp(&Version::parse(&b.version).ok()));
        Ok(())
    }

    pub fn withdraw(&mut self, platform: Platform, version: &str) -> Result<()> {
        let p = self.platforms.get_mut(platform.id()).ok_or_else(|| Error::Install("platform not in catalog".into()))?;
        let r = p.releases.iter_mut().find(|r| r.version == version).ok_or_else(|| Error::Install("version not in catalog".into()))?;
        r.withdrawn = true;
        if p.latest == version {
            p.latest = p
                .releases
                .iter()
                .filter(|r| !r.withdrawn)
                .max_by_key(|r| Version::parse(&r.version).ok())
                .map(|r| r.version.clone())
                .ok_or_else(|| Error::Install("cannot withdraw the only release".into()))?;
        }
        Ok(())
    }

    pub fn to_index(&self, version: u64) -> Index {
        Index {
            kind: "index".into(),
            spec: SPEC,
            version,
            channel: self.channel.clone(),
            platforms: self.platforms.clone(),
            patcher: self.patcher.clone(),
            changelog: self.changelog.clone(),
        }
    }
}

pub fn next_index_version(prev: Option<u64>) -> u64 {
    let now = now_unix().max(0) as u64;
    match prev {
        Some(p) if p >= now => p + 1,
        _ => now,
    }
}
