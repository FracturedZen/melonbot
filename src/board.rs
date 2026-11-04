//! MelonBoard: report the bot's melon statistics to a community leaderboard. Port of the Java
//! standalone's board/ (Board, BoardClient, MelonStats, BoardConfig). Off unless config.json names
//! the server it is for (`board_server`) and bots/<name>/melonboard.json has the board's
//! `endpoint` (its URL).
//!
//! As the Java client: ABSOLUTE totals from the server's own statistics (never counts of our own),
//! so a missed or repeated submission cannot skew a score. Every 3 minutes a REQUEST_STATS goes
//! out, 2 s later the totals are read and submitted if they moved; also on disconnect.
//!
//! Stats: after a join the server marks every stat dirty (PlayerList.placeNewPlayer ->
//! markAllDirty), so the first reply holds all of them; ServerStatsCounter.sendStats sends only
//! the dirty ones after that. So the replies are kept as one running map, cleared at each join.
//!
//! Ownership is proven with the chat-signing key Mojang certifies for this account (BoardClient's
//! handshake): sign the API's one-time challenge with the private key, send the public key and
//! Mojang's signature over it. The bearer token that comes back lasts a day.
//!
//! bots/<name>/melonboard.json keeps the token and the last totals sent (Java field names).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use azalea::{
    Client,
    account::Account,
    app::{App, Plugin, Update},
    ecs::prelude::*,
    packet::game::ReceiveGamePacketEvent,
    protocol::packets::game::{
        ClientboundGamePacket, ServerboundClientCommand,
        c_award_stats::{ClientboundAwardStats, Stat},
        s_client_command::Action,
    },
    registry::builtin::{BlockKind, CustomStat, ItemKind},
};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use parking_lot::Mutex;
use rsa::signature::{SignatureEncoding, Signer};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::{info, warn};

/// BoardClient.PROTOCOL: the API refuses anything older with a 426.
const PROTOCOL: u32 = 3;
/// After a join, before the first request, so login traffic settles first.
const JOIN_GRACE: Duration = Duration::from_secs(5);
/// After REQUEST_STATS, before trusting the totals.
const SETTLE: Duration = Duration::from_secs(2);
const PRESENCE_EVERY: Duration = Duration::from_secs(20);
const BACKOFF_MIN: Duration = Duration::from_secs(20);
const BACKOFF_MAX: Duration = Duration::from_secs(600);

/// melonboard.json, in the Java BoardConfig's field names.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct BoardFile {
    enabled: bool,
    endpoint: String,
    interval_minutes: u64,
    token: Option<String>,
    /// Epoch seconds.
    token_expires: i64,
    /// The account the token belongs to.
    token_uuid: Option<String>,
    /// Piston-broken melons: not a server statistic, and this bot runs no piston farm.
    afk_melons: i64,
    last_placed: i64,
    last_mined: i64,
    last_crafted: i64,
    last_planted: i64,
    last_afk: i64,
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

impl Default for BoardFile {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: String::new(),
            interval_minutes: 3,
            token: None,
            token_expires: 0,
            token_uuid: None,
            afk_melons: 0,
            last_placed: -1,
            last_mined: -1,
            last_crafted: -1,
            last_planted: -1,
            last_afk: -1,
            other: Default::default(),
        }
    }
}

impl BoardFile {
    fn token_valid(&self) -> bool {
        self.token.as_deref().is_some_and(|t| !t.is_empty()) && self.token_expires > epoch_secs() + 60
    }

    fn last(&self) -> Option<Totals> {
        (self.last_placed >= 0).then(|| Totals {
            placed: self.last_placed,
            mined: self.last_mined,
            crafted: self.last_crafted,
            planted: self.last_planted,
            play_time: 1,
            afk: self.last_afk.max(0),
        })
    }

    fn remember(&mut self, t: &Totals) {
        self.last_placed = t.placed;
        self.last_mined = t.mined;
        self.last_crafted = t.crafted;
        self.last_planted = t.planted;
        self.last_afk = t.afk;
    }
}

