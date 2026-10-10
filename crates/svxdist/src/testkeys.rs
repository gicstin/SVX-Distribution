use crate::meta::{KeyDef, Role, Roles, Root, SPEC};
use crate::sign;
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const SEED_PREFIX: &str = "SVX TEST ONLY KEY - PUBLICLY KNOWN - NEVER USE FOR RELEASES/";
pub const TEST_ROOT_EXPIRES: &str = "2099-12-31T00:00:00Z";

pub fn key(name: &str) -> SigningKey {
    let seed: [u8; 32] = Sha256::digest(format!("{SEED_PREFIX}{name}").as_bytes()).into();
    SigningKey::from_bytes(&seed)
}

pub fn root_body(version: u64, expires: &str, root_keys: &[&SigningKey], root_threshold: u32, release_keys: &[&SigningKey], release_threshold: u32) -> Root {
    let mut keys = BTreeMap::new();
    let mut ids = |list: &[&SigningKey]| -> Vec<String> {
        list.iter()
            .map(|k| {
                let vk = k.verifying_key();
                let id = sign::key_id(&vk);
                keys.insert(id.clone(), KeyDef { scheme: "ed25519".into(), public: hex::encode(vk.as_bytes()) });
                id
            })
            .collect()
    };
    let root_ids = ids(root_keys);
    let release_ids = ids(release_keys);
    Root {
        kind: "root".into(),
        spec: SPEC,
        version,
        expires: expires.into(),
        keys,
        roles: Roles { root: Role { keyids: root_ids, threshold: root_threshold }, release: Role { keyids: release_ids, threshold: release_threshold } },
    }
}

pub fn root_v1() -> Vec<u8> {
    let r = key("root");
    let rel = key("release");
    let body = root_body(1, TEST_ROOT_EXPIRES, &[&r], 1, &[&rel], 1);
    crate::release::sign_json(&body, &[&r]).expect("sign test root")
}

pub fn is_test_root(root: &Root) -> bool {
    let test_ids = [sign::key_id(&key("root").verifying_key()), sign::key_id(&key("release").verifying_key())];
    root.keys.keys().any(|k| test_ids.contains(k))
}
