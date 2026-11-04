//! The perimeter panel: what it shows (/api/state) and what its buttons do (/api/cmd). Served by the
//! same panel code as the others (same key, same loopback checks) on its own port and page.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tracing::{info, warn};

use super::{JobConfig, Perimeter};
use crate::swarm::{SwarmCtx, accounts, in_use_elsewhere};

pub const PAGE: &str = include_str!("perimeter.html");

/// Every 250 ms: the page's commands, then a fresh state. The map every 3 s.
pub async fn run(ctx: Arc<SwarmCtx>) {
    let (Some(panel), Some(p)) = (ctx.panel.clone(), ctx.perimeter.clone()) else {
        warn!(target: "perimeter", "no perimeter panel (its port was taken)");
        return;
    };
    info!(target: "perimeter", "perimeter panel: http://127.0.0.1:{}/", panel.port);
    let mut accounts_at = Instant::now();
    let mut map: (Instant, Value) = (Instant::now() - Duration::from_secs(60), json!(null));
    loop {
        let cmds: Vec<Value> = panel.commands.lock().drain(..).collect();
        for c in cmds {
            info!(target: "perimeter", "panel: {c}");
            if let Err(e) = handle(&ctx, &p, &c) {
                warn!(target: "perimeter", "panel command failed: {e:#}");
                ctx.feed("say", &format!("{e:#}"));
            }
        }
        // Accounts added from the botswarm's panel turn up here too.
        if accounts_at.elapsed() >= Duration::from_secs(10) {
            accounts_at = Instant::now();
            for (name, _) in accounts::load_all(&ctx.accounts_root) {
                ctx.ensure_bot(&name);
            }
        }
        let with_map = map.0.elapsed() >= Duration::from_secs(3);
        let progress = {
            let mut b = p.board.lock();
            b.save_if_due(&p.dir);
            b.summary(with_map)
        };
        if with_map {
            map = (Instant::now(), json!({"map": progress["map"], "step": progress["mapStep"], "bands": progress["mapBands"]}));
        }
        *panel.state.lock() = snapshot(&ctx, &p, progress, &map.1);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn snapshot(ctx: &SwarmCtx, p: &Perimeter, mut progress: Value, map: &Value) -> Value {
    let cfg = ctx.cfg.read().clone();
    let job = p.job.read().clone();
    let chosen = p.chosen.read().clone();
    let bases = p.bases.lock().clone();
    let bots: Vec<Value> = ctx
        .bots
        .read()
        .values()
        .map(|b| {
            let mut row = b.row.lock().clone();
            let online = b.online.load(Ordering::Relaxed);
            let running = b.running.load(Ordering::Relaxed);
            let starting = b.starting.load(Ordering::Relaxed);
            row["name"] = json!(b.name);
            row["chosen"] = json!(chosen.contains(&b.name));
            row["running"] = json!(running);
            row["starting"] = json!(starting);
            row["online"] = json!(online);
            row["error"] = json!(*b.last_error.lock());
            row["onlineFor"] = json!(b.joined_at.lock().map_or(0, |t| t.elapsed().as_secs()));
            row["elsewhere"] = json!(if running || starting { None } else { in_use_elsewhere(ctx, &b.name) });
            row["base"] = json!(bases.get(&b.name));
            if !online {
                row["state"] = json!("");
            }
            row
        })
        .collect();
    if progress.is_object() {
        progress["map"] = map["map"].clone();
        progress["mapStep"] = map["step"].clone();
        progress["mapBands"] = map["bands"].clone();
    }
    json!({
        "name": "PERIMETER",
        "client": "rust-perimeter",
        "server": cfg.server,
        "offline": ctx.offline,
        "bots": bots,
        "login": *ctx.login.lock(),
        "job": job,
        "firstJob": JobConfig::default(),
        "world": p.board.lock().world,
        "progress": progress,
        "settings": {"maxSpeed": cfg.max_speed, "maxBots": cfg.max_bots, "foodSign": cfg.food_sign, "foodLow": cfg.food_low_points},
    })
}

fn f64_of(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// A y from the page: a number, or blank / null for the world's own limit.
fn y_of(v: &Value) -> Option<Option<i32>> {
    match v {
        Value::Null => Some(None),
        Value::String(s) if s.trim().is_empty() => Some(None),
        _ => f64_of(v).map(|y| Some(y as i32)),
    }
}

fn handle(ctx: &Arc<SwarmCtx>, p: &Perimeter, c: &Value) -> anyhow::Result<()> {
    let name = c["name"].as_str().unwrap_or("");
    match c["type"].as_str().unwrap_or("") {
        "add-account" => crate::swarm::start_login(ctx),
        "cancel-login" => crate::swarm::cancel_login(ctx),
        // "*": every ticked account.
        "start" => {
            let names: Vec<String> = if name == "*" { p.chosen.read().iter().cloned().collect() } else { vec![name.to_string()] };
            for n in names {
                ctx.ensure_bot(&n);
                ctx.start(&n);
            }
        }
        "stop" => {
            let names: Vec<String> = if name == "*" {
                ctx.bots.read().values().filter(|b| b.running.load(Ordering::SeqCst) || b.starting.load(Ordering::SeqCst)).map(|b| b.name.clone()).collect()
            } else {
                vec![name.to_string()]
            };
            for n in names {
                ctx.stop(&n);
            }
        }
        "choose" => p.choose(name, c["on"].as_bool().unwrap_or(true)),
        // The box: corners, the y range (blank: the world's), the band.
        "job" => {
            let before = p.job.read().clone();
            {
                let mut guard = p.job.write();
                let j = &mut *guard;
                for (k, slot) in [("x1", &mut j.x1), ("z1", &mut j.z1), ("x2", &mut j.x2), ("z2", &mut j.z2)] {
                    if let Some(v) = f64_of(&c[k]) {
                        *slot = v as i32;
                    }
                }
                if c.get("top").is_some()
                    && let Some(v) = y_of(&c["top"])
                {
                    j.top = v;
                }
                if c.get("bottom").is_some()
                    && let Some(v) = y_of(&c["bottom"])
                {
                    j.bottom = v;
                }
                if let Some(v) = f64_of(&c["band"]).filter(|v| (1.0..=16.0).contains(v)) {
                    j.band = v as i32;
                }
                // The board looks over every cell for each claim: a side of 4,000 is 250,000 cells.
                if (j.x2 - j.x1).abs() >= 4_000 || (j.z2 - j.z1).abs() >= 4_000 {
                    let (x, z) = ((j.x2 - j.x1).abs() + 1, (j.z2 - j.z1).abs() + 1);
                    *j = before.clone();
                    anyhow::bail!("{x} x {z} is too big a box - keep each side at 4,000 or less");
                }
            }
            let after = p.job.read().clone();
            if (before.x1, before.z1, before.x2, before.z2, before.top, before.bottom, before.band) != (after.x1, after.z1, after.x2, after.z2, after.top, after.bottom, after.band) {
                p.save_job();
                p.sync_area();
                ctx.feed(
                    "event",
                    &format!("new box: x {}..{}, z {}..{}, y {}..{} - progress starts over", after.x1, after.x2, after.z1, after.z2, y_text(after.bottom, "bedrock"), y_text(after.top, "build height")),
                );
            }
        }
        "run" => {
            let on = c["on"].as_bool().unwrap_or(true);
            p.job.write().running = on;
            p.save_job();
            ctx.feed("event", if on { "perimeter job running" } else { "perimeter job paused - the bots hold where they are" });
        }
        "reset-progress" => {
            {
                let mut b = p.board.lock();
                b.reset();
                // The world's height is learnt again too (a box in another dimension).
                b.world = None;
            }
            ctx.feed("event", "progress cleared - the bots read every cell's ground again");
        }
        "settings" => {
            {
                let mut j = p.job.write();
                if let Some(list) = c["fill"].as_array() {
                    let ids: Vec<String> = list
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.trim().trim_start_matches("minecraft:").to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !ids.is_empty() {
                        j.fill = ids;
                    }
                }
                let n = |k: &str, lo: f64, hi: f64| f64_of(&c[k]).filter(|v| *v >= lo && *v <= hi);
                if let Some(v) = n("keepFill", 0.0, 30.0) {
                    j.keep_fill_stacks = v as usize;
                }
                if let Some(v) = n("breaks", 1.0, 20.0) {
                    j.max_breaks_per_tick = v as usize;
                }
                if let Some(v) = n("places", 1.0, 8.0) {
                    j.places_per_tick = v as usize;
                }
                if let Some(v) = n("stopProgress", 0.7, 1.0) {
                    j.stop_progress = v;
                }
                if let Some(v) = n("retire", 1.0, 1000.0) {
                    j.retire_uses = v as i32;
                }
                if let Some(v) = n("spread", 0.0, 100.0) {
                    j.spread = v as i32;
                }
                if let Some(v) = c["protectContainers"].as_bool() {
                    j.protect_containers = v;
                }
                if let Some(s) = c["toolsSign"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    j.tools_sign = s.to_string();
                }
                if let Some(s) = c["repairSign"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    j.repair_sign = s.to_string();
                }
            }
            p.save_job();
            {
                let mut cfg = ctx.cfg.write();
                if let Some(s) = c["foodSign"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    cfg.food_sign = s.to_string();
                }
                if let Some(v) = f64_of(&c["foodLow"]).filter(|v| (0.0..=200.0).contains(v)) {
                    cfg.food_low_points = v as i32;
                }
                if let Some(v) = f64_of(&c["maxSpeed"]).filter(|v| (1.0..=100.0).contains(v)) {
                    cfg.max_speed = v;
                }
                if let Some(v) = f64_of(&c["maxBots"]).filter(|v| (1.0..=10.0).contains(v)) {
                    cfg.max_bots = v as usize;
                }
            }
            ctx.save_cfg();
            for b in ctx.bots.read().values() {
                *b.food_retry_at.lock() = None;
                *b.tools_retry_at.lock() = None;
            }
            ctx.feed("event", "perimeter settings saved");
        }
        // Done by the bot on its tick (it reads the signs around it).
        "record-base" => {
            let b = ctx.bot(name).ok_or_else(|| anyhow::anyhow!("no bot {name}"))?;
            if !b.online.load(Ordering::Relaxed) {
                anyhow::bail!("{name} is not online - start it by the chests first");
            }
            b.inbox.lock().push_back(c.clone());
        }
        "forget-base" => {
            p.bases.lock().remove(name);
            p.save_bases();
            ctx.feed("event", &format!("{name}: base forgotten (it uses another bot's until it is logged in by the chests again)"));
        }
        // A /command typed for one bot or all of them.
        "command" => {
            let names: Vec<String> = if name == "*" { ctx.bots.read().keys().cloned().collect() } else { vec![name.to_string()] };
            for n in names {
                if let Some(b) = ctx.bot(&n) {
                    b.inbox.lock().push_back(c.clone());
                }
            }
        }
        other => anyhow::bail!("unknown command {other}"),
    }
    Ok(())
}

fn y_text(y: Option<i32>, none: &str) -> String {
    y.map_or(none.to_string(), |y| y.to_string())
}
