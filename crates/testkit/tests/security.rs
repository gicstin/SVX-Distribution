mod common;

use common::*;
use std::fs;
use svxdist::Error;
use svxdist::install::Install;
use svxdist::meta::{Index, Manifest};
use svxdist::release;
use svxdist::sign;
use svxdist::testkeys::{self, key};
use svxdist::time::{format_utc, now_unix};
use svxdist::txn::Kind;

fn index_now(f: &Fixture, version: u64) -> Index {
    let mut c = f.catalog.clone();
    c.platforms.get_mut(P.id()).unwrap().latest = "1.0.0".into();
    c.to_index(version)
}

fn assert_untouched(dir: &std::path::Path) {
    assert!(!dir.join("SVX.x86_64").exists());
    assert!(!dir.join("Core").exists());
    assert!(Install::new(dir, P).read_record().unwrap().is_none());
}

#[test]
fn tampered_index_signature_is_rejected() {
    let mut f = Fixture::new("badsig");
    f.publish("1.0.0");
    let path = f.site.join("index.json");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(&path, text.replacen("\"latest\": \"1.0.0\"", "\"latest\": \"1.1.0\"", 1)).unwrap();
    assert!(matches!(f.session().check(), Err(Error::Signature(_))));
    let idx = index_now(&f, 5000);
    f.publish_index(&idx, &[&key("attacker")]);
    assert!(matches!(f.session().check(), Err(Error::Signature(_))));
    let mut env = sign::sign_value(&serde_json::to_value(&idx).unwrap(), &[&key("release")]).unwrap();
    let sig = &env.signatures[0].sig;
    let first = if sig.starts_with('A') { "B" } else { "A" };
    env.signatures[0].sig = format!("{first}{}", &sig[1..]);
    fs::write(&path, sign::envelope_bytes(&env)).unwrap();
    assert!(matches!(f.session().check(), Err(Error::Signature(_))));
    let root = key("root");
    f.publish_index(&idx, &[&root]);
    assert!(matches!(f.session().check(), Err(Error::Signature(_))), "root key must not sign the index");
}

#[test]
fn old_indexes_stay_valid_but_rollback_is_rejected() {
    let f = Fixture::new("fresh-meta");
    let rel = key("release");
    let mut legacy = serde_json::to_value(index_now(&f, 2000)).unwrap();
    legacy["expires"] = serde_json::json!(format_utc(now_unix() - 86400 * 400));
    fs::write(f.site.join("index.json"), sign::envelope_bytes(&sign::sign_value(&legacy, &[&rel]).unwrap())).unwrap();
    f.session().check().expect("an index signed long ago is still accepted");
    f.publish_index(&index_now(&f, 1999), &[&rel]);
    assert!(matches!(f.session().check(), Err(Error::Rollback(_))));
    let mut other = index_now(&f, 2000);
    other.channel = "beta".into();
    f.publish_index(&other, &[&rel]);
    assert!(matches!(f.session().check(), Err(Error::Rollback(_))), "same version with new content");
    f.publish_index(&index_now(&f, 2001), &[&rel]);
    f.session().check().unwrap();
}

