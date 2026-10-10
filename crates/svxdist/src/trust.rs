use crate::error::{Error, Result};
use crate::meta::{self, Index, Manifest, ReleaseRef, Root};
use crate::net::Fetcher;
use crate::platform::Platform;
use crate::sign::{self, Envelope};
use crate::time::{now_unix, parse_utc};
use crate::util::{sha256_hex, write_atomic};
use crate::version::Version;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

pub const MAX_ROOT_STEPS: u64 = 64;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrustState {
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub index_version: u64,
    #[serde(default)]
    pub index_sha256: String,
}

pub struct Trust {
    pub root: Root,
    state: TrustState,
    state_path: Option<PathBuf>,
}

pub fn verify_root_self(raw: &[u8]) -> Result<Root> {
    let env = sign::parse_envelope(raw)?;
    let root: Root = meta::decode(&env, "root")?;
    root.validate()?;
    sign::verify_threshold(&env, &root.key_map(), &root.roles.root.keyids, root.roles.root.threshold, "root")?;
    Ok(root)
}

fn trust_peek_version(raw: &[u8]) -> Result<u64> {
    let env = sign::parse_envelope(raw)?;
    Ok(meta::decode::<Root>(&env, "root")?.version)
}

pub fn verify_root_successor(prev: &Root, raw: &[u8]) -> Result<Root> {
    let env = sign::parse_envelope(raw)?;
    let next: Root = meta::decode(&env, "root")?;
    next.validate()?;
    if next.version != prev.version + 1 {
        return Err(Error::Rollback(format!("root version {} does not follow {}", next.version, prev.version)));
    }
    sign::verify_threshold(&env, &prev.key_map(), &prev.roles.root.keyids, prev.roles.root.threshold, "root (previous keys)")?;
    sign::verify_threshold(&env, &next.key_map(), &next.roles.root.keyids, next.roles.root.threshold, "root (new keys)")?;
    Ok(next)
}

fn verify_release_role(root: &Root, env: &Envelope, what: &str) -> Result<()> {
    sign::verify_threshold(env, &root.key_map(), &root.roles.release.keyids, root.roles.release.threshold, what)
}

pub fn canonical_sha(env: &Envelope) -> Result<String> {
    Ok(sha256_hex(&sign::canonical(&env.signed)?))
}

impl Trust {
    pub fn load(embedded_root: &[u8], state_path: Option<&Path>) -> Result<Trust> {
        let mut root = verify_root_self(embedded_root)?;
        let mut state: TrustState = state_path.and_then(|p| fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mut chain = Vec::new();
        for raw in &state.roots {
            let Ok(saved) = trust_peek_version(raw.as_bytes()) else { break };
            if saved <= root.version {
                continue;
            }
            match verify_root_successor(&root, raw.as_bytes()) {
                Ok(next) => {
                    root = next;
                    chain.push(raw.clone());
                }
                Err(_) => break,
            }
        }
        state.roots = chain;
        Ok(Trust { root, state, state_path: state_path.map(|p| p.to_path_buf()) })
    }

    pub fn state(&self) -> &TrustState {
        &self.state
    }

    fn save(&self) -> Result<()> {
        if let Some(p) = &self.state_path {
            write_atomic(p, &serde_json::to_vec_pretty(&self.state).expect("serialize trust state"))?;
        }
        Ok(())
    }

    pub fn refresh_root(&mut self, fetcher: &Fetcher, index_url: &Url) -> Result<()> {
        let mut changed = false;
        for _ in 0..MAX_ROOT_STEPS {
            let next = self.root.version + 1;
            let url = fetcher.policy.resolve(index_url, &format!("{next}.root.json"))?;
            match fetcher.get_small(&url, meta::MAX_ROOT_BYTES) {
                Ok(raw) => {
                    self.root = verify_root_successor(&self.root, &raw)?;
                    self.state.roots.push(String::from_utf8(raw).map_err(|_| Error::Metadata("root is not UTF-8".into()))?);
                    changed = true;
                }
                Err(Error::NotFound(_)) => break,
                Err(e) => return Err(e),
            }
        }
        if changed {
            self.save()?;
        }
        if parse_utc(&self.root.expires)? <= now_unix() {
            return Err(Error::Expired(format!("trusted root version {} expired {}; a newer SVX Patcher is required", self.root.version, self.root.expires)));
        }
        Ok(())
    }

    pub fn verify_index(&mut self, raw: &[u8]) -> Result<Index> {
        let env = sign::parse_envelope(raw)?;
        verify_release_role(&self.root, &env, "index")?;
        let index: Index = meta::decode(&env, "index")?;
        index.validate()?;
        let sha = canonical_sha(&env)?;
        if index.version < self.state.index_version {
            return Err(Error::Rollback(format!("index version {} is older than {} seen before", index.version, self.state.index_version)));
        }
        if index.version == self.state.index_version && !self.state.index_sha256.is_empty() && sha != self.state.index_sha256 {
            return Err(Error::Rollback(format!("index version {} changed content without a new version", index.version)));
        }
        if index.version > self.state.index_version || self.state.index_sha256.is_empty() {
            self.state.index_version = index.version;
            self.state.index_sha256 = sha;
            self.save()?;
        }
        Ok(index)
    }

    pub fn fetch_index(&mut self, fetcher: &Fetcher, index_url: &Url) -> Result<Index> {
        self.refresh_root(fetcher, index_url)?;
        let raw = fetcher.get_small(index_url, meta::MAX_INDEX_BYTES)?;
        self.verify_index(&raw)
    }

    pub fn verify_manifest(&self, raw: &[u8], expect: &ReleaseRef, platform: Platform, patcher: Version) -> Result<Manifest> {
        if raw.len() as u64 != expect.manifest.size || sha256_hex(raw) != expect.manifest.sha256 {
            return Err(Error::Hash(format!("manifest for {} does not match the signed index", expect.version)));
        }
        let env = sign::parse_envelope(raw)?;
        verify_release_role(&self.root, &env, "manifest")?;
        let m: Manifest = meta::decode(&env, "manifest")?;
        m.validate(platform)?;
        if m.version != expect.version || m.build != expect.build {
            return Err(Error::Metadata(format!("manifest is {} build {}, index promised {} build {}", m.version, m.build, expect.version, expect.build)));
        }
        let need = Version::parse(&m.min_patcher)?;
        if need > patcher {
            return Err(Error::PatcherTooOld { required: need.to_string(), current: patcher.to_string() });
        }
        Ok(m)
    }

    pub fn fetch_manifest(&self, fetcher: &Fetcher, index_url: &Url, r: &ReleaseRef, platform: Platform, patcher: Version) -> Result<(Manifest, Vec<u8>, Url)> {
        let url = fetcher.policy.resolve(index_url, &r.manifest.url)?;
        let raw = fetcher.get_small(&url, r.manifest.size.min(meta::MAX_MANIFEST_BYTES))?;
        let m = self.verify_manifest(&raw, r, platform, patcher)?;
        Ok((m, raw, url))
    }
}

pub fn parse_stored_manifest(raw: &[u8], platform: Platform) -> Result<Manifest> {
    let env = sign::parse_envelope(raw)?;
    let m: Manifest = meta::decode(&env, "manifest")?;
    m.validate(platform)?;
    Ok(m)
}
