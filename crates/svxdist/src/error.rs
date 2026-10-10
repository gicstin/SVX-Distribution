use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum Error {
    Io { context: String, source: io::Error },
    NotWritable { path: PathBuf, source: io::Error },
    Network(String),
    NotFound(String),
    Http { url: String, status: u16 },
    UnsafeUrl(String),
    TooLarge(String),
    Metadata(String),
    Signature(String),
    Expired(String),
    Rollback(String),
    Platform(String),
    UnsafePath(String),
    Hash(String),
    GameRunning(Vec<u32>),
    PatcherTooOld { required: String, current: String },
    Busy,
    Cancelled,
    Interrupted(String),
    Install(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io(context: impl Into<String>, source: io::Error) -> Error {
        Error::Io { context: context.into(), source }
    }

    pub fn io_path(action: &str, path: &Path, source: io::Error) -> Error {
        Error::Io { context: format!("{action} {}", path.display()), source }
    }

    pub fn is_disk_full(&self) -> bool {
        match self {
            Error::Io { source, .. } | Error::NotWritable { source, .. } => is_disk_full(source),
            _ => false,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Network(_) | Error::Http { .. } | Error::GameRunning(_) | Error::Busy | Error::Cancelled | Error::Hash(_))
    }

    pub fn is_security(&self) -> bool {
        matches!(self, Error::Signature(_) | Error::Expired(_) | Error::Rollback(_) | Error::UnsafeUrl(_) | Error::UnsafePath(_) | Error::Platform(_))
    }
}

pub fn is_disk_full(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(28) | Some(112) | Some(39)) || e.kind() == io::ErrorKind::StorageFull
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { context, source } if is_disk_full(source) => write!(f, "Not enough disk space ({context})"),
            Error::Io { context, source } => write!(f, "{context}: {source}"),
            Error::NotWritable { path, source } => {
                write!(f, "Cannot write to {} ({source}). Choose a folder you own, such as one inside your home folder.", path.display())
            }
            Error::Network(m) => write!(f, "Network error: {m}"),
            Error::NotFound(u) => write!(f, "Not found on the server: {u}"),
            Error::Http { url, status } => write!(f, "Server answered {status} for {url}"),
            Error::UnsafeUrl(m) => write!(f, "Refused unsafe address: {m}"),
            Error::TooLarge(m) => write!(f, "Refused oversized response: {m}"),
            Error::Metadata(m) => write!(f, "Invalid release metadata: {m}"),
            Error::Signature(m) => write!(f, "Release metadata failed signature verification: {m}"),
            Error::Expired(m) => write!(f, "Release metadata has expired: {m}"),
            Error::Rollback(m) => write!(f, "Refused older release metadata than already seen: {m}"),
            Error::Platform(m) => write!(f, "Release does not match this system: {m}"),
            Error::UnsafePath(m) => write!(f, "Refused unsafe file path: {m}"),
            Error::Hash(m) => write!(f, "Downloaded data failed verification: {m}"),
            Error::GameRunning(pids) => write!(f, "SVX is running (process {}). Close the game to continue.", join_pids(pids)),
            Error::PatcherTooOld { required, current } => {
                write!(f, "This release needs SVX Patcher {required} or newer (this is {current}). Update the patcher first.")
            }
            Error::Busy => write!(f, "Another SVX Patcher is working on this installation."),
            Error::Cancelled => write!(f, "Cancelled."),
            Error::Interrupted(m) => write!(f, "Interrupted: {m}"),
            Error::Install(m) => write!(f, "{m}"),
        }
    }
}

fn join_pids(pids: &[u32]) -> String {
    pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
}

impl std::error::Error for Error {}
