#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod cli;
mod config;
mod worker;

fn attach_console() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Ok(exe) = std::env::current_exe() {
        svxdist::selfupdate::cleanup(&exe);
    }
    if args.first().map(|a| a.as_str()) == Some("--cli") {
        attach_console();
        std::process::exit(cli::main(&args[1..]));
    }
    if args.first().map(|a| a.as_str()) == Some("--version") {
        attach_console();
        println!("SVX Patcher {}{}", config::VERSION, if config::test_build() { " (test keys)" } else { "" });
        return;
    }
    if let Err(e) = app::run() {
        attach_console();
        eprintln!("SVX Patcher could not open its window: {e}");
        std::process::exit(1);
    }
}
