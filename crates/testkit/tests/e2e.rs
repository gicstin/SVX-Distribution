mod common;

use common::*;
use std::fs;
use std::time::Duration;
use svxdist::Error;
use svxdist::install::{Detected, Install};
use svxdist::launch;
use svxdist::procs;
use svxdist::release;
use svxdist::testkeys;
use svxdist::time::now_unix;
use svxdist::txn::{self, Kind, Options, Recovery, TxnState};

fn record_version(dir: &std::path::Path) -> Option<String> {
    Install::new(dir, P).read_record().unwrap().map(|r| r.version)
}

#[test]
fn fresh_install_then_launch() {
    let mut f = Fixture::new("fresh");
    f.publish("1.0.0");
    let dir = f.install_dir("a");
    fs::create_dir_all(&dir).unwrap();
    seed_protected(&dir);
    let before = snapshot_protected(&dir);
    assert!(matches!(Install::new(&dir, P).detect(), Detected::Absent));
    let mut s = f.session();
    let latest = s.latest().unwrap();
    assert_eq!(latest.version, "1.0.0");
    assert!(latest.download_size > 0 && latest.install_size > latest.download_size);
    let plan = install_version(&f, &mut s, &dir, None).unwrap();
    assert_eq!(plan.kind, Kind::Install);
    assert_matches_payload(&f, &dir, "1.0.0");
    assert_eq!(before, snapshot_protected(&dir));
    match Install::new(&dir, P).detect() {
        Detected::Managed(i) => assert!(i.health.ok()),
        _ => panic!("expected managed install"),
    }
    let marker = f.dir.join("launch-marker");
    unsafe { std::env::set_var("SVX_FIXTURE_MARKER", &marker) };
    let mut child = launch::launch(&Install::new(&dir, P), &[]).unwrap();
    match launch::watch(&mut child, Duration::from_secs(10)) {
        launch::Outcome::Exited(Some(0)) => {}
        _ => panic!("fixture game did not run"),
    }
    let text = fs::read_to_string(&marker).unwrap();
    assert!(text.contains(&format!("cwd={}", Install::new(&dir, P).root.display())), "{text}");
}

#[test]
fn update_changed_unchanged_and_version_jump() {
    let mut f = Fixture::new("update");
    f.publish("1.0.0");
    let dir = f.install_dir("a");
    fs::create_dir_all(&dir).unwrap();
    seed_protected(&dir);
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    let before = snapshot_protected(&dir);
    let b_mtime = fs::metadata(dir.join("Core/b.json")).unwrap().modified().unwrap();
    let lib_mtime = fs::metadata(dir.join(LIB)).unwrap().modified().unwrap();

    f.publish("1.1.0");
    let mut s = f.session();
    assert_eq!(s.latest().unwrap().version, "1.1.0");
    f.server.clear_log();
    let plan = install_version(&f, &mut s, &dir, None).unwrap();
    assert_eq!(plan.kind, Kind::Update);
    let mut changed: Vec<&str> = plan.ops.iter().map(|o| o.path.as_str()).collect();
    changed.sort();
    assert_eq!(changed, vec!["Core/a.bin", "Core/new.txt", "Core/old-only.txt", "SVX.x86_64"]);
    let full: u64 = f.manifests["1.1.0"].0.files.iter().map(|x| x.length).sum();
    assert!(plan.download_estimate() < full, "update should download less than a full install");
    let pack_requests: Vec<_> = f.server.requests().into_iter().filter(|r| r.path.ends_with(".pack")).collect();
    assert!(pack_requests.iter().all(|r| r.range.is_some()), "pack downloads must be ranged: {pack_requests:?}");
    assert!(pack_requests.iter().any(|r| r.path.contains("svx-1.0.0")) || pack_requests.iter().any(|r| r.path.contains("svx-1.1.0")));
    assert_matches_payload(&f, &dir, "1.1.0");
    assert_eq!(fs::metadata(dir.join("Core/b.json")).unwrap().modified().unwrap(), b_mtime, "unchanged file was rewritten");
    assert_eq!(fs::metadata(dir.join(LIB)).unwrap().modified().unwrap(), lib_mtime, "unchanged library was rewritten");
    assert!(!dir.join("Core/old-only.txt").exists());

    f.publish("2.0.0");
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    assert_matches_payload(&f, &dir, "2.0.0");

    let dir2 = f.install_dir("jump");
    fs::create_dir_all(&dir2).unwrap();
    seed_protected(&dir2);
    let mut s = f.session();
    install_version(&f, &mut s, &dir2, Some("1.0.0")).unwrap();
    let before2 = snapshot_protected(&dir2);
    let plan = install_version(&f, &mut s, &dir2, None).unwrap();
    assert_eq!(plan.kind, Kind::Update);
    assert_matches_payload(&f, &dir2, "2.0.0");
    assert_eq!(before2, snapshot_protected(&dir2));
    assert_eq!(before, snapshot_protected(&dir));
}

