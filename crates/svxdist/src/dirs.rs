use std::env;
use std::path::PathBuf;

fn home() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    env::var_os(var).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    if let Some(p) = env::var_os("SVX_PATCHER_CONFIG") {
        return PathBuf::from(p);
    }
    if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home).join("SVX Patcher")
    } else {
        env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home().join(".config")).join("svx-patcher")
    }
}

pub fn default_install_dir() -> PathBuf {
    home().join("Games").join("SVX")
}
