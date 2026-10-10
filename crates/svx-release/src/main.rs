use ed25519_dalek::SigningKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use svxdist::meta::{Artifact, Index, PatcherInfo, Root};
use svxdist::platform::Platform;
use svxdist::release::{self, Catalog, ReleaseInfo, ReuseIndex};
use svxdist::sign;
use svxdist::time::{format_utc, now_unix, parse_utc};
use svxdist::trust::{self, Trust};
use svxdist::util::{human_bytes, sha256_file, sha256_hex, write_atomic};
use svxdist::version::Version;
use svxdist::{Error, Result};
use url::Url;

const USAGE: &str = "svx-release <command> [options]

  keygen        --out DIR --name NAME
  root          --version N --expires YYYY-MM-DDTHH:MM:SSZ --root-pub FILE... [--root-threshold N]
                --release-pub FILE... [--release-threshold N] --sign KEY... --out FILE
  test-root     --out FILE
  test-keys     --out DIR
  check-keys    --root FILE [--roots DIR] --key FILE...
  release       --payload DIR --platform P --min-patcher V --key KEY... --out DIR
                (--github OWNER/REPO | --asset-base URL --manifest-url URL)
                [--version V] [--build N] [--commit C] [--date TS] [--tag TAG] [--reuse MANIFEST=URL]...
                [--pack-limit BYTES] [--level 0-9] [--notes TEXT]
  catalog-add   --catalog FILE --platform P --entry FILE [--latest]
  withdraw      --catalog FILE --platform P --version V
  patcher       --catalog FILE --version V --base-url URL [--linux FILE] [--windows FILE]
  index         --catalog FILE --key KEY... [--prev FILE] --out FILE
  index-dir     --catalog FILE --key KEY... --roots DIR --out DIR [--prev FILE]
  verify        --root FILE [--roots DIR] [--index FILE] [--manifest FILE --platform P]
  publish-plan  --release-dir DIR --repo OWNER/REPO --tag TAG --kind game|patcher|index --out FILE
";

struct Args {
    map: BTreeMap<String, Vec<String>>,
}

impl Args {
    fn parse(raw: &[String]) -> Result<Args> {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut i = 0;
        while i < raw.len() {
            let k = raw[i].strip_prefix("--").ok_or_else(|| usage(&format!("unexpected argument {}", raw[i])))?;
            if k == "latest" {
                map.entry(k.into()).or_default().push("true".into());
                i += 1;
                continue;
            }
            let v = raw.get(i + 1).ok_or_else(|| usage(&format!("--{k} needs a value")))?;
            map.entry(k.into()).or_default().push(v.clone());
            i += 2;
        }
        Ok(Args { map })
    }

    fn opt(&self, k: &str) -> Option<String> {
        self.map.get(k).and_then(|v| v.last().cloned())
    }

    fn req(&self, k: &str) -> Result<String> {
        self.opt(k).ok_or_else(|| usage(&format!("missing --{k}")))
    }

    fn all(&self, k: &str) -> Vec<String> {
        self.map.get(k).cloned().unwrap_or_default()
    }

    fn num(&self, k: &str, default: u64) -> Result<u64> {
        match self.opt(k) {
            Some(v) => v.parse().map_err(|_| usage(&format!("--{k} must be a number"))),
            None => Ok(default),
        }
    }

    fn flag(&self, k: &str) -> bool {
        self.map.contains_key(k)
    }
}

fn usage(msg: &str) -> Error {
    Error::Install(format!("{msg}\n\n{USAGE}"))
}

fn read(path: &str) -> Result<Vec<u8>> {
    fs::read(path).map_err(|e| Error::io(format!("read {path}"), e))
}

fn load_key(path: &str) -> Result<SigningKey> {
    let text = String::from_utf8(read(path)?).map_err(|_| Error::Metadata(format!("{path} is not text")))?;
    sign::parse_secret(&text)
}

