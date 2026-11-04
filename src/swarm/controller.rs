//! The controller link - each swarm bot is one of the bots of whichever Java client runs the
//! controller mod as Controller (any instance with its Controller on). The line
//! protocol is the Java mod's (core/swarm), unchanged: one TCP link per bot to controller-host:port
//! (HELLO, then STATUS each second, PONG to PING), and the controller's orders and syncs back.
//! The controller player's own position comes from its WebSocket hub (port + ws offset), the
//! "POS|x|y|z|yaw|dim|ms" stream the Java bots follow with - many times a second, with the yaw.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use azalea::Vec3;
use parking_lot::Mutex;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
    task::JoinHandle,
};
use tracing::{Instrument, debug, info, warn};

use super::{SwarmCtx, orders::Offsets};

/// A controller line, parsed.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    ComeHere { pos: Vec3, dim: String },
    Goto { pos: Vec3, dim: String },
    StayNear { pos: Vec3, dim: String, hold: f64, follow: Option<String>, offsets: Option<Offsets> },
    StayNearOff,
    FollowTarget(String),
    Defend(bool),
    AttackTarget(String),
    Module { name: String, on: bool },
    /// SAY: text typed at the controller for this bot.
    Say(String),
    /// NUKER_SYNC, PRINTER_SYNC, AURA_SYNC, COMBAT_SYNC, SIGHTING, JOB...: kept whole for the
    /// modules that use them (later phases) and for the panel.
    Sync { kind: String, raw: String },
    Pong,
    Unknown(String),
}

fn num(p: &[&str], i: usize) -> Option<f64> {
    p.get(i).and_then(|s| s.trim().parse::<f64>().ok())
}

pub fn parse(line: &str) -> Inbound {
    let p: Vec<&str> = line.split('|').collect();
    let pos = |p: &[&str]| -> Option<Vec3> { Some(Vec3::new(num(p, 1)?, num(p, 2)?, num(p, 3)?)) };
    let s = |i: usize| p.get(i).map(|s| s.trim().to_string()).unwrap_or_default();
    match p[0] {
        "COME_HERE" | "GOTO" => match pos(&p) {
            Some(v) if p[0] == "GOTO" => Inbound::Goto { pos: v, dim: s(4) },
            Some(v) => Inbound::ComeHere { pos: v, dim: s(4) },
            None => Inbound::Unknown(line.into()),
        },
        "STAY_NEAR" => match pos(&p) {
            Some(v) => {
                let follow = p.get(7).map(|f| f.trim().to_string()).filter(|f| !f.is_empty());
                let offsets = (p.len() > 13).then(|| Offsets {
                    front: num(&p, 8).unwrap_or(0.0),
                    back: num(&p, 9).unwrap_or(0.0),
                    left: num(&p, 10).unwrap_or(0.0),
                    right: num(&p, 11).unwrap_or(0.0),
                    above: num(&p, 12).unwrap_or(0.0),
                    below: num(&p, 13).unwrap_or(0.0),
                });
                Inbound::StayNear { pos: v, dim: s(4), hold: num(&p, 5).unwrap_or(3.0), follow, offsets }
            }
            None => Inbound::Unknown(line.into()),
        },
        "STAY_NEAR_OFF" => Inbound::StayNearOff,
        "FOLLOW_TARGET" => Inbound::FollowTarget(s(1)),
        "DEFEND" => Inbound::Defend(s(1).eq_ignore_ascii_case("true")),
        "ATTACK_TARGET" => Inbound::AttackTarget(s(1)),
        "MODULE" if p.len() >= 3 => Inbound::Module { name: s(1), on: s(2).eq_ignore_ascii_case("true") },
        // The rest of the raw line: chat text may itself contain '|'.
        "SAY" => Inbound::Say(line.split_once('|').map(|(_, t)| t.trim().to_string()).unwrap_or_default()),
        "PONG" => Inbound::Pong,
        "NUKER_SYNC" | "PRINTER_SYNC" | "AURA_SYNC" | "COMBAT_SYNC" | "SIGHTING" | "JOB" | "JOB_CANCEL" | "POV_REQ" => {
            Inbound::Sync { kind: p[0].to_string(), raw: line.to_string() }
        }
        _ => Inbound::Unknown(line.into()),
    }
}

/// The protocol's status line, the extended form (health and food on the end).
#[allow(clippy::too_many_arguments)]
pub fn status_line(pos: Vec3, dim: &str, state: &str, fast_nuker: bool, printer: bool, aura: bool, health: i32, food: i32) -> String {
    format!(
        "STATUS|{}|{}|{}|{}|{}|{fast_nuker}|{printer}|false|{aura}|{health}|{food}|false",
        pos.x,
        pos.y,
        pos.z,
        clean(dim),
        clean(state)
    )
}

/// The protocol's sanitize: no separators or line breaks inside a field.
pub fn clean(s: &str) -> String {
    s.replace('|', "/").replace(['\n', '\r'], " ").trim().to_string()
}

