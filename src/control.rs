//! What the control panel shows and what its buttons do, for the Rust bot. The state is the Java
//! ControlPanel.snapshot() shape (name, instance, online, health, food, x/y/z, inventory, modules,
//! ai{job, ...}, places) so the same page and the Discord status watcher read it unchanged. The
//! watcher's states come from ai.job: "melon routine: <phase>" while the routine runs, empty when
//! it is stopped, and "waiting for a Silk Touch axe" in it means repairs are needed.

use std::sync::atomic::Ordering;

use azalea::{Client, Vec3};
use serde_json::{Value, json};
use tracing::info;

use crate::{bot::Shared, inv, movement};

/// /api/state, minus the feed (the panel adds that).
pub fn snapshot(bot: &Client, s: &Shared) -> Value {
    let pos = bot.position().ok();
    let online = pos.is_some();
    let r = |v: f64| (v * 10.0).round() / 10.0;
    let mut o = json!({
        "name": s.cfg.account,
        "port": s.panel.as_ref().map_or(0, |p| p.port),
        "instance": s.cfg.instance,
        "server": s.cfg.server,
        "online": online,
        "screen": "",
        "watching": 0,
        "client": "rust",
    });
    if let Some(p) = pos {
        let ping = bot.tab_list().ok().and_then(|t| t.get(&bot.uuid()).map(|i| i.latency)).unwrap_or(-1);
        let joined = s.joined_secs();
        o["health"] = json!(r(bot.health().unwrap_or(0.0) as f64));
        o["maxHealth"] = json!(20.0);
        o["food"] = json!(bot.hunger().map(|h| h.food).unwrap_or(0));
        o["x"] = json!(r(p.x));
        o["y"] = json!(r(p.y));
        o["z"] = json!(r(p.z));
        o["dim"] = json!("overworld");
        o["dead"] = json!(crate::routine::is_dead(bot));
        o["ping"] = json!(ping);
        o["onlineFor"] = json!(joined);
        o["goto"] = json!(if movement::busy(bot) { movement::status(bot) } else { String::new() });
        // Player inventory indices as the Java panel sends them: 0-8 hotbar, 9-35 main.
        let inv: Vec<Value> = inv::bags(bot)
            .into_iter()
            .filter(|(_, st)| !st.is_empty())
            .map(|(slot, st)| {
                let idx = if slot >= 36 { slot - 36 } else { slot };
                let mut j = json!({"slot": idx, "id": inv::stack_id(&st), "count": st.count()});
                if let Some(left) = inv::uses_left(&st) {
                    let max = st.get_component::<azalea::inventory::components::MaxDamage>().map_or(0, |m| m.amount);
                    j["dur"] = json!(format!("{left}/{max}"));
                }
                j
            })
            .collect();
        o["inventory"] = Value::Array(inv);
    }
    // The modules this bot runs, with what each is doing.
    let routine_running = s.routine_running();
    let phase = s.routine_phase();
    let mut mods = vec![
        json!({"name": "melon-routine", "cat": "AI", "on": routine_running, "info": phase}),
        json!({"name": "flight", "cat": "Movement", "on": true, "info": if online { movement::status(bot) } else { String::new() }}),
        json!({"name": "no-fall", "cat": "Movement", "on": true}),
        json!({"name": "velocity", "cat": "Movement", "on": true}),
    ];
    if s.autoeat.is_some() {
        mods.push(json!({"name": "auto-eat", "cat": "Player", "on": s.autoeat_on.load(Ordering::Relaxed)}));
    }
    if s.killaura.is_some() {
        mods.push(json!({"name": "kill-aura", "cat": "Combat", "on": s.killaura_on.load(Ordering::Relaxed)}));
    }
    if let Some(h) = &s.harvest {
        mods.push(json!({"name": "melon-farmer", "cat": "World", "on": h.on(), "info": h.status()}));
    }
    o["modules"] = Value::Array(mods);
    o["ai"] = json!({
        "running": routine_running,
        "question": "",
        "lastAction": phase,
        "steps": 0,
        "provider": "rust",
        "model": "melon routine (no model)",
        "instruction": if routine_running { "melon routine" } else { "" },
        "outcome": "",
        "deaths": s.deaths.load(Ordering::Relaxed),
        "job": if routine_running && !phase.is_empty() { format!("melon routine: {phase}") } else { String::new() },
    });
    let places: Vec<Value> = s.cfg.base.iter().map(|b| json!({"name": "base", "x": b.x, "y": b.y, "z": b.z})).collect();
    o["places"] = Value::Array(places);
    // The seller's Sell tab and the buyer's Shop tab (the page shows each only when it is here).
    if s.cfg.is_seller() {
        o["sell"] = crate::seller::panel_state(bot, s);
    }
    if s.cfg.is_buyer() {
        o["shop"] = crate::buyer::panel_state(bot, s);
    }
    o
}

