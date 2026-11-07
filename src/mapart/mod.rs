//! The mapart bot (`"job": "mapart"`, start-mapart.cmd): map art in carpet. A .litematic, .schem
//! or .nbt comes in one of two ways: dropped in the bot's inbox folder (inbox.rs - named after the
//! file), or, with Discord set up, sent in the bot's channel by someone with the mapart role
//! (discord.rs - named on the Name it form). Either way it is checked (art.rs: carpets only,
//! completely flat, whole maps - a 2x2 is its 4 maps - a site's cobble layer and extra line left
//! out) and queued (queue.rs).
//!
//! The site (site.rs; the in-game side is build.rs): glowstone on the corners of the whole of it - the bot
//! never leaves that box, and lives in it (its bed, food, chests); the art area is the 128x128
//! platform inside a lip of froglights at carpet height (the lip gives the top row its shading).
//! Each map is laid in carpet on the platform - sneaking, so the dispensers in it (and any chest)
//! never open instead, and a spot that will not take a carpet after a few tries is skipped and
//! reported, never retried for ever. One colour at a time: every carpet of a colour is placed
//! before the next colour is begun (plan.rs), its leftovers put back after. Chests by their signs:
//! several per carpet colour round the area, signed with the colour alone ("RED", "GRAY", "LIME" -
//! the user, 6 Nov; art::carpet_for_sign) - always the nearest one to where the bot is, the next
//! nearest when one is empty - "maps", "glass pane", "XP" (bottles o' enchanting, for the anvil),
//! "finished", "food", and "used carpet" if the washed-off carpets are to be gathered. A colour sign
//! is read from its lines, never found by searching the sign's whole NBT the way chests::signed
//! does: "GRAY" is in "LIGHT GRAY", every sign's NBT says color "black", and "filtered" holds
//! "red". Then: an empty map opened over the art, the
//! cartography table locks it with a glass pane, the anvil names it (1X1 "name", 2X2 "name" c1r1),
//! it goes in "finished", and the water button (a sign "clear" by it) is pressed, waited on, and
//! pressed again to wash the carpets off.
//!
//! The inbox and the Discord side run from the start of the process, signed into Minecraft or not.

pub mod art;
pub mod build;
pub mod discord;
pub mod inbox;
pub mod plan;
pub mod queue;
pub mod site;

use std::{
    fs,
    path::Path,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use azalea::Client;
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::{bot::Shared, config::Config};

pub struct Mapart {
    pub queue: Mutex<queue::Queue>,
    /// Posts to the mapart channel, once Discord is up.
    pub poster: Mutex<Option<discord::Poster>>,
}

static MAPART: OnceLock<Arc<Mapart>> = OnceLock::new();

/// The process's mapart side, once started.
pub fn get() -> Option<&'static Arc<Mapart>> {
    MAPART.get()
}

/// Post a line in the mapart channel (dropped if Discord is not up).
pub fn post(text: impl Into<String>) {
    if let Some(p) = get().and_then(|m| m.poster.lock().clone()) {
        p.post(text.into());
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A file name safe to keep on disk: letters, digits, '-', '_' and '.'.
fn safe_name(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    s.trim_start_matches('.').chars().take(80).collect()
}

/// Start the mapart side: the queue, the inbox folder, and the Discord bot when its token and
/// channel are set (Discord is optional - without it, maps come in through the inbox only).
pub fn start(dir: &Path, cfg: &Config) {
    let m = MAPART.get_or_init(|| Arc::new(Mapart { queue: Mutex::new(queue::Queue::load(dir)), poster: Mutex::new(None) })).clone();
    tokio::spawn(inbox::run(m.clone(), dir.to_path_buf()));
    let token = fs::read_to_string(dir.join("discord-token.txt")).unwrap_or_default();
    let token = token.trim().trim_start_matches('\u{feff}').trim_start_matches("Bot ").trim().to_string();
    if token.is_empty() {
        info!(target: "mapart", "no Discord bot token ({}) - maps come in through the inbox folder only", dir.join("discord-token.txt").display());
        return;
    }
    if cfg.discord_channel_id.is_empty() || cfg.discord_role_id.is_empty() || cfg.discord_guild_id.is_empty() {
        warn!(target: "mapart", "config.json has no discord_guild_id / discord_channel_id / discord_role_id - Discord is off, maps come in through the inbox folder only");
        return;
    }
    // rustls needs its crypto chosen once per process; aws-lc-rs is the one the rest of the build uses.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let s = discord::Settings {
        token,
        guild: cfg.discord_guild_id.clone(),
        channel: cfg.discord_channel_id.clone(),
        role: cfg.discord_role_id.clone(),
        dir: dir.to_path_buf(),
    };
    let waiting = m.queue.lock().waiting().len();
    info!(target: "mapart", "mapart: {waiting} map(s) waiting; starting the Discord bot");
    tokio::spawn(discord::run(m, s));
}

/// The in-game side (build.rs).
pub async fn run(bot: Client, ctx: Arc<Shared>) {
    build::run(bot, ctx).await;
}
