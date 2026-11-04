//! The control panel - the standalone's, served by the Rust bot: the same page, the same API
//! (/api/state, /api/peers, /api/cmd), the same shared key (~/.melonhack/panels/key) and heartbeat
//! files (~/.melonhack/panels/<port>.json), so one browser page shows the Java clients and the Rust
//! bots side by side, and the Discord status watcher (~/.melonhack/discord-status/status.js) reads
//! the Rust bot exactly as it read the Java one.
//!
//! Who may drive the account (ControlPanel's three checks): the Host must be 127.0.0.1/localhost
//! (no DNS rebinding), an Origin must be one of our loopback pages, and every API call carries the
//! key - only our own page, served on loopback without CORS, contains it.
//!
//! No live picture: there is nothing rendering. /pov answers 404 and the page says so.

use std::{
    collections::VecDeque,
    fs,
    io::Read,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};
use tracing::{info, warn};

const PAGE: &str = include_str!("panel.html");

pub struct Panel {
    pub port: u16,
    page: &'static str,
    key: String,
    /// The latest /api/state body (without the feed), rebuilt by the bot every few ticks.
    pub state: Mutex<Value>,
    /// Orders from the page, for the bot to run on its next tick.
    pub commands: Mutex<VecDeque<Value>>,
    feed: Mutex<VecDeque<Value>>,
    feed_n: Mutex<u64>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn registry() -> PathBuf {
    crate::config::melonhack_home().join("panels")
}

/// The shared key every panel on this machine uses (made once if missing).
fn key() -> String {
    let f = registry().join("key");
    if let Ok(k) = fs::read_to_string(&f)
        && k.trim().len() >= 32
    {
        return k.trim().to_string();
    }
    // 48 hex characters from two random v4 uuids (the OS random source).
    let k = format!("{}{}", uuid::Uuid::new_v4().simple(), &uuid::Uuid::new_v4().simple().to_string()[..16]);
    let _ = fs::create_dir_all(registry());
    let _ = fs::write(&f, &k);
    k
}

impl Panel {
    /// Serve on the first free port from `first` (up to +20, as the Java panel does).
    pub fn start(first: u16) -> Option<Arc<Panel>> {
        Self::start_with(first, PAGE, true)
    }

    /// Another page on the same server code (the swarm panel). `heartbeat`: list it in
    /// ~/.melonhack/panels/ for the Java panels' multi-account view - only a one-account panel
    /// belongs there.
    pub fn start_with(first: u16, page: &'static str, heartbeat: bool) -> Option<Arc<Panel>> {
        let mut bound = None;
        for p in first..=first.saturating_add(20) {
            if let Ok(s) = Server::http(("127.0.0.1", p)) {
                bound = Some((p, s));
                break;
            }
        }
        let Some((port, server)) = bound else {
            warn!("control panel: no free port between {first} and {}", first + 20);
            return None;
        };
        let panel = Arc::new(Panel {
            port,
            page,
            key: key(),
            state: Mutex::new(json!({})),
            commands: Mutex::new(VecDeque::new()),
            feed: Mutex::new(VecDeque::new()),
            feed_n: Mutex::new(0),
        });
        let p = panel.clone();
        std::thread::Builder::new()
            .name("panel".into())
            .spawn(move || {
                for req in server.incoming_requests() {
                    p.handle(req);
                }
            })
            .ok()?;
        if heartbeat {
            let p = panel.clone();
            std::thread::Builder::new()
                .name("panel-heartbeat".into())
                .spawn(move || {
                    loop {
                        p.heartbeat();
                        std::thread::sleep(Duration::from_secs(5));
                    }
                })
                .ok()?;
        }
        info!("control panel: http://127.0.0.1:{port}/");
        Some(panel)
    }

    /// A line in the page's feed (kinds as the Java AiFeed: say, event, ...).
    pub fn feed(&self, kind: &str, text: &str) {
        let mut n = self.feed_n.lock();
        *n += 1;
        let mut f = self.feed.lock();
        f.push_back(json!({"n": *n, "t": now_ms(), "kind": kind, "text": text}));
        while f.len() > 200 {
            f.pop_front();
        }
    }

    fn heartbeat(&self) {
        let _ = fs::create_dir_all(registry());
        let name = self.state.lock()["name"].as_str().unwrap_or("?").to_string();
        let o = json!({"port": self.port, "name": name, "at": now_ms()});
        let _ = fs::write(registry().join(format!("{}.json", self.port)), o.to_string());
    }