/// One order from the page. Runs on the bot's tick.
pub fn handle(bot: &Client, s: &std::sync::Arc<Shared>, cmd: &Value) {
    let kind = cmd["type"].as_str().unwrap_or("");
    let text = cmd["text"].as_str().unwrap_or("").trim().to_string();
    info!(target: "panel", "cmd {kind} {text}");
    let say = |t: &str| {
        if let Some(p) = &s.panel {
            p.feed("say", t);
        }
        info!(target: "panel", "{t}");
    };
    match kind {
        "input" if text.is_empty() => {}
        // A server command typed on the panel (the Java panel does the same). Never plain chat.
        "input" if text.starts_with('/') => {
            bot.chat(text.clone());
            say(&format!("sent {text}"));
        }
        "input" if text.starts_with('.') => {
            let w: Vec<&str> = text[1..].split_whitespace().collect();
            match w.as_slice() {
                ["stop"] | ["routine", "stop"] => {
                    s.stop_routine(bot);
                    say("melon routine stopped");
                }
                ["start"] | ["continue"] | ["routine", "start"] => {
                    s.start_routine(bot, false);
                    say("melon routine starting");
                }
                ["goto", x, y, z] => match (x.parse::<f64>(), y.parse::<f64>(), z.parse::<f64>()) {
                    (Ok(x), Ok(y), Ok(z)) => {
                        s.stop_routine(bot);
                        movement::go(bot, Vec3::new(x, y, z), "panel goto");
                        say(&format!("routine stopped - flying to {x} {y} {z}"));
                    }
                    _ => say("goto needs three numbers"),
                },
                ["status"] => say(&format!("{} | {}", s.routine_phase(), movement::status(bot))),
                _ => say("commands here: .start .stop .goto x y z .status (and /server commands)"),
            }
        }
        "input" => say("the AI pilot is not ported to the Rust bot yet - only the melon routine runs here"),
        "ai-stop" => {
            s.stop_routine(bot);
            say("melon routine stopped");
        }
        "ai-continue" => {
            s.start_routine(bot, false);
            say("melon routine starting");
        }
        "toggle" => match text.as_str() {
            "auto-eat" => flip(&s.autoeat_on, &say, "auto-eat"),
            "kill-aura" => flip(&s.killaura_on, &say, "kill-aura"),
            "melon-routine" => {
                if s.routine_running() {
                    s.stop_routine(bot);
                } else {
                    s.start_routine(bot, false);
                }
            }
            "melon-farmer" => {
                if let Some(h) = &s.harvest {
                    h.set_on(!h.on());
                }
            }
            other => say(&format!("{other} cannot be switched here")),
        },
        "disconnect" => {
            say("logging out - stays out until relaunched");
            s.leave(bot, "disconnected from the control panel");
        }
        "connect" => say("the Rust bot reconnects by itself; after a logout, start it again (its launcher, start-melon-farmer.cmd)"),
        "respawn" => say("respawning is automatic"),
        // "Go to base" and the Places tab's Go: base is the only place the Rust bot knows.
        "job" if cmd["name"] == "go" => match (&s.cfg.base, cmd["args"]["place"].as_str()) {
            (Some(b), Some("base")) => {
                s.stop_routine(bot);
                movement::go(bot, Vec3::new(b.x as f64 + 0.5, b.y as f64, b.z as f64 + 0.5), "panel go to base");
                say("routine stopped - flying to base (Continue AI starts it again)");
            }
            (None, _) => say("no base saved for this bot"),
            (_, p) => say(&format!("the Rust bot only knows the base, not {}", p.unwrap_or("?"))),
        },
        "job" => say("jobs need the AI pilot - not ported to the Rust bot yet"),
        "sell-list" if s.cfg.is_seller() => say(&crate::seller::panel_cmd(s, cmd)),
        "shop-spend" | "shop-buy" if s.cfg.is_buyer() => say(&crate::buyer::panel_cmd(s, cmd)),
        _ => say(&format!("unknown command {kind}")),
    }
}

fn flip(flag: &std::sync::atomic::AtomicBool, say: &impl Fn(&str), name: &str) {
    let now = !flag.load(Ordering::Relaxed);
    flag.store(now, Ordering::Relaxed);
    say(&format!("{name} {}", if now { "on" } else { "off" }));
}
