use crate::config::{self, Settings};
use crate::worker::{self, CheckInfo, Ctx, Done, FailKind, Failure, Job, Msg, Status, View};
use eframe::egui::{self, Align, Color32, Layout, RichText, Vec2};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use svxdist::install::Install;
use svxdist::paths::STATE_DIR;
use svxdist::platform::Platform;
use svxdist::txn::{Phase, Progress};
use svxdist::util::human_bytes;

const ACCENT: Color32 = Color32::from_rgb(70, 130, 240);
const WARN: Color32 = Color32::from_rgb(230, 160, 60);
const BAD: Color32 = Color32::from_rgb(225, 85, 85);
const GOOD: Color32 = Color32::from_rgb(90, 190, 120);

pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("SVX Patcher").with_inner_size([620.0, 470.0]).with_min_inner_size([500.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native("SVX Patcher", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    Install,
    Locate,
}

enum Screen {
    Loading,
    Main,
    Busy(String),
    Versions,
    ConfirmDowngrade(String),
    Picker(Pick),
    Error(Failure),
    Launched,
    Changelog,
}

struct Picker {
    text: String,
    dirs: Vec<String>,
    error: Option<String>,
}

struct App {
    egui: egui::Context,
    dir: PathBuf,
    status: Option<Status>,
    check: Option<Result<CheckInfo, Failure>>,
    checking: bool,
    screen: Screen,
    progress: Option<Progress>,
    waiting: Option<Vec<u32>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    picker: Picker,
    selected: Option<String>,
    last_job: Option<Job>,
    info: Option<(String, Color32)>,
    close_at: Option<Instant>,
    running: Vec<u32>,
    running_at: Option<Instant>,
    running_busy: bool,
    confirm_close: bool,
    closing: bool,
}

fn install_target(path: &Path) -> PathBuf {
    let looks_svx = [Platform::current().exe_name(), "Core", "Content", STATE_DIR].iter().any(|n| path.join(n).exists());
    let empty = fs::read_dir(path).map(|mut d| d.next().is_none()).unwrap_or(true);
    if path.exists() && !empty && !looks_svx { path.join("SVX") } else { path.to_path_buf() }
}

fn task_manager() -> &'static str {
    if cfg!(windows) { "Task Manager" } else { "your system monitor" }
}

fn danger(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).size(16.0).strong().color(Color32::WHITE)).fill(BAD).min_size(Vec2::new(160.0, 36.0))
}

fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).size(19.0).strong().color(Color32::WHITE)).fill(ACCENT).min_size(Vec2::new(200.0, 44.0))
}

fn secondary(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).size(15.0)).min_size(Vec2::new(0.0, 32.0))
}

impl App {
    fn new(ctx: &egui::Context) -> App {
        ctx.set_theme(egui::Theme::Dark);
        ctx.all_styles_mut(|s| {
            s.visuals.selection.bg_fill = ACCENT;
            s.spacing.item_spacing = Vec2::new(10.0, 10.0);
            s.spacing.button_padding = Vec2::new(14.0, 6.0);
        });
        let (tx, rx) = mpsc::channel();
        let dir = Settings::load().install_dir();
        let mut app = App {
            egui: ctx.clone(),
            dir: dir.clone(),
            status: None,
            check: None,
            checking: false,
            screen: Screen::Loading,
            progress: None,
            waiting: None,
            tx,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            picker: Picker { text: String::new(), dirs: Vec::new(), error: None },
            selected: None,
            last_job: None,
            info: None,
            close_at: None,
            running: Vec::new(),
            running_at: None,
            running_busy: false,
            confirm_close: false,
            closing: false,
        };
        app.background(Job::Detect { dir });
        app.start_check();
        app
    }

    fn ctx(&self, cancel: Arc<AtomicBool>) -> Ctx {
        let egui = self.egui.clone();
        Ctx { tx: self.tx.clone(), cancel, notify: Arc::new(move || egui.request_repaint()) }
    }

    fn background(&mut self, job: Job) {
        worker::spawn(job, self.ctx(Arc::new(AtomicBool::new(false))));
    }

    fn start_check(&mut self) {
        self.checking = true;
        self.background(Job::Check);
    }