    /// The heartbeat file goes with the process.
    pub fn shut(&self) {
        let _ = fs::remove_file(registry().join(format!("{}.json", self.port)));
    }

    fn header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
        req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
    }

    fn handle(&self, mut req: Request) {
        let url = req.url().to_string();
        let (path, query) = url.split_once('?').map_or((url.as_str(), ""), |(p, q)| (p, q));
        let path = path.to_string();
        let query = query.to_string();
        let host_ok = Self::header(&req, "Host").is_some_and(|h| {
            let h = h.rsplit_once(':').map_or(h, |(n, _)| n);
            h == "127.0.0.1" || h == "localhost"
        });
        if !host_ok {
            return respond(req, 403, "text/plain", "wrong host".into(), None);
        }
        if path == "/" || path == "/index.html" {
            // The page carries the key: served with no CORS header, so another site cannot read it.
            let page = self
                .page
                .replace("__MELON_KEY__", &self.key)
                .replace("No picture yet - the game window has to be open and rendering. Retrying...", "No live picture - this is a headless Rust bot (nothing is rendering).");
            return respond(req, 200, "text/html; charset=utf-8", page, None);
        }
        let origin = Self::header(&req, "Origin").map(str::to_string);
        let origin_ok = origin.as_deref().is_none_or(|o| {
            let rest = o.strip_prefix("http://127.0.0.1:").or_else(|| o.strip_prefix("http://localhost:"));
            rest.is_some_and(|p| !p.is_empty() && p.len() <= 5 && p.chars().all(|c| c.is_ascii_digit()))
        });
        if !origin_ok {
            return respond(req, 403, "text/plain", "refused".into(), None);
        }
        if *req.method() == Method::Options {
            return respond(req, 204, "text/plain", String::new(), origin);
        }
        let given = Self::header(&req, "X-Melon-Key")
            .map(str::to_string)
            .or_else(|| query.split('&').find_map(|p| p.strip_prefix("k=").map(str::to_string)));
        if given.as_deref() != Some(self.key.as_str()) {
            return respond(req, 403, "text/plain", "missing or wrong key - open the panel page first".into(), origin);
        }
        match path.as_str() {
            "/api/state" => {
                let after: u64 = query.split('&').find_map(|p| p.strip_prefix("feed=")).and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut o = self.state.lock().clone();
                let feed: Vec<Value> = self.feed.lock().iter().filter(|f| f["n"].as_u64().unwrap_or(0) > after).cloned().collect();
                o["feed"] = Value::Array(feed);
                respond(req, 200, "application/json", o.to_string(), origin)
            }
            "/api/peers" => {
                let mut a = vec![json!({"port": self.port})];
                if let Ok(dir) = fs::read_dir(registry()) {
                    for f in dir.flatten() {
                        let Ok(t) = fs::read_to_string(f.path()) else { continue };
                        let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                        let fresh = v["at"].as_u64().is_some_and(|at| now_ms().saturating_sub(at) <= 20_000);
                        if fresh && v["port"].as_u64() != Some(self.port as u64) {
                            a.push(v);
                        }
                    }
                }
                respond(req, 200, "application/json", Value::Array(a).to_string(), origin)
            }
            "/api/cmd" => {
                if *req.method() != Method::Post {
                    return respond(req, 405, "text/plain", "POST only".into(), origin);
                }
                let mut body = String::new();
                let _ = req.as_reader().take(64 * 1024).read_to_string(&mut body);
                match serde_json::from_str::<Value>(&body) {
                    Ok(cmd) if cmd.get("type").is_some() => {
                        self.commands.lock().push_back(cmd);
                        respond(req, 200, "application/json", "{\"ok\":true}".into(), origin)
                    }
                    _ => respond(req, 400, "application/json", "{\"ok\":false,\"error\":\"no type\"}".into(), origin),
                }
            }
            _ => respond(req, 404, "text/plain", "not found".into(), origin),
        }
    }
}

fn respond(req: Request, code: u16, ctype: &str, body: String, origin: Option<String>) {
    let mut r = Response::from_string(body).with_status_code(code);
    let mut add = |k: &str, v: &str| {
        if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            r.add_header(h);
        }
    };
    add("Content-Type", ctype);
    add("Cache-Control", "no-store");
    if let Some(o) = origin {
        add("Access-Control-Allow-Origin", &o);
        add("Vary", "Origin");
        add("Access-Control-Allow-Methods", "GET, POST, OPTIONS");
        add("Access-Control-Allow-Headers", "Content-Type, X-Melon-Key");
    }
    let _ = req.respond(r);
}
