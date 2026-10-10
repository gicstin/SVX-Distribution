use crate::error::{Error, Result};
use crate::install::Install;
use crate::util::is_link;
use std::fs;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub enum Outcome {
    Running,
    Exited(Option<i32>),
}

pub fn launch(install: &Install, args: &[String]) -> Result<Child> {
    let exe = install.exe();
    let meta = fs::symlink_metadata(&exe).map_err(|e| Error::io_path("find", &exe, e))?;
    if is_link(&meta) || !meta.is_file() {
        return Err(Error::UnsafePath(format!("{} is not a regular file", exe.display())));
    }
    let mut cmd = Command::new(&exe);
    cmd.args(args).current_dir(&install.root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0200 | 0x0000_0008);
    }
    cmd.spawn().map_err(|e| Error::io_path("start", &exe, e))
}

pub fn watch(child: &mut Child, wait: Duration) -> Outcome {
    let start = Instant::now();
    while start.elapsed() < wait {
        match child.try_wait() {
            Ok(Some(status)) => return Outcome::Exited(status.code()),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return Outcome::Running,
        }
    }
    Outcome::Running
}