#[test]
fn downgrade_then_rollback_offline() {
    let mut f = Fixture::new("downgrade");
    f.publish("2.0.0");
    let dir = f.install_dir("a");
    fs::create_dir_all(&dir).unwrap();
    seed_protected(&dir);
    let before = snapshot_protected(&dir);
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    let offered: Vec<String> = s.releases().into_iter().map(|r| r.version).collect();
    assert_eq!(offered, vec!["3.0.0", "2.0.0", "1.1.0", "1.0.0"]);
    let plan = install_version(&f, &mut s, &dir, Some("1.0.0")).unwrap();
    assert_eq!(plan.kind, Kind::Switch);
    assert_matches_payload(&f, &dir, "1.0.0");
    let install = Install::new(&dir, P);
    let snap = txn::read_snapshot(&install).unwrap();
    assert!(snap.restorable());
    assert_eq!(snap.from.as_ref().unwrap().version, "2.0.0");

    for v in ["1.0.0", "1.1.0", "2.0.0", "3.0.0"] {
        for p in f.pack_paths(v) {
            f.server.status(&p, 500);
        }
    }
    let s = f.session();
    let _lock = install.lock().unwrap();
    let mut q = quiet();
    let plan = s.plan_rollback(&install, &mut q).unwrap();
    assert_eq!(plan.kind, Kind::Rollback);
    s.execute(&install, &plan, &mut q).unwrap();
    assert_matches_payload(&f, &dir, "2.0.0");
    let plan = s.plan_rollback(&install, &mut q).unwrap();
    s.execute(&install, &plan, &mut q).unwrap();
    assert_matches_payload(&f, &dir, "1.0.0");
    assert_eq!(before, snapshot_protected(&dir));
}

#[test]
fn repair_missing_and_corrupt_files() {
    let mut f = Fixture::new("repair");
    f.publish("1.1.0");
    let dir = f.install_dir("a");
    fs::create_dir_all(&dir).unwrap();
    seed_protected(&dir);
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    let before = snapshot_protected(&dir);
    fs::remove_file(dir.join("SVX.x86_64")).unwrap();
    let mut lib = fs::read(dir.join(LIB)).unwrap();
    lib[1000] ^= 0xff;
    fs::write(dir.join(LIB), &lib).unwrap();
    fs::write(dir.join("Core/a.bin"), b"truncated").unwrap();
    fs::write(dir.join("Core/junk.txt"), b"stray").unwrap();
    let install = Install::new(&dir, P);
    match install.detect() {
        Detected::Managed(i) => {
            assert!(i.health.missing.contains(&"SVX.x86_64".to_string()));
            assert!(i.health.changed.contains(&"Core/a.bin".to_string()));
        }
        _ => panic!("expected managed"),
    }
    {
        let _lock = install.lock().unwrap();
        let mut q = quiet();
        let plan = s.plan_repair(&install, &mut q).unwrap();
        assert_eq!(plan.kind, Kind::Repair);
        let paths: Vec<&str> = plan.ops.iter().map(|o| o.path.as_str()).collect();
        assert!(paths.contains(&LIB), "same-size corruption must be found by hashing: {paths:?}");
        s.execute(&install, &plan, &mut q).unwrap();
    }
    assert_matches_payload(&f, &dir, "1.1.0");
    assert_eq!(before, snapshot_protected(&dir));

    fs::remove_dir_all(dir.join("Core")).unwrap();
    fs::remove_file(dir.join(".svx-patcher/installed.json")).unwrap();
    assert!(matches!(install.detect(), Detected::Unmanaged));
    {
        let _lock = install.lock().unwrap();
        let mut q = quiet();
        let plan = s.plan_repair(&install, &mut q).unwrap();
        assert_eq!(plan.kind, Kind::Adopt);
        s.execute(&install, &plan, &mut q).unwrap();
    }
    assert_matches_payload(&f, &dir, "1.1.0");

    fs::write(dir.join(".svx-patcher/installed.json"), b"{ not json").unwrap();
    assert!(matches!(install.detect(), Detected::Damaged(_)));
    {
        let _lock = install.lock().unwrap();
        let mut q = quiet();
        let plan = s.plan_repair(&install, &mut q).unwrap();
        s.execute(&install, &plan, &mut q).unwrap();
    }
    assert_matches_payload(&f, &dir, "1.1.0");
    assert_eq!(before, snapshot_protected(&dir));
}

