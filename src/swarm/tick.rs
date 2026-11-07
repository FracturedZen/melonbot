//! A swarm bot's tick, after the shared handler's (queue, status line): its panel commands, the
//! controller's lines, its modules, what players it can see - then who moves it this tick, in
//! this order: an errand (food, tools, storage), combat (Defend, an attack target), the nuker's
//! box work, and last its order (come here, follow, formation...).

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use azalea::{Client, Vec3, entity::LookDirection, world::WorldName};
use serde_json::{Value, json};
use tracing::{debug, info};

use super::{
    BotLink, Sighting, SwarmCtx, combat, eclink, errand,
    controller::{self, Inbound},
    nuker,
    orders::{self, Centre, Follow, Order, Stamped},
    printer,
};
use crate::{
    bot::Shared,
    modules::{ModuleConfig, autoeat::AutoEat, killaura::{self, KillAura}},
    movement,
};

/// A position stream or Stay Near anchor older than this is not followed.
const FRESH: Duration = Duration::from_secs(3);
/// A sighting older than this is not followed.
const SEEN_FRESH: Duration = Duration::from_secs(5);

pub fn tick(bot: &Client, s: &Arc<Shared>, link: &Arc<BotLink>, ticks: u64) {
    let Some(ctx) = super::ctx() else { return };
    link.online.store(true, Ordering::Relaxed);
    // A perimeter bot (`melonbot perimeter`): its own tick, no orders or controller.
    if let Some(p) = &ctx.perimeter {
        super::perimeter::tick(bot, s, ctx, p, link, ticks);
        return;
    }

    // The panel's commands for this bot.
    let cmds: Vec<Value> = link.inbox.lock().drain(..).collect();
    for c in cmds {
        bot_command(bot, ctx, &c);
    }
    // The controller's lines.
    for line in link.controller_link.drain() {
        on_controller(bot, ctx, link, controller::parse(&line));
    }

    let (eating, _) = modules_tick(bot, ctx, link, ticks);

    if ticks.is_multiple_of(10) {
        record_sightings(bot, ctx, link);
    }
    // The flying speed the panel set.
    if ticks.is_multiple_of(20) {
        movement::set_max_bps(bot, ctx.cfg.read().max_speed);
    }

    // Who moves the bot: an errand, else combat, else the nuker's box work, else its order.
    if ticks.is_multiple_of(40) && !link.errand_running() {
        food_check(bot, ctx, link);
    }
    recover_after_death(bot, link, ticks);
    // Low and under attack: up out of reach first, whatever else it was doing.
    let retreating = combat::retreat(bot, ctx, link);
    if retreating && link.errand_running() {
        link.stop_errand();
    }
    let errand = if retreating { Some(link.combat_status.lock().clone()) } else { link.errand_what() };
    let fighting = errand.is_none() && combat::tick(bot, ctx, link, ticks);
    let nuker_cfg = link.nuker_cfg();
    let nuker_moving = match (&errand, fighting, &nuker_cfg) {
        (None, false, Some(cfg)) if !eating => match nuker::tick(bot, ctx, link, cfg, ticks) {
            nuker::Want::Moving => true,
            nuker::Want::Nothing => false,
            nuker::Want::Tools => {
                tools_errand(bot, ctx, link);
                true
            }
            nuker::Want::Store => {
                store_errand(bot, ctx, link);
                true
            }
        },
        (_, _, None) => {
            ctx.nuker_board.lock().release(&link.name);
            false
        }
        _ => false,
    };
    // The printer: after errands, combat and the nuker's box work.
    let printer_cfg = link.printer_cfg();
    let printer_moving = match (&errand, fighting, nuker_moving, &printer_cfg) {
        (None, false, false, Some(cfg)) if !eating => match printer::tick(bot, ctx, link, cfg, ticks) {
            printer::Want::Moving => true,
            printer::Want::Nothing => false,
            printer::Want::Restock(items) => {
                restock_errand(bot, ctx, link, items);
                true
            }
        },
        (_, _, _, None) => {
            ctx.print_board.lock().release(&link.name);
            false
        }
        _ => false,
    };
    // The ender chest link, always held - never for a print job.
    let printing = printer_cfg.is_some() && ctx.print_job.read().is_some();
    eclink::tick(bot, ctx, link, printing, fighting || retreating, s.deaths.load(Ordering::Relaxed), ticks);
    let busy = if let Some(e) = &errand {
        Some(e.clone())
    } else if fighting {
        Some(link.combat_status.lock().clone())
    } else if nuker_moving {
        Some(format!("nuker: {}", link.nuker.lock().status))
    } else if printer_moving {
        Some(format!("printer: {}", link.printer.lock().status))
    } else {
        None
    };
    let (order, state) = follow_order(bot, ctx, link, busy.is_none());
    let state = busy.unwrap_or(state);

    if ticks.is_multiple_of(20) {
        let pos = bot.position().unwrap_or(Vec3::ZERO);
        let m = link.mods.lock();
        link.controller_link.send(controller::status_line(
            pos,
            &dimension(bot),
            &state,
            false,
            false,
            m.killaura.is_some(),
            bot.health().unwrap_or(0.0).round() as i32,
            bot.hunger().map(|h| h.food as i32).unwrap_or(0),
        ));
    }
    if ticks.is_multiple_of(5) {
        *link.row.lock() = row(bot, s, link, &order, &state);
    }
}

