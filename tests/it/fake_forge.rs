//! A small in-process Forgejo for the release tests: the release API ccid speaks (branches, releases,
//! assets, tags, commit statuses, workflow dispatches) over plain HTTP on the loopback address, with the
//! two behaviours that matter: a DRAFT's assets can be downloaded with the read credential only, and an
//! upload can be made to fail after storing a part of the file.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

pub const JOB_TOKEN: &str = "job-token-1234";
pub const READ_TOKEN: &str = "read-token-1234";
pub const OWNER_REPO: &str = "o/r";

#[derive(Clone, Debug)]
pub struct Asset {
    pub id: u64,
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub id: u64,
    pub tag: String,
    pub target: String,
    pub draft: bool,
    pub name: String,
    pub body: String,
    pub assets: Vec<Asset>,
}

#[derive(Default, Debug)]
pub struct State {
    pub head: String,
    pub releases: Vec<Release>,
    pub tags: BTreeMap<String, String>,
    pub statuses: Vec<(String, String)>,
    pub dispatched: Vec<String>,
    pub requests: Vec<String>,
    next: u64,
    /// The next upload stores this many bytes and then answers 500 (a connection that broke part-way).
    pub truncate_next_upload: Option<usize>,
}

pub struct Forge {
    pub url: String,
    pub state: Arc<Mutex<State>>,
}

impl Forge {
    pub fn start(head: &str) -> Forge {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state = Arc::new(Mutex::new(State {
            head: head.to_owned(),
            next: 100,
            ..State::default()
        }));
        let shared = state.clone();
        let base = url.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                let base = base.clone();
                thread::spawn(move || {
                    let _ = serve(stream, &state, &base);
                });
            }
        });
        Forge { url, state }
    }

    pub fn release(&self, tag: &str) -> Option<Release> {
        self.state
            .lock()
            .unwrap()
            .releases
            .iter()
            .find(|r| r.tag == tag)
            .cloned()
    }

    pub fn asset(&self, tag: &str, name: &str) -> Option<Vec<u8>> {
        self.release(tag)?
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.bytes.clone())
    }

    pub fn names(&self, tag: &str) -> Vec<String> {
        self.release(tag)
            .map(|r| r.assets.iter().map(|a| a.name.clone()).collect())
            .unwrap_or_default()
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    token: String,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("").to_owned();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let mut length = 0usize;
    let mut token = String::new();
    let mut expect = false;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end().to_owned();
        if header.is_empty() {
            break;
        }
        let lower = header.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("expect:") {
            expect = true;
        }
        if lower.starts_with("authorization:") {
            token = header
                .split_whitespace()
                .last()
                .unwrap_or("")
                .to_owned();
        }
    }
    if expect {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        token,
        body,
    })
}