    fn start(&mut self, job: Job, title: &str) {
        self.cancel = Arc::new(AtomicBool::new(false));
        self.progress = None;
        self.waiting = None;
        self.info = None;
        self.last_job = Some(job.clone());
        self.screen = Screen::Busy(title.to_string());
        worker::spawn(job, self.ctx(self.cancel.clone()));
    }

    fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(s) => {
                    self.status = Some(s);
                    if matches!(self.screen, Screen::Loading) {
                        self.screen = Screen::Main;
                    }
                }
                Msg::Checked(r) => {
                    self.checking = false;
                    self.check = Some(r);
                }
                Msg::Progress(p) => {
                    self.waiting = None;
                    self.progress = Some(p);
                }
                Msg::Waiting(p) => self.waiting = Some(p),
                Msg::Running(p) => {
                    self.running_busy = false;
                    self.running = p;
                }
                Msg::ForceClosed(r) => {
                    self.closing = false;
                    self.info = Some(match r {
                        Ok(c) if c.remaining > 0 => (format!("SVX did not close. Close it from {}.", task_manager()), BAD),
                        Ok(c) if c.closed > 1 => (format!("Closed {} copies of SVX.", c.closed), GOOD),
                        Ok(c) if c.closed == 1 => ("Closed SVX.".into(), GOOD),
                        Ok(_) => ("SVX was not running.".into(), GOOD),
                        Err(f) => (f.message, BAD),
                    });
                }
                Msg::Finished(r) => {
                    self.progress = None;
                    self.waiting = None;
                    match r {
                        Ok(Done::Launched) | Ok(Done::Applied { launched: true, .. }) => {
                            self.screen = Screen::Launched;
                            self.close_at = Some(Instant::now() + Duration::from_millis(1500));
                        }
                        Ok(Done::Applied { version, .. }) => {
                            self.screen = Screen::Main;
                            self.info = Some((version.map(|v| format!("SVX {v} is ready.")).unwrap_or_else(|| "Done.".into()), GOOD));
                        }
                        Ok(Done::PatcherUpdated) => self.egui.send_viewport_cmd(egui::ViewportCommand::Close),
                        Err(f) if f.kind == FailKind::Cancelled => {
                            self.screen = Screen::Main;
                            self.info = Some(("Cancelled. Downloaded parts are kept and will resume.".into(), WARN));
                        }
                        Err(f) => self.screen = Screen::Error(f),
                    }
                }
            }
        }
    }

    fn installed_version(&self) -> Option<String> {
        match &self.status.as_ref()?.view {
            View::Managed { version, .. } => Some(version.clone()),
            _ => None,
        }
    }

    fn latest(&self) -> Option<&svxdist::meta::ReleaseRef> {
        self.check.as_ref()?.as_ref().ok()?.latest.as_ref()
    }

    fn open_picker(&mut self, purpose: Pick) {
        self.picker.text = self.dir.display().to_string();
        self.picker.error = None;
        self.refresh_picker();
        self.screen = Screen::Picker(purpose);
    }

    fn refresh_picker(&mut self) {
        let base = existing_ancestor(Path::new(&self.picker.text));
        let mut dirs: Vec<String> = fs::read_dir(&base)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort_by_key(|d| d.to_lowercase());
        self.picker.dirs = dirs;
    }

    fn choose(&mut self, path: PathBuf, purpose: Pick) {
        let path = if purpose == Pick::Install { install_target(&path) } else { path };
        self.dir = path.clone();
        let mut s = Settings::load();
        s.install_dir = Some(path.clone());
        s.save();
        self.status = None;
        self.screen = Screen::Loading;
        self.background(Job::Detect { dir: path });
    }

    fn header(&self, ui: &mut egui::Ui) {
        if config::test_build() {
            egui::Frame::new().fill(BAD).inner_margin(6.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("TEST BUILD - trusts publicly known test keys. Not for distribution.").color(Color32::WHITE).strong());
            });
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("SVX").size(30.0).strong().color(ACCENT));
            ui.label(RichText::new("Patcher").size(22.0).weak());
        });
        if let Some(Ok(c)) = &self.check
            && let Some(v) = &c.patcher_update
        {
            ui.horizontal(|ui| {
                ui.colored_label(WARN, format!("SVX Patcher {v} is available."));
            });
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Folder: {}", self.dir.display())).small().weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("Patcher {}", config::VERSION)).small().weak());
            });
        });
        ui.separator();
    }

    fn version_line(&mut self, ui: &mut egui::Ui) {
        match &self.check {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Checking for the latest version...");
                });
            }
            Some(Ok(c)) => match &c.latest {
                Some(r) => {
                    ui.label(
                        RichText::new(format!(
                            "Version {}  -  download {}  -  needs {} of space",
                            r.version,
                            human_bytes(r.download_size),
                            human_bytes(r.install_size)
                        ))
                        .size(15.0),
                    );
                }
                None => {
                    ui.colored_label(WARN, "No release is published for this system yet.");
                }
            },
            Some(Err(f)) => {
                let msg = f.message.clone();
                ui.colored_label(WARN, format!("Could not check for releases: {msg}"));
                if ui.add_enabled(!self.checking, secondary("Try again")).clicked() {
                    self.start_check();
                }
            }
        }
    }

    fn main_screen(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.status.clone() else { return };
        if let Some((text, color)) = self.info.clone() {
            ui.colored_label(color, text);
        }
        if let Some(note) = &status.note {
            ui.colored_label(WARN, note);
        }
        if let Some(p) = &status.pending {
            ui.label(RichText::new(p).weak());
        }
        if !matches!(status.view, View::Absent) {
            self.running_banner(ui);
        }
        match &status.view {
            View::Absent => {
                ui.add_space(6.0);
                ui.heading(RichText::new("Install SVX").size(24.0));
                ui.label("Install location");
                ui.horizontal(|ui| {
                    ui.label(RichText::new(self.dir.display().to_string()).monospace());
                    if ui.button("Change...").clicked() {
                        self.open_picker(Pick::Install);
                    }
                });
                self.version_line(ui);
                ui.add_space(8.0);
                let ready = self.latest().is_some();
                if ui.add_enabled(ready, primary("Install and Play")).clicked() {
                    let dir = self.dir.clone();
                    self.start(Job::Apply { dir, version: None, play: true }, "Installing SVX");
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.link("Already have SVX? Locate its folder").clicked() {
                        self.open_picker(Pick::Locate);
                    }
                    if self.has_changelog() && ui.link("Changelog").clicked() {
                        self.screen = Screen::Changelog;
                    }
                });
            }
            View::Unmanaged => {
                ui.heading("SVX found");
                ui.label("This copy is not managed by the patcher yet. Updating lets the patcher keep it current, repair it and roll it back. Content, User, Cache and logs stay as they are.");
                self.version_line(ui);
                ui.horizontal(|ui| {
                    let label = self.latest().map(|r| format!("Update to {}", r.version)).unwrap_or_else(|| "Update".into());
                    if ui.add_enabled(self.latest().is_some(), primary(&label)).clicked() {
                        let dir = self.dir.clone();
                        self.start(Job::Apply { dir, version: None, play: false }, "Updating SVX");
                    }
                    if ui.add(secondary("Play")).clicked() {
                        let dir = self.dir.clone();
                        self.start(Job::Play { dir }, "Starting SVX");
                    }
                });
                self.secondary_row(ui, &status, false);
            }
            View::Damaged(e) => {
                ui.heading("SVX needs repair");
                ui.colored_label(BAD, format!("The installation record is damaged ({e})."));
                if ui.add(primary("Repair")).clicked() {
                    let dir = self.dir.clone();
                    self.start(Job::Repair { dir }, "Repairing SVX");
                }
                self.secondary_row(ui, &status, false);
            }
            View::Managed { version, build, health } => {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new(format!("SVX {version}")).size(26.0));
                    ui.label(RichText::new(format!("build {build}")).weak());
                });
                if let Some(h) = health {
                    egui::Frame::new().stroke(egui::Stroke::new(1.0, BAD)).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.colored_label(BAD, format!("{h}. Repair restores the game files without touching your Content or saves."));
                    });
                }
                match &self.check {
                    Some(Ok(c)) => match &c.latest {
                        Some(r) if worker::newer(&r.version, version) => {
                            let text = format!("Version {} is available ({} to download at most).", r.version, human_bytes(r.download_size));
                            let has_notes = !c.changelog.is_empty();
                            let mut open = false;
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(ACCENT, text);
                                open = has_notes && ui.link("What's new").clicked();
                            });
                            if open {
                                self.screen = Screen::Changelog;
                            }
                        }
                        Some(_) => {
                            ui.colored_label(GOOD, "Up to date.");
                        }
                        None => {}
                    },
                    Some(Err(f)) => {
                        ui.colored_label(WARN, format!("Could not check for updates: {}", f.message));
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Checking for updates...");
                        });
                    }
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if health.is_some() {
                        if ui.add(primary("Repair")).clicked() {
                            let dir = self.dir.clone();
                            self.start(Job::Repair { dir }, "Repairing SVX");
                        }
                    } else if let Some(r) = self.latest().filter(|r| worker::newer(&r.version, version)).cloned() {
                        if ui.add(primary(&format!("Update to {}", r.version))).clicked() {
                            let dir = self.dir.clone();
                            self.start(Job::Apply { dir, version: Some(r.version), play: false }, "Updating SVX");
                        }
                        if ui.add(secondary("Play")).clicked() {
                            let dir = self.dir.clone();
                            self.start(Job::Play { dir }, "Starting SVX");
                        }
                    } else if ui.add(primary("Play")).clicked() {
                        let dir = self.dir.clone();
                        self.start(Job::Play { dir }, "Starting SVX");
                    }
                });
                self.secondary_row(ui, &status, health.is_none());
            }
        }
    }

    fn secondary_row(&mut self, ui: &mut egui::Ui, status: &Status, offer_repair: bool) {
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(!self.checking, secondary("Check for updates")).clicked() {
                self.info = None;
                self.start_check();
            }
            if offer_repair && ui.add(secondary("Repair")).clicked() {
                let dir = self.dir.clone();
                self.start(Job::Repair { dir }, "Repairing SVX");
            }
            if ui.add(secondary("Versions")).clicked() {
                self.selected = self.installed_version();
                if self.check.as_ref().is_none_or(|c| c.is_err()) {
                    self.start_check();
                }
                self.screen = Screen::Versions;
            }
            if self.has_changelog() && ui.add(secondary("Changelog")).clicked() {
                self.screen = Screen::Changelog;
            }
            if let Some(label) = &status.rollback
                && ui.add(secondary(&format!("Restore {label}"))).on_hover_text("Put back the files from before the last update").clicked()
            {
                let dir = self.dir.clone();
                self.start(Job::Rollback { dir }, "Restoring the previous version");
            }
        });
        if ui.link("Use a different folder").clicked() {
            self.open_picker(Pick::Locate);
        }
        if let Some(Ok(c)) = &self.check
            && c.patcher_update.is_some()
            && ui.add(secondary("Update the patcher")).clicked()
        {
            self.start(Job::SelfUpdate, "Updating SVX Patcher");
        }
    }

    fn has_changelog(&self) -> bool {
        matches!(&self.check, Some(Ok(c)) if !c.changelog.is_empty())
    }

    fn running_banner(&mut self, ui: &mut egui::Ui) {
        if self.running.is_empty() {
            return;
        }
        let n = self.running.len();
        egui::Frame::new().stroke(egui::Stroke::new(1.0, WARN)).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let label = if n > 1 { format!("{n} copies of SVX are running.") } else { "SVX is running.".to_string() };
            egui::Sides::new().height(32.0).show(
                ui,
                |ui| ui.colored_label(WARN, label),
                |ui| {
                    if ui.add_enabled(!self.closing, secondary(if self.closing { "Closing..." } else { "Force close" })).clicked() {
                        self.confirm_close = true;
                    }
                },
            );
        });
    }

    fn close_dialog(&mut self, ctx: &egui::Context) {
        let n = self.running.len();
        let exe = Install::new(&self.dir, Platform::current()).exe();
        let modal = egui::Modal::new(egui::Id::new("force_close")).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.heading(if n > 1 { format!("Force close {n} copies of SVX?") } else { "Force close SVX?".to_string() });
            ui.add_space(4.0);
            egui::Frame::new().stroke(egui::Stroke::new(1.0, WARN)).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.colored_label(WARN, RichText::new("Save your progress in SVX first.").strong());
                ui.label("Force closing ends the game immediately. Scenes, poses and settings that were not saved are lost.");
            });
            ui.label(RichText::new("Use this when SVX is stuck or keeps running in the background after its window closed.").weak());
            ui.label(RichText::new(format!("Only SVX started from this folder is closed:\n{}", exe.display())).small().weak());
            ui.add_space(8.0);
            let mut close = false;
            ui.horizontal(|ui| {
                if ui.add(danger("Force close")).clicked() {
                    self.closing = true;
                    let dir = self.dir.clone();
                    self.background(Job::ForceClose { dir });
                    close = true;
                }
                if ui.add(secondary("Cancel")).clicked() {
                    close = true;
                }
            });
            close
        });
        if modal.inner || modal.should_close() {
            self.confirm_close = false;
        }
    }

    fn poll_running(&mut self, ctx: &egui::Context) {
        if !matches!(self.screen, Screen::Main | Screen::Busy(_)) || self.status.is_none() {
            return;
        }
        let every = Duration::from_millis(1500);
        if !self.running_busy && self.running_at.is_none_or(|t| t.elapsed() >= every) {
            self.running_busy = true;
            self.running_at = Some(Instant::now());
            let dir = self.dir.clone();
            self.background(Job::Running { dir });
        }
        ctx.request_repaint_after(every);
    }

    fn changelog_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Changelog");
        let installed = self.installed_version();
        let latest = self.latest().map(|r| r.version.clone());
        let entries = match &self.check {
            Some(Ok(c)) => c.changelog.clone(),
            _ => Vec::new(),
        };
        let height = (ui.available_height() - 70.0).max(120.0);
        egui::ScrollArea::vertical().max_height(height).auto_shrink([false, true]).show(ui, |ui| {
            for e in &entries {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&e.version).size(19.0).strong());
                    if !e.date.is_empty() {
                        ui.label(RichText::new(&e.date).weak());
                    }
                    if Some(&e.version) == latest.as_ref() {
                        ui.colored_label(ACCENT, "latest");
                    }
                    if Some(&e.version) == installed.as_ref() {
                        ui.colored_label(GOOD, "installed");
                    } else if installed.as_ref().is_some_and(|i| worker::newer(&e.version, i)) {
                        ui.colored_label(ACCENT, "new");
                    }
                });
                for s in &e.sections {
                    if !s.title.is_empty() {
                        ui.label(RichText::new(&s.title).strong().color(ACCENT));
                    }
                    for item in &s.items {
                        ui.horizontal_top(|ui| {
                            ui.add_space(8.0);
                            ui.label("•");
                            ui.add(egui::Label::new(item).wrap());
                        });
                    }
                }
                ui.separator();
            }
        });
        if ui.add(secondary("Back")).clicked() {
            self.screen = Screen::Main;
        }
    }

    fn versions_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Versions");
        let installed = self.installed_version();
        let releases = match &self.check {
            Some(Ok(c)) => c.releases.clone(),
            Some(Err(f)) => {
                ui.colored_label(WARN, format!("Could not load versions: {}", f.message));
                Vec::new()
            }
            None => {
                ui.spinner();
                Vec::new()
            }
        };
        let latest = self.latest().map(|r| r.version.clone());
        egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
            for r in &releases {
                let mut tags = Vec::new();
                if Some(&r.version) == latest.as_ref() {
                    tags.push("latest");
                }
                if Some(&r.version) == installed.as_ref() {
                    tags.push("installed");
                }
                let tag = if tags.is_empty() { String::new() } else { format!("  ({})", tags.join(", ")) };
                let text = format!("{}{tag}  -  build {}  -  {}  -  {}", r.version, r.build, r.date.get(..10).unwrap_or(&r.date), human_bytes(r.download_size));
                ui.radio_value(&mut self.selected, Some(r.version.clone()), text);
                if !r.notes.is_empty() {
                    ui.label(RichText::new(&r.notes).small().weak());
                }
            }
        });
        ui.horizontal(|ui| {
            let chosen = self.selected.clone();
            let label = match (&chosen, &installed) {
                (Some(c), Some(i)) if c == i => "Reinstall".to_string(),
                (Some(c), _) => format!("Install {c}"),
                _ => "Install".to_string(),
            };
            if ui.add_enabled(chosen.is_some(), primary(&label)).clicked() {
                let c = chosen.expect("selected");
                match &installed {
                    Some(i) if worker::newer(i, &c) => self.screen = Screen::ConfirmDowngrade(c),
                    _ => {
                        let dir = self.dir.clone();
                        self.start(Job::Apply { dir, version: Some(c), play: false }, "Installing SVX");
                    }
                }
            }
            if ui.add(secondary("Back")).clicked() {
                self.screen = Screen::Main;
            }
        });
    }

    fn downgrade_screen(&mut self, ui: &mut egui::Ui, version: String) {
        ui.heading(format!("Switch to the older version {version}?"));
        ui.add_space(4.0);
        ui.label("Older versions may not read settings, saved poses, scenes or content that a newer version created or changed. They could fail to load or be shown incorrectly.");
        ui.label("The patcher does not change your Content, User, Cache or logs folders. Back them up if you want to be safe, especially User.");
        ui.label("You can return to the latest version at any time from Versions, or with Restore.");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new(format!("Switch to {version}")).size(17.0).strong().color(Color32::WHITE))
                        .fill(WARN)
                        .min_size(Vec2::new(200.0, 40.0)),
                )
                .clicked()
            {
                let dir = self.dir.clone();
                self.start(Job::Apply { dir, version: Some(version.clone()), play: false }, &format!("Switching to {version}"));
            }
            if ui.add(secondary("Cancel")).clicked() {
                self.screen = Screen::Versions;
            }
        });
    }

    fn busy_screen(&mut self, ui: &mut egui::Ui, title: String) {
        ui.heading(title);
        ui.add_space(8.0);
        if let Some(pids) = &self.waiting {
            egui::Frame::new().stroke(egui::Stroke::new(1.0, WARN)).inner_margin(8.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.colored_label(WARN, format!("SVX is running (process {pids:?}). Close the game; the update continues on its own."));
                if !self.running.is_empty() && ui.add_enabled(!self.closing, secondary(if self.closing { "Closing..." } else { "Force close SVX" })).clicked() {
                    self.confirm_close = true;
                }
            });
        }
        let p = self.progress.clone().unwrap_or_default();
        ui.label(RichText::new(p.phase.label()).size(16.0));
        let frac = if p.total > 0 { p.done as f32 / p.total as f32 } else { 0.0 };
        let text = match p.phase {
            Phase::Downloading => format!("{} of {}", human_bytes(p.done), human_bytes(p.total)),
            _ if p.total > 0 => format!("{} of {}", p.done, p.total),
            _ => String::new(),
        };
        ui.add(egui::ProgressBar::new(frac).text(text).animate(p.total == 0));
        if !p.detail.is_empty() {
            ui.label(RichText::new(&p.detail).small().weak().monospace());
        }
        ui.add_space(8.0);
        let can_cancel = !matches!(p.phase, Phase::Installing | Phase::Finishing);
        if ui.add_enabled(can_cancel && !self.cancel.load(Ordering::Relaxed), secondary("Cancel")).clicked() {
            self.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn error_screen(&mut self, ui: &mut egui::Ui, f: Failure) {
        ui.heading(match f.kind {
            FailKind::Security => "Release check failed",
            FailKind::NotWritable => "Cannot write to this folder",
            FailKind::PatcherTooOld => "Patcher update needed",
            FailKind::Damaged => "SVX could not start",
            _ => "Something went wrong",
        });
        ui.add_space(4.0);
        ui.add(egui::Label::new(RichText::new(&f.message).size(15.0)).selectable(true));
        if f.kind == FailKind::Security {
            ui.label(RichText::new("Nothing was changed. The download did not pass its signature or safety checks; try again later.").weak());
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if f.retry
                && let Some(job) = self.last_job.clone()
                && ui.add(primary("Retry")).clicked()
            {
                self.start(job, "Retrying");
            }
            if f.kind == FailKind::NotWritable && ui.add(primary("Choose another folder")).clicked() {
                self.dir = svxdist::dirs::default_install_dir();
                self.open_picker(Pick::Install);
                return;
            }
            if f.kind == FailKind::Damaged && ui.add(primary("Repair")).clicked() {
                let dir = self.dir.clone();
                self.start(Job::Repair { dir }, "Repairing SVX");
                return;
            }
            if f.kind == FailKind::PatcherTooOld && ui.add(primary("Update the patcher")).clicked() {
                self.start(Job::SelfUpdate, "Updating SVX Patcher");
                return;
            }
            if ui.add(secondary("Back")).clicked() {
                self.screen = Screen::Main;
                let dir = self.dir.clone();
                self.background(Job::Detect { dir });
            }
        });
    }

    fn picker_screen(&mut self, ui: &mut egui::Ui, purpose: Pick) {
        ui.heading(if purpose == Pick::Install { "Choose where to install SVX" } else { "Locate your SVX folder" });
        ui.label(
            RichText::new(if purpose == Pick::Install {
                "Pick a folder you own. If it already holds other files, SVX goes into an SVX subfolder."
            } else {
                "Pick the folder that contains the SVX game, Core and Content."
            })
            .weak(),
        );
        let resp = ui.add(egui::TextEdit::singleline(&mut self.picker.text).desired_width(f32::INFINITY));
        if resp.changed() {
            self.refresh_picker();
        }
        ui.horizontal(|ui| {
            if ui.button("Up").clicked()
                && let Some(parent) = existing_ancestor(Path::new(&self.picker.text)).parent()
            {
                self.picker.text = parent.display().to_string();
                self.refresh_picker();
            }
            if ui.button("Home").clicked() {
                self.picker.text = svxdist::dirs::default_install_dir().parent().and_then(|p| p.parent()).unwrap_or(Path::new("/")).display().to_string();
                self.refresh_picker();
            }
            if ui.button("Default location").clicked() {
                self.picker.text = svxdist::dirs::default_install_dir().display().to_string();
                self.refresh_picker();
            }
        });
        let base = existing_ancestor(Path::new(&self.picker.text));
        let mut go = None;
        egui::ScrollArea::vertical().max_height(170.0).show(ui, |ui| {
            for d in &self.picker.dirs {
                if ui.selectable_label(false, format!("[{d}]")).clicked() {
                    go = Some(base.join(d));
                }
            }
        });
        if let Some(p) = go {
            self.picker.text = p.display().to_string();
            self.refresh_picker();
        }
        if let Some(e) = &self.picker.error {
            ui.colored_label(BAD, e);
        }
        ui.horizontal(|ui| {
            let path = PathBuf::from(self.picker.text.trim());
            let label = if purpose == Pick::Install { format!("Install to {}", install_target(&path).display()) } else { "Use this folder".to_string() };
            if ui.add(primary(&label)).clicked() {
                if !path.is_absolute() {
                    self.picker.error = Some("Enter a full folder path.".into());
                } else if purpose == Pick::Locate && !path.join(Platform::current().exe_name()).exists() && !path.join("Core").exists() {
                    self.picker.error = Some("No SVX game was found in that folder.".into());
                } else {
                    self.choose(path, purpose);
                }
            }
            if ui.add(secondary("Cancel")).clicked() {
                self.screen = if self.status.is_some() { Screen::Main } else { Screen::Loading };
            }
        });
    }
}

