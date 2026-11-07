//! One bot's settings: `bots/<name>/config.json`, made once by `melonbot import <instance>`
//! from the Java instance the bot replaces, so nothing is typed in twice.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// A box given by two corners, inclusive (melon-farmer's plot corners).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Area {
    pub min: Pos,
    pub max: Pos,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The Minecraft name this bot must be logged in as. A login that comes back as anyone else is
    /// refused - the job the Java side's profile.properties guard does, since one Microsoft login
    /// cache can hold several accounts. Left empty, the first sign-in decides it and is written
    /// here (a new bot whose account is picked when it is first started).
    pub account: String,
    /// The server to join (host, or host:port).
    pub server: String,
    /// Discord webhook for the bot's posts; empty = none. Never printed.
    pub discord_webhook: String,
    /// The name it posts under.
    pub discord_name: String,
    /// Players never treated as a threat, besides your own accounts (~/.melonhack/own-accounts.txt).
    pub friends: Vec<String>,
    /// Any other player in view: log out and stay out until relaunched.
    pub player_guard: bool,
    /// Sent from a queue/lobby server to get back onto the game server ("/queue <name>", say).
    /// Empty: none - queue detection is off.
    pub queue_command: String,
    /// Chat (regex, any case) meaning we were dropped in the queue.
    pub queue_trigger: String,
    /// The server whose melon statistics go to the MelonBoard leaderboard (board.rs): this host and
    /// its subdomains. Empty: no reporting.
    pub board_server: String,
    pub base: Option<Pos>,
    pub farm: Option<Area>,
    pub tools_chest: Option<Pos>,
    pub repair_chest: Option<Pos>,
    /// The folder the build schematics are read from (the Java instance's schematics/).
    pub schematics: Option<std::path::PathBuf>,
    /// The instance name the control panel reports (the Java instance's) - the Discord status
    /// watcher finds its bot by it.
    pub instance: String,
    /// Which routine this bot runs: "seller" sells to the villagers in a pen, "buyer" buys from them,
    /// "mapart" builds carpet map art sent in on Discord (mapart/). Empty or anything else = the
    /// melon routine, gated as before by modules.json.
    pub job: String,
    /// The seller's or the buyer's perimeter: it stays inside this box. Left unset, it is read once
    /// from the corner signs ("1", "2", ...) placed in the world.
    pub bounds: Option<Area>,
    /// Seller: what it sells for emeralds - an item id each ("stick", "iron_ingot"), taken from the
    /// chests signed with its name (an ingot's with the metal: "iron") and sold to the villagers in
    /// the box whose job buys it. Changed from the control panel's Sell tab while the bot runs; it
    /// saves them here.
    pub sell: Vec<String>,
    /// Buyer: spend emeralds on the `buy` list (the Shop tab's switch). Off, it only waits.
    pub spend_emeralds: bool,
    /// Buyer: what to buy with the emeralds from the chest signed "spend". A block or item id
    /// ("glowstone", "glass") is bought about a stack a visit and banked in the chest signed with
    /// its name. An enchanted book is `enchanted_book:<enchantment>`, optionally with the lowest
    /// level that counts (`enchanted_book:mending`, `enchanted_book:sharpness:5`): it is paid with
    /// emeralds and a book from the chest signed "books", bought one trade at a time, and banked in
    /// the chest signed with the enchantment ("mending", "silk touch"). A bare "enchanted_book" is
    /// refused (every enchanted book has that id). Nothing without a signed chest is bought.
    /// Both can be changed from the control panel's Shop tab while the bot runs; it saves them here.
    pub buy: Vec<String>,
    /// Mapart: the Discord server, channel and role it works with, by id (the names are for
    /// people). Its Discord bot token is bots/<name>/discord-token.txt - never in here.
    pub discord_guild_id: String,
    pub discord_channel_id: String,
    pub discord_role_id: String,
    pub discord_channel: String,
    pub discord_role: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            account: String::new(),
            server: "localhost".into(),
            discord_webhook: String::new(),
            discord_name: String::new(),
            friends: Vec::new(),
            player_guard: true,
            queue_command: String::new(),
            queue_trigger: "you are (now )?in (the )?queue|position in (the )?queue|queue position|sent to (the )?queue".into(),
            board_server: String::new(),
            base: None,
            farm: None,
            tools_chest: None,
            repair_chest: None,
            schematics: None,
            instance: String::new(),
            job: String::new(),
            bounds: None,
            sell: vec!["stick".into(), "iron_ingot".into()],
            spend_emeralds: false,
            buy: Vec::new(),
            discord_guild_id: String::new(),
            discord_channel_id: String::new(),
            discord_role_id: String::new(),
            discord_channel: String::new(),
            discord_role: String::new(),
        }
    }
}

impl Config {
    pub fn load(dir: &Path) -> Result<Self> {
        let file = dir.join("config.json");
        let text = fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let cfg: Config = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        Ok(cfg)
    }

    /// The seller: sells the `sell` list to the villagers in its pen (seller.rs).
    pub fn is_seller(&self) -> bool {
        self.job.eq_ignore_ascii_case("seller")
    }

    /// The buyer: spends the emeralds from the chest signed "spend" on the `buy` list (buyer.rs).
    pub fn is_buyer(&self) -> bool {
        self.job.eq_ignore_ascii_case("buyer")
    }

    /// The mapart builder: maps sent in on Discord, built in carpet on water (mapart/).
    pub fn is_mapart(&self) -> bool {
        self.job.eq_ignore_ascii_case("mapart")
    }

