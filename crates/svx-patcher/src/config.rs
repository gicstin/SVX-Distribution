use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use svxdist::dirs;
use svxdist::net::UrlPolicy;
use svxdist::platform::Platform;
use svxdist::session::Config;
use svxdist::txn::Options;
use svxdist::util::write_atomic;
use svxdist::version::Version;
use svxdist::{Error, Result};
use url::Url;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ROOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/trusted_root.json"));

pub fn test_build() -> bool {
    env!("SVX_BUILD_TEST_ROOT") == "1"
}

pub fn index_url() -> String {
    std::env::var("SVX_PATCHER_INDEX_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| env!("SVX_BUILD_INDEX_URL").to_string())
}

pub fn options() -> Options {
    let mut o = Options::default();
    if test_build() {
        o.abort_after_ops = std::env::var("SVX_PATCHER_ABORT_AFTER_OPS").ok().and_then(|v| v.parse().ok());
    }
    o
}

pub fn session_config() -> Result<Config> {
    let index_url = Url::parse(&index_url()).map_err(|e| Error::UnsafeUrl(format!("index address: {e}")))?;
    let mut extra: Vec<String> = env!("SVX_BUILD_EXTRA_HOSTS").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if let Some(h) = Url::parse(env!("SVX_BUILD_INDEX_URL")).ok().and_then(|u| u.host_str().map(|s| s.to_string())) {
        extra.push(h);
    }
    let state = dirs::config_dir().join("trust.json");
    Ok(Config {
        platform: Platform::current(),
        patcher_version: Version::parse(VERSION).expect("package version"),
        index_url,
        embedded_root: ROOT.to_vec(),
        trust_state: Some(state),
        policy: UrlPolicy::new(&extra, test_build()),
        options: options(),
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub install_dir: Option<PathBuf>,
}

impl Settings {
    pub fn load() -> Settings {
        std::fs::read(dirs::config_dir().join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let _ = write_atomic(&dirs::config_dir().join("settings.json"), &b);
        }
    }

    pub fn install_dir(&self) -> PathBuf {
        self.install_dir.clone().unwrap_or_else(dirs::default_install_dir)
    }
}
