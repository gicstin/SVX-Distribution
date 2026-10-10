use std::io::Write;

fn main() {
    let exe = std::env::current_exe().unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(marker) = std::env::var_os("SVX_FIXTURE_MARKER")
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(marker)
    {
        let _ = writeln!(f, "launched exe={} cwd={}", exe.display(), cwd.display());
    }
    let secs: u64 = std::env::var("SVX_FIXTURE_SLEEP").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let code: i32 = std::env::var("SVX_FIXTURE_EXIT").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    std::process::exit(code);
}
