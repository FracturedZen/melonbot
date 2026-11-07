//! The mapart bot's Discord side: a Discord bot account (its token in bots/<name>/discord-token.txt,
//! never printed) on Discord's gateway, watching one channel (#mapart). A .litematic, .schem or .nbt
//! dropped there by someone with the mapart role - or the server's owner - is downloaded and checked
//! (art.rs), and the answer goes back as a reply with a Name it button: it opens a form for the
//! map's name, and once named its maps are queued (queue.rs). The builder's progress is posted there
//! too (Poster). Text commands from the same people: "queue", "cancel <n>", "help".
//!
//! The gateway by hand (Discord's v10 protocol): HELLO, IDENTIFY - or RESUME after a dropped link -
//! heartbeats at the interval it asks for, the events (READY, GUILD_CREATE, MESSAGE_CREATE,
//! INTERACTION_CREATE for the button and the form), and a reconnect whenever the link drops or
//! Discord asks for one.

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use super::{
    Mapart,
    art::{self, grouped},
    now_ms,
    queue::{Job, JobState},
    safe_name,
};

const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const API: &str = "https://discord.com/api/v10";
/// GUILDS (the server's owner and channels), GUILD_MESSAGES, MESSAGE_CONTENT (the files on them).
const INTENTS: u64 = 1 | 1 << 9 | 1 << 15;
/// A mapart file is ~100 KB; nothing near this.
const MAX_FILE: u64 = 8 << 20;
const FILE_KINDS: [&str; 3] = ["litematic", "schem", "nbt"];
/// Discord's message length limit, with room to spare.
const MAX_TEXT: usize = 1900;

/// Who and where: from config.json, the token from its own file.
#[derive(Clone)]
pub struct Settings {
    pub token: String,
    pub guild: String,
    pub channel: String,
    pub role: String,
    pub dir: PathBuf,
}

/// Posts to the mapart channel (answers, the builder's progress).
#[derive(Clone)]
pub struct Poster {
    http: reqwest::Client,
    token: Arc<String>,
    channel: String,
}

impl Poster {
    pub fn new(token: &str, channel: &str) -> Self {
        let http = reqwest::Client::builder()
            .user_agent("DiscordBot (https://github.com/azalea-rs/azalea, 0.1)")
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        Self { http, token: Arc::new(token.to_string()), channel: channel.to_string() }
    }

    /// Post `text` (as a reply to message `reply_to`, if given). Pings nobody.
    pub async fn send(&self, text: &str, reply_to: Option<&str>) -> Result<(), String> {
        if public(text).is_empty() {
            return Ok(());
        }
        self.send_body(message(text, reply_to, None)).await
    }

    /// Post a message body (message()). Retries after Discord's rate limit.
    pub async fn send_body(&self, body: Value) -> Result<(), String> {
        for _ in 0..3 {
            let r = self
                .http
                .post(format!("{API}/channels/{}/messages", self.channel))
                .header("Authorization", format!("Bot {}", self.token))
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("could not reach Discord: {e}"))?;
            let status = r.status();
            if status.as_u16() == 429 {
                let wait = r.json::<Value>().await.ok().and_then(|v| v["retry_after"].as_f64()).unwrap_or(1.0);
                tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.1, 30.0))).await;
                continue;
            }
            if !status.is_success() {
                return Err(format!("Discord said {status}: {}", cut(&r.text().await.unwrap_or_default(), 300)));
            }
            return Ok(());
        }
        Err("Discord kept rate-limiting the post".into())
    }

    /// Answer an interaction (a button press, a form sent) - within Discord's 3 seconds.
    pub async fn respond(&self, id: &str, token: &str, body: Value) -> Result<(), String> {
        let r = self
            .http
            .post(format!("{API}/interactions/{id}/{token}/callback"))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("could not reach Discord: {e}"))?;
        if r.status().is_success() { Ok(()) } else { Err(format!("Discord said {}: {}", r.status(), cut(&r.text().await.unwrap_or_default(), 300))) }
    }

    /// Fire and forget, logged if it fails.
    pub fn post(&self, text: String) {
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.send(&text, None).await {
                warn!(target: "mapart", "Discord post failed: {e}");
            }
        });
    }
}