fn load_keys(paths: &[String]) -> Result<Vec<SigningKey>> {
    if paths.is_empty() {
        return Err(usage("at least one --key/--sign is required"));
    }
    paths.iter().map(|p| load_key(p)).collect()
}

fn load_pub(path: &str) -> Result<ed25519_dalek::VerifyingKey> {
    let text = String::from_utf8(read(path)?).map_err(|_| Error::Metadata(format!("{path} is not text")))?;
    sign::parse_public(text.trim())
}

fn platform(a: &Args) -> Result<Platform> {
    let p = a.req("platform")?;
    Platform::parse(&p).ok_or_else(|| usage(&format!("unknown platform {p}; use linux-x86_64 or windows-x86_64")))
}

fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic(path, data)
}

fn keygen(a: &Args) -> Result<()> {
    let dir = PathBuf::from(a.req("out")?);
    let name = a.req("name")?;
    fs::create_dir_all(&dir).map_err(|e| Error::io_path("create", &dir, e))?;
    let key_path = dir.join(format!("{name}.key"));
    if key_path.exists() {
        return Err(Error::Install(format!("{} exists; refusing to overwrite a key", key_path.display())));
    }
    let k = sign::generate_secret();
    write_file(&key_path, format!("{}\n", hex::encode(k.to_bytes())).as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).map_err(|e| Error::io_path("protect", &key_path, e))?;
    }
    let vk = k.verifying_key();
    write_file(&dir.join(format!("{name}.pub")), format!("{}\n", hex::encode(vk.as_bytes())).as_bytes())?;
    println!("secret {}  (keep offline, never commit)", key_path.display());
    println!("public {}", dir.join(format!("{name}.pub")).display());
    println!("keyid  {}", sign::key_id(&vk));
    Ok(())
}

fn root_cmd(a: &Args) -> Result<()> {
    let version = a.num("version", 1)?;
    let expires = a.req("expires")?;
    parse_utc(&expires)?;
    let root_pubs: Vec<_> = a.all("root-pub").iter().map(|p| load_pub(p)).collect::<Result<_>>()?;
    let release_pubs: Vec<_> = a.all("release-pub").iter().map(|p| load_pub(p)).collect::<Result<_>>()?;
    if root_pubs.is_empty() || release_pubs.is_empty() {
        return Err(usage("need --root-pub and --release-pub"));
    }
    let mut keys = BTreeMap::new();
    let mut ids = |list: &[ed25519_dalek::VerifyingKey]| -> Vec<String> {
        list.iter()
            .map(|vk| {
                let id = sign::key_id(vk);
                keys.insert(id.clone(), svxdist::meta::KeyDef { scheme: "ed25519".into(), public: hex::encode(vk.as_bytes()) });
                id
            })
            .collect()
    };
    let root_ids = ids(&root_pubs);
    let release_ids = ids(&release_pubs);
    let body = Root {
        kind: "root".into(),
        spec: svxdist::meta::SPEC,
        version,
        expires,
        keys,
        roles: svxdist::meta::Roles {
            root: svxdist::meta::Role { keyids: root_ids, threshold: a.num("root-threshold", 1)? as u32 },
            release: svxdist::meta::Role { keyids: release_ids, threshold: a.num("release-threshold", 1)? as u32 },
        },
    };
    body.validate()?;
    let signers = load_keys(&a.all("sign"))?;
    let refs: Vec<&SigningKey> = signers.iter().collect();
    let bytes = release::sign_json(&body, &refs)?;
    trust::verify_root_self(&bytes)?;
    let out = PathBuf::from(a.req("out")?);
    write_file(&out, &bytes)?;
    println!("wrote root version {version} to {}", out.display());
    if version > 1 {
        println!("check the chain with: svx-release verify --root <root.1.json> --roots <dir with N.root.json files>");
    }
    Ok(())
}