/// MelonStats: the four scored statistics plus play time as the liveness probe.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Totals {
    placed: i64,
    mined: i64,
    crafted: i64,
    planted: i64,
    play_time: i64,
    afk: i64,
}

impl Totals {
    fn read(stats: &HashMap<Stat, i32>, afk: i64) -> Self {
        let get = |s: Stat| stats.get(&s).copied().unwrap_or(0) as i64;
        Self {
            // ITEM_USED melon = a melon block placed (MELON is the block's item; slices are MELON_SLICE).
            placed: get(Stat::Used(ItemKind::Melon)),
            mined: get(Stat::Mined(BlockKind::Melon)),
            crafted: get(Stat::Crafted(ItemKind::Melon)),
            planted: get(Stat::Used(ItemKind::MelonSeeds)),
            play_time: get(Stat::Custom(CustomStat::PlayTime)),
            afk,
        }
    }

    /// Any time on the server gives a nonzero play time: zero means no stat map came, which is
    /// not "zero melons" (9 Oct: zeros sent as totals wiped a row on the board).
    fn live(&self) -> bool {
        self.play_time > 0
    }

    fn same_counts(&self, o: &Totals) -> bool {
        (self.placed, self.mined, self.crafted, self.planted, self.afk) == (o.placed, o.mined, o.crafted, o.planted, o.afk)
    }

    /// A total below what was last sent: the API would take it for a stats reset and lower the
    /// baseline, so it is never sent (the server's totals only grow).
    fn below(&self, o: &Totals) -> Option<&'static str> {
        [("placed", self.placed, o.placed), ("mined", self.mined, o.mined), ("crafted", self.crafted, o.crafted), ("planted", self.planted, o.planted)]
            .into_iter()
            .find(|(_, now, was)| now < was)
            .map(|(k, ..)| k)
    }
}

/// The server's stats replies, to the bot's board (the build has no packet events for the handler).
pub struct BoardPlugin;

impl Plugin for BoardPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, record_stats);
    }
}