/// One bot's link to the controller.
#[derive(Default)]
pub struct Link {
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    inbound: Mutex<VecDeque<String>>,
    pub connected: AtomicBool,
    pub last_error: Mutex<String>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Link {
    /// Keep a link open for this bot (reconnecting every 5 s) until [`Link::stop`].
    pub fn start(self: &Arc<Self>, ctx: Arc<SwarmCtx>, name: String) {
        let mut task = self.task.lock();
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let me = self.clone();
        let span = tracing::info_span!("bot", name = %name);
        *task = Some(tokio::spawn(async move { me.run(ctx, name).await }.instrument(span)));
    }

    pub fn stop(&self) {
        if let Some(t) = self.task.lock().take() {
            t.abort();
        }
        *self.tx.lock() = None;
        self.connected.store(false, Ordering::SeqCst);
    }

    /// Send a line if connected (dropped otherwise: a STATUS is stale by the next one anyway).
    pub fn send(&self, line: String) {
        if let Some(tx) = self.tx.lock().as_ref() {
            let _ = tx.send(line);
        }
    }

    pub fn drain(&self) -> Vec<String> {
        self.inbound.lock().drain(..).collect()
    }

    async fn run(self: Arc<Self>, ctx: Arc<SwarmCtx>, name: String) {
        loop {
            let (on, host, port) = {
                let c = ctx.cfg.read();
                (c.controller_enabled, c.controller_host.clone(), c.controller_port)
            };
            if !on || port == 0 {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            match TcpStream::connect((host.as_str(), port)).await {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    info!(target: "controller", "linked to the controller at {host}:{port}");
                    self.last_error.lock().clear();
                    let why = self.session(stream, &name).await;
                    info!(target: "controller", "controller link closed: {why}");
                    *self.last_error.lock() = why;
                }
                Err(e) => {
                    let why = format!("no controller at {host}:{port} ({e})");
                    let mut last = self.last_error.lock();
                    if *last != why {
                        debug!(target: "controller", "{why}");
                        *last = why;
                    }
                }
            }
            self.connected.store(false, Ordering::SeqCst);
            *self.tx.lock() = None;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn session(&self, stream: TcpStream, name: &str) -> String {
        let (r, mut w) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let _ = tx.send(format!("HELLO|{}|", clean(name)));
        *self.tx.lock() = Some(tx.clone());
        self.connected.store(true, Ordering::SeqCst);
        let writer = async {
            while let Some(line) = rx.recv().await {
                if let Err(e) = w.write_all(format!("{line}\n").as_bytes()).await {
                    return format!("write failed: {e}");
                }
            }
            "writer closed".to_string()
        };
        let reader = async {
            let mut lines = BufReader::new(r).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(l)) => {
                        let l = l.trim().to_string();
                        if l.is_empty() {
                            continue;
                        }
                        if l == "PING" {
                            let _ = tx.send("PONG".into());
                            continue;
                        }
                        debug!(target: "controller", "<- {}", if l.len() > 300 { &l[..300] } else { &l });
                        let mut q = self.inbound.lock();
                        q.push_back(l);
                        while q.len() > 500 {
                            q.pop_front();
                        }
                    }
                    Ok(None) => return "the controller closed the link".to_string(),
                    Err(e) => return format!("read failed: {e}"),
                }
            }
        };
        tokio::select! {
            why = writer => why,
            why = reader => why,
        }
    }
}

