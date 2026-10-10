use crate::error::{Error, Result};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use url::{Host, Url};

pub const MAX_REDIRECTS: usize = 5;
pub const DEFAULT_HOSTS: [&str; 5] =
    ["github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com", "github-releases.githubusercontent.com", "*.github.io"];

#[derive(Clone, Debug)]
pub struct UrlPolicy {
    pub allow_loopback_http: bool,
    pub hosts: Vec<String>,
}

impl UrlPolicy {
    pub fn new(extra_hosts: &[String], allow_loopback_http: bool) -> UrlPolicy {
        let mut hosts: Vec<String> = DEFAULT_HOSTS.iter().map(|s| s.to_string()).collect();
        for h in extra_hosts {
            if !hosts.contains(h) {
                hosts.push(h.to_ascii_lowercase());
            }
        }
        UrlPolicy { allow_loopback_http, hosts }
    }

    pub fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.hosts.iter().any(|pattern| match pattern.strip_prefix("*.") {
            Some(suffix) => host.len() > suffix.len() + 1 && host.ends_with(&format!(".{suffix}")),
            None => &host == pattern,
        })
    }

    pub fn check(&self, url: &Url) -> Result<()> {
        let fail = |why: &str| Error::UnsafeUrl(format!("{url}: {why}"));
        if !url.username().is_empty() || url.password().is_some() {
            return Err(fail("credentials in address"));
        }
        let loopback = match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            None => return Err(fail("no host")),
        };
        if loopback {
            if self.allow_loopback_http && matches!(url.scheme(), "http" | "https") {
                return Ok(());
            }
            return Err(fail("local addresses are only allowed in test builds"));
        }
        if url.scheme() != "https" {
            return Err(fail("only https is allowed"));
        }
        match url.host() {
            Some(Host::Domain(d)) if self.host_allowed(d) => Ok(()),
            Some(Host::Domain(_)) => Err(fail("host is not an allowed release host")),
            _ => Err(fail("numeric addresses are not allowed")),
        }
    }

    pub fn resolve(&self, base: &Url, reference: &str) -> Result<Url> {
        let url = base.join(reference).map_err(|e| Error::UnsafeUrl(format!("{reference}: {e}")))?;
        self.check(&url)?;
        Ok(url)
    }
}

pub struct Fetcher {
    agent: ureq::Agent,
    pub policy: UrlPolicy,
    pub cancel: Arc<AtomicBool>,
}

type Response = ureq::http::Response<ureq::Body>;