#[test]
fn interrupted_download_resumes() {
    let mut f = Fixture::new("resume");
    f.publish("1.0.0");
    let dir = f.install_dir("a");
    let packs = f.pack_paths("1.0.0");
    for p in &packs {
        f.server.cut_once(p, 150_000);
    }
    let mut s = f.session();
    let err = install_version(&f, &mut s, &dir, None).unwrap_err();
    assert!(matches!(err, Error::Network(_)), "{err}");
    let parts: Vec<_> = fs::read_dir(dir.join(".svx-patcher/downloads")).unwrap().flatten().collect();
    assert!(!parts.is_empty());
    assert!(fs::metadata(parts[0].path()).unwrap().len() >= 150_000);
    assert!(!dir.join("SVX.x86_64").exists(), "nothing may be activated before verification");
    assert!(record_version(&dir).is_none());
    f.server.clear_log();
    install_version(&f, &mut s, &dir, None).unwrap();
    let resumed = f.server.requests().into_iter().filter(|r| r.path.ends_with(".pack")).any(|r| {
        r.range
            .as_deref()
            .and_then(|x| x.strip_prefix("bytes="))
            .and_then(|x| x.split('-').next())
            .and_then(|x| x.parse::<u64>().ok())
            .is_some_and(|start| start >= 150_000)
    });
    assert!(resumed, "second attempt should resume with a Range request");
    assert_matches_payload(&f, &dir, "1.0.0");
}

#[test]
fn server_without_range_support_still_works() {
    let mut f = Fixture::new("norange");
    f.publish("1.1.0");
    f.server.ignore_range(true);
    let dir = f.install_dir("a");
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    assert_matches_payload(&f, &dir, "1.1.0");
}

#[test]
fn interrupted_activation_rolls_forward() {
    let mut f = Fixture::new("crashfwd");
    f.publish("1.0.0");
    let dir = f.install_dir("a");
    fs::create_dir_all(&dir).unwrap();
    seed_protected(&dir);
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    let before = snapshot_protected(&dir);
    f.publish("2.0.0");
    let mut s = f.session_with(Options { fail_after_ops: Some(3), abort_after_ops: None });
    let err = install_version(&f, &mut s, &dir, None).unwrap_err();
    assert!(matches!(err, Error::Interrupted(_)), "{err}");
    let install = Install::new(&dir, P);
    assert_eq!(txn::read_journal(&install).unwrap().unwrap().state, TxnState::Activating);
    assert_eq!(record_version(&dir).as_deref(), Some("1.0.0"));
    let _lock = install.lock().unwrap();
    let mut q = quiet();
    let r = txn::recover(&install, &std::sync::atomic::AtomicBool::new(false), &mut q).unwrap();
    assert_eq!(r, Recovery::RolledForward { kind: Kind::Update, version: Some("2.0.0".into()) });
    assert!(txn::read_journal(&install).unwrap().is_none());
    assert_matches_payload(&f, &dir, "2.0.0");
    assert_eq!(before, snapshot_protected(&dir));
    assert_eq!(txn::read_snapshot(&install).unwrap().from.unwrap().version, "1.0.0");
}

