//! Swarm accounts. Each is a folder `swarm/bots/<name>/`: its Microsoft login (azalea-auth.json,
//! the same cache a bot of its own keeps in bots/<name>/) and bot.json - whether it
//! starts with the swarm, whether it listens to the controller, and its per-bot setting overrides.
//!
//! Accounts are added from the panel, any time: the Microsoft device
//! code is shown on the page, you sign in with whichever account, and the account it turns
//! out to be becomes a bot. Signing in again for an account already in the swarm renews its login.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{info, warn};

use super::SwarmCtx;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BotFile {
    /// Started when the swarm starts.
    pub autostart: bool,
    /// Takes the controller's orders (else only the panel's).
    pub listen_controller: bool,
    /// Per-bot setting overrides, in the standalone's shape: {"kill-aura": {"settings": {...}}}.
    pub overrides: Value,
}

impl Default for BotFile {
    fn default() -> Self {
        Self { autostart: true, listen_controller: true, overrides: json!({}) }
    }
}

pub fn bots_dir(swarm_dir: &Path) -> PathBuf {
    swarm_dir.join("bots")
}

/// Every account folder under swarm/bots/, with its bot.json (defaults when missing).
pub fn load_all(swarm_dir: &Path) -> Vec<(String, BotFile)> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir(bots_dir(swarm_dir)) else { return out };
    for e in dir.flatten() {
        let path = e.path();
        if !path.is_dir() || !(path.join("azalea-auth.json").exists() || path.join("bot.json").exists()) {
            continue;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let file = fs::read_to_string(path.join("bot.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        out.push((name, file));
    }
    out.sort_by_key(|(n, _)| n.to_lowercase());
    out
}

pub fn save(swarm_dir: &Path, name: &str, file: &BotFile) -> Result<()> {
    let dir = bots_dir(swarm_dir).join(name);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("bot.json"), serde_json::to_string_pretty(file)? + "\n")?;
    Ok(())
}

/// The sign-in shown in the panel while it runs.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Login {
    pub code: String,
    pub url: String,
    /// When the code stops working (ms since the epoch).
    pub expires_at: u64,
    pub status: String,
    pub done: bool,
    pub ok: bool,
}

/// Run one Microsoft device-code sign-in; the account it turns out to be joins the swarm.
pub async fn device_login(ctx: Arc<SwarmCtx>) {
    let result = login_inner(&ctx).await;
    let mut l = ctx.login.lock();
    let login = l.get_or_insert_with(Login::default);
    login.done = true;
    match result {
        Ok(name) => {
            login.ok = true;
            login.status = format!("signed in as {name} - added to the swarm");
            ctx.feed("event", &format!("account {name} added"));
        }
        Err(e) => {
            login.ok = false;
            login.status = format!("{e:#}");
            warn!(target: "accounts", "sign-in failed: {e:#}");
        }
    }
}

async fn login_inner(ctx: &Arc<SwarmCtx>) -> Result<String> {
    use azalea::auth::{self, cache::CachedAccount};
    let http = reqwest::Client::new();
    let code = auth::get_ms_link_code(&http, None, None).await.context("asking Microsoft for a sign-in code")?;
    let expires_at = super::orders::now_ms() + code.expires_in * 1000;
    *ctx.login.lock() = Some(Login {
        code: code.user_code.clone(),
        url: format!("{}?otc={}", code.verification_uri, code.user_code),
        expires_at,
        status: "waiting for you to sign in (use the account you want to add)".into(),
        done: false,
        ok: false,
    });
    info!(target: "accounts", "sign-in code issued - waiting for the sign-in");
    let msa = auth::get_ms_auth_token(&http, code, None).await.context("waiting for the sign-in")?;
    set_status(ctx, "signed in to Microsoft - getting the Minecraft profile");
    let mc = auth::get_minecraft_token(&http, &msa.data.access_token).await.context("Minecraft sign-in")?;
    let profile = auth::get_profile(&http, &mc.minecraft_access_token).await.context("reading the Minecraft profile")?;
    let name = profile.name.clone();
    if let Some(why) = refuse(ctx, &name) {
        bail!("{why}");
    }
    let dir = bots_dir(&ctx.accounts_root).join(&name);
    fs::create_dir_all(&dir)?;
    auth::cache::set_account_in_cache(
        &dir.join("azalea-auth.json"),
        &name,
        CachedAccount { cache_key: name.clone(), msa, xbl: mc.xbl, mca: mc.mca, profile },
    )
    .await
    .map_err(|e| anyhow::anyhow!("saving the login: {e}"))?;
    let renewing = ctx.bots.read().contains_key(&name);
    if !dir.join("bot.json").exists() {
        // Added from the perimeter panel: the botswarm does not start it by itself.
        let file = BotFile { autostart: ctx.perimeter.is_none(), ..Default::default() };
        save(&ctx.accounts_root, &name, &file)?;
    }
    if let Some(p) = &ctx.perimeter {
        p.choose(&name, true);
    }
    crate::config::register_own_account(&name);
    info!(target: "accounts", "{} {name}", if renewing { "renewed the login of" } else { "added" });
    ctx.ensure_bot(&name);
    Ok(name)
}

fn set_status(ctx: &SwarmCtx, s: &str) {
    if let Some(l) = ctx.login.lock().as_mut() {
        l.status = s.to_string();
    }
}

/// Accounts that must not be swarm bots: a MelonBot bot of its own (bots/<name>/config.json - the
/// melon farmer, say, which runs in its own process; the two are kept apart).
pub fn refuse(ctx: &SwarmCtx, name: &str) -> Option<String> {
    let melonbot_bots = ctx.accounts_root.parent().map(|p| p.join("bots")).unwrap_or_else(|| PathBuf::from("bots"));
    if melonbot_bots.join(name).join("config.json").exists() {
        return Some(format!("{name} has a bot of its own (bots/{name}/) - it runs on its own, not in the swarm"));
    }
    None
}