impl Fetcher {
    pub fn new(policy: UrlPolicy, cancel: Arc<AtomicBool>, user_agent: &str) -> Fetcher {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(45)))
            .user_agent(user_agent)
            .build()
            .into();
        Fetcher { agent, policy, cancel }
    }

    fn open(&self, start: &Url, range: Option<(u64, u64)>) -> Result<(Response, Url)> {
        self.policy.check(start)?;
        let mut url = start.clone();
        for _ in 0..=MAX_REDIRECTS {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let mut req = self.agent.get(url.as_str());
            if let Some((a, b)) = range {
                req = req.header("Range", format!("bytes={a}-{b}"));
            }
            let resp = req.call().map_err(|e| Error::Network(format!("{url}: {e}")))?;
            let status = resp.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                let loc =
                    resp.headers().get("location").and_then(|v| v.to_str().ok()).ok_or_else(|| Error::Network(format!("{url}: redirect without location")))?;
                let next = url.join(loc).map_err(|e| Error::UnsafeUrl(format!("redirect to {loc}: {e}")))?;
                if url.scheme() == "https" && next.scheme() != "https" {
                    return Err(Error::UnsafeUrl(format!("redirect from {url} downgrades to {next}")));
                }
                self.policy.check(&next)?;
                url = next;
                continue;
            }
            if status == 404 || status == 410 {
                return Err(Error::NotFound(url.to_string()));
            }
            return Ok((resp, url));
        }
        Err(Error::UnsafeUrl(format!("{start}: too many redirects")))
    }

    pub fn get_small(&self, url: &Url, max: u64) -> Result<Vec<u8>> {
        let (mut resp, final_url) = self.open(url, None)?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(Error::Http { url: final_url.to_string(), status });
        }
        if let Some(len) = content_length(&resp)
            && len > max
        {
            return Err(Error::TooLarge(format!("{final_url} is {len} bytes, limit {max}")));
        }
        let mut out = Vec::new();
        let mut reader = resp.body_mut().as_reader().take(max + 1);
        reader.read_to_end(&mut out).map_err(|e| Error::Network(format!("{final_url}: {e}")))?;
        if out.len() as u64 > max {
            return Err(Error::TooLarge(format!("{final_url} exceeds {max} bytes")));
        }
        Ok(out)
    }

    pub fn fetch_range(&self, url: &Url, offset: u64, len: u64, part: &Path, progress: &mut dyn FnMut(u64)) -> Result<()> {
        let mut have = match fs::metadata(part) {
            Ok(m) => m.len(),
            Err(_) => 0,
        };
        if have > len {
            fs::remove_file(part).map_err(|e| Error::io_path("remove", part, e))?;
            have = 0;
        }
        if have > 0 {
            progress(have);
        }
        if have == len {
            return Ok(());
        }
        let start = offset + have;
        let end = offset + len - 1;
        let (mut resp, final_url) = self.open(url, Some((start, end)))?;
        let status = resp.status().as_u16();
        let skip = match status {
            206 => {
                let cr = resp.headers().get("content-range").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                if !cr.starts_with(&format!("bytes {start}-")) {
                    return Err(Error::Network(format!("{final_url}: unexpected content range {cr:?}")));
                }
                0
            }
            200 => start,
            416 => return Err(Error::Network(format!("{final_url}: requested range is outside the file; it may have been replaced"))),
            _ => return Err(Error::Http { url: final_url.to_string(), status }),
        };
        if let Some(parent) = part.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io_path("create", parent, e))?;
        }
        let mut out = OpenOptions::new().create(true).append(true).open(part).map_err(|e| Error::io_path("open", part, e))?;
        let mut reader = resp.body_mut().as_reader();
        if skip > 0 {
            let copied = io::copy(&mut (&mut reader).take(skip), &mut io::sink()).map_err(|e| Error::Network(format!("{final_url}: {e}")))?;
            if copied != skip {
                return Err(Error::Network(format!("{final_url}: connection closed early")));
            }
        }
        let mut remaining = len - have;
        let mut buf = vec![0u8; 256 * 1024];
        while remaining > 0 {
            if self.cancel.load(Ordering::Relaxed) {
                let _ = out.sync_all();
                return Err(Error::Cancelled);
            }
            let want = remaining.min(buf.len() as u64) as usize;
            let n = match reader.read(&mut buf[..want]) {
                Ok(0) => {
                    let _ = out.sync_all();
                    return Err(Error::Network(format!("{final_url}: connection closed early; the download will resume")));
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = out.sync_all();
                    return Err(Error::Network(format!("{final_url}: {e}; the download will resume")));
                }
            };
            out.write_all(&buf[..n]).map_err(|e| Error::io_path("write", part, e))?;
            remaining -= n as u64;
            progress(n as u64);
        }
        out.sync_all().map_err(|e| Error::io_path("sync", part, e))?;
        Ok(())
    }
}

fn content_length(resp: &Response) -> Option<u64> {
    resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy() {
        let p = UrlPolicy::new(&[], false);
        let ok = |s: &str| p.check(&Url::parse(s).unwrap()).is_ok();
        assert!(ok("https://github.com/gicstin/SVX-Distribution/releases/download/a/b"));
        assert!(ok("https://gicstin.github.io/SVX-Distribution/index.json"));
        assert!(!ok("http://github.com/x"));
        assert!(!ok("https://evil.example/x"));
        assert!(!ok("https://github.io.evil.example/x"));
        assert!(!ok("https://user:pw@github.com/x"));
        assert!(!ok("file:///etc/passwd"));
        assert!(!ok("https://127.0.0.1/x"));
        assert!(!ok("http://localhost:8080/x"));
        assert!(!ok("https://140.82.112.3/x"));
        let t = UrlPolicy::new(&[], true);
        assert!(t.check(&Url::parse("http://127.0.0.1:9/x").unwrap()).is_ok());
        assert!(t.check(&Url::parse("http://example.com/x").unwrap()).is_err());
    }
}