fn record_stats(mut events: MessageReader<ReceiveGamePacketEvent>, bots: Query<&crate::bot::State>) {
    for ev in events.read() {
        let ClientboundGamePacket::AwardStats(p) = ev.packet.as_ref() else { continue };
        if let Some(b) = bots.get(ev.entity).ok().and_then(|s| s.board()) {
            b.on_packet(p);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// Not in the world.
    Idle,
    NeedAuth,
    Authing,
    Ready,
    Requested(Instant),
    Submitting,
    /// Something a quick retry will not fix; looked at again after BACKOFF_MAX.
    Halted(Instant),
}

struct St {
    file: BoardFile,
    stats: HashMap<Stat, i32>,
    phase: Phase,
    /// Not before this (backoff, or the next interval).
    next_at: Instant,
    next_presence: Instant,
    backoff: Duration,
    latest: Option<Totals>,
    points: i64,
    rank: i64,
}

#[derive(Clone)]
pub struct Board {
    path: PathBuf,
    http: reqwest::Client,
    st: Arc<Mutex<St>>,
}

impl Board {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("melonboard.json");
        let file = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                warn!("{}: unreadable ({e}) - starting from defaults", path.display());
                BoardFile::default()
            }),
            Err(_) => BoardFile::default(),
        };
        info!(
            "MelonBoard {} -> {}, every {} min",
            if file.endpoint.is_empty() { "OFF (no endpoint in melonboard.json)" } else if file.enabled { "on" } else { "OFF (melonboard.json enabled=false)" },
            file.endpoint,
            file.interval_minutes
        );
        let board = Self {
            path,
            http: reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default(),
            st: Arc::new(Mutex::new(St {
                file,
                stats: HashMap::new(),
                phase: Phase::Idle,
                next_at: Instant::now(),
                next_presence: Instant::now(),
                backoff: Duration::ZERO,
                latest: None,
                points: -1,
                rank: 0,
            })),
        };
        board.save();
        board
    }

    fn save(&self) {
        let json = serde_json::to_string_pretty(&self.st.lock().file);
        if let Err(e) = json.map_err(anyhow::Error::from).and_then(|j| Ok(std::fs::write(&self.path, j)?)) {
            warn!("could not write {}: {e}", self.path.display());
        }
    }

    /// "1.13M points, rank #2" for the status line; empty until the API has answered.
    pub fn standing(&self) -> String {
        let st = self.st.lock();
        if st.points < 0 {
            return String::new();
        }
        format!("{} points, rank #{}", st.points, st.rank)
    }

    pub fn on_join(&self) {
        let mut st = self.st.lock();
        st.stats.clear();
        st.latest = None;
        st.backoff = Duration::ZERO;
        st.phase = Phase::NeedAuth;
        st.next_at = Instant::now() + JOIN_GRACE;
        info!("joined - stats request in {} s", JOIN_GRACE.as_secs());
    }

    pub fn on_packet(&self, p: &ClientboundAwardStats) {
        let mut st = self.st.lock();
        let had = st.stats.len();
        st.stats.extend(p.stats.iter().map(|(k, v)| (*k, *v)));
        info!("stats packet: {} stat(s), {} known now (was {had})", p.stats.len(), st.stats.len());
    }

    /// Flush the last read totals (if they moved) - best effort, the next session catches up.
    pub fn on_leave(&self) {
        let (snap, token, endpoint) = {
            let mut st = self.st.lock();
            st.phase = Phase::Idle;
            let snap = st.latest.filter(|t| t.live() && st.file.last().is_none_or(|l| !t.same_counts(&l)));
            (snap, st.file.token.clone().filter(|_| st.file.token_valid()), st.file.endpoint.clone())
        };
        let (Some(snap), Some(token)) = (snap, token) else { return };
        if endpoint.is_empty() {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            match me.submit(&endpoint, &token, &snap).await {
                Ok(r) => {
                    me.st.lock().file.remember(&snap);
                    me.save();
                    info!("flushed on disconnect - {} points", r["points"]);
                }
                Err(e) => info!("flush on disconnect failed ({e}); the next session catches up"),
            }
        });
    }

    /// Never calls the client while holding the board's lock: record_stats takes that lock inside an
    /// ECS system, so holding it while waiting for the ECS would deadlock.
    pub fn tick(&self, bot: &Client) {
        let now = Instant::now();
        let auth = {
            let mut st = self.st.lock();
            if !st.file.enabled || st.file.endpoint.is_empty() || st.phase == Phase::Idle {
                return;
            }
            if st.file.token_valid() && now >= st.next_presence {
                st.next_presence = now + PRESENCE_EVERY;
                let (me, endpoint, token) = (self.clone(), st.file.endpoint.clone(), st.file.token.clone().unwrap_or_default());
                tokio::spawn(async move {
                    let _ = me.post(&endpoint, "/presence", serde_json::json!({}), Some(&token)).await;
                });
            }
            match st.phase {
                Phase::NeedAuth => now >= st.next_at,
                Phase::Ready if now >= st.next_at => {
                    st.phase = Phase::Requested(now);
                    drop(st);
                    bot.write_packet(ServerboundClientCommand { action: Action::RequestStats });
                    return;
                }
                _ => {
                    self.settle(&mut st, now);
                    return;
                }
            }
        };
        if !auth {
            return;
        }
        let Some(account) = bot.query_self::<&Account, _>(|a| a.clone()).ok() else { return };
        let mut st = self.st.lock();
        if st.phase != Phase::NeedAuth {
            return;
        }
        // A token saved for another account is dropped (BoardConfig.tokenUuid, 10 Oct).
        let uuid = account.uuid().to_string();
        if st.file.token_uuid.as_deref() != Some(uuid.as_str()) {
            if st.file.token_uuid.is_some() {
                info!("token was for another account - forgetting it and the last totals");
                let afk = st.file.afk_melons;
                st.file = BoardFile { enabled: st.file.enabled, endpoint: st.file.endpoint.clone(), interval_minutes: st.file.interval_minutes, afk_melons: afk, ..Default::default() };
            }
            st.file.token = None;
            st.file.token_uuid = Some(uuid);
        }
        if st.file.token_valid() {
            info!("saved token still good until {} - reporting is active", st.file.token_expires);
            st.phase = Phase::Ready;
            return;
        }
        st.phase = Phase::Authing;
        let (me, endpoint) = (self.clone(), st.file.endpoint.clone());
        tokio::spawn(async move { me.authenticate(account, endpoint).await });
    }

    /// The reply to a stats request, once it has had time to land: read, check, submit.
    fn settle(&self, st: &mut St, now: Instant) {
        match st.phase {
            Phase::Requested(at) if now >= at + SETTLE => {
                let t = Totals::read(&st.stats, st.file.afk_melons);
                if !t.live() {
                    warn!("no statistics in the reply ({} stats known, play time 0) - nothing sent", st.stats.len());
                    st.phase = Phase::Halted(now);
                    return;
                }
                st.latest = Some(t);
                let last = st.file.last();
                if let Some(l) = last
                    && let Some(k) = t.below(&l)
                {
                    warn!("{k} reads lower than last sent ({t:?} vs {l:?}) - NOT sent: the board would take it for a stats reset");
                    st.next_at = now + interval(&st.file);
                    st.phase = Phase::Ready;
                    return;
                }
                if last.is_some_and(|l| t.same_counts(&l)) {
                    st.next_at = now + interval(&st.file);
                    st.phase = Phase::Ready;
                    return;
                }
                st.phase = Phase::Submitting;
                let (me, endpoint, token) = (self.clone(), st.file.endpoint.clone(), st.file.token.clone().unwrap_or_default());
                tokio::spawn(async move { me.submit_and_settle(endpoint, token, t).await });
            }
            Phase::Halted(at) if now >= at + BACKOFF_MAX => st.phase = Phase::NeedAuth,
            _ => {}
        }
    }

    async fn authenticate(&self, account: Account, endpoint: String) {
        let res = self.handshake(&account, &endpoint).await;
        let mut st = self.st.lock();
        match res {
            Ok((token, expires)) => {
                st.file.token = Some(token);
                st.file.token_expires = expires;
                st.backoff = Duration::ZERO;
                st.phase = Phase::Ready;
                st.next_at = Instant::now();
                drop(st);
                self.save();
                info!("verified with Mojang's key - leaderboard reporting is active");
            }
            Err(Fail::Outdated(m)) => {
                warn!("halted: this build is too old for the leaderboard ({m})");
                st.phase = Phase::Halted(Instant::now());
            }
            Err(Fail::Auth(m)) => {
                warn!("halted: could not verify the account: {m}");
                st.phase = Phase::Halted(Instant::now());
            }
            Err(Fail::Retry(m)) => {
                st.backoff = bump(st.backoff);
                warn!("auth failed: {m} - retrying in {} s", st.backoff.as_secs());
                st.next_at = Instant::now() + st.backoff;
                st.phase = Phase::NeedAuth;
            }
        }
    }

    async fn handshake(&self, account: &Account, endpoint: &str) -> Result<(String, i64), Fail> {
        // azalea's chat signing fetches the certificates after each join; fetch them here if not yet.
        let fresh = |c: &azalea::auth::certs::Certificates| c.expires_at.timestamp_millis() > epoch_secs() * 1000 + 60_000;
        let certs = match account.certs().filter(fresh) {
            Some(c) => c,
            None => {
                let token = account.access_token().ok_or_else(|| Fail::Auth("no Minecraft access token".into()))?;
                azalea::auth::certs::fetch_certificates(&token).await.map_err(|e| Fail::Retry(format!("fetching Mojang certificates: {e}")))?
            }
        };
        let ask = serde_json::json!({ "username": account.username(), "uuid": account.uuid().to_string() });
        let (code, body) = self.post(endpoint, "/auth/challenge", ask, None).await.map_err(|e| Fail::Retry(format!("challenge: {e:#}")))?;
        if code == 426 {
            return Err(Fail::Outdated(body.to_string()));
        }
        if code != 200 {
            return Err(Fail::Retry(format!("challenge returned {code}: {body}")));
        }
        let server_id = body["serverId"].as_str().ok_or_else(|| Fail::Retry(format!("challenge reply unreadable: {body}")))?.to_string();
        // SHA256withRSA (PKCS#1 v1.5), as Crypt.SIGNING_ALGORITHM in the game.
        let signature = rsa::pkcs1v15::SigningKey::<Sha256>::new(certs.private_key.clone()).sign(server_id.as_bytes()).to_bytes();
        let verify = serde_json::json!({
            "serverId": server_id,
            "signature": B64.encode(&*signature),
            "publicKey": B64.encode(&certs.public_key_der),
            // V2: Mojang's signature over uuid | expiry | key, the one ProfilePublicKey.Data carries.
            "keySignature": B64.encode(&certs.signature_v2),
            "expiresAt": certs.expires_at.timestamp_millis(),
        });
        let (code, body) = self.post(endpoint, "/auth/verify", verify, None).await.map_err(|e| Fail::Retry(format!("verify: {e:#}")))?;
        if code != 200 {
            return Err(Fail::Auth(format!("verify returned {code}: {body}")));
        }
        match (body["token"].as_str(), body["expires"].as_i64()) {
            (Some(t), Some(e)) => Ok((t.to_string(), e)),
            _ => Err(Fail::Auth(format!("verify reply unreadable: {body}"))),
        }
    }

    async fn submit_and_settle(&self, endpoint: String, token: String, t: Totals) {
        let res = self.submit(&endpoint, &token, &t).await;
        let mut st = self.st.lock();
        let every = interval(&st.file);
        match res {
            Ok(r) => {
                st.file.remember(&t);
                st.points = r["points"].as_i64().unwrap_or(st.points);
                st.rank = r["rank"].as_i64().unwrap_or(st.rank);
                st.backoff = Duration::ZERO;
                st.next_at = Instant::now() + every;
                st.phase = Phase::Ready;
                drop(st);
                self.save();
                info!("submitted {} placed, {} mined, {} crafted, {} planted - {} points, rank #{}", t.placed, t.mined, t.crafted, t.planted, r["points"], r["rank"]);
            }
            Err(SubmitFail::Unauthorized(m)) => {
                info!("token refused ({m}) - verifying again");
                st.file.token = None;
                st.file.token_expires = 0;
                st.phase = Phase::NeedAuth;
                st.next_at = Instant::now();
                drop(st);
                self.save();
            }
            Err(SubmitFail::Outdated(m)) => {
                warn!("halted: this build is too old for the leaderboard ({m})");
                st.phase = Phase::Halted(Instant::now());
            }
            Err(SubmitFail::Rejected(m)) => {
                st.backoff = bump(st.backoff);
                warn!("submission rejected: {m}");
                st.next_at = Instant::now() + every.max(st.backoff);
                st.phase = Phase::Ready;
            }
            Err(SubmitFail::Network(m)) => {
                st.backoff = bump(st.backoff);
                warn!("submission failed: {m} - retrying in {} s", st.backoff.as_secs());
                st.next_at = Instant::now() + st.backoff;
                st.phase = Phase::Ready;
            }
        }
    }

    async fn submit(&self, endpoint: &str, token: &str, t: &Totals) -> Result<serde_json::Value, SubmitFail> {
        let body = serde_json::json!({
            "placed": t.placed, "mined": t.mined, "crafted": t.crafted, "planted": t.planted,
            "playTime": t.play_time, "afk": t.afk,
        });
        let (code, body) = self.post(endpoint, "/submit", body, Some(token)).await.map_err(|e| SubmitFail::Network(format!("{e:#}")))?;
        match code {
            200 => Ok(body),
            426 => Err(SubmitFail::Outdated(body.to_string())),
            401 | 403 => Err(SubmitFail::Unauthorized(body.to_string())),
            _ => Err(SubmitFail::Rejected(format!("{code}: {body}"))),
        }
    }

    async fn post(&self, endpoint: &str, path: &str, body: serde_json::Value, bearer: Option<&str>) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut req = self
            .http
            .post(format!("{endpoint}{path}"))
            .header("User-Agent", "MelonBoard/1.0")
            .header("X-MelonBoard-Protocol", PROTOCOL.to_string())
            .json(&body);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        let res = req.send().await?;
        let code = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        let json = serde_json::from_str(&text).unwrap_or_else(|_| serde_json::Value::String(text.chars().take(200).collect()));
        Ok((code, json))
    }
}

