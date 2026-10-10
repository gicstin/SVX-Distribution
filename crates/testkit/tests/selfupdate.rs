mod common;

use common::*;
use std::collections::BTreeMap;
use std::fs;
use svxdist::Error;
use svxdist::meta::{Artifact, PatcherInfo};
use svxdist::selfupdate;
use svxdist::util::sha256_hex;
use svxdist::version::Version;

#[test]
fn patcher_self_update_is_verified_and_replaces_the_binary() {
    let mut f = Fixture::new("selfupdate");
    let new_build = b"#!/bin/sh\necho patcher 0.2.0\n".to_vec();
    fs::create_dir_all(f.site.join("patcher")).unwrap();
    fs::write(f.site.join("patcher/svx-patcher"), &new_build).unwrap();
    let mut builds = BTreeMap::new();
    builds.insert(P.id().to_string(), Artifact { url: "patcher/svx-patcher".into(), sha256: sha256_hex(&new_build), size: new_build.len() as u64 });
    f.catalog.patcher = Some(PatcherInfo { version: "0.2.0".into(), builds });
    f.publish("1.0.0");
    let mut s = f.session();
    let idx = s.check().unwrap().clone();
    assert!(selfupdate::available(&idx, Version(0, 2, 0), P).is_none());
    let (v, art) = selfupdate::available(&idx, Version(0, 1, 0), P).expect("update offered");
    assert_eq!(v, Version(0, 2, 0));

    let bin_dir = f.dir.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let exe = bin_dir.join("svx-patcher");
    fs::write(&exe, b"old patcher").unwrap();
    fs::write(f.site.join("patcher/svx-patcher"), b"#!/bin/sh\necho evil\n").unwrap();
    let err = selfupdate::install(&s.fetcher, &s.cfg.index_url, &art, &exe).unwrap_err();
    assert!(matches!(err, Error::Hash(_) | Error::TooLarge(_)), "{err}");
    assert_eq!(fs::read(&exe).unwrap(), b"old patcher");

    fs::write(f.site.join("patcher/svx-patcher"), &new_build).unwrap();
    selfupdate::install(&s.fetcher, &s.cfg.index_url, &art, &exe).unwrap();
    assert_eq!(fs::read(&exe).unwrap(), new_build);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(fs::metadata(&exe).unwrap().permissions().mode() & 0o111 != 0);
    }
    selfupdate::cleanup(&exe);
    assert_eq!(fs::read_dir(&bin_dir).unwrap().count(), 1);
}