#[test]
fn interrupted_activation_with_damaged_stage_rolls_back() {
    let mut f = Fixture::new("crashback");
    f.publish("1.0.0");
    let dir = f.install_dir("a");
    fs::create_dir_all(&dir).unwrap();
    seed_protected(&dir);
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    let before = snapshot_protected(&dir);
    f.publish("2.0.0");
    let mut s = f.session_with(Options { fail_after_ops: Some(1), abort_after_ops: None });
    install_version(&f, &mut s, &dir, None).unwrap_err();
    let install = Install::new(&dir, P);
    let staged_dir = install.txn_dir().join("new");
    let j = txn::read_journal(&install).unwrap().unwrap();
    let victim = j.ops.iter().rev().find(|o| o.new.is_some() && staged_dir.join(&o.path).exists()).expect("a staged file").path.clone();
    fs::write(staged_dir.join(&victim), b"bit rot").unwrap();
    let mut q = quiet();
    {
        let _lock = install.lock().unwrap();
        let r = txn::recover(&install, &std::sync::atomic::AtomicBool::new(false), &mut q).unwrap();
        assert_eq!(r, Recovery::RolledBack { kind: Kind::Update });
    }
    assert_matches_payload(&f, &dir, "1.0.0");
    assert_eq!(before, snapshot_protected(&dir));
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    assert_matches_payload(&f, &dir, "2.0.0");
    assert_eq!(before, snapshot_protected(&dir));
}

#[test]
fn running_game_blocks_activation_until_closed() {
    let mut f = Fixture::new("running");
    f.publish("1.0.0");
    let dir = f.install_dir("a");
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    f.publish("1.1.0");
    let mut s = f.session();
    let install = Install::new(&dir, P);
    let mut child = std::process::Command::new(install.exe()).env("SVX_FIXTURE_SLEEP", "60").spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let _lock = install.lock().unwrap();
    let target = s.target(None).unwrap();
    let mut q = quiet();
    let plan = s.plan_target(&install, &target, Kind::Update, &mut q).unwrap();
    s.stage(&install, &plan, &mut q).unwrap();
    let err = s.activate(&install, &mut q).unwrap_err();
    assert!(matches!(&err, Error::GameRunning(p) if p.contains(&child.id())), "{err}");
    assert_eq!(record_version(&dir).as_deref(), Some("1.0.0"));
    assert_eq!(txn::read_journal(&install).unwrap().unwrap().state, TxnState::Ready);
    child.kill().unwrap();
    child.wait().unwrap();
    s.activate(&install, &mut q).unwrap();
    drop(_lock);
    assert_matches_payload(&f, &dir, "1.1.0");
}

#[test]
fn signed_index_carries_changelog() {
    let mut f = Fixture::new("changelog");
    f.catalog.changelog = svxdist::changelog::parse("## [1.0.0] - 2026-10-10\n### Added\n- First\n").unwrap();
    f.publish("1.0.0");
    let mut s = f.session();
    let idx = s.check().unwrap();
    assert_eq!(idx.changelog.len(), 1);
    assert_eq!(idx.changelog[0].sections[0].items, vec!["First"]);
}

#[test]
fn force_close_only_ends_this_installs_game() {
    let a = fresh_dir("close-a");
    let b = fresh_dir("close-b");
    let game_a = Install::new(&a, P).exe();
    let game_b = Install::new(&b, P).exe();
    let helper = a.join("helper");
    for p in [&game_a, &game_b, &helper] {
        fs::copy(env!("CARGO_BIN_EXE_svx-fixture-game"), p).unwrap();
    }
    let spawn = |exe: &std::path::Path| std::process::Command::new(exe).env("SVX_FIXTURE_SLEEP", "60").spawn().unwrap();
    let mut targets = vec![spawn(&game_a), spawn(&game_a)];
    let mut others = vec![spawn(&game_b), spawn(&helper)];
    std::thread::sleep(Duration::from_millis(300));
    let mut found = procs::instances_of(&game_a);
    found.sort();
    let mut want: Vec<u32> = targets.iter().map(|c| c.id()).collect();
    want.sort();
    assert_eq!(found, want);
    let report = procs::force_close(&game_a, Duration::from_secs(5)).unwrap();
    assert_eq!(report, procs::CloseReport { closed: 2, remaining: 0 });
    for c in &mut targets {
        assert!(c.try_wait().unwrap().is_some());
    }
    for c in &mut others {
        assert!(c.try_wait().unwrap().is_none());
    }
    assert!(procs::instances_of(&game_a).is_empty());
    assert_eq!(procs::instances_of(&game_b), vec![others[0].id()]);
    assert_eq!(procs::force_close(&game_a, Duration::from_secs(1)).unwrap(), procs::CloseReport::default());
    for c in &mut others {
        c.kill().unwrap();
        c.wait().unwrap();
    }
}