fn reply(stream: &mut TcpStream, status: u16, body: &[u8]) -> std::io::Result<()> {
    let text = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {text}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn release_json(release: &Release, base: &str) -> serde_json::Value {
    serde_json::json!({
        "id": release.id, "tag_name": release.tag, "target_commitish": release.target,
        "draft": release.draft, "name": release.name, "body": release.body,
        "assets": release.assets.iter().map(|a| serde_json::json!({
            "id": a.id, "name": a.name, "size": a.bytes.len(),
            "browser_download_url": format!("{base}/{OWNER_REPO}/releases/download/{}/{}", release.tag, a.name)
        })).collect::<Vec<_>>()
    })
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{key}=")).map(str::to_owned))
}

fn serve(mut stream: TcpStream, state: &Arc<Mutex<State>>, base: &str) -> std::io::Result<()> {
    let request = read_request(&mut stream)?;
    let mut state = state.lock().unwrap();
    state
        .requests
        .push(format!("{} {}", request.method, request.path));
    let writer = request.token == JOB_TOKEN || request.token == READ_TOKEN;
    // Downloads of release assets: a draft's only with the read credential.
    if let Some(rest) = request.path.strip_prefix(&format!("/{OWNER_REPO}/releases/download/")) {
        let (tag, name) = rest.split_once('/').unwrap_or((rest, ""));
        let found = state.releases.iter().find(|r| r.tag == tag);
        return match found {
            Some(r) if r.draft && request.token != READ_TOKEN => reply(&mut stream, 404, b"{}"),
            Some(r) => match r.assets.iter().find(|a| a.name == name) {
                Some(a) => reply(&mut stream, 200, &a.bytes),
                None => reply(&mut stream, 404, b"{}"),
            },
            None => reply(&mut stream, 404, b"{}"),
        };
    }
    let Some(path) = request
        .path
        .strip_prefix(&format!("/api/v1/repos/{OWNER_REPO}/"))
    else {
        return reply(&mut stream, 404, b"{}");
    };
    if !writer {
        return reply(&mut stream, 404, b"{}");
    }
    let json = |value: serde_json::Value| value.to_string().into_bytes();
    let segments: Vec<&str> = path.split('/').collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["branches", _]) => {
            let body = json(serde_json::json!({ "commit": { "id": state.head } }));
            reply(&mut stream, 200, &body)
        }
        ("GET", ["releases"]) => {
            let page: u32 = query_value(&request.query, "page")
                .and_then(|p| p.parse().ok())
                .unwrap_or(1);
            let list: Vec<_> = if page == 1 {
                state.releases.iter().map(|r| release_json(r, base)).collect()
            } else {
                Vec::new()
            };
            reply(&mut stream, 200, &json(serde_json::Value::Array(list)))
        }
        ("POST", ["releases"]) => {
            let value: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            state.next += 1;
            let release = Release {
                id: state.next,
                tag: value["tag_name"].as_str().unwrap().to_owned(),
                target: value["target_commitish"].as_str().unwrap().to_owned(),
                draft: value["draft"].as_bool().unwrap(),
                name: value["name"].as_str().unwrap().to_owned(),
                body: value["body"].as_str().unwrap().to_owned(),
                assets: Vec::new(),
            };
            let body = json(release_json(&release, base));
            state.releases.push(release);
            reply(&mut stream, 201, &body)
        }
        ("PATCH", ["releases", id]) => {
            let id: u64 = id.parse().unwrap();
            let value: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let Some(index) = state.releases.iter().position(|r| r.id == id) else {
                return reply(&mut stream, 404, b"{}");
            };
            let release = &mut state.releases[index];
            if let Some(tag) = value["tag_name"].as_str() {
                release.tag = tag.to_owned();
            }
            if let Some(name) = value["name"].as_str() {
                release.name = name.to_owned();
            }
            if let Some(draft) = value["draft"].as_bool() {
                release.draft = draft;
            }
            let (tag, target, draft) = (release.tag.clone(), release.target.clone(), release.draft);
            let body = json(release_json(release, base));
            if !draft {
                state.tags.insert(tag, target);
            }
            reply(&mut stream, 200, &body)
        }
        ("DELETE", ["releases", id]) => {
            let id: u64 = id.parse().unwrap();
            state.releases.retain(|r| r.id != id);
            reply(&mut stream, 204, b"")
        }
        ("POST", ["releases", id, "assets"]) => {
            let id: u64 = id.parse().unwrap();
            let name = query_value(&request.query, "name").unwrap();
            let truncate = state.truncate_next_upload.take();
            state.next += 1;
            let asset_id = state.next;
            let Some(release) = state.releases.iter_mut().find(|r| r.id == id) else {
                return reply(&mut stream, 404, b"{}");
            };
            if release.assets.iter().any(|a| a.name == name) {
                return reply(&mut stream, 409, b"{}");
            }
            let (bytes, status) = match truncate {
                Some(n) => (request.body[..n.min(request.body.len())].to_vec(), 500),
                None => (request.body.clone(), 201),
            };
            release.assets.push(Asset {
                id: asset_id,
                name,
                bytes,
            });
            reply(&mut stream, status, b"{}")
        }
        ("DELETE", ["releases", id, "assets", asset]) => {
            let (id, asset): (u64, u64) = (id.parse().unwrap(), asset.parse().unwrap());
            if let Some(release) = state.releases.iter_mut().find(|r| r.id == id) {
                release.assets.retain(|a| a.id != asset);
            }
            reply(&mut stream, 204, b"")
        }
        ("GET", ["tags", tag]) => match state.tags.get(*tag) {
            Some(commit) => reply(
                &mut stream,
                200,
                &json(serde_json::json!({ "commit": { "sha": commit } })),
            ),
            None => reply(&mut stream, 404, b"{}"),
        },
        ("GET", ["commits", _, "status"]) => {
            let statuses: Vec<_> = state
                .statuses
                .iter()
                .map(|(c, s)| serde_json::json!({ "context": c, "status": s }))
                .collect();
            reply(
                &mut stream,
                200,
                &json(serde_json::json!({ "state": "pending", "statuses": statuses })),
            )
        }
        ("POST", ["actions", "workflows", workflow, "dispatches"]) => {
            state.dispatched.push((*workflow).to_owned());
            reply(&mut stream, 204, b"")
        }
        _ => reply(&mut stream, 404, b"{}"),
    }
}