fn test_root(a: &Args) -> Result<()> {
    let out = PathBuf::from(a.req("out")?);
    write_file(&out, &svxdist::testkeys::root_v1())?;
    println!("wrote TEST-ONLY root (publicly known keys) to {}", out.display());
    Ok(())
}

fn test_keys(a: &Args) -> Result<()> {
    let dir = PathBuf::from(a.req("out")?);
    for name in ["root", "release"] {
        let k = svxdist::testkeys::key(name);
        write_file(&dir.join(format!("TEST-ONLY-{name}.key")), format!("{}\n", hex::encode(k.to_bytes())).as_bytes())?;
        write_file(&dir.join(format!("TEST-ONLY-{name}.pub")), format!("{}\n", hex::encode(k.verifying_key().as_bytes())).as_bytes())?;
    }
    println!("wrote publicly known TEST-ONLY keys to {}", dir.display());
    Ok(())
}

fn check_keys(a: &Args) -> Result<()> {
    let mut root = trust::verify_root_self(&read(&a.req("root")?)?)?;
    if let Some(dir) = a.opt("roots") {
        let mut n = root.version + 1;
        while let Ok(raw) = fs::read(Path::new(&dir).join(format!("{n}.root.json"))) {
            root = trust::verify_root_successor(&root, &raw)?;
            n += 1;
        }
    }
    let paths = a.all("key");
    if paths.is_empty() {
        return Err(usage("give at least one --key"));
    }
    for p in paths {
        let id = sign::key_id(&load_key(&p)?.verifying_key());
        let mut roles = Vec::new();
        if root.roles.root.keyids.contains(&id) {
            roles.push("root");
        }
        if root.roles.release.keyids.contains(&id) {
            roles.push("release");
        }
        if roles.is_empty() {
            return Err(Error::Signature(format!("{p} is not a key of trusted root version {}", root.version)));
        }
        println!("{p}: {} key of root version {}", roles.join(" and "), root.version);
    }
    Ok(())
}

fn release_cmd(a: &Args) -> Result<()> {
    let payload = PathBuf::from(a.req("payload")?);
    let platform = platform(a)?;
    let info = release::read_payload_info(&payload).unwrap_or_default();
    if !info.platform.is_empty() && info.platform != platform.id() && info.platform != platform.id().split('-').next().unwrap_or("") {
        return Err(Error::Platform(format!("payload was built for {}, not {}", info.platform, platform.id())));
    }
    let version = a.opt("version").or(Some(info.version.clone()).filter(|v| !v.is_empty())).ok_or_else(|| usage("missing --version"))?;
    Version::parse(&version)?;
    let build: u64 = match a.opt("build").or(Some(info.build.clone()).filter(|v| !v.is_empty())) {
        Some(b) => b.parse().map_err(|_| usage("build must be a number"))?,
        None => return Err(usage("missing --build")),
    };
    let commit = a.opt("commit").unwrap_or(info.commit.clone());
    let date = a.opt("date").or(Some(info.date.clone()).filter(|d| !d.is_empty())).unwrap_or_else(|| format_utc(now_unix()));
    parse_utc(&date)?;
    let tag = a.opt("tag").unwrap_or_else(|| format!("svx-{version}"));
    let manifest_name = format!("manifest-{}.json", platform.id());
    let (asset_base, manifest_url) = match a.opt("github") {
        Some(repo) => {
            let base = format!("https://github.com/{repo}/releases/download/{tag}/");
            (base.clone(), format!("{base}{manifest_name}"))
        }
        None => (a.req("asset-base")?, a.req("manifest-url")?),
    };
    let keys = load_keys(&a.all("key"))?;
    let refs: Vec<&SigningKey> = keys.iter().collect();
    let mut reuse = ReuseIndex::default();
    for spec in a.all("reuse") {
        let (file, url) = spec.split_once('=').ok_or_else(|| usage("--reuse takes MANIFEST=URL"))?;
        let m = trust::parse_stored_manifest(&read(file)?, platform)?;
        reuse.add(&m, &Url::parse(url).map_err(|e| usage(&format!("--reuse url: {e}")))?)?;
    }
    let (files, skipped) = release::collect_payload(&payload, platform)?;
    if !skipped.is_empty() {
        println!("not part of the managed release: {}", skipped.join(", "));
    }
    let out = PathBuf::from(a.req("out")?);
    let assets = out.join("assets");
    let rinfo = ReleaseInfo { version: version.clone(), build, commit, platform, min_patcher: a.req("min-patcher")?, date };
    let built =
        release::build_release(&files, &rinfo, &asset_base, &assets, &reuse, a.num("pack-limit", release::DEFAULT_PACK_LIMIT)?, a.num("level", 6)? as u32)?;
    let signed = release::sign_json(&built.manifest, &refs)?;
    write_file(&assets.join(&manifest_name), &signed)?;
    let entry = release::release_ref(&built.manifest, &signed, &manifest_url, &a.opt("notes").unwrap_or_default());
    write_file(&out.join("release-entry.json"), &serde_json::to_vec_pretty(&entry).expect("entry"))?;
    println!(
        "{} {} build {}: {} files, install {}, full download {}, new packs {}, reused {}",
        platform.id(),
        version,
        build,
        built.manifest.files.len(),
        human_bytes(entry.install_size),
        human_bytes(entry.download_size),
        built.new_packs.len(),
        human_bytes(built.reused_bytes)
    );
    println!("assets in {}  entry {}", assets.display(), out.join("release-entry.json").display());
    Ok(())
}

