//! The swarm panel: what it shows (/api/state) and what its buttons do (/api/cmd). Served by the
//! same panel code as a bot of its own (same key, same loopback checks), on its own port with its
//! own page, so it is never mistaken for one.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use azalea::Vec3;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{
    SWARM_MODULES, SwarmCtx, accounts,
    formation::Formation,
    nuker,
    orders::{Centre, Order, Source, Stamped},
    printer,
};

pub const PAGE: &str = include_str!("swarm.html");

/// Every 250 ms: the page's commands, then a fresh state.
pub async fn run(ctx: Arc<SwarmCtx>) {
    let Some(panel) = ctx.panel.clone() else {
        warn!(target: "swarm", "no swarm panel (its port was taken)");
        return;
    };
    info!(target: "swarm", "swarm panel: http://127.0.0.1:{}/", panel.port);
    loop {
        let cmds: Vec<Value> = panel.commands.lock().drain(..).collect();
        for c in cmds {
            info!(target: "swarm", "panel: {c}");
            if let Err(e) = handle(&ctx, &c) {
                warn!(target: "swarm", "panel command failed: {e:#}");
                ctx.feed("say", &format!("{e:#}"));
            }
        }
        let snap = snapshot(&ctx);
        *panel.state.lock() = snap;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn snapshot(ctx: &SwarmCtx) -> Value {
    let cfg = ctx.cfg.read().clone();
    let bots: Vec<Value> = ctx
        .bots
        .read()
        .values()
        .map(|b| {
            let file = b.file.lock().clone();
            let mut row = b.row.lock().clone();
            let online = b.online.load(Ordering::Relaxed);
            row["name"] = json!(b.name);
            row["running"] = json!(b.running.load(Ordering::Relaxed));
            row["starting"] = json!(b.starting.load(Ordering::Relaxed));
            row["online"] = json!(online);
            row["error"] = json!(*b.last_error.lock());
            row["autostart"] = json!(file.autostart);
            row["listen"] = json!(file.listen_controller);
            row["link"] = json!(b.controller_link.connected.load(Ordering::Relaxed));
            row["linkError"] = json!(*b.controller_link.last_error.lock());
            row["onlineFor"] = json!(b.joined_at.lock().map_or(0, |t| t.elapsed().as_secs()));
            row["own"] = json!(b.orders.lock().own.as_ref().map(|o| o.order.describe()));
            row["overrides"] = file.overrides;
            if !online {
                row["order"] = json!("");
                row["state"] = json!("");
            }
            row
        })
        .collect();
    let cv = ctx.controller.lock();
    let controller = json!({
        "enabled": cfg.controller_enabled,
        "host": cfg.controller_host,
        "port": cfg.controller_port,
        "ws": cfg.controller_ws,
        "wsOffset": cfg.controller_ws_offset,
        "wsConnected": cv.ws_connected,
        "wsError": cv.ws_error,
        "pos": cv.pos.as_ref().map(|(p, yaw, dim, at)| json!({
            "x": (p.x * 10.0).round() / 10.0, "y": (p.y * 10.0).round() / 10.0, "z": (p.z * 10.0).round() / 10.0,
            "yaw": yaw.round(), "dim": dim, "age": at.elapsed().as_secs_f64(),
        })),
        "links": ctx.bots.read().values().filter(|b| b.controller_link.connected.load(Ordering::Relaxed)).count(),
        "syncs": cv.syncs.iter().map(|(k, (raw, at))| json!({"kind": k, "text": super::controller::short(raw), "age": at.elapsed().as_secs()})).collect::<Vec<_>>(),
    });
    drop(cv);
    let order = ctx.order.lock().clone();
    let seen: Vec<Value> = ctx
        .sightings
        .lock()
        .values()
        .filter(|s| s.at.elapsed() < Duration::from_secs(10))
        .map(|s| json!({"name": s.name, "x": s.pos.x.round(), "y": s.pos.y.round(), "z": s.pos.z.round(), "by": s.by}))
        .collect();
    let modules = ctx.modules.read().clone();
    let mods: Vec<Value> = SWARM_MODULES.iter().map(|m| json!({"name": m, "enabled": modules[*m]["enabled"].as_bool().unwrap_or(false), "settings": modules[*m]["settings"].clone()})).collect();
    let nk = &modules[nuker::MODULE];
    let nuker_area = nuker::Cfg::from_settings(&crate::modules::ModuleConfig::from_value(json!({ nuker::MODULE: {"enabled": true, "settings": nk["settings"].clone()} }))).and_then(|c| c.area);
    let progress = nuker_area.map(|a| ctx.nuker_board.lock().progress(a));
    let nuker = json!({
        "on": nk["enabled"].as_bool().unwrap_or(false),
        "settings": nk["settings"].clone(),
        "cells": progress.map(|(d, all)| json!({"done": d, "all": all})),
    });
    let combat = ctx.combat.lock().clone();
    let attackers: Vec<Value> = ctx
        .attackers
        .lock()
        .0
        .iter()
        .map(|(u, (at, victim))| json!({"uuid": u.to_string(), "victim": victim, "age": at.elapsed().as_secs()}))
        .collect();
    let ci = ctx.controller_instance.lock().clone();
    let places = ctx.places.lock().clone();
    let printer = {
        let pm = &modules[printer::MODULE];
        // What the bots run: the controller's printer settings under the swarm's own.
        let mut effective = ci.as_ref().and_then(|c| c.printer.clone()).map(|p| p["settings"].clone()).filter(Value::is_object).unwrap_or_else(|| json!({}));
        if let (Some(e), Some(o)) = (effective.as_object_mut(), pm["settings"].as_object()) {
            e.extend(o.clone());
        }
        let job = ctx.print_job.read().clone();
        let (supply, mapart) = ctx.print_chests.lock().clone();
        json!({
            "on": pm["enabled"].as_bool().unwrap_or(false),
            "settings": pm["settings"].clone(),
            "effective": effective,
            "controller": ci.is_some(),
            "job": job.as_ref().map(|j| json!({
                "name": j.name,
                "source": j.source,
                "file": j.schematic.file_name().map(|f| f.to_string_lossy().to_string()),
                "blocks": j.target.blocks.len(),
                "cells": j.by_cell.len(),
                "min": [j.area.min.x, j.area.min.y, j.area.min.z],
                "max": [j.area.max.x, j.area.max.y, j.area.max.z],
                "disabled": j.target.disabled,
                "unknown": j.target.unknown,
            })),
            "cellsDone": ctx.print_board.lock().done_total(),
            "offers": ctx.printer_offers.lock().iter().map(|(n, on, d)| json!({"name": n, "enabled": on, "dim": d})).collect::<Vec<_>>(),
            "chosen": cfg.printer_placement,
            "dropDir": ctx.dir.join("schematics").display().to_string(),
            "chests": {"supply": supply.len(), "mapart": mapart.len()},
        })
    };
    let ec_search = {
        let s = ctx.ec_search.lock();
        match (&s.by, s.none_found) {
            (Some(by), _) => format!("{by} is searching - {}", s.progress),
            (None, Some((c, r))) => format!("the last search found nothing within {r:.0} blocks of {:.0} {:.0}", c.x, c.z),
            _ => String::new(),
        }
    };
    json!({
        "name": "BOTSWARM",
        "client": "rust-swarm",
        "server": cfg.server,
        "offline": ctx.offline,
        "bots": bots,
        "controller": controller,
        "order": {"text": order.order.describe(), "source": order.source, "raw": order.order},
        "formation": cfg.formation,
        "shapes": super::formation::Shape::ALL.iter().map(|s| s.name()).collect::<Vec<_>>(),
        "friends": cfg.friends,
        "maxSpeed": cfg.max_speed,
        "fanOut": cfg.fan_out_spacing,
        "seen": seen,
        "modules": mods,
        "login": *ctx.login.lock(),
        "maxBots": cfg.max_bots,
        "food": {"chest": cfg.food_chest, "sign": cfg.food_sign, "low": cfg.food_low_points},
        "chests": {"toolsSign": cfg.tools_sign, "toolsRadius": cfg.tools_radius, "storageSign": cfg.storage_sign, "known": places.signed},
        "eclink": {"on": cfg.ec_link, "chest": cfg.ender_chest, "panic": cfg.ec_panic_health, "range": cfg.ec_search_range, "search": ec_search},
        "nuker": nuker,
        "printer": printer,
        "combat": {"defend": combat.defend, "target": combat.target, "attackers": attackers, "fightBack": cfg.fight_back},
        "controllerInstance": ci.map(|c| json!({"name": c.name, "targets": c.killaura_targets.map(|t| t.len()), "nukerBox": c.nuker.is_some()})),
    })
}

fn names(ctx: &SwarmCtx, c: &Value) -> Vec<String> {
    match c["name"].as_str() {
        Some("*") | None => ctx.bots.read().keys().cloned().collect(),
        Some(n) => vec![n.to_string()],
    }
}

fn f64_of(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// An order from the page. A formation round "here" becomes round the swarm's middle now.
fn order_of(ctx: &SwarmCtx, v: &Value) -> anyhow::Result<Order> {
    if v["kind"] == "formation" && v["centre"]["kind"] == "here" {
        let spots: Vec<Vec3> = ctx
            .bots
            .read()
            .values()
            .filter_map(|b| {
                let r = b.row.lock();
                Some(Vec3::new(r["x"].as_f64()?, r["y"].as_f64()?, r["z"].as_f64()?))
            })
            .collect();
        if spots.is_empty() {
            anyhow::bail!("no bot is online to take \"here\" from");
        }
        let n = spots.len() as f64;
        let mid = spots.iter().fold(Vec3::ZERO, |a, p| a + *p);
        return Ok(Order::Formation { centre: Centre::Point { x: (mid.x / n).round(), y: (mid.y / n).round(), z: (mid.z / n).round() } });
    }
    Ok(serde_json::from_value(v.clone())?)
}

fn handle(ctx: &Arc<SwarmCtx>, c: &Value) -> anyhow::Result<()> {
    match c["type"].as_str().unwrap_or("") {
        "add-account" => super::start_login(ctx),
        "cancel-login" => super::cancel_login(ctx),
        "start" => {
            for n in names(ctx, c) {
                ctx.start(&n);
            }
        }
        "stop" => {
            for n in names(ctx, c) {
                ctx.stop(&n);
            }
        }
        "remove" => {
            let n = c["name"].as_str().unwrap_or("");
            super::remove(ctx, n)?;
            ctx.feed("event", &format!("{n} removed from the swarm"));
        }
        "autostart" | "listen" => {
            let on = c["on"].as_bool().unwrap_or(true);
            for n in names(ctx, c) {
                let Some(b) = ctx.bot(&n) else { continue };
                let mut f = b.file.lock();
                if c["type"] == "autostart" {
                    f.autostart = on;
                } else {
                    f.listen_controller = on;
                }
                accounts::save(&ctx.accounts_root, &n, &f)?;
                drop(f);
                b.mods.lock().dirty = true;
            }
        }
        // The whole swarm's order.
        "order" => {
            let order = order_of(ctx, &c["order"])?;
            let was_formation = matches!(ctx.order.lock().order, Order::Formation { .. });
            if matches!(order, Order::Formation { .. }) && !was_formation {
                *ctx.formation_epoch.lock() = std::time::Instant::now();
            }
            ctx.feed("event", &format!("swarm order: {}", order.describe()));
            *ctx.order.lock() = Stamped::new(order, Source::Panel);
        }
        // One bot's own order (null = back to the swarm's).
        "bot-order" => {
            let n = c["name"].as_str().unwrap_or("");
            let b = ctx.bot(n).ok_or_else(|| anyhow::anyhow!("no bot {n}"))?;
            let own = if c["order"].is_null() { None } else { Some(Stamped::new(order_of(ctx, &c["order"])?, Source::Bot)) };
            ctx.feed("event", &format!("{n}: {}", own.as_ref().map_or("back to the swarm's orders".into(), |o| o.order.describe())));
            b.orders.lock().own = own;
        }
        "formation" => {
            let f: Formation = serde_json::from_value(c["formation"].clone())?;
            ctx.cfg.write().formation = f;
            ctx.save_cfg();
        }
        "controller" => {
            {
                let mut cfg = ctx.cfg.write();
                if let Some(on) = c["enabled"].as_bool() {
                    cfg.controller_enabled = on;
                }
                if let Some(h) = c["host"].as_str().filter(|h| !h.trim().is_empty()) {
                    cfg.controller_host = h.trim().to_string();
                }
                if let Some(p) = f64_of(&c["port"]).filter(|p| *p >= 1.0 && *p <= 65535.0) {
                    cfg.controller_port = p as u16;
                }
                if let Some(on) = c["ws"].as_bool() {
                    cfg.controller_ws = on;
                }
                if let Some(o) = f64_of(&c["wsOffset"]).filter(|o| *o >= 0.0 && *o < 1000.0) {
                    cfg.controller_ws_offset = o as u16;
                }
            }
            ctx.save_cfg();
            // Re-link on the new address.
            for b in ctx.bots.read().values() {
                if b.running.load(Ordering::SeqCst) {
                    b.controller_link.stop();
                    b.controller_link.start(ctx.clone(), b.name.clone());
                }
            }
        }
        "friends" => {
            let list: Vec<String> = c["list"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default();
            ctx.cfg.write().friends = list;
            ctx.save_cfg();
        }
        "speed" => {
            if let Some(v) = f64_of(&c["maxSpeed"]).filter(|v| *v >= 1.0 && *v <= 100.0) {
                ctx.cfg.write().max_speed = v;
            }
            if let Some(v) = f64_of(&c["fanOut"]).filter(|v| *v >= 0.0 && *v <= 50.0) {
                ctx.cfg.write().fan_out_spacing = v;
            }
            ctx.save_cfg();
        }
        // A module setting (or its on/off), for the swarm or as one bot's override. A null value
        // removes the override.
        "setting" | "module" => {
            let m = c["module"].as_str().unwrap_or("");
            if !SWARM_MODULES.contains(&m) {
                anyhow::bail!("{m} is not a swarm module");
            }
            let is_module = c["type"] == "module";
            let key = c["key"].as_str().unwrap_or("").to_string();
            let value = if is_module { c["on"].clone() } else { c["value"].clone() };
            match c["bot"].as_str() {
                Some(n) if !n.is_empty() => {
                    let b = ctx.bot(n).ok_or_else(|| anyhow::anyhow!("no bot {n}"))?;
                    let mut f = b.file.lock();
                    if !f.overrides.is_object() {
                        f.overrides = json!({});
                    }
                    let entry = &mut f.overrides[m];
                    set_or_remove(entry, is_module, &key, value);
                    accounts::save(&ctx.accounts_root, n, &f)?;
                    drop(f);
                    b.mods.lock().dirty = true;
                }
                _ => {
                    {
                        let mut all = ctx.modules.write();
                        set_or_remove(&mut all[m], is_module, &key, value);
                    }
                    ctx.save_modules();
                    ctx.modules_changed();
                }
            }
        }
        // The food chest: where the controller player stands now, a typed spot, or none.
        "food" => {
            {
                let mut cfg = ctx.cfg.write();
                match &c["chest"] {
                    Value::String(s) if s == "controller" => {
                        let p = ctx.controller.lock().pos.as_ref().filter(|p| p.3.elapsed().as_secs() < 5).map(|p| p.0);
                        let p = p.ok_or_else(|| anyhow::anyhow!("no live controller position - is the controller mod on as Controller, with its position stream?"))?;
                        cfg.food_chest = Some(crate::config::Pos { x: p.x.floor() as i32, y: p.y.floor() as i32, z: p.z.floor() as i32 });
                    }
                    Value::Null if c.get("chest").is_some() => cfg.food_chest = None,
                    Value::Object(_) => {
                        let (x, y, z) = (f64_of(&c["chest"]["x"]), f64_of(&c["chest"]["y"]), f64_of(&c["chest"]["z"]));
                        if let (Some(x), Some(y), Some(z)) = (x, y, z) {
                            cfg.food_chest = Some(crate::config::Pos { x: x as i32, y: y as i32, z: z as i32 });
                        }
                    }
                    _ => {}
                }
                if let Some(s) = c["sign"].as_str() {
                    cfg.food_sign = s.trim().to_string();
                }
                if let Some(v) = f64_of(&c["low"]).filter(|v| *v >= 0.0 && *v <= 200.0) {
                    cfg.food_low_points = v as i32;
                }
            }
            ctx.save_cfg();
            // Try again now rather than after the wait an empty search left.
            for b in ctx.bots.read().values() {
                *b.food_retry_at.lock() = None;
            }
        }
        // The ender chest link: on/off, its chest (where the controller player stands now, or
        // none - then the nearest one any bot has seen), the health that sends loans back.
        "eclink" => {
            {
                let mut cfg = ctx.cfg.write();
                if let Some(on) = c["on"].as_bool() {
                    cfg.ec_link = on;
                }
                match &c["chest"] {
                    Value::String(s) if s == "controller" => {
                        let p = ctx.controller.lock().pos.as_ref().filter(|p| p.3.elapsed().as_secs() < 5).map(|p| p.0);
                        let p = p.ok_or_else(|| anyhow::anyhow!("no live controller position - is the controller mod on as Controller, with its position stream?"))?;
                        cfg.ender_chest = Some(crate::config::Pos { x: p.x.floor() as i32, y: p.y.floor() as i32, z: p.z.floor() as i32 });
                    }
                    Value::Null if c.get("chest").is_some() => cfg.ender_chest = None,
                    _ => {}
                }
                if let Some(v) = f64_of(&c["panic"]).filter(|v| *v >= 0.0 && *v <= 20.0) {
                    cfg.ec_panic_health = v as f32;
                }
                if let Some(v) = f64_of(&c["range"]).filter(|v| *v >= 16.0 && *v <= 30_000.0) {
                    cfg.ec_search_range = v;
                }
            }
            ctx.save_cfg();
            // Any change (a chest set, "search again", a new range): the next try searches afresh.
            ctx.ec_search.lock().none_found = None;
            ctx.bad_ender_chests.lock().clear();
            for b in ctx.bots.read().values() {
                b.eclink.lock().retry_at = 0;
            }
        }
        "chests" => {
            {
                let mut cfg = ctx.cfg.write();
                if let Some(s) = c["toolsSign"].as_str() {
                    cfg.tools_sign = s.trim().to_string();
                }
                if let Some(v) = f64_of(&c["toolsRadius"]).filter(|v| *v >= 4.0 && *v <= 1000.0) {
                    cfg.tools_radius = v;
                }
                if let Some(s) = c["storageSign"].as_str() {
                    cfg.storage_sign = s.trim().to_string();
                }
            }
            ctx.save_cfg();
        }
        "max-bots" => {
            if let Some(v) = f64_of(&c["n"]).filter(|v| *v >= 1.0 && *v <= 10.0) {
                ctx.cfg.write().max_bots = v as usize;
                ctx.save_cfg();
            }
        }
        // The nuker for the swarm: on/off, mode, box, blocks, list mode, collect.
        "nuker" => {
            let before = ctx.modules.read()[nuker::MODULE]["settings"].clone();
            {
                let mut all = ctx.modules.write();
                let m = &mut all[nuker::MODULE];
                if !m["settings"].is_object() {
                    m["settings"] = json!({});
                }
                if let Some(on) = c["on"].as_bool() {
                    m["enabled"] = json!(on);
                }
                for k in ["target-mode", "list-mode"] {
                    if let Some(v) = c[k].as_str() {
                        m["settings"][k] = json!(v);
                    }
                }
                for k in ["x1", "y1", "z1", "x2", "y2", "z2", "stop-progress", "max-breaks-per-tick", "range"] {
                    if let Some(v) = f64_of(&c[k]) {
                        m["settings"][k] = json!(v);
                    }
                }
                for k in ["collect", "auto-move", "auto-tool"] {
                    if let Some(v) = c[k].as_bool() {
                        m["settings"][k] = json!(v);
                    }
                }
                if let Some(list) = c["blocks"].as_array() {
                    let ids: Vec<String> = list
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| if s.contains(':') { s.to_string() } else { format!("minecraft:{s}") })
                        .collect();
                    m["settings"]["blocks"] = json!(ids);
                }
            }
            nuker_box_changed(ctx, &before);
            ctx.save_modules();
            ctx.modules_changed();
        }
        // A corner of the box at the controller player's feet.
        "nuker-corner" => {
            let which = if c["which"].as_i64() == Some(2) { "2" } else { "1" };
            let p = ctx.controller.lock().pos.as_ref().filter(|p| p.3.elapsed().as_secs() < 5).map(|p| p.0);
            let p = p.ok_or_else(|| anyhow::anyhow!("no live controller position - is the controller mod on as Controller, with its position stream?"))?;
            let before = ctx.modules.read()[nuker::MODULE]["settings"].clone();
            {
                let mut all = ctx.modules.write();
                let s = &mut all[nuker::MODULE]["settings"];
                s[format!("x{which}")] = json!(p.x.floor());
                s[format!("y{which}")] = json!(p.y.floor());
                s[format!("z{which}")] = json!(p.z.floor());
            }
            nuker_box_changed(ctx, &before);
            ctx.save_modules();
            ctx.modules_changed();
        }
        // The controller instance's own FastNuker: its box, blocks and modes.
        "nuker-from-controller" => {
            let src = ctx.controller_instance.lock().as_ref().and_then(|c| c.nuker.clone());
            let src = src.ok_or_else(|| anyhow::anyhow!("no instance has the controller mod on as Controller (or it has no fast-nuker settings)"))?;
            let before = ctx.modules.read()[nuker::MODULE]["settings"].clone();
            {
                let mut all = ctx.modules.write();
                let s = &mut all[nuker::MODULE]["settings"];
                for k in ["target-mode", "list-mode", "blocks", "x1", "y1", "z1", "x2", "y2", "z2", "stop-progress", "max-breaks-per-tick", "range", "min-durability"] {
                    if !src["settings"][k].is_null() {
                        s[k] = src["settings"][k].clone();
                    }
                }
            }
            nuker_box_changed(ctx, &before);
            ctx.save_modules();
            ctx.modules_changed();
            ctx.feed("event", "nuker settings copied from the controller instance");
        }
        // The printer for the swarm: on/off, its settings (reset: back to the controller's), and
        // which placement when the controller has several.
        "printer" => {
            if c.get("placement").is_some() {
                let p = c["placement"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
                ctx.feed("event", &format!("printer placement: {}", p.as_deref().unwrap_or("the one nearest the controller")));
                ctx.cfg.write().printer_placement = p;
                ctx.save_cfg();
            }
            {
                let mut all = ctx.modules.write();
                let m = &mut all[printer::MODULE];
                if !m["settings"].is_object() || c["reset"].as_bool() == Some(true) {
                    m["settings"] = json!({});
                }
                if let Some(on) = c["on"].as_bool() {
                    m["enabled"] = json!(on);
                }
                for k in ["strict-state-match", "fix-block-states", "auto-till", "clear-wrong-blocks", "use-storage-chests"] {
                    if let Some(v) = c[k].as_bool() {
                        m["settings"][k] = json!(v);
                    }
                }
                for k in ["places-per-window", "place-window-ms"] {
                    if let Some(v) = f64_of(&c[k]) {
                        m["settings"][k] = json!(v);
                    }
                }
            }
            ctx.save_modules();
            ctx.modules_changed();
        }
        "fight-back" => {
            let on = c["on"].as_bool().unwrap_or(true);
            ctx.cfg.write().fight_back = on;
            ctx.save_cfg();
        }
        "defend" => {
            let on = c["on"].as_bool().unwrap_or(false);
            let mut cb = ctx.combat.lock();
            cb.defend = on;
            cb.defend_from = "panel";
            ctx.feed("event", &format!("Defend {}", if on { "on" } else { "off" }));
        }
        "attack" => {
            let name = c["name"].as_str().map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
            if let Some(n) = &name
                && super::combat::protected(ctx).contains(&n.to_lowercase())
            {
                anyhow::bail!("{n} is a friend or one of your accounts - not a target");
            }
            ctx.feed("event", &format!("attack target: {}", name.as_deref().unwrap_or("none")));
            let mut cb = ctx.combat.lock();
            cb.target = name;
            cb.target_from = "panel";
        }
        // A /command typed for one bot or all of them.
        "command" => {
            for n in names(ctx, c) {
                if let Some(b) = ctx.bot(&n) {
                    b.inbox.lock().push_back(c.clone());
                }
            }
        }
        other => anyhow::bail!("unknown command {other}"),
    }
    Ok(())
}

/// A new box (or a box taken from the controller): every cell is open again.
fn nuker_box_changed(ctx: &SwarmCtx, before: &Value) {
    let after = ctx.modules.read()[nuker::MODULE]["settings"].clone();
    let corners = |v: &Value| ["x1", "y1", "z1", "x2", "y2", "z2"].map(|k| v[k].as_f64());
    if corners(before) != corners(&after) {
        ctx.nuker_board.lock().reset();
        info!(target: "swarm", "nuker box set: {:?}", corners(&after));
    }
}

fn set_or_remove(entry: &mut Value, is_module: bool, key: &str, value: Value) {
    if !entry.is_object() {
        *entry = json!({});
    }
    if is_module {
        if value.is_null() {
            entry.as_object_mut().map(|o| o.remove("enabled"));
        } else {
            entry["enabled"] = value;
        }
        return;
    }
    if !entry["settings"].is_object() {
        entry["settings"] = json!({});
    }
    if value.is_null() {
        entry["settings"].as_object_mut().map(|o| o.remove(key));
    } else {
        entry["settings"][key] = value;
    }
}
