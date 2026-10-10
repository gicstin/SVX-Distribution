use crate::config::{self, Settings};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;
use svxdist::install::{Detected, Install};
use svxdist::launch::{self, Outcome};
use svxdist::meta::ReleaseRef;
use svxdist::platform::Platform;
use svxdist::session::{self, Session};
use svxdist::txn::{self, Plan, Progress, Recovery};
use svxdist::version::Version;
use svxdist::{Error, Result, selfupdate};

#[derive(Clone, Debug)]
pub enum Job {
    Detect { dir: PathBuf },
    Check,
    Apply { dir: PathBuf, version: Option<String>, play: bool },
    Repair { dir: PathBuf },
    Rollback { dir: PathBuf },
    Play { dir: PathBuf },
    SelfUpdate,
}

#[derive(Clone, Debug)]
pub enum View {
    Absent,
    Unmanaged,
    Managed { version: String, build: u64, health: Option<String> },
    Damaged(String),
}

#[derive(Clone, Debug)]
pub struct Status {
    pub dir: PathBuf,
    pub view: View,
    pub note: Option<String>,
    pub rollback: Option<String>,
    pub pending: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CheckInfo {
    pub releases: Vec<ReleaseRef>,
    pub latest: Option<ReleaseRef>,
    pub patcher_update: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    Generic,
    NotWritable,
    PatcherTooOld,
    Cancelled,
    Damaged,
    Security,
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub message: String,
    pub kind: FailKind,
    pub retry: bool,
}

#[derive(Clone, Debug)]
pub enum Done {
    Applied { version: Option<String>, launched: bool },
    Launched,
    PatcherUpdated,
}

pub enum Msg {
    Status(Status),
    Checked(std::result::Result<CheckInfo, Failure>),
    Progress(Progress),
    Waiting(Vec<u32>),
    Finished(std::result::Result<Done, Failure>),
}

pub fn failure(e: Error) -> Failure {
    let kind = match &e {
        Error::NotWritable { .. } => FailKind::NotWritable,
        Error::PatcherTooOld { .. } => FailKind::PatcherTooOld,
        Error::Cancelled => FailKind::Cancelled,
        e if e.is_security() => FailKind::Security,
        _ => FailKind::Generic,
    };
    let retry = e.is_retryable() || e.is_disk_full() || matches!(e, Error::Io { .. } | Error::NotFound(_));
    Failure { message: e.to_string(), kind, retry }
}

pub struct Ctx {
    pub tx: Sender<Msg>,
    pub cancel: Arc<AtomicBool>,
    pub notify: Arc<dyn Fn() + Send + Sync>,
}

impl Ctx {
    fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
        (self.notify)();
    }
}

pub fn spawn(job: Job, ctx: Ctx) {
    std::thread::spawn(move || run(job, &ctx));
}

pub fn run(job: Job, ctx: &Ctx) {
    match job {
        Job::Detect { dir } => {
            let s = detect(&dir, ctx);
            ctx.send(Msg::Status(s));
        }
        Job::Check => {
            let r = check(ctx).map_err(failure);
            ctx.send(Msg::Checked(r));
        }
        Job::Apply { dir, version, play } => {
            let r = apply(&dir, version.as_deref(), play, ctx);
            finish(&dir, r, ctx);
        }
        Job::Repair { dir } => {
            let r = with_session(ctx, |s| run_plan(&dir, s, ctx, &mut |s, i, p| s.plan_repair(i, p))).map(|_| Done::Applied { version: None, launched: false });
            finish(&dir, r, ctx);
        }
        Job::Rollback { dir } => {
            let r =
                with_session(ctx, |s| run_plan(&dir, s, ctx, &mut |s, i, p| s.plan_rollback(i, p))).map(|_| Done::Applied { version: None, launched: false });
            finish(&dir, r, ctx);
        }
        Job::Play { dir } => {
            let r = play(&dir, ctx);
            ctx.send(Msg::Finished(r));
        }
        Job::SelfUpdate => {
            let r = self_update(ctx).map_err(failure);
            ctx.send(Msg::Finished(r));
        }
    }
}

fn finish(dir: &Path, r: std::result::Result<Done, Failure>, ctx: &Ctx) {
    let s = detect(dir, ctx);
    ctx.send(Msg::Status(s));
    ctx.send(Msg::Finished(r));
}

fn new_session(ctx: &Ctx) -> Result<Session> {
    Session::new(config::session_config()?, ctx.cancel.clone())
}

fn with_session<T>(ctx: &Ctx, f: impl FnOnce(&mut Session) -> Result<T>) -> std::result::Result<T, Failure> {
    new_session(ctx).and_then(|mut s| f(&mut s)).map_err(failure)
}

pub fn detect(dir: &Path, ctx: &Ctx) -> Status {
    let install = Install::new(dir, Platform::current());
    let mut note = None;
    let mut pending = None;
    if install.txn_dir().exists() {
        match install.lock() {
            Ok(_lock) => {
                let mut quiet = |_p: &Progress| {};
                match txn::recover(&install, &ctx.cancel, &mut quiet) {
                    Ok(Recovery::RolledForward { version, .. }) => {
                        note = Some(format!("Finished an interrupted change{}.", version.map(|v| format!(" to {v}")).unwrap_or_default()))
                    }
                    Ok(Recovery::RolledBack { .. }) => note = Some("Undid an interrupted update; the previous version is intact.".into()),
                    Ok(Recovery::Staged { version, .. }) => {
                        pending = version.map(|v| format!("A download for {v} is partly complete and will resume."));
                    }
                    Ok(Recovery::Clean) => {}
                    Err(Error::GameRunning(_)) => note = Some("An interrupted update will be completed once SVX is closed.".into()),
                    Err(e) => note = Some(format!("Could not finish an interrupted update: {e}")),
                }
            }
            Err(Error::Busy) => note = Some("Another SVX Patcher window is working on this folder.".into()),
            Err(_) => {}
        }
    }
    let view = match install.detect() {
        Detected::Absent => View::Absent,
        Detected::Unmanaged => View::Unmanaged,
        Detected::Damaged(e) => View::Damaged(e),
        Detected::Managed(i) => {
            View::Managed { version: i.record.version.clone(), build: i.record.build, health: if i.health.ok() { None } else { Some(i.health.summary()) } }
        }
    };
    let rollback = txn::read_snapshot(&install).filter(|s| s.restorable()).map(|s| s.label());
    Status { dir: install.root.clone(), view, note, rollback, pending }
}