fn on(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

/// The modules, rebuilt when their settings changed, then auto-eat and kill-aura's tick. True
/// while a meal has the hand.
/// The modules' tick: (a meal has the hand, kill-aura has the hand).
pub(super) fn modules_tick(bot: &Client, ctx: &SwarmCtx, link: &BotLink, ticks: u64) -> (bool, bool) {
    {
        let mut m = link.mods.lock();
        if m.dirty {
            let cfg = ModuleConfig::from_value(ctx.merged_modules(link));
            m.autoeat = AutoEat::from_config(&cfg);
            m.killaura = KillAura::from_config(&cfg);
            m.nuker = nuker::Cfg::from_settings(&cfg);
            m.printer = printer::Cfg::from_settings(&cfg);
            m.killaura_targets = cfg.get::<Vec<String>>("kill-aura", "targets", Vec::new()).into_iter().collect();
            m.dirty = false;
            // Switched on but built nothing: say why (kill-aura with no targets has nothing to hit).
            let ka = if m.killaura.is_some() {
                "on"
            } else if cfg.enabled("kill-aura") {
                "on but no targets - it has nothing to hit"
            } else {
                "off"
            };
            info!(target: "swarm", "modules: auto-eat {}, kill-aura {ka}, nuker {}, printer {}", on(m.autoeat.is_some()), on(m.nuker.is_some()), on(m.printer.is_some()));
        }
    }
    let eating = {
        let m = link.mods.lock();
        match &m.autoeat {
            Some(ae) => {
                ae.tick(bot, ticks);
                ae.eating()
            }
            None => false,
        }
    };
    // A perimeter bot strikes inside its box only.
    let bounds = ctx.perimeter.as_ref().map(|p| {
        let j = p.job.read();
        killaura::Bounds { min_x: j.x1.min(j.x2) as f64, min_z: j.z1.min(j.z2) as f64, max_x: (j.x1.max(j.x2) + 1) as f64, max_z: (j.z1.max(j.z2) + 1) as f64 }
    });
    let fighting = link.mods.lock().killaura.as_ref().is_some_and(|k| k.tick(bot, eating, bounds));
    (eating, fighting)
}

/// Back from the dead: its items lie where it died for 5 minutes - go and get them.
pub(super) fn recover_after_death(bot: &Client, link: &Arc<BotLink>, ticks: u64) {
    if ticks.is_multiple_of(20) && !link.errand_running() && bot.health().is_ok_and(|h| h > 0.0) {
        let due = link.died_at.lock().filter(|(_, at)| at.elapsed() > Duration::from_secs(3)).map(|(p, at)| (p, at.elapsed()));
        if let Some((p, since)) = due {
            *link.died_at.lock() = None;
            if since < Duration::from_secs(270) {
                link.start_errand("getting its items back", errand::recover_items(bot.clone(), p));
            }
        }
    }
}

pub fn dimension(bot: &Client) -> String {
    bot.query_self::<&WorldName, _>(|w| w.to_string()).unwrap_or_else(|_| "minecraft:overworld".into())
}

/// Players in view, for every bot's "follow <name>" and formations round a player.
fn record_sightings(bot: &Client, ctx: &SwarmCtx, link: &BotLink) {
    let players: Vec<_> = crate::entities::near(bot).into_iter().filter(|(_, e)| e.player && !e.dead).collect();
    if players.is_empty() {
        return;
    }
    let tab = bot.tab_list().unwrap_or_default();
    let now = Instant::now();
    let yaws: Vec<f32> = {
        let ecs = bot.ecs.read();
        players.iter().map(|(_, e)| ecs.get::<LookDirection>(e.entity).map_or(0.0, |l| l.y_rot())).collect()
    };
    let mut seen = ctx.sightings.lock();
    for ((_, p), yaw) in players.iter().zip(yaws) {
        let Some(name) = p.uuid.and_then(|u| tab.get(&u).map(|i| i.profile.name.to_string())) else { continue };
        seen.insert(name.to_lowercase(), Sighting { name, pos: p.pos, yaw, at: now, by: link.name.clone() });
    }
    seen.retain(|_, s| now.duration_since(s.at) < Duration::from_secs(60));
}

/// Where the controller player is (its stream if fresh, else the last Stay Near anchor).
fn controller_spot(ctx: &SwarmCtx, link: &BotLink) -> Option<(Vec3, f32)> {
    if let Some((p, yaw, _, at)) = &ctx.controller.lock().pos
        && at.elapsed() < FRESH
    {
        return Some((*p, *yaw));
    }
    link.orders.lock().anchor
}

fn player_spot(ctx: &SwarmCtx, name: &str) -> Option<(Vec3, f32)> {
    ctx.sightings.lock().get(&name.to_lowercase()).filter(|s| s.at.elapsed() < SEEN_FRESH).map(|s| (s.pos, s.yaw))
}

/// The bots following the same order as this one, by name: who stands where in a fan-out or a
/// formation. The same order, not the same stamp: the controller sends Come Here down every bot's
/// own link, so each stamps it a moment apart (16 Oct test: two bots fanned out, the third stood
/// on the point).
fn place_in_group(ctx: &SwarmCtx, me: &str, order: &Stamped) -> (usize, usize) {
    let mut names: Vec<String> = ctx
        .bots
        .read()
        .values()
        .filter(|b| b.online.load(Ordering::Relaxed))
        .filter(|b| b.name == me || b.current.lock().as_ref().is_some_and(|c| c.order == order.order))
        .map(|b| b.name.clone())
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    let i = names.iter().position(|n| n == me).unwrap_or(0);
    (i, names.len().max(1))
}

/// Follow this bot's order for one tick: (the order, the state word for STATUS and the panel).
/// `moving` false: something else has the movement this tick (an errand, combat, the nuker) -
/// the order is still worked out (others group by it) but nothing is steered.
fn follow_order(bot: &Client, ctx: &SwarmCtx, link: &BotLink, moving: bool) -> (Stamped, String) {
    let panel = ctx.order.lock().clone();
    let eff = orders::effective(&panel, &link.orders.lock(), link.listens());
    let changed = link.current.lock().as_ref().is_none_or(|c| c.at != eff.at || c.order != eff.order);
    *link.current.lock() = Some(eff.clone());
    if changed {
        info!(target: "swarm", "order: {} (from {:?})", eff.order.describe(), eff.source);
        if matches!(eff.order, Order::Idle) && moving {
            movement::stop(bot);
        }
    }
    if !moving {
        return (eff, String::new());
    }
    let Ok(here) = bot.position() else { return (eff, "offline".into()) };
    let spacing = ctx.cfg.read().fan_out_spacing;
    let state = match &eff.order {
        Order::Idle => "idle".to_string(),
        Order::Goto { x, y, z } | Order::ComeHere { x, y, z } => {
            let (i, n) = place_in_group(ctx, &link.name, &eff);
            let spot = orders::fan_out(Vec3::new(*x, *y, *z), i, n, spacing);
            go(bot, spot, "goto", 0.6, here)
        }
        Order::StayNear { follow, offsets, hold } => {
            let base = match follow {
                Follow::Controller => controller_spot(ctx, link),
                Follow::Player { name } => player_spot(ctx, name).or_else(|| link.orders.lock().anchor),
            };
            match base {
                None => "stay: lost them".to_string(),
                Some((p, yaw)) => {
                    let spot = orders::hold_point(p, yaw, offsets);
                    let flat = ((spot.x - here.x).powi(2) + (spot.z - here.z).powi(2)).sqrt();
                    if flat <= *hold && (spot.y - here.y).abs() <= 1.0 {
                        "stay-hold".to_string()
                    } else {
                        movement::steer(bot, spot, "stay-near", 0.5);
                        "stay-travel".to_string()
                    }
                }
            }
        }
        Order::Formation { centre } => {
            let c = match centre {
                Centre::Controller => controller_spot(ctx, link),
                Centre::Player { name } => player_spot(ctx, name),
                Centre::Point { x, y, z } => Some((Vec3::new(*x, *y, *z), 0.0)),
            };
            match c {
                None => "formation: no centre".to_string(),
                Some((p, yaw)) => {
                    let (i, n) = place_in_group(ctx, &link.name, &eff);
                    let f = ctx.cfg.read().formation.clone();
                    let t = ctx.formation_epoch.lock().elapsed().as_secs_f64();
                    let spot = p + super::formation::slot(&f, i, n, t, yaw);
                    movement::steer(bot, spot, "formation", 0.35);
                    format!("formation {} {}/{n}", f.shape.name(), i + 1)
                }
            }
        }
    };
    (eff, state)
}

fn go(bot: &Client, spot: Vec3, label: &str, arrive: f64, here: Vec3) -> String {
    if here.distance_to(spot) <= arrive.max(1.0) {
        return "arrived".into();
    }
    // A spot the server keeps refusing: wait rather than push into it again.
    if movement::refused_near(bot, spot, 1.5) {
        return "refused by the server".into();
    }
    movement::steer(bot, spot, label, arrive);
    "travel".into()
}

fn on_controller(bot: &Client, ctx: &SwarmCtx, link: &BotLink, msg: Inbound) {
    debug!(target: "controller", "{msg:?}");
    match msg {
        Inbound::ComeHere { pos, .. } => {
            info!(target: "controller", "Come Here to {:.0} {:.0} {:.0}", pos.x, pos.y, pos.z);
            link.orders.lock().from_controller(Order::ComeHere { x: pos.x, y: pos.y, z: pos.z });
        }
        Inbound::Goto { pos, .. } => {
            info!(target: "controller", "Go To {:.0} {:.0} {:.0}", pos.x, pos.y, pos.z);
            link.orders.lock().from_controller(Order::Goto { x: pos.x, y: pos.y, z: pos.z });
        }
        Inbound::StayNear { pos, hold, follow, offsets, .. } => {
            let mut o = link.orders.lock();
            // Its yaw rides on the position stream; a bare Stay Near has none.
            let yaw = ctx.controller.lock().pos.as_ref().map_or(o.anchor.map_or(0.0, |a| a.1), |p| p.1);
            o.anchor = Some((pos, yaw));
            let prev_offsets = match &o.controller {
                Some(Stamped { order: Order::StayNear { offsets, .. }, .. }) => *offsets,
                _ => Default::default(),
            };
            let follow = match follow {
                Some(n) => Follow::Player { name: n },
                None => Follow::Controller,
            };
            o.from_controller(Order::StayNear { follow, offsets: offsets.unwrap_or(prev_offsets), hold });
        }
        Inbound::StayNearOff => {
            info!(target: "controller", "Stay Near released");
            link.orders.lock().controller_release();
        }
        Inbound::FollowTarget(name) => {
            let mut o = link.orders.lock();
            if let Some(Stamped { order: Order::StayNear { follow, .. }, .. }) = o.controller.as_mut() {
                *follow = if name.is_empty() { Follow::Controller } else { Follow::Player { name } };
            }
        }
        Inbound::Module { name, on } => {
            if super::SWARM_MODULES.contains(&name.as_str()) {
                info!(target: "controller", "{name} {}", if on { "on" } else { "off" });
                link.controller_modules.lock().insert(name, on);
                link.mods.lock().dirty = true;
            } else {
                ctx.warn_once(&format!("the controller switched {name} - the Rust bots do not have it yet"));
            }
        }
        Inbound::Say(text) => {
            // Never plain chat (the AI pilot's hard rule holds for the swarm): commands only.
            if text.starts_with('/') {
                info!(target: "controller", "sending {text}");
                bot.chat(text);
            } else {
                ctx.warn_once("the controller asked a bot to say plain chat - swarm bots only send /commands");
            }
        }
        Inbound::Defend(on) => {
            keep_sync(ctx, "DEFEND", &on.to_string());
            let mut c = link.controller_combat.lock();
            if c.defend != on {
                info!(target: "controller", "Defend {}", if on { "on" } else { "off" });
            }
            c.defend = on;
            c.defend_from = "controller";
        }
        Inbound::AttackTarget(name) => {
            keep_sync(ctx, "ATTACK_TARGET", &name);
            info!(target: "controller", "attack target: {}", if name.is_empty() { "none" } else { name.as_str() });
            let mut c = link.controller_combat.lock();
            c.target = (!name.is_empty()).then_some(name);
            c.target_from = "controller";
        }
        Inbound::Sync { kind, raw } => {
            let fresh = ctx.controller.lock().syncs.get(&kind).is_none_or(|(r, _)| *r != raw);
            if fresh {
                info!(target: "controller", "{kind}: {}", controller::short(&raw));
            }
            if kind == "NUKER_SYNC" {
                let cfg = nuker::Cfg::from_sync(&raw);
                if cfg.is_none() {
                    ctx.warn_once("a NUKER_SYNC from the controller could not be read");
                }
                // The controller's strip for this bot: claims start over in it.
                let mut cur = link.nuker_sync.lock();
                if cur.as_ref().map(|c| c.area) != cfg.as_ref().map(|c| c.area) {
                    ctx.nuker_board.lock().release(&link.name);
                }
                *cur = cfg;
            }
            if kind == "PRINTER_SYNC" {
                let sync = printer::region_from_sync(&raw);
                if sync.is_none() {
                    ctx.warn_once("a PRINTER_SYNC from the controller could not be read");
                }
                let mut cur = link.printer_sync.lock();
                if cur.map(|c| c.0) != sync.map(|s| s.0) {
                    ctx.print_board.lock().release(&link.name);
                }
                *cur = sync;
            }
            keep_sync(ctx, &kind, &raw);
        }
        Inbound::Pong => {}
        Inbound::Unknown(l) => debug!(target: "controller", "unknown line: {l}"),
    }
}

fn keep_sync(ctx: &SwarmCtx, kind: &str, raw: &str) {
    ctx.controller.lock().syncs.insert(kind.to_string(), (raw.to_string(), Instant::now()));
}

/// A command from the panel for this bot alone, on its tick.
pub(super) fn bot_command(bot: &Client, ctx: &SwarmCtx, c: &Value) {
    match c["type"].as_str().unwrap_or("") {
        "command" => {
            let text = c["text"].as_str().unwrap_or("").trim().to_string();
            if text.starts_with('/') {
                info!(target: "swarm", "panel: sending {text}");
                bot.chat(text);
            } else {
                ctx.feed("say", "swarm bots only send /commands - never plain chat");
            }
        }
        "stop-moving" => movement::stop(bot),
        other => debug!(target: "swarm", "bot command {other} not handled here"),
    }
}

/// This bot's line in the panel.
fn row(bot: &Client, s: &Shared, link: &BotLink, order: &Stamped, state: &str) -> Value {
    let r = |v: f64| (v * 10.0).round() / 10.0;
    let mut o = json!({
        "order": order.order.describe(),
        "source": order.source,
        "state": state,
        "move": movement::status(bot),
        "deaths": s.deaths.load(Ordering::Relaxed),
        "queue": s.in_queue(),
    });
    if let Ok(p) = bot.position() {
        o["x"] = json!(r(p.x));
        o["y"] = json!(r(p.y));
        o["z"] = json!(r(p.z));
        o["dim"] = json!(dimension(bot));
        o["health"] = json!(r(bot.health().unwrap_or(0.0) as f64));
        o["food"] = json!(bot.hunger().map(|h| h.food).unwrap_or(0));
        o["ping"] = json!(bot.tab_list().ok().and_then(|t| t.get(&bot.uuid()).map(|i| i.latency)).unwrap_or(-1));
    }
    let m = link.mods.lock();
    o["modules"] = json!({"auto-eat": m.autoeat.is_some(), "kill-aura": m.killaura.is_some(), "fast-nuker": m.nuker.is_some()});
    o["food"] = json!(m.autoeat.as_ref().map(|a| a.food_points(bot)));
    drop(m);
    let n = link.nuker.lock();
    o["nuker"] = json!({"on": link.nuker_cfg().map(|c| c.source), "status": n.status, "broken": n.broken});
    drop(n);
    let p = link.printer.lock();
    o["printer"] = json!({
        "on": link.printer_cfg().is_some(),
        "status": p.status,
        "placed": p.placed,
        "fixed": p.fixed,
        "cleared": p.cleared,
        "missing": p.missing,
        "cell": p.cell,
    });
    drop(p);
    o["errand"] = json!(link.errand_what());
    o["combat"] = json!(*link.combat_status.lock());
    let e = link.eclink.lock();
    o["eclink"] = json!({
        "status": e.status,
        "borrows": e.borrows,
        "loans": e.loans.iter().filter(|l| l.how == eclink::Worn::Hand).map(|l| l.item).collect::<Vec<_>>(),
        "worn": e.loans.iter().filter(|l| l.how != eclink::Worn::Hand).map(|l| l.item).collect::<Vec<_>>(),
    });
    o
}

/// Low on food and a food source known: fetch some (not more often than every 2 minutes, so an
/// empty chest is not visited every few seconds).
pub(super) fn food_check(bot: &Client, ctx: &Arc<SwarmCtx>, link: &Arc<BotLink>) {
    if let Some((source, rule)) = food_due(bot, ctx, link) {
        link.start_errand("fetching food", errand::fetch_food(bot.clone(), ctx.clone(), source, rule, 1));
    }
}

/// Low on food, none in the linked ender chest, and a food chest known: where to fetch it from
/// (and what auto-eat eats) - at most every 2 minutes.
pub(super) fn food_due(bot: &Client, ctx: &Arc<SwarmCtx>, link: &Arc<BotLink>) -> Option<(errand::FoodSource, crate::modules::autoeat::FoodRule)> {
    let (points, rule) = {
        let m = link.mods.lock();
        let ae = m.autoeat.as_ref()?;
        (ae.food_points(bot), ae.rule())
    };
    if points >= ctx.cfg.read().food_low_points {
        return None;
    }
    // From the linked ender chest first: no trip, and the link stays.
    if eclink::take_food(bot, link, &rule) {
        return None;
    }
    if link.food_retry_at.lock().is_some_and(|t| Instant::now() < t) {
        return None;
    }
    *link.food_retry_at.lock() = Some(Instant::now() + Duration::from_secs(120));
    let source = errand::food_source(bot, ctx);
    if source.is_none() {
        ctx.warn_once(&format!("{} is low on food and no food chest is set or signed - it eats what it carries", link.name));
    }
    source.map(|s| (s, rule))
}

/// The nuker is hand-breaking hard blocks: a tools chest nearby, if there is one.
fn tools_errand(bot: &Client, ctx: &Arc<SwarmCtx>, link: &Arc<BotLink>) {
    if link.errand_running() || link.tools_retry_at.lock().is_some_and(|t| Instant::now() < t) {
        return;
    }
    // The linked ender chest has tools: the nuker borrows from there - no trip to break the link.
    if eclink::contents(bot, link).iter().any(|(_, s)| errand::is_digging_tool(s)) {
        return;
    }
    *link.tools_retry_at.lock() = Some(Instant::now() + Duration::from_secs(300));
    let (word, radius) = {
        let c = ctx.cfg.read();
        (c.tools_sign.clone(), c.tools_radius)
    };
    match errand::signed_chest(bot, ctx, &word, radius) {
        Some(chest) => link.start_errand("fetching tools", errand::fetch_tools(bot.clone(), chest)),
        None => ctx.warn_once(&format!("{} has no tool for what it is breaking, and no chest signed \"{word}\" is near - it carries on by hand", link.name)),
    }
}

/// The printer is out of a material: fetch it from the controller's supply chests (not more often
/// than every 30 s, so empty chests are not visited over and over).
fn restock_errand(bot: &Client, ctx: &Arc<SwarmCtx>, link: &Arc<BotLink>, items: Vec<(String, i32)>) {
    if link.errand_running() || link.restock_retry_at.lock().is_some_and(|t| Instant::now() < t) {
        return;
    }
    *link.restock_retry_at.lock() = Some(Instant::now() + Duration::from_secs(30));
    let what = items.iter().map(|(i, n)| format!("{n} {i}")).collect::<Vec<_>>().join(", ");
    link.start_errand(&format!("fetching {what} for the print"), printer::restock(bot.clone(), ctx.clone(), items));
}

/// Bags full while collecting: empty them into the storage chest.
fn store_errand(bot: &Client, ctx: &Arc<SwarmCtx>, link: &Arc<BotLink>) {
    if link.errand_running() || link.store_retry_at.lock().is_some_and(|t| Instant::now() < t) {
        return;
    }
    *link.store_retry_at.lock() = Some(Instant::now() + Duration::from_secs(60));
    let word = ctx.cfg.read().storage_sign.clone();
    let rule = link.mods.lock().autoeat.as_ref().map(|a| a.rule());
    match errand::signed_chest(bot, ctx, &word, 256.0) {
        Some(chest) => link.start_errand("storing what it collected", errand::store_haul(bot.clone(), chest, rule)),
        None => ctx.warn_once(&format!("{}: bags full while collecting, and no chest signed \"{word}\" is known", link.name)),
    }
}
