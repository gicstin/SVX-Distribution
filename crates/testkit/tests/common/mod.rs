#![allow(dead_code)]

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use svxdist::install::Install;
use svxdist::meta::{Index, Manifest};
use svxdist::net::UrlPolicy;
use svxdist::platform::Platform;
use svxdist::release::{self, Catalog, ReleaseInfo, ReuseIndex};
use svxdist::session::{Config, Session};
use svxdist::testkeys;
use svxdist::txn::{Options, Plan, Progress};
use svxdist::util::is_link;
use svxdist::version::Version;
use testkit::Server;
use url::Url;

pub const P: Platform = Platform::LinuxX86_64;
pub const LIB: &str = "libsvx.linux.template_release.x86_64.so";
static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct Fixture {
    pub dir: PathBuf,
    pub site: PathBuf,
    pub server: Server,
    pub catalog: Catalog,
    pub manifests: BTreeMap<String, (Manifest, Vec<u8>, Url)>,
    pub payloads: BTreeMap<String, PathBuf>,
    pub index_version: u64,
}

pub fn fresh_dir(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e-{}-{name}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn noise(seed: &str, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut block: [u8; 32] = Sha256::digest(seed.as_bytes()).into();
    while out.len() < len {
        out.extend_from_slice(&block);
        block = Sha256::digest(block).into();
    }
    out.truncate(len);
    out
}

fn text(seed: &str, len: usize) -> Vec<u8> {
    let line = format!("{seed} lorem ipsum dolor sit amet consectetur\n");
    line.repeat(len / line.len() + 1).into_bytes()[..len].to_vec()
}

fn write(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, data).unwrap();
}

pub fn exe_bytes(tag: &str) -> Vec<u8> {
    let mut b = fs::read(env!("CARGO_BIN_EXE_svx-fixture-game")).unwrap();
    b.extend_from_slice(format!("\nSVX-FIXTURE-{tag}\n").as_bytes());
    b
}