#[test]
fn tampered_payload_and_manifest_rejected_before_activation() {
    let mut f = Fixture::new("badhash");
    f.publish("1.0.0");
    let (m, _, _) = f.manifests["1.0.0"].clone();
    let stored = m.files.iter().find(|x| x.encoding == svxdist::meta::Encoding::Store && x.size > 100).expect("a stored blob");
    let deflated = m.files.iter().find(|x| x.encoding == svxdist::meta::Encoding::Deflate && x.size > 100).expect("a deflated blob");
    let dir = f.install_dir("a");
    for entry in [stored, deflated] {
        let pack = f.site.join(format!("releases/svx-1.0.0/{}", m.packs[entry.pack].name));
        let original = fs::read(&pack).unwrap();
        let mut bad = original.clone();
        bad[(entry.offset + entry.length / 2) as usize] ^= 0x55;
        fs::write(&pack, &bad).unwrap();
        let mut s = f.session();
        let err = install_version(&f, &mut s, &dir, None).unwrap_err();
        assert!(matches!(err, Error::Hash(_)), "{}: {err}", entry.path);
        assert_untouched(&dir);
        fs::write(&pack, &original).unwrap();
    }

    let mpath = f.site.join("releases/svx-1.0.0/manifest-linux-x86_64.json");
    let mraw = fs::read(&mpath).unwrap();
    let env = sign::parse_envelope(&mraw).unwrap();
    let mut m: Manifest = serde_json::from_value(env.signed.clone()).unwrap();
    m.files[0].sha256 = "0".repeat(64);
    fs::write(&mpath, release::sign_json(&m, &[&key("release")]).unwrap()).unwrap();
    let err = f.session().target(None).err().unwrap();
    assert!(matches!(err, Error::Hash(_)), "manifest not matching index hash: {err}");
    fs::write(&mpath, &mraw).unwrap();
    install_version(&f, &mut f.session(), &dir, None).unwrap();
    assert_matches_payload(&f, &dir, "1.0.0");
}

fn publish_manifest(f: &mut Fixture, m: &Manifest, name: &str) {
    let signed = release::sign_json(m, &[&key("release")]).unwrap();
    let rel = format!("evil/{name}.json");
    fs::create_dir_all(f.site.join("evil")).unwrap();
    fs::write(f.site.join(&rel), &signed).unwrap();
    let mut entry = release::release_ref(m, &signed, &rel, "");
    entry.version = "9.9.9".into();
    let p = f.catalog.platforms.get_mut(P.id()).unwrap();
    p.releases.retain(|r| r.version != "9.9.9");
    p.releases.push(entry);
    f.publish("9.9.9");
}

fn base_manifest(f: &Fixture) -> Manifest {
    let mut m = f.manifests["1.0.0"].0.clone();
    m.version = "9.9.9".into();
    for p in &mut m.packs {
        p.url = format!("../releases/svx-1.0.0/{}", p.name);
    }
    m
}

#[test]
fn malicious_paths_rejected() {
    let mut f = Fixture::new("paths");
    let outside = f.dir.join("outside-target");
    for (i, evil) in [
        "../outside-target",
        "/tmp/evil",
        "Content/x.svxfig",
        "User/settings.json",
        "logs/latest.log",
        "Core/../../x",
        "core/a.bin",
        "Core\\a",
        "C:/a",
        ".svx-patcher/installed.json",
        "Core/aux",
    ]
    .iter()
    .enumerate()
    {
        let mut m = base_manifest(&f);
        m.files[3].path = evil.to_string();
        publish_manifest(&mut f, &m, &format!("p{i}"));
        let err = f.session().target(None).err().unwrap_or_else(|| panic!("{evil} accepted"));
        assert!(matches!(err, Error::UnsafePath(_) | Error::Metadata(_)), "{evil}: {err}");
    }
    let mut m = base_manifest(&f);
    let mut dup = m.files[3].clone();
    dup.path = dup.path.to_uppercase().replacen("CORE", "Core", 1);
    m.files.push(dup);
    publish_manifest(&mut f, &m, "case");
    assert!(matches!(f.session().target(None), Err(Error::UnsafePath(_))));
    assert!(!outside.exists());
}

#[test]
fn platform_mismatch_rejected() {
    let mut f = Fixture::new("platform");
    let mut m = base_manifest(&f);
    m.platform = "windows-x86_64".into();
    publish_manifest(&mut f, &m, "win");
    assert!(matches!(f.session().target(None), Err(Error::Platform(_))));
}