enum Fail {
    Outdated(String),
    /// The account could not be verified: retrying soon will not help.
    Auth(String),
    Retry(String),
}

enum SubmitFail {
    Outdated(String),
    Unauthorized(String),
    Rejected(String),
    Network(String),
}

impl std::fmt::Display for SubmitFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outdated(m) => write!(f, "outdated: {m}"),
            Self::Unauthorized(m) => write!(f, "unauthorized: {m}"),
            Self::Rejected(m) => write!(f, "rejected: {m}"),
            Self::Network(m) => write!(f, "network: {m}"),
        }
    }
}

/// The tracked server only (BoardConfig.matchesServer): `board` (config.json's board_server) and
/// its subdomains, any port. An empty `board` tracks nothing.
pub fn tracks(server: &str, board: &str) -> bool {
    let host_of = |s: &str| {
        let s = s.trim().to_lowercase();
        s.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map_or(s.clone(), |(h, _)| h.to_string())
    };
    let (host, board) = (host_of(server), host_of(board));
    !board.is_empty() && (host == board || host.ends_with(&format!(".{board}")))
}

fn interval(f: &BoardFile) -> Duration {
    Duration::from_secs(f.interval_minutes.max(1) * 60)
}

fn bump(b: Duration) -> Duration {
    if b.is_zero() { BACKOFF_MIN } else { (b * 2).min(BACKOFF_MAX) }
}

