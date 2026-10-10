use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Platform {
    #[serde(rename = "linux-x86_64")]
    LinuxX86_64,
    #[serde(rename = "windows-x86_64")]
    WindowsX86_64,
}

impl Platform {
    pub const ALL: [Platform; 2] = [Platform::LinuxX86_64, Platform::WindowsX86_64];

    pub fn current() -> Platform {
        if cfg!(windows) { Platform::WindowsX86_64 } else { Platform::LinuxX86_64 }
    }

    pub fn id(self) -> &'static str {
        match self {
            Platform::LinuxX86_64 => "linux-x86_64",
            Platform::WindowsX86_64 => "windows-x86_64",
        }
    }

    pub fn parse(s: &str) -> Option<Platform> {
        Platform::ALL.into_iter().find(|p| p.id() == s)
    }

    pub fn exe_name(self) -> &'static str {
        match self {
            Platform::LinuxX86_64 => "SVX.x86_64",
            Platform::WindowsX86_64 => "SVX.exe",
        }
    }

    pub fn native_ext(self) -> &'static str {
        match self {
            Platform::LinuxX86_64 => ".so",
            Platform::WindowsX86_64 => ".dll",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Platform::LinuxX86_64 => "Linux x86-64",
            Platform::WindowsX86_64 => "Windows x86-64",
        }
    }
}