fn make_payload(root: &Path, version: &str) {
    match version {
        "1.0.0" => {
            write(root, "SVX.x86_64", &exe_bytes("1.0.0"));
            write(root, LIB, &noise("lib-1", 200_000));
            write(root, "README.txt", b"SVX fixture readme\n");
            write(root, "Core/a.bin", &noise("a-1", 300_000));
            write(root, "Core/b.json", &text("b", 50_000));
            write(root, "Core/sub/c.dds", &text("c-1", 1_200_000));
            write(root, "Core/old-only.txt", b"only in 1.0.0\n");
            write(root, "Core/dup1.txt", &text("dup", 10_000));
            write(root, "Core/dup2.txt", &text("dup", 10_000));
        }
        "1.1.0" => {
            make_payload(root, "1.0.0");
            write(root, "SVX.x86_64", &exe_bytes("1.1.0"));
            write(root, "Core/a.bin", &noise("a-1.1", 300_000));
            fs::remove_file(root.join("Core/old-only.txt")).unwrap();
            write(root, "Core/new.txt", b"added in 1.1.0\n");
        }
        "2.0.0" | "3.0.0" => {
            make_payload(root, "1.1.0");
            write(root, "SVX.x86_64", &exe_bytes(version));
            write(root, LIB, &noise("lib-2", 210_000));
            write(root, "Core/sub/c.dds", &text("c-2", 1_100_000));
            write(root, "Core/sub2/d.bin", &noise("d-2", 64_000));
        }
        _ => panic!("unknown fixture version"),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("SVX.x86_64"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(root, "Content/should-not-ship.txt", b"content is never part of a release\n");
    write(root, "release.json", format!("{{\"version\":\"{version}\"}}").as_bytes());
}

impl Fixture {
    pub fn new(name: &str) -> Fixture {
        let dir = fresh_dir(name);
        let site = dir.join("site");
        fs::create_dir_all(&site).unwrap();
        let server = Server::start(&site, 0);
        let mut f = Fixture {
            dir,
            site,
            server,
            catalog: Catalog { channel: "stable".into(), ..Default::default() },
            manifests: BTreeMap::new(),
            payloads: BTreeMap::new(),
            index_version: 1000,
        };
        for (v, build, reuse, min) in
            [("1.0.0", 10, None, "0.1.0"), ("1.1.0", 12, Some("1.0.0"), "0.1.0"), ("2.0.0", 20, None, "0.1.0"), ("3.0.0", 30, None, "9.0.0")]
        {
            f.add_release(v, build, reuse, min);
        }
        f
    }

    pub fn add_release(&mut self, version: &str, build: u64, reuse_from: Option<&str>, min_patcher: &str) {
        let payload = self.dir.join("payloads").join(version);
        make_payload(&payload, version);
        let (files, skipped) = release::collect_payload(&payload, P).unwrap();
        assert!(skipped.contains(&"Content".to_string()));
        let mut reuse = ReuseIndex::default();
        if let Some(prev) = reuse_from {
            let (m, _, url) = &self.manifests[prev];
            reuse.add(m, url).unwrap();
        }
        let tag = format!("svx-{version}");
        let assets = self.site.join("releases").join(&tag);
        let info = ReleaseInfo {
            version: version.into(),
            build,
            commit: format!("c{build}"),
            platform: P,
            min_patcher: min_patcher.into(),
            date: "2026-10-01T00:00:00Z".into(),
        };
        let built = release::build_release(&files, &info, "", &assets, &reuse, 4_000_000, 6).unwrap();
        let rel = testkeys::key("release");
        let signed = release::sign_json(&built.manifest, &[&rel]).unwrap();
        let mname = format!("manifest-{}.json", P.id());
        fs::write(assets.join(&mname), &signed).unwrap();
        let murl = format!("releases/{tag}/{mname}");
        let entry = release::release_ref(&built.manifest, &signed, &murl, "");
        let latest = self.catalog.platforms.get(P.id()).map(|p| p.latest.clone());
        self.catalog.add(P, entry, false).unwrap();
        if let Some(l) = latest {
            self.catalog.platforms.get_mut(P.id()).unwrap().latest = l;
        }
        let url = Url::parse(&self.server.url(&murl)).unwrap();
        self.manifests.insert(version.into(), (built.manifest, signed, url));
        self.payloads.insert(version.into(), payload);
    }

    pub fn index_url(&self) -> Url {
        Url::parse(&self.server.url("index.json")).unwrap()
    }

    pub fn publish(&mut self, latest: &str) {
        self.catalog.platforms.get_mut(P.id()).unwrap().latest = latest.into();
        self.index_version += 1;
        let idx = self.catalog.to_index(self.index_version);
        self.publish_index(&idx, &[&testkeys::key("release")]);
    }

    pub fn publish_index(&self, idx: &Index, keys: &[&ed25519_dalek::SigningKey]) {
        let bytes = release::sign_json(idx, keys).unwrap();
        fs::write(self.site.join("index.json"), bytes).unwrap();
    }

    pub fn config(&self, options: Options) -> Config {
        Config {
            platform: P,
            patcher_version: Version(0, 1, 0),
            index_url: self.index_url(),
            embedded_root: testkeys::root_v1(),
            trust_state: Some(self.dir.join("trust-state.json")),
            policy: UrlPolicy::new(&[], true),
            options,
        }
    }

    pub fn session(&self) -> Session {
        self.session_with(Options::default())
    }

    pub fn session_with(&self, options: Options) -> Session {
        Session::new(self.config(options), Arc::new(AtomicBool::new(false))).unwrap()
    }

    pub fn install_dir(&self, name: &str) -> PathBuf {
        self.dir.join("installs").join(name)
    }

    pub fn pack_paths(&self, version: &str) -> Vec<String> {
        let (m, _, _) = &self.manifests[version];
        m.packs.iter().filter(|p| !p.url.contains("://")).map(|p| format!("releases/svx-{version}/{}", p.name)).collect()
    }
}

pub fn quiet() -> impl FnMut(&Progress) {
    |_p: &Progress| {}
}

pub fn install_version(f: &Fixture, s: &mut Session, dir: &Path, version: Option<&str>) -> svxdist::Result<Plan> {
    let install = Install::new(dir, P);
    let _lock = install.lock()?;
    let target = s.target(version)?;
    let kind = svxdist::session::kind_for(&install, &target.release.version);
    let mut q = quiet();
    let plan = s.plan_target(&install, &target, kind, &mut q)?;
    s.execute(&install, &plan, &mut q)?;
    let _ = f;
    Ok(plan)
}

pub fn assert_matches_payload(f: &Fixture, dir: &Path, version: &str) {
    let payload = &f.payloads[version];
    let (files, _) = release::collect_payload(payload, P).unwrap();
    for pf in &files {
        let got = fs::read(dir.join(&pf.rel)).unwrap_or_else(|_| panic!("{} missing for {version}", pf.rel));
        assert!(got == fs::read(&pf.abs).unwrap(), "{} differs from {version}", pf.rel);
    }
    let install = Install::new(dir, P);
    let known: Vec<String> = files.iter().map(|f| f.rel.clone()).collect();
    let extra = install.unknown_managed_files(&|p| known.iter().any(|k| k == p)).unwrap();
    assert!(extra.is_empty(), "unexpected managed files for {version}: {extra:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir.join("SVX.x86_64")).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "executable bit missing");
    }
    let record = install.read_record().unwrap().expect("installed record");
    assert_eq!(record.version, version);
}

pub fn seed_protected(dir: &Path) {
    write(dir, "Content/SVX/maren/1/figure.svxfig", &noise("content-1", 150_000));
    write(dir, "Content/Core/looks-like-core.txt", b"inside Content, not managed\n");
    write(dir, "Content/.index/built.json", b"{}\n");
    write(dir, "User/settings.json", b"{\"vr\":true}\n");
    write(dir, "User/poses/p1.json", b"{}\n");
    write(dir, "Cache/textures/x.bin", &noise("cache", 40_000));
    write(dir, "logs/latest.log", b"log line\n");
    write(dir, "logs/svx_2026_crashed.log", b"crash\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", dir.join("Content/link-outside")).unwrap();
}

pub type Snap = BTreeMap<String, String>;

pub fn snapshot_protected(dir: &Path) -> Snap {
    let mut out = BTreeMap::new();
    for top in ["Content", "User", "Cache", "logs"] {
        snap_walk(&dir.join(top), top, &mut out);
    }
    out
}

fn snap_walk(p: &Path, rel: &str, out: &mut Snap) {
    let Ok(meta) = fs::symlink_metadata(p) else { return };
    let mtime = meta.modified().ok().map(|t| format!("{t:?}")).unwrap_or_default();
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode()
    };
    #[cfg(not(unix))]
    let mode = 0;
    if is_link(&meta) {
        out.insert(rel.into(), format!("link {:?} {mode:o}", fs::read_link(p).ok()));
    } else if meta.is_dir() {
        out.insert(rel.into(), format!("dir {mode:o} {mtime}"));
        let mut names: Vec<_> = fs::read_dir(p).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        names.sort();
        for n in names {
            snap_walk(&p.join(&n), &format!("{rel}/{n}"), out);
        }
    } else {
        let data = fs::read(p).unwrap();
        out.insert(rel.into(), format!("file {} {} {mode:o} {mtime}", data.len(), hex::encode(Sha256::digest(&data))));
    }
}
