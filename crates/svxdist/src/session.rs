use crate::error::{Error, Result};
use crate::install::{Install, InstalledRecord};
use crate::meta::{Index, Manifest, ReleaseRef};
use crate::net::{Fetcher, UrlPolicy};
use crate::platform::Platform;
use crate::trust::Trust;
use crate::txn::{self, Kind, Options, Plan, ProgressFn, RemoteTarget};
use crate::version::Version;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use url::Url;

#[derive(Clone, Debug)]
pub struct Config {
    pub platform: Platform,
    pub patcher_version: Version,
    pub index_url: Url,
    pub embedded_root: Vec<u8>,
    pub trust_state: Option<PathBuf>,
    pub policy: UrlPolicy,
    pub options: Options,
}

pub struct Session {
    pub cfg: Config,
    pub fetcher: Fetcher,
    pub trust: Trust,
    pub index: Option<Index>,
    pub cancel: Arc<AtomicBool>,
}

pub struct Target {
    pub release: ReleaseRef,
    pub manifest: Manifest,
    pub raw: Vec<u8>,
    pub url: Url,
}

impl Session {
    pub fn new(cfg: Config, cancel: Arc<AtomicBool>) -> Result<Session> {
        cfg.policy.check(&cfg.index_url)?;
        let trust = Trust::load(&cfg.embedded_root, cfg.trust_state.as_deref())?;
        let fetcher = Fetcher::new(cfg.policy.clone(), cancel.clone(), &format!("SVX-Patcher/{}", cfg.patcher_version));
        Ok(Session { cfg, fetcher, trust, index: None, cancel })
    }

    pub fn check(&mut self) -> Result<&Index> {
        let index = self.trust.fetch_index(&self.fetcher, &self.cfg.index_url)?;
        if index.platform(self.cfg.platform).is_none() {
            return Err(Error::Platform(format!("no SVX releases are published for {}", self.cfg.platform.label())));
        }
        self.index = Some(index);
        Ok(self.index.as_ref().expect("index"))
    }

    fn index(&mut self) -> Result<&Index> {
        if self.index.is_none() {
            self.check()?;
        }
        Ok(self.index.as_ref().expect("index"))
    }

    pub fn releases(&self) -> Vec<ReleaseRef> {
        self.index.as_ref().and_then(|i| i.platform(self.cfg.platform)).map(|p| p.offered().into_iter().cloned().collect()).unwrap_or_default()
    }

    pub fn latest(&mut self) -> Result<ReleaseRef> {
        let platform = self.cfg.platform;
        let idx = self.index()?;
        idx.platform(platform).and_then(|p| p.latest_ref()).cloned().ok_or_else(|| Error::Platform("no release for this system".into()))
    }

    pub fn target(&mut self, version: Option<&str>) -> Result<Target> {
        let platform = self.cfg.platform;
        let release = match version {
            None => self.latest()?,
            Some(v) => {
                let idx = self.index()?;
                idx.platform(platform).and_then(|p| p.find(v)).cloned().ok_or_else(|| Error::Install(format!("Version {v} is not offered.")))?
            }
        };
        let (manifest, raw, url) = self.trust.fetch_manifest(&self.fetcher, &self.cfg.index_url, &release, platform, self.cfg.patcher_version)?;
        Ok(Target { release, manifest, raw, url })
    }

    pub fn plan_target(&self, install: &Install, target: &Target, kind: Kind, progress: ProgressFn) -> Result<Plan> {
        let rt = RemoteTarget { kind, manifest: &target.manifest, raw: &target.raw, url: &target.url };
        txn::plan_remote(install, rt, &self.cfg.policy, matches!(kind, Kind::Repair | Kind::Adopt), &self.cancel, progress)
    }

    pub fn plan_repair(&mut self, install: &Install, progress: ProgressFn) -> Result<Plan> {
        let record: Option<InstalledRecord> = install.read_record().ok().flatten();
        if let Some(r) = record
            && let Ok(m) = install.record_manifest(&r)
        {
            let url = Url::parse(&r.manifest_url).map_err(|e| Error::UnsafeUrl(e.to_string()))?;
            let rt = RemoteTarget { kind: Kind::Repair, manifest: &m, raw: r.manifest.as_bytes(), url: &url };
            return txn::plan_remote(install, rt, &self.cfg.policy, true, &self.cancel, progress);
        }
        let target = self.target(None)?;
        self.plan_target(install, &target, Kind::Adopt, progress)
    }

    pub fn plan_rollback(&self, install: &Install, progress: ProgressFn) -> Result<Plan> {
        txn::plan_rollback(install, &self.cfg.policy, &self.cancel, progress)
    }

    pub fn stage(&self, install: &Install, plan: &Plan, progress: ProgressFn) -> Result<()> {
        txn::stage(install, plan, Some(&self.fetcher), &self.cancel, progress)
    }

    pub fn activate(&self, install: &Install, progress: ProgressFn) -> Result<()> {
        txn::activate(install, &self.cfg.options, progress)
    }

    pub fn execute(&self, install: &Install, plan: &Plan, progress: ProgressFn) -> Result<()> {
        if plan.is_noop() {
            return Ok(());
        }
        self.stage(install, plan, progress)?;
        self.activate(install, progress)
    }
}

pub fn kind_for(install: &Install, target_version: &str) -> Kind {
    match install.read_record().ok().flatten() {
        None if install.looks_installed() => Kind::Adopt,
        None => Kind::Install,
        Some(r) => match (Version::parse(&r.version), Version::parse(target_version)) {
            (Ok(a), Ok(b)) if b > a => Kind::Update,
            (Ok(a), Ok(b)) if b == a => Kind::Repair,
            _ => Kind::Switch,
        },
    }
}