fn load_catalog(path: &str) -> Result<Catalog> {
    match fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).map_err(|e| Error::Metadata(format!("{path}: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Catalog { channel: "stable".into(), ..Default::default() }),
        Err(e) => Err(Error::io(format!("read {path}"), e)),
    }
}

fn save_catalog(path: &str, c: &Catalog) -> Result<()> {
    let mut b = serde_json::to_vec_pretty(c).expect("catalog");
    b.push(b'\n');
    write_file(Path::new(path), &b)
}

fn catalog_add(a: &Args) -> Result<()> {
    let path = a.req("catalog")?;
    let mut c = load_catalog(&path)?;
    let entry: svxdist::meta::ReleaseRef = serde_json::from_slice(&read(&a.req("entry")?)?).map_err(|e| Error::Metadata(e.to_string()))?;
    let v = entry.version.clone();
    c.add(platform(a)?, entry, a.flag("latest"))?;
    save_catalog(&path, &c)?;
    println!("catalog now lists {v}");
    Ok(())
}

fn withdraw(a: &Args) -> Result<()> {
    let path = a.req("catalog")?;
    let mut c = load_catalog(&path)?;
    c.withdraw(platform(a)?, &a.req("version")?)?;
    save_catalog(&path, &c)
}

fn patcher_cmd(a: &Args) -> Result<()> {
    let path = a.req("catalog")?;
    let mut c = load_catalog(&path)?;
    let version = a.req("version")?;
    Version::parse(&version)?;
    let base = a.req("base-url")?;
    let mut builds = BTreeMap::new();
    for (flag, p) in [("linux", Platform::LinuxX86_64), ("windows", Platform::WindowsX86_64)] {
        if let Some(file) = a.opt(flag) {
            let (sha, size) = sha256_file(Path::new(&file), None)?;
            let name = Path::new(&file).file_name().and_then(|n| n.to_str()).ok_or_else(|| usage("bad patcher file name"))?;
            builds.insert(p.id().to_string(), Artifact { url: format!("{base}{name}"), sha256: sha, size });
        }
    }
    if builds.is_empty() {
        return Err(usage("give --linux and/or --windows"));
    }
    c.patcher = Some(PatcherInfo { version, builds });
    save_catalog(&path, &c)
}

fn build_index(a: &Args) -> Result<(Vec<u8>, Index)> {
    let c = load_catalog(&a.req("catalog")?)?;
    let prev = match a.opt("prev") {
        Some(p) if Path::new(&p).exists() => {
            let env = sign::parse_envelope(&read(&p)?)?;
            Some(svxdist::meta::decode::<Index>(&env, "index")?.version)
        }
        _ => None,
    };
    let index = c.to_index(release::next_index_version(prev));
    index.validate()?;
    let keys = load_keys(&a.all("key"))?;
    let refs: Vec<&SigningKey> = keys.iter().collect();
    Ok((release::sign_json(&index, &refs)?, index))
}

fn index_cmd(a: &Args) -> Result<()> {
    let (bytes, index) = build_index(a)?;
    let out = PathBuf::from(a.req("out")?);
    write_file(&out, &bytes)?;
    println!("index version {} -> {}", index.version, out.display());
    Ok(())
}

fn site_cmd(a: &Args) -> Result<()> {
    let out = PathBuf::from(a.req("out")?);
    let (bytes, index) = build_index(a)?;
    let roots = PathBuf::from(a.req("roots")?);
    let mut root_files = Vec::new();
    for e in fs::read_dir(&roots).map_err(|e| Error::io_path("list", &roots, e))?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(n) = name.strip_suffix(".root.json").and_then(|n| n.parse::<u64>().ok()) {
            root_files.push((n, e.path()));
        }
    }
    root_files.sort();
    if root_files.first().map(|r| r.0) != Some(1) {
        return Err(Error::Install(format!("{} must contain 1.root.json", roots.display())));
    }
    let mut current = trust::verify_root_self(&fs::read(&root_files[0].1).map_err(|e| Error::io_path("read", &root_files[0].1, e))?)?;
    fs::create_dir_all(&out).map_err(|e| Error::io_path("create", &out, e))?;
    for (i, (n, p)) in root_files.iter().enumerate() {
        let raw = fs::read(p).map_err(|e| Error::io_path("read", p, e))?;
        if i > 0 {
            current = trust::verify_root_successor(&current, &raw)?;
        }
        write_file(&out.join(format!("{n}.root.json")), &raw)?;
    }
    let mut t = Trust::load(&fs::read(&root_files[0].1).map_err(|e| Error::io_path("read", &root_files[0].1, e))?, None)?;
    t.root = current;
    t.verify_index(&bytes)?;
    write_file(&out.join("index.json"), &bytes)?;
    println!("index files in {}: index version {}, roots 1..{}", out.display(), index.version, root_files.last().map(|r| r.0).unwrap_or(1));
    Ok(())
}