#[test]
fn unsafe_urls_and_redirects_rejected() {
    let mut f = Fixture::new("urls");
    for (i, url) in ["https://evil.example/x.pack", "file:///etc/passwd", "http://github.com/x.pack", "https://user:pw@github.com/x"].iter().enumerate() {
        let mut m = base_manifest(&f);
        m.packs[0].url = url.to_string();
        publish_manifest(&mut f, &m, &format!("u{i}"));
        let mut s = f.session();
        let target = s.target(None).unwrap();
        let dir = f.install_dir(&format!("u{i}"));
        let install = Install::new(&dir, P);
        let _lock = install.lock().unwrap();
        let mut q = quiet();
        let err = s.plan_target(&install, &target, Kind::Install, &mut q).expect_err("unsafe url accepted");
        assert!(matches!(err, Error::UnsafeUrl(_)), "{url}: {err}");
        assert!(!dir.join("SVX.x86_64").exists());
    }
    f.publish("1.0.0");
    let pack = f.pack_paths("1.0.0")[0].clone();
    for (i, to) in ["http://example.com/x.pack", "https://evil.example/x.pack", "ftp://127.0.0.1/x"].iter().enumerate() {
        f.server.redirect(&pack, to);
        let dir = f.install_dir(&format!("r{i}"));
        let err = install_version(&f, &mut f.session(), &dir, None).unwrap_err();
        assert!(matches!(err, Error::UnsafeUrl(_)), "{to}: {err}");
        assert_untouched(&dir);
    }
    let moved = format!("moved/{}", pack.rsplit('/').next().unwrap());
    fs::create_dir_all(f.site.join("moved")).unwrap();
    fs::copy(f.site.join(&pack), f.site.join(&moved)).unwrap();
    f.server.redirect(&pack, &f.server.url(&moved));
    let dir = f.install_dir("ok-redirect");
    install_version(&f, &mut f.session(), &dir, None).unwrap();
    assert_matches_payload(&f, &dir, "1.0.0");
    let mut cfg = f.config(Default::default());
    cfg.policy = svxdist::net::UrlPolicy::new(&[], false);
    assert!(matches!(svxdist::session::Session::new(cfg, Default::default()), Err(Error::UnsafeUrl(_))), "production policy must refuse loopback http");
}

#[test]
fn oversized_metadata_rejected() {
    let mut f = Fixture::new("oversize");
    f.publish("1.0.0");
    let big = vec![b' '; 3 * 1024 * 1024];
    fs::write(f.site.join("index.json"), &big).unwrap();
    assert!(matches!(f.session().check(), Err(Error::TooLarge(_))));
    f.publish("1.0.0");
    let mpath = f.site.join("releases/svx-1.0.0/manifest-linux-x86_64.json");
    let mut raw = fs::read(&mpath).unwrap();
    raw.extend(std::iter::repeat_n(b' ', 1000));
    fs::write(&mpath, &raw).unwrap();
    assert!(matches!(f.session().target(None), Err(Error::TooLarge(_)) | Err(Error::Hash(_))));
}

