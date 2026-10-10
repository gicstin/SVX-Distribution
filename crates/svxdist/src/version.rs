use crate::error::{Error, Result};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    pub fn parse(s: &str) -> Result<Version> {
        let bad = || Error::Metadata(format!("bad version {s:?}, expected MAJOR.MINOR.PATCH"));
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(bad());
        }
        let mut n = [0u64; 3];
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() || p.len() > 9 || !p.bytes().all(|c| c.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
                return Err(bad());
            }
            n[i] = p.parse().map_err(|_| bad())?;
        }
        Ok(Version(n[0], n[1], n[2]))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering() {
        assert!(Version::parse("1.10.0").unwrap() > Version::parse("1.9.9").unwrap());
        assert!(Version::parse("01.0.0").is_err());
        assert!(Version::parse("1.0").is_err());
        assert!(Version::parse("1.0.0-beta").is_err());
    }
}