fn verify_cmd(a: &Args) -> Result<()> {
    let mut root = trust::verify_root_self(&read(&a.req("root")?)?)?;
    println!("root v{} ok, expires {}{}", root.version, root.expires, if svxdist::testkeys::is_test_root(&root) { "  (TEST KEYS)" } else { "" });
    if let Some(dir) = a.opt("roots") {
        let mut n = root.version + 1;
        loop {
            let p = Path::new(&dir).join(format!("{n}.root.json"));
            let Ok(raw) = fs::read(&p) else { break };
            root = trust::verify_root_successor(&root, &raw)?;
            println!("root v{} ok (chained), expires {}", root.version, root.expires);
            n += 1;
        }
    }
    let state = Trust::load(&read(&a.req("root")?)?, None)?;
    let mut t = state;
    t.root = root;
    if let Some(ip) = a.opt("index") {
        let raw = read(&ip)?;
        let index = t.verify_index(&raw)?;
        println!("index v{} ok", index.version);
        for (p, pi) in &index.platforms {
            println!("  {p}: latest {} ({} releases)", pi.latest, pi.releases.len());
        }
        if let Some(mp) = a.opt("manifest") {
            let p = platform(a)?;
            let raw = read(&mp)?;
            let env = sign::parse_envelope(&raw)?;
            let m: svxdist::meta::Manifest = svxdist::meta::decode(&env, "manifest")?;
            let r = index
                .platform(p)
                .and_then(|pi| pi.releases.iter().find(|r| r.version == m.version))
                .ok_or_else(|| Error::Metadata("manifest version not in index".into()))?;
            let m = t.verify_manifest(&raw, r, p, Version(u64::MAX, 0, 0))?;
            println!("manifest {} {} ok: {} files, sha {}", m.platform, m.version, m.files.len(), sha256_hex(&raw));
        }
    }
    Ok(())
}