#[test]
fn unwritable_install_is_reported() {
    if unsafe { libc_geteuid() } == 0 {
        return;
    }
    let mut f = Fixture::new("readonly");
    f.publish("1.0.0");
    let parent = f.install_dir("locked");
    fs::create_dir_all(&parent).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
    let mut s = f.session();
    let err = install_version(&f, &mut s, &parent.join("SVX"), None).unwrap_err();
    assert!(matches!(err, Error::NotWritable { .. }), "{err}");
    assert!(err.to_string().contains("Choose a folder"));
    let err = install_version(&f, &mut s, &parent, None).unwrap_err();
    assert!(matches!(err, Error::NotWritable { .. }), "{err}");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    install_version(&f, &mut s, &parent, None).unwrap();
}

unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[test]
fn adopt_existing_portable_install_and_restore() {
    let mut f = Fixture::new("adopt");
    f.publish("1.1.0");
    let dir = f.install_dir("portable");
    let src = &f.payloads["1.0.0"];
    let (files, _) = release::collect_payload(src, P).unwrap();
    for pf in &files {
        let to = dir.join(&pf.rel);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(&pf.abs, &to).unwrap();
    }
    seed_protected(&dir);
    let before = snapshot_protected(&dir);
    let install = Install::new(&dir, P);
    assert!(matches!(install.detect(), Detected::Unmanaged));
    let mut s = f.session();
    f.server.clear_log();
    let plan = install_version(&f, &mut s, &dir, None).unwrap();
    assert_eq!(plan.kind, Kind::Adopt);
    assert!(!plan.ops.iter().any(|o| o.path == LIB || o.path == "Core/sub/c.dds"), "unchanged files must not be replaced");
    assert_matches_payload(&f, &dir, "1.1.0");
    assert_eq!(before, snapshot_protected(&dir));
    let _lock = install.lock().unwrap();
    let mut q = quiet();
    let plan = s.plan_rollback(&install, &mut q).unwrap();
    s.execute(&install, &plan, &mut q).unwrap();
    assert!(matches!(install.detect(), Detected::Unmanaged));
    for pf in &files {
        assert_eq!(fs::read(dir.join(&pf.rel)).unwrap(), fs::read(&pf.abs).unwrap(), "{}", pf.rel);
    }
    assert_eq!(before, snapshot_protected(&dir));
}

#[test]
fn newer_patcher_required() {
    let mut f = Fixture::new("minpatcher");
    f.publish("2.0.0");
    let mut s = f.session();
    match s.target(Some("3.0.0")) {
        Err(Error::PatcherTooOld { required, .. }) => assert_eq!(required, "9.0.0"),
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("accepted"),
    }
}

#[test]
fn concurrent_patchers_are_excluded() {
    let mut f = Fixture::new("lock");
    f.publish("1.0.0");
    let dir = f.install_dir("a");
    let install = Install::new(&dir, P);
    let _held = install.lock().unwrap();
    assert!(matches!(Install::new(&dir, P).lock(), Err(Error::Busy)));
}

#[test]
fn check_for_updates_fetches_only_metadata() {
    let mut f = Fixture::new("check");
    f.publish("1.0.0");
    let dir = f.install_dir("a");
    let mut s = f.session();
    install_version(&f, &mut s, &dir, None).unwrap();
    f.publish("1.1.0");
    f.server.clear_log();
    let mut s = f.session();
    s.check().unwrap();
    let latest = s.latest().unwrap();
    assert_eq!(latest.version, "1.1.0");
    let paths: Vec<String> = f.server.requests().into_iter().map(|r| r.path).collect();
    assert!(paths.iter().all(|p| p == "/index.json" || p.ends_with(".root.json")), "{paths:?}");
    let _ = now_unix();
    let _ = testkeys::key("release");
}
