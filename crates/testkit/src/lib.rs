use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Debug)]
pub struct Request {
    pub path: String,
    pub range: Option<String>,
    pub status: u16,
}

#[derive(Default)]
struct State {
    redirects: HashMap<String, String>,
    cut_once: HashMap<String, u64>,
    status_override: HashMap<String, u16>,
    ignore_range: bool,
    log: Vec<Request>,
}

#[derive(Clone)]
pub struct Server {
    pub port: u16,
    root: PathBuf,
    state: Arc<Mutex<State>>,
}

impl Server {
    pub fn start(root: &Path, port: u16) -> Server {
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind test server");
        let port = listener.local_addr().expect("addr").port();
        let server = Server { port, root: root.to_path_buf(), state: Arc::new(Mutex::new(State::default())) };
        let s = server.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let s = s.clone();
                thread::spawn(move || {
                    let _ = s.handle(stream);
                });
            }
        });
        server
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base(), path.trim_start_matches('/'))
    }

    pub fn redirect(&self, from: &str, to: &str) {
        self.state.lock().unwrap().redirects.insert(normalize(from), to.to_string());
    }

    pub fn cut_once(&self, path: &str, after: u64) {
        self.state.lock().unwrap().cut_once.insert(normalize(path), after);
    }

    pub fn status(&self, path: &str, status: u16) {
        self.state.lock().unwrap().status_override.insert(normalize(path), status);
    }

    pub fn ignore_range(&self, on: bool) {
        self.state.lock().unwrap().ignore_range = on;
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn clear_log(&self) {
        self.state.lock().unwrap().log.clear();
    }

    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("/").to_string();
        let mut range = None;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
                break;
            }
            if let Some((k, v)) = h.split_once(':')
                && k.trim().eq_ignore_ascii_case("range")
            {
                range = Some(v.trim().to_string());
            }
        }
        let path = normalize(target.split('?').next().unwrap_or("/"));
        let (redirect, cut, forced, ignore_range) = {
            let mut st = self.state.lock().unwrap();
            let cut = st.cut_once.remove(&path);
            (st.redirects.get(&path).cloned(), cut, st.status_override.get(&path).copied(), st.ignore_range)
        };
        let log = |status: u16| {
            self.state.lock().unwrap().log.push(Request { path: path.clone(), range: range.clone(), status });
        };
        if method != "GET" {
            log(405);
            return respond(&mut stream, 405, &[], b"");
        }
        if let Some(status) = forced {
            log(status);
            return respond(&mut stream, status, &[], b"forced");
        }
        if let Some(loc) = redirect {
            log(302);
            return respond(&mut stream, 302, &[("Location", loc.as_str())], b"");
        }
        let Some(file_path) = safe_join(&self.root, &path) else {
            log(400);
            return respond(&mut stream, 400, &[], b"");
        };
        let Ok(mut file) = fs::File::open(&file_path) else {
            log(404);
            return respond(&mut stream, 404, &[], b"not found");
        };
        let len = file.metadata()?.len();
        let (status, start, end) = match range.as_deref().filter(|_| !ignore_range).and_then(|r| parse_range(r, len)) {
            Some((a, b)) => (206, a, b),
            None if range.is_some() && !ignore_range => {
                log(416);
                return respond(&mut stream, 416, &[], b"");
            }
            None => (200, 0, len.saturating_sub(1)),
        };
        let body_len = if len == 0 { 0 } else { end - start + 1 };
        log(status);
        let cr = format!("bytes {start}-{end}/{len}");
        let cl = body_len.to_string();
        let mut headers = vec![("Content-Length", cl.as_str()), ("Accept-Ranges", "bytes")];
        if status == 206 {
            headers.push(("Content-Range", cr.as_str()));
        }
        write_head(&mut stream, status, &headers)?;
        file.seek(SeekFrom::Start(start))?;
        let limit = cut.map(|c| c.min(body_len)).unwrap_or(body_len);
        let mut taken = file.take(limit);
        std::io::copy(&mut taken, &mut stream)?;
        stream.flush()?;
        if cut.is_some() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        Ok(())
    }
}

fn normalize(p: &str) -> String {
    format!("/{}", p.trim_start_matches('/'))
}

fn safe_join(root: &Path, path: &str) -> Option<PathBuf> {
    let rel = Path::new(path.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

fn parse_range(r: &str, len: u64) -> Option<(u64, u64)> {
    let spec = r.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let a: u64 = a.parse().ok()?;
    let b: u64 = if b.is_empty() { len.checked_sub(1)? } else { b.parse().ok()? };
    if a >= len || b < a {
        return None;
    }
    Some((a, b.min(len - 1)))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn write_head(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)]) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\nConnection: close\r\n", reason(status));
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())
}

fn respond(stream: &mut TcpStream, status: u16, headers: &[(&str, &str)], body: &[u8]) -> std::io::Result<()> {
    let cl = body.len().to_string();
    let mut all = headers.to_vec();
    all.push(("Content-Length", cl.as_str()));
    write_head(stream, status, &all)?;
    stream.write_all(body)?;
    stream.flush()
}