fn check(ctx: &Ctx) -> Result<CheckInfo> {
    let mut s = new_session(ctx)?;
    let platform = s.cfg.platform;
    let idx = s.check()?.clone();
    let patcher_update = selfupdate::available(&idx, s.cfg.patcher_version, platform).map(|(v, _)| v.to_string());
    Ok(CheckInfo { releases: s.releases(), latest: idx.platform(platform).and_then(|p| p.latest_ref()).cloned(), patcher_update })
}

type PlanFn<'a> = dyn FnMut(&mut Session, &Install, &mut dyn FnMut(&Progress)) -> Result<Plan> + 'a;

fn run_plan(dir: &Path, s: &mut Session, ctx: &Ctx, make: &mut PlanFn) -> Result<Option<String>> {
    let install = Install::new(dir, Platform::current());
    let _lock = install.lock()?;
    let mut progress = |p: &Progress| ctx.send(Msg::Progress(p.clone()));
    loop {
        match txn::recover(&install, &ctx.cancel, &mut progress) {
            Err(Error::GameRunning(p)) => wait(ctx, p)?,
            other => {
                other?;
                break;
            }
        }
    }
    let plan = make(s, &install, &mut progress)?;
    let version = plan.next.as_ref().map(|r| r.version.clone());
    if plan.is_noop() {
        return Ok(version);
    }
    s.stage(&install, &plan, &mut progress)?;
    loop {
        match s.activate(&install, &mut progress) {
            Err(Error::GameRunning(p)) => wait(ctx, p)?,
            other => {
                other?;
                break;
            }
        }
    }
    Ok(version)
}

fn wait(ctx: &Ctx, pids: Vec<u32>) -> Result<()> {
    ctx.send(Msg::Waiting(pids));
    for _ in 0..10 {
        if ctx.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

fn apply(dir: &Path, version: Option<&str>, play_after: bool, ctx: &Ctx) -> std::result::Result<Done, Failure> {
    let v = with_session(ctx, |s| {
        run_plan(dir, s, ctx, &mut |s, install, p| {
            let target = s.target(version)?;
            let kind = session::kind_for(install, &target.release.version);
            s.plan_target(install, &target, kind, p)
        })
    })?;
    let mut settings = Settings::load();
    settings.install_dir = Some(dir.to_path_buf());
    settings.save();
    if play_after {
        play(dir, ctx)?;
    }
    Ok(Done::Applied { version: v, launched: play_after })
}

fn play(dir: &Path, ctx: &Ctx) -> std::result::Result<Done, Failure> {
    let install = Install::new(dir, Platform::current());
    if matches!(txn::read_journal(&install), Ok(Some(j)) if matches!(j.state, txn::TxnState::Activating | txn::TxnState::Committed)) {
        let _lock = install.lock().map_err(failure)?;
        let mut quiet = |_p: &Progress| {};
        txn::recover(&install, &ctx.cancel, &mut quiet).map_err(failure)?;
    }
    match install.detect() {
        Detected::Managed(i) if !i.health.ok() => {
            return Err(Failure { message: format!("{}. Repair fixes this.", i.health.summary()), kind: FailKind::Damaged, retry: false });
        }
        Detected::Absent => return Err(Failure { message: "SVX is not installed in this folder.".into(), kind: FailKind::Damaged, retry: false }),
        _ => {}
    }
    let mut child = launch::launch(&install, &[]).map_err(|e| Failure { message: e.to_string(), kind: FailKind::Damaged, retry: false })?;
    match launch::watch(&mut child, Duration::from_secs(4)) {
        Outcome::Exited(Some(code)) if code != 0 => Err(Failure {
            message: format!(
                "SVX closed right after starting (exit code {code}). Repair can fix damaged game files; logs/latest.log in the game folder has details."
            ),
            kind: FailKind::Damaged,
            retry: false,
        }),
        Outcome::Exited(None) => Err(Failure { message: "SVX stopped right after starting.".into(), kind: FailKind::Damaged, retry: false }),
        _ => Ok(Done::Launched),
    }
}

fn self_update(ctx: &Ctx) -> Result<Done> {
    let mut s = new_session(ctx)?;
    let platform = s.cfg.platform;
    let idx = s.check()?.clone();
    let (_, art) = selfupdate::available(&idx, s.cfg.patcher_version, platform).ok_or_else(|| Error::Install("The patcher is already up to date.".into()))?;
    let exe = std::env::current_exe().map_err(|e| Error::io("locate the patcher", e))?;
    let exe = selfupdate::install(&s.fetcher, &s.cfg.index_url, &art, &exe)?;
    std::process::Command::new(&exe).arg("--after-self-update").spawn().map_err(|e| Error::io_path("start", &exe, e))?;
    Ok(Done::PatcherUpdated)
}

pub fn newer(a: &str, b: &str) -> bool {
    matches!((Version::parse(a), Version::parse(b)), (Ok(x), Ok(y)) if x > y)
}