fn epoch_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(pairs: &[(Stat, i32)]) -> HashMap<Stat, i32> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn reads_the_four_scored_stats_and_play_time() {
        let s = stats(&[
            (Stat::Used(ItemKind::Melon), 120_400),
            (Stat::Mined(BlockKind::Melon), 118_950),
            (Stat::Used(ItemKind::MelonSeeds), 1337),
            (Stat::Used(ItemKind::MelonSlice), 99),
            (Stat::Custom(CustomStat::PlayTime), 1000),
        ]);
        let t = Totals::read(&s, 0);
        assert_eq!((t.placed, t.mined, t.crafted, t.planted, t.play_time), (120_400, 118_950, 0, 1337, 1000));
        assert!(t.live());
    }

    #[test]
    fn only_the_board_server_is_tracked() {
        assert!(tracks("example.com", "example.com"));
        assert!(tracks("mc.Example.com:25565", "example.com"));
        assert!(!tracks("127.0.0.1:25570", "example.com"));
        assert!(!tracks("notexample.com", "example.com"));
        assert!(!tracks("example.com", ""), "no board server: nothing is tracked");
    }

    #[test]
    fn an_empty_map_is_not_live() {
        assert!(!Totals::read(&HashMap::new(), 0).live());
    }

    #[test]
    fn a_lower_total_is_never_sent() {
        let was = Totals { placed: 10, mined: 10, crafted: 0, planted: 1, play_time: 5, afk: 0 };
        let now = Totals { placed: 12, mined: 9, ..was };
        assert_eq!(now.below(&was), Some("mined"));
        assert_eq!(Totals { placed: 12, ..was }.below(&was), None);
    }

    #[test]
    fn the_file_keeps_java_field_names() {
        let f: BoardFile = serde_json::from_str(r#"{"enabled":true,"tokenUuid":"a","lastPlaced":120400,"lastMined":118950,"lastCrafted":0,"lastPlanted":1337,"lastAfk":0,"openKey":96}"#).unwrap();
        assert_eq!(f.last().map(|t| (t.placed, t.mined, t.planted)), Some((120_400, 118_950, 1337)));
        assert_eq!(f.interval_minutes, 3);
        let out = serde_json::to_value(&f).unwrap();
        assert_eq!(out["openKey"], 96, "unknown fields are kept");
        assert_eq!(out["lastPlaced"], 120_400);
    }
}