static LABELLED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b[xyz]\s*[=:]?\s*-?\d{2,}(?:\.\d+)?").unwrap());
static GROUP: LazyLock<Regex> = LazyLock::new(|| {
    let num = r"-?\d+(?:\.\d+)?";
    Regex::new(&format!(r"{num}(?:(?:\s+(?:to\s+|and\s+)?|\s*[,/]\s+|\s*,\s*(?:-))\s*{num}){{1,2}}")).unwrap()
});
static THREE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|[^\d.])\d{3,}").unwrap());
static LONG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{4,}").unwrap());
/// A bracketed part: "(225 froglights there, x 412952 to 413064)", "Vec3 { x: 1.5, ... }".
static BRACKETED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*(?:Vec3\s*|BlockPos\s*)?(?:\([^()]*\)|\{[^{}]*\}|\[[^\[\]]*\])").unwrap());
/// Where a clause ends: ": ", "; ", ", ", " - ", ". ".
static CLAUSE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:[:;,.]\s+|\s+-\s+)").unwrap());

/// Something in it reads as a position: an x, y or z with a number, two or three numbers in a row
/// one of which has 3+ digits, or any run of 4+ digits (counts are written grouped, "6,957").
fn has_position(s: &str) -> bool {
    LABELLED.is_match(s) || LONG.is_match(s) || GROUP.find_iter(s).any(|m| THREE.is_match(m.as_str()))
}

/// What may be said on Discord: never where anything is (6 Nov, the user: "the discord bot leaked
/// the coords it was working on. it should never do that ever" - and "not blurred out not x'd out.
/// just removed gone"). Every message the bot sends goes through this, whatever wrote it: a
/// bracketed part with a position in it goes, then every clause with one (between ": ", ", ",
/// " - "...), whole - no placeholder is left. Empty when nothing is left: then nothing is sent.
pub fn public(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        let line = BRACKETED.replace_all(line, |c: &regex::Captures| if has_position(&c[0]) { String::new() } else { c[0].to_string() });
        // Each position marked whole first - "-1200, 70, 455" is one, though it has commas in it.
        const MARK: &str = "\u{1}";
        let line = GROUP.replace_all(&line, |c: &regex::Captures| if THREE.is_match(&c[0]) { MARK.to_string() } else { c[0].to_string() });
        let line = LABELLED.replace_all(&line, MARK);
        let line = LONG.replace_all(&line, MARK).into_owned();
        let mut out = String::new();
        let mut last = 0;
        let mut sep = "";
        let mut ends: Vec<(usize, usize)> = CLAUSE_END.find_iter(&line).map(|m| (m.start(), m.end())).collect();
        ends.push((line.len(), line.len()));
        for (start, end) in ends {
            let clause = &line[last..start];
            if !clause.trim().is_empty() && !clause.contains(MARK) {
                if !out.is_empty() {
                    out += sep;
                }
                out += clause;
            }
            sep = &line[start..end];
            last = end;
        }
        let out = out.trim();
        if !out.is_empty() {
            lines.push(out.to_string());
        }
    }
    lines.join("\n")
}

/// A message: `text`, as a reply to `reply_to`, with a button (label, custom id). Pings nobody.
pub fn message(text: &str, reply_to: Option<&str>, button: Option<(&str, &str)>) -> Value {
    let mut body = json!({"content": cut(&public(text), MAX_TEXT), "allowed_mentions": {"parse": []}});
    if let Some(id) = reply_to {
        body["message_reference"] = json!({"message_id": id, "fail_if_not_exists": false});
    }
    if let Some((label, custom_id)) = button {
        body["components"] = json!([{"type": 1, "components": [{"type": 2, "style": 1, "label": label, "custom_id": custom_id}]}]);
    }
    body
}

/// An answer only the one who pressed sees.
fn quiet(text: &str) -> Value {
    json!({"type": 4, "data": {"content": cut(&public(text), MAX_TEXT), "flags": 64, "allowed_mentions": {"parse": []}}})
}

/// The Name it form: one box, as long as the anvil name leaves room for.
fn name_form(group: &str, room: usize) -> Value {
    json!({"type": 9, "data": {
        "custom_id": format!("named:{group}"),
        "title": "Name the map",
        "components": [{"type": 1, "components": [{
            "type": 4, "custom_id": "name", "label": "Map name", "style": 1,
            "min_length": 1, "max_length": room.max(1), "required": true, "placeholder": "Castle",
        }]}],
    }})
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max).collect::<String>() + "..." }
}