/// The controller player's position and yaw from its WebSocket hub, into ctx.controller - kept
/// open (reconnecting every 3 s) for the life of the process.
pub async fn watch_controller(ctx: Arc<SwarmCtx>) {
    loop {
        let (on, host, port) = {
            let c = ctx.cfg.read();
            (c.controller_enabled && c.controller_ws, c.controller_host.clone(), c.controller_port.saturating_add(c.controller_ws_offset))
        };
        if on && port > 0 {
            let why = match ws_session(&ctx, &host, port).await {
                Ok(()) => "closed".to_string(),
                Err(e) => e,
            };
            let mut v = ctx.controller.lock();
            if v.ws_connected {
                info!(target: "controller", "controller position stream ended: {why}");
            }
            v.ws_connected = false;
            v.ws_error = why;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

async fn ws_session(ctx: &Arc<SwarmCtx>, host: &str, port: u16) -> Result<(), String> {
    let mut s = TcpStream::connect((host, port)).await.map_err(|e| format!("no position stream at {host}:{port} ({e})"))?;
    let _ = s.set_nodelay(true);
    // The hub answers any path; the key is the RFC's sample nonce (we do not check its reply).
    let req = format!(
        "GET / HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
    // The reply headers, up to the blank line.
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 || s.read(&mut b).await.map_err(|e| e.to_string())? == 0 {
            return Err("the position stream closed during the handshake".into());
        }
        head.push(b[0]);
    }
    if !String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101") {
        return Err("the position stream refused the upgrade".into());
    }
    {
        let mut v = ctx.controller.lock();
        v.ws_connected = true;
        v.ws_error.clear();
    }
    info!(target: "controller", "following the controller's position stream at {host}:{port}");
    loop {
        let mut h = [0u8; 2];
        s.read_exact(&mut h).await.map_err(|e| e.to_string())?;
        let opcode = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = (h[1] & 0x7f) as u64;
        if len == 126 {
            let mut e = [0u8; 2];
            s.read_exact(&mut e).await.map_err(|e| e.to_string())?;
            len = u16::from_be_bytes(e) as u64;
        } else if len == 127 {
            let mut e = [0u8; 8];
            s.read_exact(&mut e).await.map_err(|e| e.to_string())?;
            len = u64::from_be_bytes(e);
        }
        if len > 1 << 20 {
            return Err(format!("a {len}-byte frame - not the position stream"));
        }
        let mut mask = [0u8; 4];
        if masked {
            s.read_exact(&mut mask).await.map_err(|e| e.to_string())?;
        }
        let mut data = vec![0u8; len as usize];
        s.read_exact(&mut data).await.map_err(|e| e.to_string())?;
        if masked {
            for (i, d) in data.iter_mut().enumerate() {
                *d ^= mask[i % 4];
            }
        }
        match opcode {
            0x1 => {
                let text = String::from_utf8_lossy(&data);
                if let Some((pos, yaw, dim)) = parse_pos(&text) {
                    let mut v = ctx.controller.lock();
                    v.pos = Some((pos, yaw, dim, Instant::now()));
                }
            }
            0x8 => return Ok(()),
            // Ping: a masked pong back (clients always mask).
            0x9 => {
                let mut f = vec![0x8a, 0x80 | data.len().min(125) as u8, 0, 0, 0, 0];
                f.extend_from_slice(&data[..data.len().min(125)]);
                s.write_all(&f).await.map_err(|e| e.to_string())?;
            }
            _ => {}
        }
    }
}

/// "POS|x|y|z|yaw|dim|ms".
pub fn parse_pos(text: &str) -> Option<(Vec3, f32, String)> {
    let p: Vec<&str> = text.trim().split('|').collect();
    if p.first() != Some(&"POS") || p.len() < 6 {
        return None;
    }
    let pos = Vec3::new(num(&p, 1)?, num(&p, 2)?, num(&p, 3)?);
    Some((pos, num(&p, 4)? as f32, p[5].to_string()))
}

/// For the log: a sync's fields without its block lists.
pub fn short(raw: &str) -> String {
    if raw.len() <= 160 { raw.to_string() } else { format!("{}... ({} chars)", &raw[..160], raw.len()) }
}

/// A warning once per distinct text (the controller may send the same unsupported thing often).
pub fn warn_once(seen: &Mutex<std::collections::HashSet<String>>, text: &str) {
    if seen.lock().insert(text.to_string()) {
        warn!(target: "controller", "{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_java_controllers_lines() {
        assert_eq!(
            parse("COME_HERE|100.5|64.0|-20.25|minecraft:overworld|320.0"),
            Inbound::ComeHere { pos: Vec3::new(100.5, 64.0, -20.25), dim: "minecraft:overworld".into() }
        );
        assert!(matches!(parse("GOTO|1|2|3|minecraft:overworld"), Inbound::Goto { .. }));
        let sn = parse("STAY_NEAR|10|70|10|minecraft:overworld|3.0|3.0|SomePlayer|2|0|1|0|0|0");
        match sn {
            Inbound::StayNear { follow, offsets: Some(o), hold, .. } => {
                assert_eq!(follow.as_deref(), Some("SomePlayer"));
                assert_eq!((o.front, o.left, hold), (2.0, 1.0, 3.0));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse("STAY_NEAR|10|70|10|minecraft:overworld|3.0|3.0"), Inbound::StayNear { follow: None, offsets: None, .. }));
        assert_eq!(parse("MODULE|kill-aura|true"), Inbound::Module { name: "kill-aura".into(), on: true });
        assert_eq!(parse("SAY|/home a|b"), Inbound::Say("/home a|b".into()));
        assert!(matches!(parse("NUKER_SYNC|ClearBox|Whitelist|5"), Inbound::Sync { ref kind, .. } if kind == "NUKER_SYNC"));
        assert!(matches!(parse("COME_HERE|x|y"), Inbound::Unknown(_)));
    }

    #[test]
    fn status_is_the_extended_java_form() {
        let s = status_line(Vec3::new(1.5, 64.0, -3.0), "minecraft:overworld", "idle", false, false, false, 20, 18);
        let p: Vec<&str> = s.split('|').collect();
        assert_eq!(p.len(), 13);
        assert_eq!((p[0], p[1], p[2], p[5], p[10], p[11]), ("STATUS", "1.5", "64", "idle", "20", "18"));
    }

    #[test]
    fn reads_the_position_stream() {
        let (p, yaw, dim) = parse_pos("POS|10.0000|65.5000|-3.2500|90.00|minecraft:overworld|1739").unwrap();
        assert_eq!((p, yaw, dim.as_str()), (Vec3::new(10.0, 65.5, -3.25), 90.0, "minecraft:overworld"));
        assert!(parse_pos("HELLO|x").is_none());
    }
}