#[test]
fn root_rotation_chain() {
    let mut f = Fixture::new("rotate");
    let old_root = key("root");
    let new_root = key("root-2");
    let new_release = key("release-2");
    let body = testkeys::root_body(2, "2099-01-01T00:00:00Z", &[&new_root], 1, &[&new_release], 1);
    fs::write(f.site.join("2.root.json"), release::sign_json(&body, &[&new_root]).unwrap()).unwrap();
    f.publish_index(&index_now(&f, 3000), &[&new_release]);
    assert!(matches!(f.session().check(), Err(Error::Signature(_))), "rotation must be signed by the old root too");
    fs::write(f.site.join("2.root.json"), release::sign_json(&body, &[&old_root]).unwrap()).unwrap();
    assert!(matches!(f.session().check(), Err(Error::Signature(_))), "rotation must be signed by the new root too");
    fs::write(f.site.join("2.root.json"), release::sign_json(&body, &[&old_root, &new_root]).unwrap()).unwrap();
    f.session().check().unwrap();
    f.publish_index(&index_now(&f, 3001), &[&key("release")]);
    assert!(matches!(f.session().check(), Err(Error::Signature(_))), "the rotated-out release key must stop working");
    fs::remove_file(f.site.join("2.root.json")).unwrap();
    assert!(matches!(f.session().check(), Err(Error::Signature(_))), "a removed root file must not roll back the trusted root");
    let state_path = f.dir.join("trust-state.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(state["roots"].as_array().unwrap().len(), 1);
    let attacker = key("attacker");
    let forged = testkeys::root_body(2, "2099-01-01T00:00:00Z", &[&attacker], 1, &[&attacker], 1);
    state["roots"] = serde_json::json!([String::from_utf8(release::sign_json(&forged, &[&attacker]).unwrap()).unwrap()]);
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    f.publish_index(&index_now(&f, 3002), &[&attacker]);
    assert!(matches!(f.session().check(), Err(Error::Signature(_))), "a forged root in the local trust cache must not be trusted");
    fs::write(f.site.join("2.root.json"), release::sign_json(&body, &[&old_root, &new_root]).unwrap()).unwrap();
    f.publish_index(&index_now(&f, 3003), &[&new_release]);
    f.session().check().unwrap();
    fs::remove_file(f.site.join("2.root.json")).unwrap();
    let skip = testkeys::root_body(4, "2099-01-01T00:00:00Z", &[&new_root], 1, &[&new_release], 1);
    fs::write(f.site.join("3.root.json"), release::sign_json(&skip, &[&new_root]).unwrap()).unwrap();
    assert!(matches!(f.session().check(), Err(Error::Rollback(_))), "root versions must be consecutive");
    let expired = testkeys::root_body(3, &format_utc(now_unix() - 5), &[&new_root], 1, &[&new_release], 1);
    fs::write(f.site.join("3.root.json"), release::sign_json(&expired, &[&new_root]).unwrap()).unwrap();
    assert!(matches!(f.session().check(), Err(Error::Expired(_))));
    let _ = &mut f;
}

#[cfg(unix)]
#[test]
fn links_escaping_the_install_are_refused() {
    let mut f = Fixture::new("links");
    f.publish("1.0.0");
    let outside = f.dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let dir = f.install_dir("a");
    fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("Core")).unwrap();
    let err = install_version(&f, &mut f.session(), &dir, None).unwrap_err();
    assert!(matches!(err, Error::UnsafePath(_)), "{err}");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

    let dir = f.install_dir("b");
    install_version(&f, &mut f.session(), &dir, None).unwrap();
    fs::remove_dir_all(dir.join("Core/sub")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("Core/sub")).unwrap();
    let install = Install::new(&dir, P);
    let _lock = install.lock().unwrap();
    let mut q = quiet();
    let err = f.session().plan_repair(&install, &mut q).expect_err("link accepted");
    assert!(matches!(err, Error::UnsafePath(_)), "{err}");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

    let dir = f.install_dir("c");
    install_version(&f, &mut f.session(), &dir, None).unwrap();
    fs::remove_file(dir.join("Core/b.json")).unwrap();
    let secret = outside.join("secret");
    fs::write(&secret, b"do not touch").unwrap();
    std::os::unix::fs::symlink(&secret, dir.join("Core/b.json")).unwrap();
    let install = Install::new(&dir, P);
    let _lock = install.lock().unwrap();
    let mut s = f.session();
    let plan = s.plan_repair(&install, &mut q).unwrap();
    s.execute(&install, &plan, &mut q).unwrap();
    assert_eq!(fs::read(&secret).unwrap(), b"do not touch");
    assert!(!fs::symlink_metadata(dir.join("Core/b.json")).unwrap().file_type().is_symlink());
    assert_matches_payload(&f, &dir, "1.0.0");
}