fn publish_plan(a: &Args) -> Result<()> {
    let dir = PathBuf::from(a.req("release-dir")?);
    let repo = a.req("repo")?;
    let tag = a.req("tag")?;
    let kind = a.req("kind")?;
    let assets = if kind == "index" { dir.clone() } else { dir.join("assets") };
    let mut files: Vec<String> =
        fs::read_dir(&assets).map_err(|e| Error::io_path("list", &assets, e))?.flatten().map(|e| e.path().display().to_string()).collect();
    if kind == "index" {
        files.retain(|f| f.ends_with(".json"));
    }
    files.sort_by_key(|f| (f.ends_with("/index.json"), f.clone()));
    for f in &files {
        let size = fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        if size >= 2 * 1024 * 1024 * 1024 {
            return Err(Error::Install(format!("{f} exceeds GitHub's 2 GiB asset limit")));
        }
    }
    if files.len() > 1000 {
        return Err(Error::Install("more than 1000 assets in one release".into()));
    }
    let mut script = String::from("#!/usr/bin/env bash\nset -euo pipefail\n");
    let (create, title, notes, clobber) = match kind.as_str() {
        "game" => (
            "--latest=false",
            format!("SVX {}", tag.trim_start_matches("svx-")),
            "Game files used by the SVX Patcher. Players: download the patcher from the latest release.",
            "",
        ),
        "patcher" => (
            "--latest",
            format!("SVX Patcher {}", tag.trim_start_matches("patcher-")),
            "Download SVXPatcher.exe (Windows) or svx-patcher (Linux) and run it. It installs, updates and repairs SVX.",
            "",
        ),
        "index" => {
            ("--latest=false --prerelease", "SVX release index".to_string(), "Signed release list read by the SVX Patcher. Updated in place.", " --clobber")
        }
        other => return Err(usage(&format!("unknown --kind {other}"))),
    };
    script.push_str(&format!(
        "gh release view {tag} --repo {repo} >/dev/null 2>&1 || gh release create {tag} --repo {repo} {create} --title '{title}' --notes '{notes}'\n"
    ));
    for f in &files {
        script.push_str(&format!("gh release upload {tag} --repo {repo}{clobber} '{f}'\n"));
    }
    let out = PathBuf::from(a.req("out")?);
    write_file(&out, script.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&out, fs::Permissions::from_mode(0o755));
    }
    println!("wrote {} ({} assets); review it, then run it yourself", out.display(), files.len());
    Ok(())
}

fn run() -> Result<()> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = raw.first().cloned() else { return Err(usage("missing command")) };
    if cmd == "-h" || cmd == "--help" || cmd == "help" {
        print!("{USAGE}");
        return Ok(());
    }
    let a = Args::parse(&raw[1..])?;
    match cmd.as_str() {
        "keygen" => keygen(&a),
        "root" => root_cmd(&a),
        "test-root" => test_root(&a),
        "test-keys" => test_keys(&a),
        "check-keys" => check_keys(&a),
        "release" => release_cmd(&a),
        "catalog-add" => catalog_add(&a),
        "withdraw" => withdraw(&a),
        "patcher" => patcher_cmd(&a),
        "index" => index_cmd(&a),
        "index-dir" => site_cmd(&a),
        "verify" => verify_cmd(&a),
        "publish-plan" => publish_plan(&a),
        other => Err(usage(&format!("unknown command {other}"))),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
