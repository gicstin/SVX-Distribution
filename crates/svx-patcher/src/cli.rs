use crate::config::{self, Settings};
use crate::worker::{self, Ctx, Done, Job, Msg, View};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

pub fn main(args: &[String]) -> i32 {
    let cmd = args.first().cloned().unwrap_or_default();
    let dir = arg(args, "--dir").map(PathBuf::from).unwrap_or_else(|| Settings::load().install_dir());
    let version = arg(args, "--version");
    let report = arg(args, "--report").map(PathBuf::from);
    let no_wait = args.iter().any(|a| a == "--no-wait");
    let play = args.iter().any(|a| a == "--play");
    let job = match cmd.as_str() {
        "status" => Job::Detect { dir: dir.clone() },
        "check" => Job::Check,
        "install" => Job::Apply { dir: dir.clone(), version, play },
        "repair" => Job::Repair { dir: dir.clone() },
        "rollback" => Job::Rollback { dir: dir.clone() },
        "play" => Job::Play { dir: dir.clone() },
        "self-update" => Job::SelfUpdate,
        "running" => Job::Running { dir: dir.clone() },
        "close-game" => Job::ForceClose { dir: dir.clone() },
        _ => {
            eprintln!(
                "usage: svx-patcher --cli <status|check|install|repair|rollback|play|self-update|running|close-game> [--dir DIR] [--version V] [--play] [--no-wait] [--report FILE]"
            );
            return 2;
        }
    };
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let ctx = Ctx { tx, cancel: cancel.clone(), notify: Arc::new(|| {}) };
    let handle = std::thread::spawn(move || worker::run(job, &ctx));
    let mut result: Value = json!({ "ok": true, "patcher": config::VERSION, "test_build": config::test_build() });
    let mut last_phase = String::new();
    for msg in rx {
        match msg {
            Msg::Progress(p) => {
                let phase = p.phase.label().to_string();
                if phase != last_phase {
                    println!("phase {phase}");
                    last_phase = phase;
                }
            }
            Msg::Waiting(pids) => {
                println!("waiting game-running {pids:?}");
                result["waited_for_game"] = json!(true);
                if no_wait {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            Msg::Running(pids) => {
                println!("running {pids:?}");
                result["running"] = json!(pids);
            }
            Msg::ForceClosed(r) => match r {
                Ok(c) => {
                    println!("closed {} remaining {}", c.closed, c.remaining);
                    result["closed"] = json!(c.closed);
                    result["remaining"] = json!(c.remaining);
                    if c.remaining > 0 {
                        result["ok"] = json!(false);
                    }
                }
                Err(f) => {
                    println!("error {}", f.message);
                    result["ok"] = json!(false);
                    result["error"] = json!(f.message);
                }
            },
            Msg::Status(s) => {
                let view = match &s.view {
                    View::Absent => json!({ "state": "absent" }),
                    View::Unmanaged => json!({ "state": "unmanaged" }),
                    View::Damaged(e) => json!({ "state": "damaged", "error": e }),
                    View::Managed { version, build, health } => json!({ "state": "managed", "version": version, "build": build, "health": health }),
                };
                println!("status {view}");
                result["status"] = view;
                result["note"] = json!(s.note);
                result["rollback"] = json!(s.rollback);
                result["pending"] = json!(s.pending);
                result["dir"] = json!(s.dir);
            }
            Msg::Checked(r) => match r {
                Ok(c) => {
                    let versions: Vec<String> = c.releases.iter().map(|r| r.version.clone()).collect();
                    println!("latest {:?} offered {versions:?}", c.latest.as_ref().map(|r| &r.version));
                    result["latest"] = json!(c.latest.map(|r| r.version));
                    result["offered"] = json!(versions);
                    result["patcher_update"] = json!(c.patcher_update);
                    result["changelog"] = json!(c.changelog);
                }
                Err(f) => {
                    println!("error {}", f.message);
                    result["ok"] = json!(false);
                    result["error"] = json!(f.message);
                }
            },
            Msg::Finished(r) => match r {
                Ok(d) => {
                    let what = match d {
                        Done::Applied { version, launched } => json!({ "applied": version, "launched": launched }),
                        Done::Launched => json!({ "launched": true }),
                        Done::PatcherUpdated => json!({ "patcher_updated": true }),
                    };
                    println!("done {what}");
                    result["done"] = what;
                }
                Err(f) => {
                    println!("error {}", f.message);
                    result["ok"] = json!(false);
                    result["error"] = json!(f.message);
                    result["error_kind"] = json!(format!("{:?}", f.kind));
                }
            },
        }
    }
    let _ = handle.join();
    if let Some(p) = report {
        let _ = std::fs::write(p, serde_json::to_vec_pretty(&result).unwrap_or_default());
    }
    if result["ok"] == json!(true) { 0 } else { 1 }
}