// ------------------------------------------------------------------ the gateway

/// A session to resume after a dropped link.
struct Session {
    id: String,
    resume_url: String,
    seq: Option<u64>,
}

enum Stop {
    /// Try again (after a pause).
    Retry(String),
    /// Nothing to be gained by trying again - said once, and the Discord side stops.
    Fatal(String),
}

/// Run the Discord side for good: connect, and reconnect whenever the link drops.
pub async fn run(m: Arc<Mapart>, s: Settings) {
    let poster = Poster::new(&s.token, &s.channel);
    *m.poster.lock() = Some(poster.clone());
    let owner: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut session: Option<Session> = None;
    let mut pause = Duration::from_secs(1);
    loop {
        let began = tokio::time::Instant::now();
        match connect_once(&m, &s, &poster, &owner, &mut session).await {
            Stop::Fatal(why) => {
                error!(target: "mapart", "Discord: {why} - the mapart bot takes no maps until this is fixed and it is restarted");
                return;
            }
            Stop::Retry(why) => {
                // A link that held a while ended the ordinary way (Discord asks for a reconnect
                // every hour or so): back in a second. Only drops one after another wait longer
                // (6 Nov: 1, 2, 4, 8, 16 s on five hourly reconnects that each resumed fine).
                if began.elapsed() > Duration::from_secs(60) {
                    pause = Duration::from_secs(1);
                }
                info!(target: "mapart", "Discord: {why} - reconnecting in {} s", pause.as_secs());
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn connect_once(m: &Arc<Mapart>, s: &Settings, poster: &Poster, owner: &Arc<Mutex<Option<String>>>, session: &mut Option<Session>) -> Stop {
    let url = session.as_ref().map_or(GATEWAY.to_string(), |s| format!("{}/?v=10&encoding=json", s.resume_url.trim_end_matches('/')));
    let ws = match tokio::time::timeout(Duration::from_secs(30), tokio_tungstenite::connect_async(url.as_str())).await {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => return Stop::Retry(format!("could not connect ({e})")),
        Err(_) => return Stop::Retry("connecting timed out".into()),
    };
    let (mut tx, mut rx) = ws.split();
    // HELLO: how often to heartbeat.
    let hello = match tokio::time::timeout(Duration::from_secs(20), rx.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<Value>(&t).unwrap_or_default(),
        _ => return Stop::Retry("no hello from Discord".into()),
    };
    let Some(every) = hello["d"]["heartbeat_interval"].as_u64() else { return Stop::Retry("a hello without a heartbeat interval".into()) };
    let hello_msg = match session {
        Some(se) => json!({"op": 6, "d": {"token": s.token, "session_id": se.id, "seq": se.seq}}),
        None => json!({"op": 2, "d": {"token": s.token, "intents": INTENTS, "properties": {"os": "windows", "browser": "melonbot", "device": "melonbot"}}}),
    };
    if let Err(e) = tx.send(Message::text(hello_msg.to_string())).await {
        return Stop::Retry(format!("could not sign in ({e})"));
    }
    let every = Duration::from_millis(every);
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + every / 2, every);
    let mut acked = true;
    loop {
        tokio::select! {
            _ = beat.tick() => {
                if !acked {
                    return Stop::Retry("Discord stopped answering heartbeats".into());
                }
                acked = false;
                let seq = session.as_ref().and_then(|s| s.seq);
                if let Err(e) = tx.send(Message::text(json!({"op": 1, "d": seq}).to_string())).await {
                    return Stop::Retry(format!("heartbeat failed ({e})"));
                }
            }
            msg = rx.next() => {
                let text = match msg {
                    None => return Stop::Retry("the link closed".into()),
                    Some(Err(e)) => return Stop::Retry(format!("the link broke ({e})")),
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(f))) => return closed(f.map(|f| u16::from(f.code)), session),
                    Some(Ok(_)) => continue,
                };
                let v: Value = serde_json::from_str(&text).unwrap_or_default();
                if let (Some(n), Some(se)) = (v["s"].as_u64(), session.as_mut()) {
                    se.seq = Some(n);
                }
                match v["op"].as_u64() {
                    Some(0) => {
                        let seq = v["s"].as_u64();
                        dispatch(m, s, poster, owner, session, v["t"].as_str().unwrap_or(""), &v["d"], seq);
                    }
                    Some(1) => {
                        let seq = session.as_ref().and_then(|s| s.seq);
                        let _ = tx.send(Message::text(json!({"op": 1, "d": seq}).to_string())).await;
                    }
                    Some(7) => return Stop::Retry("Discord asked for a reconnect".into()),
                    Some(9) => {
                        // Not resumable: a fresh sign-in after a short wait, as Discord asks.
                        if !v["d"].as_bool().unwrap_or(false) {
                            *session = None;
                        }
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        return Stop::Retry("the session ended".into());
                    }
                    Some(11) => acked = true,
                    _ => {}
                }
            }
        }
    }
}

/// What a close code from Discord means for us.
fn closed(code: Option<u16>, session: &mut Option<Session>) -> Stop {
    match code {
        Some(4004) => Stop::Fatal("the bot token was refused - put the right one in discord-token.txt".into()),
        Some(4014) => Stop::Fatal("Discord refused the Message Content intent - turn on 'Message Content Intent' (Developer Portal > the app > Bot)".into()),
        Some(4010..=4013) => Stop::Fatal(format!("Discord refused the sign-in (code {})", code.unwrap_or(0))),
        Some(4007) | Some(4009) => {
            *session = None;
            Stop::Retry("the session timed out".into())
        }
        c => Stop::Retry(format!("Discord closed the link (code {})", c.map_or("none".into(), |c| c.to_string()))),
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(m: &Arc<Mapart>, s: &Settings, poster: &Poster, owner: &Arc<Mutex<Option<String>>>, session: &mut Option<Session>, kind: &str, d: &Value, seq: Option<u64>) {
    match kind {
        "READY" => {
            *session = Some(Session {
                id: d["session_id"].as_str().unwrap_or_default().to_string(),
                resume_url: d["resume_gateway_url"].as_str().unwrap_or("wss://gateway.discord.gg").to_string(),
                seq,
            });
            info!(target: "mapart", "Discord: connected as {}", d["user"]["username"].as_str().unwrap_or("?"));
        }
        "RESUMED" => info!(target: "mapart", "Discord: link resumed"),
        "GUILD_CREATE" if d["id"].as_str() == Some(s.guild.as_str()) => {
            *owner.lock() = d["owner_id"].as_str().map(str::to_string);
            let has_channel = d["channels"].as_array().is_some_and(|c| c.iter().any(|c| c["id"].as_str() == Some(s.channel.as_str())));
            if has_channel {
                info!(target: "mapart", "Discord: in {} - watching its mapart channel", d["name"].as_str().unwrap_or("the server"));
            } else {
                warn!(target: "mapart", "Discord: the mapart channel ({}) is not in {} any more", s.channel, d["name"].as_str().unwrap_or("the server"));
            }
        }
        "MESSAGE_CREATE" => {
            let msg = read_message(d, &s.channel, &s.role, owner.lock().as_deref());
            if !matches!(msg, Incoming::Ignore) {
                let (m, s, poster, d) = (m.clone(), s.clone(), poster.clone(), d.clone());
                tokio::spawn(async move { on_message(&m, &s, &poster, &d, msg).await });
            }
        }
        "INTERACTION_CREATE" => {
            let press = read_interaction(d, &s.role, owner.lock().as_deref());
            if !matches!(press, Press::Ignore) {
                let (m, poster, d) = (m.clone(), poster.clone(), d.clone());
                tokio::spawn(async move { on_interaction(&m, &poster, &d, press).await });
            }
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ messages

#[derive(Debug, PartialEq)]
pub enum Incoming {
    Ignore,
    /// Someone without the role sent a file or a command.
    NotAllowed,
    /// Files, but none of a kind that can be read.
    WrongKind(Vec<String>),
    /// The files to check and queue: (name, size, url).
    Files(Vec<(String, u64, String)>),
    Command(String),
}

/// What a message in the channel asks for, and whether its sender may: someone with the role, or
/// the server's owner.
pub fn read_message(d: &Value, channel: &str, role: &str, owner: Option<&str>) -> Incoming {
    if d["channel_id"].as_str() != Some(channel) || d["author"]["bot"].as_bool() == Some(true) {
        return Incoming::Ignore;
    }
    let allowed = d["member"]["roles"].as_array().is_some_and(|r| r.iter().any(|r| r.as_str() == Some(role))) || (owner.is_some() && d["author"]["id"].as_str() == owner);
    let attachments: Vec<(String, u64, String)> = d["attachments"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|f| (f["filename"].as_str().unwrap_or("file").to_string(), f["size"].as_u64().unwrap_or(0), f["url"].as_str().unwrap_or_default().to_string()))
                .collect()
        })
        .unwrap_or_default();
    let words = d["content"].as_str().unwrap_or("").trim().to_lowercase();
    let command = words.split_whitespace().next().is_some_and(|w| matches!(w, "queue" | "cancel" | "help"));
    if attachments.is_empty() && !command {
        return Incoming::Ignore;
    }
    if !allowed {
        return Incoming::NotAllowed;
    }
    if !attachments.is_empty() {
        let (files, other): (Vec<_>, Vec<_>) = attachments.into_iter().partition(|(name, _, _)| kind_of(name).is_some());
        return if files.is_empty() { Incoming::WrongKind(other.into_iter().map(|f| f.0).collect()) } else { Incoming::Files(files) };
    }
    Incoming::Command(words)
}

/// "nbt" for "My Map.NBT"; None for anything that is not a schematic.
fn kind_of(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_lowercase();
    FILE_KINDS.into_iter().find(|k| *k == ext)
}

async fn on_message(m: &Arc<Mapart>, s: &Settings, poster: &Poster, d: &Value, msg: Incoming) {
    let id = d["id"].as_str().unwrap_or_default().to_string();
    let who = d["member"]["nick"].as_str().or(d["author"]["global_name"].as_str()).or(d["author"]["username"].as_str()).unwrap_or("someone").to_string();
    let answer = match msg {
        Incoming::Ignore => return,
        Incoming::NotAllowed => "Only people with the **mapart** role can send maps here.".to_string(),
        Incoming::WrongKind(names) => format!("{} - send the map as a .litematic, .schem or .nbt file.", names.join(", ")),
        Incoming::Command(words) => command(m, &words),
        Incoming::Files(files) => {
            // An answer each, each with its own Name it button.
            let many = files.len() > 1;
            for (k, (name, size, url)) in files.into_iter().enumerate() {
                let group = if many && k > 0 { format!("{id}.{}", k + 1) } else { id.clone() };
                let (text, ok) = take_file(m, s, poster, &id, &group, &who, &name, size, &url).await;
                let button = format!("name:{group}");
                let body = message(&text, Some(&id), ok.then_some(("Name it", button.as_str())));
                if let Err(e) = poster.send_body(body).await {
                    warn!(target: "mapart", "could not answer on Discord: {e}");
                }
            }
            return;
        }
    };
    if let Err(e) = poster.send(&answer, Some(&id)).await {
        warn!(target: "mapart", "could not answer on Discord: {e}");
    }
}

/// Download and check one file, and keep its maps waiting for their name. The answer for it, and
/// whether it passed (the answer then carries the Name it button).
#[allow(clippy::too_many_arguments)]
async fn take_file(m: &Arc<Mapart>, s: &Settings, poster: &Poster, id: &str, group: &str, who: &str, name: &str, size: u64, url: &str) -> (String, bool) {
    if size > MAX_FILE {
        return (format!("❌ **{name}**: {} MB is far too big for map art - is it the right file?", size >> 20), false);
    }
    let bytes = match poster.http.get(url).send().await {
        Ok(r) if r.status().is_success() => match r.bytes().await {
            Ok(b) => b,
            Err(e) => return (format!("❌ **{name}**: the download broke off ({e}) - send it again"), false),
        },
        Ok(r) => return (format!("❌ **{name}**: Discord would not hand the file over ({})", r.status()), false),
        Err(e) => return (format!("❌ **{name}**: could not download it ({e})"), false),
    };
    let maps = s.dir.join("maps");
    let _ = fs::create_dir_all(&maps);
    let file = format!("{}-{}", group.replace('.', "_"), safe_name(name));
    let path = maps.join(&file);
    if let Err(e) = fs::write(&path, &bytes) {
        warn!(target: "mapart", "could not keep {}: {e}", path.display());
        return (format!("❌ **{name}**: the bot could not save it ({e})"), false);
    }
    let checked = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || art::read(&path)).await.unwrap_or_else(|e| Err(format!("the check crashed ({e})")))
    };
    match checked {
        Ok(a) => {
            // One entry per map, from the north-west map along each row; in line once named.
            let at = now_ms();
            let single = a.maps.len() == 1;
            let maps: Vec<Job> = a
                .maps
                .iter()
                .map(|map| Job {
                    id: if single { group.to_string() } else { format!("{group}-{}-{}", map.col, map.row) },
                    group: group.to_string(),
                    msg: id.to_string(),
                    name: name.to_string(),
                    given: String::new(),
                    map: (map.col, map.row),
                    of: (a.cols, a.rows),
                    file: format!("maps/{file}"),
                    by: who.to_string(),
                    at_ms: at,
                    carpets: map.carpets(),
                    state: JobState::Pending,
                    note: String::new(),
                })
                .collect();
            m.queue.lock().add_pending(maps);
            info!(target: "mapart", "checked {name} from {who}: {} map(s), {} carpets - waiting for its name", a.maps.len(), a.carpets());
            (format!("✅ **{name}**: {}\n{NAME_IT}", a.summary()), true)
        }
        Err(why) => {
            let _ = fs::remove_file(&path);
            info!(target: "mapart", "refused {name} from {who}: {why}");
            (format!("❌ **{name}**: {why}"), false)
        }
    }
}

/// The last line of a checked file's answer, until it is named.
const NAME_IT: &str = "Press **Name it** to give it its name - then it is queued.";

// ------------------------------------------------------------------ the Name it button and form

#[derive(Debug, PartialEq)]
pub enum Press {
    Ignore,
    NotAllowed,
    /// The Name it button of the file in this group.
    NameIt(String),
    /// The form sent: the group, the name typed.
    Named(String, String),
}

/// What a button press or a sent form is, and whether its sender may (the role, or the owner).
pub fn read_interaction(d: &Value, role: &str, owner: Option<&str>) -> Press {
    let custom = d["data"]["custom_id"].as_str().unwrap_or("");
    let (kind, group) = custom.split_once(':').unwrap_or(("", ""));
    let wanted = matches!((d["type"].as_u64(), kind), (Some(3), "name") | (Some(5), "named"));
    if !wanted || group.is_empty() {
        return Press::Ignore;
    }
    let user = d["member"]["user"]["id"].as_str().or(d["user"]["id"].as_str());
    let allowed = d["member"]["roles"].as_array().is_some_and(|r| r.iter().any(|r| r.as_str() == Some(role))) || (owner.is_some() && user == owner);
    if !allowed {
        return Press::NotAllowed;
    }
    if kind == "name" {
        return Press::NameIt(group.to_string());
    }
    // The box's value: in an action row (the classic form), or in a label (the newer one).
    let rows = d["data"]["components"].as_array().cloned().unwrap_or_default();
    let value = rows
        .iter()
        .flat_map(|r| r["components"].as_array().cloned().unwrap_or_default().into_iter().chain(std::iter::once(r["component"].clone())))
        .find(|c| c["custom_id"].as_str() == Some("name"))
        .and_then(|c| c["value"].as_str().map(str::to_string))
        .unwrap_or_default();
    Press::Named(group.to_string(), clean_name(&value))
}

/// A name as it goes on the anvil: one line, single spaces, no double quotes (the anvil name
/// quotes it).
fn clean_name(s: &str) -> String {
    s.replace('"', "'").split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn on_interaction(m: &Arc<Mapart>, poster: &Poster, d: &Value, press: Press) {
    let (id, token) = (d["id"].as_str().unwrap_or_default(), d["token"].as_str().unwrap_or_default());
    let who = d["member"]["nick"].as_str().or(d["member"]["user"]["global_name"].as_str()).or(d["member"]["user"]["username"].as_str()).unwrap_or("someone").to_string();
    let body = match press {
        Press::Ignore => return,
        Press::NotAllowed => quiet("Only people with the **mapart** role can name and queue maps."),
        Press::NameIt(group) => match m.queue.lock().pending(&group) {
            Some(of) => name_form(&group, super::queue::name_room(of)),
            None => quiet("That one has been named already (or it is gone)."),
        },
        Press::Named(group, name) => {
            let named = m.queue.lock().name(&group, &name, now_ms());
            match named {
                Err(e) => quiet(&format!("Not named: {e}.")),
                Ok((first, last)) => {
                    info!(target: "mapart", "{who} named {group} \"{name}\" - #{first}..#{last} in line");
                    let line = match (first == last, first) {
                        (true, 1) => "queued - it is next".to_string(),
                        (true, n) => format!("queued - #{n} in line"),
                        (false, 1) => format!("queued as #1-#{last} - the first is next"),
                        _ => format!("queued as #{first}-#{last}"),
                    };
                    // The answer it was pressed on, its button gone and the naming in its place.
                    let before = d["message"]["content"].as_str().unwrap_or("").replace(NAME_IT, "");
                    let text = format!("{}\nNamed **{name}** by {who} - {line}", before.trim_end());
                    json!({"type": 7, "data": {"content": cut(&text, MAX_TEXT), "components": [], "allowed_mentions": {"parse": []}}})
                }
            }
        }
    };
    if let Err(e) = poster.respond(id, token, body).await {
        warn!(target: "mapart", "could not answer the button on Discord: {e}");
    }
}

/// "queue", "cancel <n>", "help".
fn command(m: &Arc<Mapart>, words: &str) -> String {
    let mut parts = words.split_whitespace();
    match parts.next() {
        Some("queue") => {
            let q = m.queue.lock();
            let w = q.waiting();
            if w.is_empty() {
                return "The queue is empty - drop a .litematic, .schem or .nbt here.".into();
            }
            let lines: Vec<String> = w
                .iter()
                .enumerate()
                .map(|(i, j)| format!("{}. **{}** - from {}, {} carpets{}", i + 1, j.title(), j.by, grouped(j.carpets), if j.state == JobState::Building { " - **being built**" } else { "" }))
                .collect();
            lines.join("\n")
        }
        Some("cancel") => match parts.next().and_then(|n| n.trim_start_matches('#').parse::<usize>().ok()) {
            None => "Say which one: `cancel 2` (the numbers are in `queue`).".into(),
            Some(n) => match m.queue.lock().cancel(n) {
                Ok(j) => format!("Cancelled **{}**.", j.title()),
                Err(e) => format!("Not cancelled: {e}."),
            },
        },
        _ => "Drop a .litematic, .schem or .nbt mapart here: flat carpet map art, 1x1 or bigger (a 2x2 is built as its 4 maps, one after another; \
              the cobble layer and extra line are left out). It is checked; press **Name it** on the answer to name it, and it is queued and built in order \
              (named 1X1 \"name\", or 2X2 \"name\" c1r1 ... for a bigger one). `queue` lists what is waiting; `cancel 2` takes #2 out (not the one being built)."
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines that went out on 6 Nov, and ones like them: no position survives.
    #[test]
    fn no_position_ever_reaches_discord() {
        for leak in [
            "Site:\nart 412936 123 286616 to 413063 123 286743 (carpet layer y 123), site box 412920 120 286597 to 413079 125 286758",
            "cartography table 412998 124 286606, anvil 412997 125 286606, water button 412998 124 286610, bed 413002 124 286610",
            "1X1 \"MELON\": waiting - stuck laying green carpet at 413055 123 286695",
            "Cannot find the mapart site: y 123: not all four sides of a ring in sight (225 froglights there, x 412952 to 413064, z 286632 to 286744)",
            "could not reach Vec3 { x: 412992.5, y: 123.0, z: 286609.5 }",
            "the chest at 120 64 -340 did not open",
            "near -1200, 70, 455",
            "x=-45 z=88",
        ] {
            let out = public(leak);
            assert!(!has_position(&out) && !out.contains("hidden"), "{leak}\n-> {out}");
            for n in ["4129", "4130", "2866", "2867", "123", "124", "125", "120 64", "340", "1200", "455", "45", "88"] {
                assert!(!out.contains(n), "{n} survived: {leak}\n-> {out}");
            }
        }
    }

    #[test]
    fn what_is_left_still_reads() {
        assert_eq!(public("1X1 \"MELON\": waiting - stuck laying green carpet at 413055 123 286695"), "1X1 \"MELON\": waiting");
        assert_eq!(
            public("Cannot find the mapart site: y 123: not all four sides of a ring in sight (225 froglights there, x 412952 to 413064, z 286632 to 286744)"),
            "Cannot find the mapart site: not all four sides of a ring in sight"
        );
        assert_eq!(public("could not reach the anvil: GAVE UP on 'chest' to 412992.5 123.0 286609.5: no clear way through"), "could not reach the anvil: no clear way through");
        assert_eq!(public("at 412936 123 286616"), "");
    }

    #[test]
    fn counts_and_names_are_still_said() {
        for ok in [
            "Building 1X1 \"MELON\" - 16,384 carpets, sent by SomePlayer.",
            "green 6,957 (109), white 4,112 (65)",
            "Carpet chests for 16 colours: RED (48 chests), LIGHT BLUE (48 chests).",
            "2X2 \"Castle\" c1r2 - map 2 of 4",
            "3 carpet(s) could not be laid",
        ] {
            assert_eq!(public(ok), ok);
        }
    }

    fn msg(channel: &str, roles: &[&str], author: &str, content: &str, files: &[&str]) -> Value {
        json!({
            "id": "1", "channel_id": channel, "content": content,
            "author": {"id": author, "username": "u"},
            "member": {"roles": roles},
            "attachments": files.iter().map(|f| json!({"filename": f, "size": 1000, "url": "https://cdn/x"})).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn only_the_role_or_the_owner_may_send_maps() {
        let file = msg("C", &["R"], "A", "", &["map.nbt"]);
        assert!(matches!(read_message(&file, "C", "R", None), Incoming::Files(f) if f.len() == 1));
        assert_eq!(read_message(&msg("C", &["X"], "A", "", &["map.nbt"]), "C", "R", None), Incoming::NotAllowed);
        assert!(matches!(read_message(&msg("C", &[], "OWNER", "", &["map.litematic"]), "C", "R", Some("OWNER")), Incoming::Files(_)));
        // Another channel, or plain chat: not ours.
        assert_eq!(read_message(&msg("D", &["R"], "A", "", &["map.nbt"]), "C", "R", None), Incoming::Ignore);
        assert_eq!(read_message(&msg("C", &[], "A", "nice map!", &[]), "C", "R", None), Incoming::Ignore);
    }

    #[test]
    fn files_and_commands_are_told_apart() {
        assert_eq!(read_message(&msg("C", &["R"], "A", "", &["picture.png"]), "C", "R", None), Incoming::WrongKind(vec!["picture.png".into()]));
        assert_eq!(read_message(&msg("C", &["R"], "A", "  Cancel 2", &[]), "C", "R", None), Incoming::Command("cancel 2".into()));
        assert_eq!(read_message(&msg("C", &[], "A", "queue", &[]), "C", "R", None), Incoming::NotAllowed);
        let mut bot = msg("C", &["R"], "A", "queue", &[]);
        bot["author"]["bot"] = json!(true);
        assert_eq!(read_message(&bot, "C", "R", None), Incoming::Ignore, "its own posts");
        assert_eq!(kind_of("My Map.LITEMATIC"), Some("litematic"));
        assert_eq!(kind_of("map.zip"), None);
        assert_eq!(safe_name("../My Map (2).nbt"), "_My_Map__2_.nbt");
    }

    fn press(kind: u64, custom: &str, roles: &[&str], user: &str, value: Option<&str>) -> Value {
        let mut d = json!({"type": kind, "id": "I", "token": "T", "data": {"custom_id": custom}, "member": {"roles": roles, "user": {"id": user}}});
        if let Some(v) = value {
            d["data"]["components"] = json!([{"type": 1, "components": [{"type": 4, "custom_id": "name", "value": v}]}]);
        }
        d
    }

    #[test]
    fn the_name_it_button_and_its_form_are_read() {
        assert_eq!(read_interaction(&press(3, "name:123", &["R"], "A", None), "R", None), Press::NameIt("123".into()));
        assert_eq!(read_interaction(&press(3, "name:123", &[], "A", None), "R", None), Press::NotAllowed);
        assert_eq!(read_interaction(&press(3, "name:123", &[], "OWNER", None), "R", Some("OWNER")), Press::NameIt("123".into()));
        assert_eq!(
            read_interaction(&press(5, "named:123.2", &["R"], "A", Some("  My \"Big\"   Castle ")), "R", None),
            Press::Named("123.2".into(), "My 'Big' Castle".into())
        );
        // The newer form layout: the box inside a label.
        let mut d = press(5, "named:9", &["R"], "A", None);
        d["data"]["components"] = json!([{"type": 18, "component": {"type": 4, "custom_id": "name", "value": "Tree"}}]);
        assert_eq!(read_interaction(&d, "R", None), Press::Named("9".into(), "Tree".into()));
        assert_eq!(read_interaction(&press(3, "other:1", &["R"], "A", None), "R", None), Press::Ignore);
    }
}
