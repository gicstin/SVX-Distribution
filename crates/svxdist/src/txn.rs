use crate::error::{Error, Result};
use crate::install::{FileState, HashCache, Install, InstalledRecord, RECORD_SCHEMA};
use crate::meta::{Encoding, Manifest};
use crate::net::{Fetcher, UrlPolicy};
use crate::paths::{self, CORE_DIR};
use crate::platform::Platform;
use crate::procs;
use crate::time::{format_utc, now_unix};
use crate::trust::parse_stored_manifest;
use crate::util::{self, check_cancel, is_link, remove_dir_all_if_exists, rename, set_executable, sha256_file, sha256_hex, sync_dir, write_atomic};
use flate2::read::DeflateDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use url::Url;

pub const JOURNAL_SCHEMA: u32 = 1;
const COALESCE_GAP: u64 = 256 * 1024;
const MAX_RANGE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Install,
    Update,
    Switch,
    Repair,
    Adopt,
    Rollback,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Install => "install",
            Kind::Update => "update",
            Kind::Switch => "switch",
            Kind::Repair => "repair",
            Kind::Adopt => "adopt",
            Kind::Rollback => "rollback",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxnState {
    Staging,
    Ready,
    Activating,
    Committed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Op {
    pub path: String,
    pub new: Option<FileState>,
    pub old: Option<FileState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Journal {
    pub schema: u32,
    pub id: String,
    pub kind: Kind,
    pub state: TxnState,
    pub key: String,
    pub prev: Option<InstalledRecord>,
    pub next: Option<InstalledRecord>,
    pub ops: Vec<Op>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: u32,
    pub kind: Kind,
    pub created: String,
    pub from: Option<InstalledRecord>,
    pub to_version: Option<String>,
    pub ops: Vec<Op>,
}

impl Snapshot {
    pub fn restorable(&self) -> bool {
        self.from.is_some() || (self.kind == Kind::Adopt && self.ops.iter().any(|o| o.old.is_some()))
    }

    pub fn label(&self) -> String {
        match &self.from {
            Some(r) => format!("{} (build {})", r.version, r.build),
            None => "the files found before the patcher took over".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RemoteBlob {
    pub url: Url,
    pub offset: u64,
    pub length: u64,
    pub encoding: Encoding,
}

#[derive(Clone, Debug, Default)]
pub struct Source {
    pub local: Vec<PathBuf>,
    pub remote: Option<RemoteBlob>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub kind: Kind,
    pub key: String,
    pub prev: Option<InstalledRecord>,
    pub next: Option<InstalledRecord>,
    pub ops: Vec<Op>,
    pub sources: BTreeMap<String, Source>,
}

impl Plan {
    pub fn puts(&self) -> usize {
        self.ops.iter().filter(|o| o.new.is_some()).count()
    }

    pub fn removes(&self) -> usize {
        self.ops.iter().filter(|o| o.new.is_none()).count()
    }

    pub fn download_estimate(&self) -> u64 {
        self.sources.values().filter(|s| s.local.is_empty()).filter_map(|s| s.remote.as_ref()).map(|r| r.length).sum()
    }

    pub fn is_noop(&self) -> bool {
        self.ops.is_empty() && self.prev.as_ref().map(|r| (&r.version, &r.manifest)) == self.next.as_ref().map(|r| (&r.version, &r.manifest))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub fail_after_ops: Option<usize>,
    pub abort_after_ops: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Preparing,
    Scanning,
    Downloading,
    Verifying,
    Installing,
    Finishing,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Preparing => "Preparing",
            Phase::Scanning => "Checking installed files",
            Phase::Downloading => "Downloading",
            Phase::Verifying => "Verifying",
            Phase::Installing => "Installing",
            Phase::Finishing => "Finishing",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
    pub detail: String,
}

pub type ProgressFn<'a> = &'a mut dyn FnMut(&Progress);

fn report(progress: &mut dyn FnMut(&Progress), phase: Phase, done: u64, total: u64, detail: &str) {
    progress(&Progress { phase, done, total, detail: detail.to_string() });
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    Clean,
    Staged { kind: Kind, version: Option<String> },
    RolledForward { kind: Kind, version: Option<String> },
    RolledBack { kind: Kind },
}

fn journal_path(install: &Install) -> PathBuf {
    install.txn_dir().join("journal.json")
}

pub fn read_journal(install: &Install) -> Result<Option<Journal>> {
    let p = journal_path(install);
    match fs::read(&p) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| Error::Metadata(format!("transaction journal: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io_path("read", &p, e)),
    }
}

fn write_journal(install: &Install, j: &Journal) -> Result<()> {
    write_atomic(&journal_path(install), &serde_json::to_vec_pretty(j).expect("serialize journal"))
}

pub fn read_snapshot(install: &Install) -> Option<Snapshot> {
    fs::read(install.rollback_dir().join("snapshot.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn matches(cur: &Option<FileState>, want: &FileState, platform: Platform) -> bool {
    cur.as_ref().is_some_and(|c| c.sha256 == want.sha256 && c.size == want.size && (platform == Platform::WindowsX86_64 || !want.exec || c.exec))
}

fn same_path(a: &str, b: &str, platform: Platform) -> bool {
    if platform == Platform::WindowsX86_64 { paths::fold(a) == paths::fold(b) } else { a == b }
}

fn sort_ops(ops: &mut [Op]) {
    ops.sort_by(|a, b| match (a.new.is_none(), b.new.is_none()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => b.path.cmp(&a.path),
        (false, false) => a.path.cmp(&b.path),
    });
}

fn rollback_sources(install: &Install) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    if let Some(s) = read_snapshot(install) {
        let files = install.rollback_dir().join("files");
        for op in &s.ops {
            if let Some(old) = &op.old
                && !old.sha256.is_empty()
            {
                out.insert(old.sha256.clone(), paths::join(&files, &op.path));
            }
        }
    }
    out
}

pub struct RemoteTarget<'a> {
    pub kind: Kind,
    pub manifest: &'a Manifest,
    pub raw: &'a [u8],
    pub url: &'a Url,
}

pub fn plan_remote(install: &Install, target: RemoteTarget, policy: &UrlPolicy, full_hash: bool, cancel: &AtomicBool, progress: ProgressFn) -> Result<Plan> {
    let m = target.manifest;
    m.validate(install.platform)?;
    let prev = install.read_record().ok().flatten();
    let prev_manifest = prev.as_ref().and_then(|r| install.record_manifest(r).ok());
    let mut cache = install.hash_cache();
    let mut ops = Vec::new();
    let mut sources = BTreeMap::new();
    let mut by_sha: HashMap<String, PathBuf> = HashMap::new();
    let total = m.files.len() as u64;
    for (i, f) in m.files.iter().enumerate() {
        report(progress, Phase::Scanning, i as u64, total, &f.path);
        let cur = install.current(&f.path, !full_hash, &mut cache, cancel)?;
        if let Some(c) = &cur
            && !c.sha256.is_empty()
        {
            by_sha.insert(c.sha256.clone(), install.path(&f.path));
        }
        let want = FileState { sha256: f.sha256.clone(), size: f.size, exec: f.exec };
        if !matches(&cur, &want, install.platform) {
            let pack = &m.packs[f.pack];
            let url = policy.resolve(target.url, &pack.url)?;
            sources.insert(
                f.path.clone(),
                Source { local: Vec::new(), remote: Some(RemoteBlob { url, offset: f.offset, length: f.length, encoding: f.encoding }) },
            );
            ops.push(Op { path: f.path.clone(), new: Some(want), old: cur });
        }
    }
    let in_target = |p: &str| m.files.iter().any(|f| same_path(&f.path, p, install.platform));
    let mut extra: BTreeSet<String> = BTreeSet::new();
    if let Some(pm) = &prev_manifest {
        for f in &pm.files {
            if !in_target(&f.path) {
                extra.insert(f.path.clone());
            }
        }
    }
    if matches!(target.kind, Kind::Repair | Kind::Adopt) || prev_manifest.is_none() {
        for p in install.unknown_managed_files(&in_target)? {
            extra.insert(p);
        }
    }
    for p in extra {
        if paths::check_managed(&p, install.platform).is_err() {
            continue;
        }
        if let Some(cur) = install.current(&p, true, &mut cache, cancel)? {
            ops.push(Op { path: p, new: None, old: Some(cur) });
        }
    }
    install.save_cache(&mut cache);
    let rb = rollback_sources(install);
    for op in &ops {
        if let Some(new) = &op.new {
            let src = sources.get_mut(&op.path).expect("source");
            if let Some(p) = by_sha.get(&new.sha256) {
                src.local.push(p.clone());
            }
            if let Some(p) = rb.get(&new.sha256) {
                src.local.push(p.clone());
            }
        }
    }
    sort_ops(&mut ops);
    let next = InstalledRecord {
        schema: RECORD_SCHEMA,
        version: m.version.clone(),
        build: m.build,
        platform: m.platform.clone(),
        manifest: String::from_utf8(target.raw.to_vec()).map_err(|_| Error::Metadata("manifest is not UTF-8".into()))?,
        manifest_url: target.url.to_string(),
        installed_at: format_utc(now_unix()),
        origin: target.kind.label().into(),
    };
    Ok(Plan { kind: target.kind, key: format!("{}:{}", target.kind.label(), sha256_hex(target.raw)), prev, next: Some(next), ops, sources })
}

pub fn plan_rollback(install: &Install, policy: &UrlPolicy, cancel: &AtomicBool, progress: ProgressFn) -> Result<Plan> {
    let snap = read_snapshot(install).filter(|s| s.restorable()).ok_or_else(|| Error::Install("No previous version is available to restore.".into()))?;
    let prev = install.read_record().ok().flatten();
    let from_manifest =
        snap.from.as_ref().and_then(|r| parse_stored_manifest(r.manifest.as_bytes(), install.platform).ok().map(|m| (m, r.manifest_url.clone())));
    let mut cache = install.hash_cache();
    let files = install.rollback_dir().join("files");
    let mut ops = Vec::new();
    let mut sources = BTreeMap::new();
    let total = snap.ops.len() as u64;
    for (i, sop) in snap.ops.iter().enumerate() {
        report(progress, Phase::Scanning, i as u64, total, &sop.path);
        paths::check_managed(&sop.path, install.platform)?;
        let cur = install.current(&sop.path, true, &mut cache, cancel)?;
        match &sop.old {
            Some(old) if !old.sha256.is_empty() => {
                if matches(&cur, old, install.platform) {
                    continue;
                }
                let mut src = Source { local: vec![paths::join(&files, &sop.path)], remote: None };
                if let Some((fm, murl)) = &from_manifest
                    && let Some(f) = fm.file(&sop.path).filter(|f| f.sha256 == old.sha256)
                {
                    let base = Url::parse(murl).map_err(|e| Error::UnsafeUrl(e.to_string()))?;
                    let url = policy.resolve(&base, &fm.packs[f.pack].url)?;
                    src.remote = Some(RemoteBlob { url, offset: f.offset, length: f.length, encoding: f.encoding });
                }
                sources.insert(sop.path.clone(), src);
                ops.push(Op { path: sop.path.clone(), new: Some(old.clone()), old: cur });
            }
            Some(_) => {}
            None => {
                if cur.is_some() {
                    ops.push(Op { path: sop.path.clone(), new: None, old: cur });
                }
            }
        }
    }
    install.save_cache(&mut cache);
    sort_ops(&mut ops);
    let next = snap.from.clone().map(|mut r| {
        r.installed_at = format_utc(now_unix());
        r.origin = Kind::Rollback.label().into();
        r
    });
    let snap_bytes = serde_json::to_vec(&snap).expect("serialize snapshot");
    Ok(Plan { kind: Kind::Rollback, key: format!("rollback:{}", sha256_hex(&snap_bytes)), prev, next, ops, sources })
}

fn copy_verified(src: &Path, dest: &Path, want: &FileState, cancel: &AtomicBool) -> Result<bool> {
    let Ok(meta) = fs::symlink_metadata(src) else { return Ok(false) };
    if is_link(&meta) || !meta.is_file() || meta.len() != want.size {
        return Ok(false);
    }
    let Ok(mut input) = File::open(src) else { return Ok(false) };
    let ok = write_verified(&mut input, dest, want, cancel)?;
    Ok(ok)
}

fn write_verified(input: &mut dyn Read, dest: &Path, want: &FileState, cancel: &AtomicBool) -> Result<bool> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| Error::io_path("create", parent, e))?;
    }
    let tmp = util::tmp_sibling(dest);
    let file = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp).map_err(|e| Error::io_path("create", &tmp, e))?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    let mut limited = input.take(want.size + 1);
    loop {
        check_cancel(cancel)?;
        let n = match limited.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                if e.kind() == io::ErrorKind::InvalidData || e.kind() == io::ErrorKind::InvalidInput {
                    return Ok(false);
                }
                return Err(Error::io_path("read data for", dest, e));
            }
        };
        total += n as u64;
        if total > want.size {
            break;
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| Error::io_path("write", &tmp, e))?;
    }
    let file = out.into_inner().map_err(|e| Error::io_path("write", &tmp, e.into_error()))?;
    file.sync_all().map_err(|e| Error::io_path("sync", &tmp, e))?;
    drop(file);
    if total != want.size || hex::encode(hasher.finalize()) != want.sha256 {
        let _ = fs::remove_file(&tmp);
        return Ok(false);
    }
    rename(&tmp, dest)?;
    set_executable(dest, want.exec)?;
    Ok(true)
}

struct Range {
    url: Url,
    offset: u64,
    len: u64,
    blobs: Vec<(String, FileState, RemoteBlob)>,
}

fn coalesce(mut blobs: Vec<(String, FileState, RemoteBlob)>) -> Vec<Range> {
    blobs.sort_by(|a, b| (a.2.url.as_str(), a.2.offset).cmp(&(b.2.url.as_str(), b.2.offset)));
    let mut out: Vec<Range> = Vec::new();
    for b in blobs {
        if let Some(last) = out.last_mut() {
            let end = last.offset + last.len;
            if last.url == b.2.url && b.2.offset >= end && b.2.offset - end <= COALESCE_GAP && b.2.offset + b.2.length - last.offset <= MAX_RANGE {
                last.len = b.2.offset + b.2.length - last.offset;
                last.blobs.push(b);
                continue;
            }
            if last.url == b.2.url && b.2.offset < end {
                let new_end = (b.2.offset + b.2.length).max(end);
                last.len = new_end - last.offset;
                last.blobs.push(b);
                continue;
            }
        }
        out.push(Range { url: b.2.url.clone(), offset: b.2.offset, len: b.2.length, blobs: vec![b] });
    }
    out
}

fn part_path(install: &Install, r: &Range) -> PathBuf {
    let key = sha256_hex(format!("{}|{}|{}", r.url, r.offset, r.len).as_bytes());
    install.downloads_dir().join(format!("{}.part", &key[..32]))
}

fn extract(part: &Path, range_offset: u64, blob: &RemoteBlob, dest: &Path, want: &FileState, cancel: &AtomicBool) -> Result<bool> {
    let mut f = File::open(part).map_err(|e| Error::io_path("open", part, e))?;
    f.seek(SeekFrom::Start(blob.offset - range_offset)).map_err(|e| Error::io_path("seek", part, e))?;
    let raw = f.take(blob.length);
    match blob.encoding {
        Encoding::Store => {
            let mut r = raw;
            write_verified(&mut r, dest, want, cancel)
        }
        Encoding::Deflate => {
            let mut r = DeflateDecoder::new(raw);
            write_verified(&mut r, dest, want, cancel)
        }
    }
}

pub fn stage(install: &Install, plan: &Plan, fetcher: Option<&Fetcher>, cancel: &AtomicBool, progress: ProgressFn) -> Result<()> {
    let txn = install.txn_dir();
    let mut reuse = false;
    match read_journal(install) {
        Ok(Some(j)) => match j.state {
            TxnState::Staging | TxnState::Ready => reuse = j.key == plan.key,
            TxnState::Activating | TxnState::Committed => {
                return Err(Error::Install("An earlier update did not finish. Restart the patcher to complete it.".into()));
            }
        },
        Ok(None) => {}
        Err(_) => {}
    }
    if !reuse {
        remove_dir_all_if_exists(&txn)?;
    }
    let new_dir = txn.join("new");
    fs::create_dir_all(&new_dir).map_err(|e| Error::io_path("create", &new_dir, e))?;
    fs::create_dir_all(txn.join("old")).map_err(|e| Error::io_path("create", &txn, e))?;
    let mut journal = Journal {
        schema: JOURNAL_SCHEMA,
        id: util::random_id(),
        kind: plan.kind,
        state: TxnState::Staging,
        key: plan.key.clone(),
        prev: plan.prev.clone(),
        next: plan.next.clone(),
        ops: plan.ops.clone(),
    };
    write_journal(install, &journal)?;
    let puts: Vec<&Op> = plan.ops.iter().filter(|o| o.new.is_some()).collect();
    let mut remote = Vec::new();
    let total = puts.len() as u64;
    for (i, op) in puts.iter().enumerate() {
        let want = op.new.as_ref().expect("put");
        report(progress, Phase::Verifying, i as u64, total, &op.path);
        let dest = paths::join(&new_dir, &op.path);
        if dest.exists() {
            match sha256_file(&dest, Some(cancel)) {
                Ok((sha, size)) if sha == want.sha256 && size == want.size => {
                    set_executable(&dest, want.exec)?;
                    continue;
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                _ => remove_dir_all_if_exists(&dest)?,
            }
        }
        let src = plan.sources.get(&op.path).cloned().unwrap_or_default();
        let mut done = false;
        for local in &src.local {
            if copy_verified(local, &dest, want, cancel)? {
                done = true;
                break;
            }
        }
        if done {
            continue;
        }
        match src.remote {
            Some(r) => remote.push((op.path.clone(), want.clone(), r)),
            None => return Err(Error::Install(format!("{} is not available locally and has no download source.", op.path))),
        }
    }
    if !remote.is_empty() {
        let fetcher = fetcher.ok_or_else(|| Error::Network("downloads are needed but no network access is configured".into()))?;
        let ranges = coalesce(remote);
        let total: u64 = ranges.iter().map(|r| r.len).sum();
        let mut done = 0u64;
        report(progress, Phase::Downloading, 0, total, "");
        for r in &ranges {
            let part = part_path(install, r);
            let detail = r.blobs.first().map(|b| b.0.clone()).unwrap_or_default();
            let base = done;
            let mut got = 0u64;
            fetcher.fetch_range(&r.url, r.offset, r.len, &part, &mut |n| {
                got += n;
                progress(&Progress { phase: Phase::Downloading, done: base + got, total, detail: detail.clone() });
            })?;
            done = base + r.len;
            for (path, want, blob) in &r.blobs {
                report(progress, Phase::Verifying, done, total, path);
                let dest = paths::join(&new_dir, path);
                if !extract(&part, r.offset, blob, &dest, want, cancel)? {
                    let _ = fs::remove_file(&part);
                    return Err(Error::Hash(format!("{path} did not match its signed hash; the damaged download was discarded")));
                }
            }
            let _ = fs::remove_file(&part);
        }
    }
    journal.state = TxnState::Ready;
    write_journal(install, &journal)?;
    Ok(())
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

fn apply_forward(install: &Install, op: &Op) -> Result<()> {
    let txn = install.txn_dir();
    let target = install.path(&op.path);
    let staged = paths::join(&txn.join("new"), &op.path);
    let saved = paths::join(&txn.join("old"), &op.path);
    paths::ensure_real_parents(&install.root, &op.path)?;
    let move_aside = || -> Result<()> {
        if exists(&target) {
            if exists(&saved) {
                remove_dir_all_if_exists(&target)?;
            } else {
                if let Some(p) = saved.parent() {
                    fs::create_dir_all(p).map_err(|e| Error::io_path("create", p, e))?;
                }
                rename(&target, &saved)?;
            }
        }
        Ok(())
    };
    if op.new.is_some() {
        if exists(&staged) {
            move_aside()?;
            paths::create_parents(&install.root, &op.path)?;
            rename(&staged, &target)?;
        }
    } else {
        move_aside()?;
    }
    Ok(())
}

fn apply_backward(install: &Install, op: &Op) -> Result<()> {
    let txn = install.txn_dir();
    let target = install.path(&op.path);
    let staged = paths::join(&txn.join("new"), &op.path);
    let saved = paths::join(&txn.join("old"), &op.path);
    paths::ensure_real_parents(&install.root, &op.path)?;
    if op.new.is_some() && !exists(&staged) && exists(&target) {
        if let Some(p) = staged.parent() {
            fs::create_dir_all(p).map_err(|e| Error::io_path("create", p, e))?;
        }
        rename(&target, &staged)?;
    }
    if exists(&saved) {
        remove_dir_all_if_exists(&target)?;
        paths::create_parents(&install.root, &op.path)?;
        rename(&saved, &target)?;
    }
    Ok(())
}

fn sync_parents(install: &Install, ops: &[Op]) {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    dirs.insert(install.root.clone());
    for op in ops {
        if let Some(p) = install.path(&op.path).parent() {
            dirs.insert(p.to_path_buf());
        }
    }
    for d in dirs {
        sync_dir(&d);
    }
}

fn prune_empty(install: &Install, rel: &str) {
    let mut parts: Vec<&str> = rel.split('/').collect();
    parts.pop();
    while parts.len() > 1 && parts[0] == CORE_DIR {
        let dir = paths::join(&install.root, &parts.join("/"));
        if fs::remove_dir(&dir).is_err() {
            break;
        }
        parts.pop();
    }
}

fn forward_all(install: &Install, j: &Journal, opts: &Options, progress: ProgressFn) -> Result<()> {
    let total = j.ops.len() as u64;
    for (i, op) in j.ops.iter().enumerate() {
        report(progress, Phase::Installing, i as u64, total, &op.path);
        apply_forward(install, op)?;
        if opts.abort_after_ops == Some(i + 1) {
            std::process::abort();
        }
        if opts.fail_after_ops == Some(i + 1) {
            return Err(Error::Interrupted(format!("simulated crash after {} of {} file operations", i + 1, j.ops.len())));
        }
    }
    sync_parents(install, &j.ops);
    Ok(())
}

fn commit(install: &Install, j: &mut Journal) -> Result<()> {
    install.write_record(j.next.as_ref())?;
    let mut cache: HashCache = install.hash_cache();
    for op in &j.ops {
        install.forget_cached(&mut cache, &op.path);
        match &op.new {
            Some(state) => install.remember(&mut cache, &op.path, state),
            None => prune_empty(install, &op.path),
        }
    }
    install.save_cache(&mut cache);
    j.state = TxnState::Committed;
    write_journal(install, j)
}

fn finalize(install: &Install, j: &Journal) -> Result<()> {
    let txn = install.txn_dir();
    if j.kind != Kind::Repair {
        let old = txn.join("old");
        let rb = install.rollback_dir();
        if exists(&old) {
            remove_dir_all_if_exists(&rb)?;
            fs::create_dir_all(&rb).map_err(|e| Error::io_path("create", &rb, e))?;
            rename(&old, &rb.join("files"))?;
        }
        let snap = Snapshot {
            schema: JOURNAL_SCHEMA,
            kind: j.kind,
            created: format_utc(now_unix()),
            from: j.prev.clone(),
            to_version: j.next.as_ref().map(|r| r.version.clone()),
            ops: j.ops.clone(),
        };
        write_atomic(&rb.join("snapshot.json"), &serde_json::to_vec_pretty(&snap).expect("serialize snapshot"))?;
    }
    remove_dir_all_if_exists(&txn)
}

pub fn activate(install: &Install, opts: &Options, progress: ProgressFn) -> Result<()> {
    let mut j = read_journal(install)?.ok_or_else(|| Error::Install("Nothing is staged to install.".into()))?;
    if j.state != TxnState::Ready {
        return Err(Error::Install("The staged update is incomplete.".into()));
    }
    let running = procs::running_in(&install.root);
    if !running.is_empty() {
        return Err(Error::GameRunning(running));
    }
    j.state = TxnState::Activating;
    write_journal(install, &j)?;
    forward_all(install, &j, opts, progress)?;
    report(progress, Phase::Finishing, 0, 1, "");
    commit(install, &mut j)?;
    finalize(install, &j)
}

fn can_roll_forward(install: &Install, j: &Journal, cancel: &AtomicBool) -> Result<bool> {
    let new_dir = install.txn_dir().join("new");
    for op in &j.ops {
        let Some(want) = &op.new else { continue };
        let staged = paths::join(&new_dir, &op.path);
        let candidate = if exists(&staged) { staged } else { install.path(&op.path) };
        match sha256_file(&candidate, Some(cancel)) {
            Ok((sha, size)) if sha == want.sha256 && size == want.size => {}
            Ok(_) | Err(Error::Io { .. }) => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

pub fn recover(install: &Install, cancel: &AtomicBool, progress: ProgressFn) -> Result<Recovery> {
    let mut j = match read_journal(install)? {
        Some(j) => j,
        None => {
            if install.txn_dir().exists() {
                remove_dir_all_if_exists(&install.txn_dir())?;
            }
            return Ok(Recovery::Clean);
        }
    };
    let version = j.next.as_ref().map(|r| r.version.clone());
    for op in &j.ops {
        paths::check_managed(&op.path, install.platform)?;
    }
    match j.state {
        TxnState::Staging | TxnState::Ready => Ok(Recovery::Staged { kind: j.kind, version }),
        TxnState::Committed => {
            finalize(install, &j)?;
            Ok(Recovery::RolledForward { kind: j.kind, version })
        }
        TxnState::Activating => {
            let running = procs::running_in(&install.root);
            if !running.is_empty() {
                return Err(Error::GameRunning(running));
            }
            if can_roll_forward(install, &j, cancel)? {
                forward_all(install, &j, &Options::default(), progress)?;
                commit(install, &mut j)?;
                finalize(install, &j)?;
                Ok(Recovery::RolledForward { kind: j.kind, version })
            } else {
                for op in j.ops.iter().rev() {
                    apply_backward(install, op)?;
                }
                sync_parents(install, &j.ops);
                j.state = TxnState::Ready;
                write_journal(install, &j)?;
                Ok(Recovery::RolledBack { kind: j.kind })
            }
        }
    }
}

pub fn discard_staged(install: &Install) -> Result<()> {
    if let Some(j) = read_journal(install)?
        && matches!(j.state, TxnState::Activating | TxnState::Committed)
    {
        return Err(Error::Install("An update is half applied; it must be completed first.".into()));
    }
    remove_dir_all_if_exists(&install.txn_dir())
}

pub fn pending_names(install: &Install) -> HashSet<String> {
    read_journal(install).ok().flatten().map(|j| j.ops.into_iter().map(|o| o.path).collect()).unwrap_or_default()
}