    /// A job of its own in a marked-out place - the seller or the buyer in the villager pen, the
    /// mapart builder at its site - not the melon routine.
    pub fn in_pen(&self) -> bool {
        self.is_seller() || self.is_buyer() || self.is_mapart()
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        fs::write(dir.join("config.json"), serde_json::to_string_pretty(self)? + "\n")?;
        Ok(())
    }

    /// Set some keys of `config.json` in place, leaving everything else in the file as it is (the
    /// panel's Shop tab saves the buy list this way, so the next start reads the same list).
    pub fn update_file(dir: &Path, set: &[(&str, Value)]) -> Result<()> {
        let file = dir.join("config.json");
        let text = fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let mut v: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        let Some(o) = v.as_object_mut() else { bail!("{} is not a JSON object", file.display()) };
        for (k, val) in set {
            o.insert(k.to_string(), val.clone());
        }
        fs::write(&file, serde_json::to_string_pretty(&v)? + "\n")?;
        Ok(())
    }

    /// Settings from a Java MelonHack instance (its folder, or its `.minecraft`): the account it is
    /// guarded to, ai-pilot's Discord and queue settings, melon-farmer's plot 1, the saved places.
    pub fn import(instance: &Path) -> Result<Self> {
        let mc = if instance.join(".minecraft").is_dir() { instance.join(".minecraft") } else { instance.to_path_buf() };
        let mh = mc.join("melonhack");
        let standalone: Value = serde_json::from_str(
            &fs::read_to_string(mh.join("standalone.json")).with_context(|| format!("no melonhack/standalone.json under {}", mc.display()))?,
        )?;
        let mut cfg = Config::default();
        let folder = if instance.join(".minecraft").is_dir() { instance.to_path_buf() } else { instance.parent().map(|p| p.to_path_buf()).unwrap_or_default() };
        cfg.instance = folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();

        if let Ok(p) = fs::read_to_string(mh.join("ai").join("profile.properties")) {
            for line in p.lines() {
                if let Some(v) = line.trim().strip_prefix("account=") {
                    cfg.account = v.trim().to_string();
                }
            }
        }

        let ai = &standalone["ai-pilot"]["settings"];
        let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
        cfg.discord_webhook = s(&ai["discord-webhook"]);
        cfg.discord_name = s(&ai["discord-name"]);
        if let Some(q) = ai["queue-command"].as_str() {
            cfg.queue_command = q.into();
        }
        if let Some(q) = ai["queue-trigger"].as_str() {
            cfg.queue_trigger = q.into();
        }
        if let Some(f) = ai["friends"].as_array() {
            cfg.friends = f.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
        }

        let mf = &standalone["melon-farmer"]["settings"];
        let pos = |v: &Value| -> Option<Pos> {
            Some(Pos { x: v["x"].as_i64()? as i32, y: v["y"].as_i64()? as i32, z: v["z"].as_i64()? as i32 })
        };
        if let (Some(a), Some(b)) = (pos(&mf["p1-corner-1"]), pos(&mf["p1-corner-2"]))
            && (a != Pos::default() || b != Pos::default())
        {
            cfg.farm = Some(Area {
                min: Pos { x: a.x.min(b.x), y: a.y.min(b.y), z: a.z.min(b.z) },
                max: Pos { x: a.x.max(b.x), y: a.y.max(b.y), z: a.z.max(b.z) },
            });
        }

        if mc.join("schematics").is_dir() {
            cfg.schematics = Some(mc.join("schematics"));
        }
        let places = mh.join("ai").join("servers").join(&cfg.server).join("places.json");
        if let Ok(text) = fs::read_to_string(&places) {
            let p: Value = serde_json::from_str(&text)?;
            cfg.base = pos(&p["base"]);
            cfg.tools_chest = pos(&p["tools-chest"]);
            cfg.repair_chest = pos(&p["repair-chest"]);
        }
        Ok(cfg)
    }
}

/// `~/.melonhack` - shared with the Java clients (own-accounts.txt, panels/).
pub fn melonhack_home() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap_or_default();
    PathBuf::from(home).join(".melonhack")
}

/// Your own accounts, lower case: every account a MelonHack client has joined with on this PC
/// (own-accounts.txt) and every client with a control panel open (panels/*.json).
pub fn own_accounts() -> HashSet<String> {
    let mut out = HashSet::new();
    let home = melonhack_home();
    if let Ok(text) = fs::read_to_string(home.join("own-accounts.txt")) {
        for n in text.lines().map(str::trim).filter(|n| !n.is_empty()) {
            out.insert(n.to_lowercase());
        }
    }
    if let Ok(dir) = fs::read_dir(home.join("panels")) {
        for f in dir.flatten() {
            if f.path().extension().is_some_and(|e| e == "json")
                && let Ok(text) = fs::read_to_string(f.path())
                && let Ok(v) = serde_json::from_str::<Value>(&text)
                && let Some(n) = v["name"].as_str()
            {
                out.insert(n.to_lowercase());
            }
        }
    }
    out
}

/// Note this account as one of your own, the way the Java client does on join, so the other bots
/// never take it for a stranger.
pub fn register_own_account(name: &str) {
    let file = melonhack_home().join("own-accounts.txt");
    let have = fs::read_to_string(&file).unwrap_or_default();
    if have.lines().any(|n| n.trim().eq_ignore_ascii_case(name)) {
        return;
    }
    let _ = fs::create_dir_all(file.parent().unwrap());
    let mut text = have;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(name);
    text.push('\n');
    let _ = fs::write(&file, text);
}