fn existing_ancestor(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    while !cur.is_dir() {
        match cur.parent() {
            Some(parent) => cur = parent.to_path_buf(),
            None => return PathBuf::from("/"),
        }
    }
    cur
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        self.poll_running(ui.ctx());
        if let Some(t) = self.close_at {
            if Instant::now() >= t {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::central_panel(ui.style()).inner_margin(18.0)).show(ui, |ui| {
            self.header(ui);
            ui.add_space(4.0);
            match &self.screen {
                Screen::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Looking for SVX...");
                    });
                }
                Screen::Main => self.main_screen(ui),
                Screen::Busy(t) => {
                    let t = t.clone();
                    self.busy_screen(ui, t)
                }
                Screen::Versions => self.versions_screen(ui),
                Screen::ConfirmDowngrade(v) => {
                    let v = v.clone();
                    self.downgrade_screen(ui, v)
                }
                Screen::Picker(p) => {
                    let p = *p;
                    self.picker_screen(ui, p)
                }
                Screen::Error(f) => {
                    let f = f.clone();
                    self.error_screen(ui, f)
                }
                Screen::Launched => {
                    ui.heading("SVX is starting...");
                    ui.label("Have fun. The patcher closes now.");
                }
                Screen::Changelog => self.changelog_screen(ui),
            }
            ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| self.footer(ui));
        });
        if self.confirm_close {
            if self.running.is_empty() {
                self.confirm_close = false;
            } else {
                let ctx = ui.ctx().clone();
                self.close_dialog(&ctx);
            }
        }
    }
}
