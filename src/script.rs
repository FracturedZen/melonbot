//! A test script for live checks, run once after the first join: `--script "wait 5; goto base;
//! up 10; goto farm; status"`. Steps: `wait <s>`, `goto <x> <y> <z>|base|farm`, `up <n>`,
//! `down <n>`, `status`, `quit`. Logged under the "script" target; never chats.

use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use azalea::{Client, Vec3};
use tracing::{info, warn};

use crate::{bot::Shared, inv, movement};

pub async fn run(bot: Client, ctx: Arc<Shared>, script: String) {
    let cfg = &ctx.cfg;
    for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let words: Vec<&str> = step.split_whitespace().collect();
        info!(target: "script", "> {step}");
        let pos = bot.position().unwrap_or_default();
        let result = match words.as_slice() {
            ["wait", s] => {
                tokio::time::sleep(Duration::from_secs_f64(s.parse().unwrap_or(1.0))).await;
                Ok(())
            }
            ["status"] => {
                info!(target: "script", "at {:.1} {:.1} {:.1}: {}", pos.x, pos.y, pos.z, movement::status(&bot));
                Ok(())
            }
            ["up", n] | ["down", n] => {
                let n: f64 = n.parse().unwrap_or(0.0);
                let dy = if words[0] == "up" { n } else { -n };
                movement::fly_to(&bot, Vec3::new(pos.x, pos.y + dy, pos.z), step).await
            }
            ["goto", "base"] => match cfg.base {
                Some(b) => movement::fly_to(&bot, Vec3::new(b.x as f64 + 0.5, b.y as f64, b.z as f64 + 0.5), "base").await,
                None => Err("no base in config".into()),
            },
            ["goto", "farm"] => match cfg.farm {
                Some(f) => {
                    let c = Vec3::new((f.min.x + f.max.x) as f64 / 2.0 + 0.5, f.max.y as f64 + 3.0, (f.min.z + f.max.z) as f64 / 2.0 + 0.5);
                    movement::fly_to(&bot, c, "farm").await
                }
                None => Err("no farm in config".into()),
            },
            ["goto", x, y, z] => match (x.parse(), y.parse(), z.parse()) {
                (Ok(x), Ok(y), Ok(z)) => movement::fly_to(&bot, Vec3::new(x, y, z), step).await,
                _ => Err("goto needs numbers".into()),
            },
            ["bags"] => {
                let mut parts: Vec<String> = Vec::new();
                for (slot, st) in inv::bags(&bot) {
                    if !st.is_empty() {
                        let wear = inv::uses_left(&st).map(|u| format!(" ({u} uses)")).unwrap_or_default();
                        parts.push(format!("{slot}:{}x{}{wear}", inv::stack_id(&st), st.count()));
                    }
                }
                info!(target: "script", "bags ({} empty): {}", inv::empty_slots(&bot), parts.join(", "));
                Ok(())
            }
            ["eat"] => match &ctx.autoeat {
                None => Err("auto-eat is off".into()),
                Some(ae) => {
                    let before = bot.hunger().map(|h| h.food).unwrap_or(0);
                    let slot = bot.selected_hotbar_slot().unwrap_or(0);
                    ae.force();
                    let until = Instant::now() + Duration::from_secs(6);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    while Instant::now() < until && ae.eating() {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    let after = bot.hunger().map(|h| h.food).unwrap_or(0);
                    let slot_after = bot.selected_hotbar_slot().unwrap_or(0);
                    info!(target: "script", "eat: hunger {before} -> {after}, hotbar slot {slot} -> {slot_after}");
                    if after > before { Ok(()) } else { Err("hunger did not go up".into()) }
                }
            },
            ["food"] => {
                let pts = ctx.autoeat.as_ref().map_or(-1, |a| a.food_points(&bot));
                let h = bot.hunger().map(|h| h.food as i32).unwrap_or(-1);
                info!(target: "script", "hunger {h}/20, health {:.0}, food carried {pts} points", bot.health().unwrap_or(-1.0));
                Ok(())
            }
            ["farm", secs] => match &ctx.harvest {
                None => Err("no farm plot in config".into()),
                Some(h) => {
                    let before = inv::count_id(&bot, "melon");
                    h.set_on(true);
                    let until = Instant::now() + Duration::from_secs_f64(secs.parse().unwrap_or(30.0));
                    while Instant::now() < until && h.on() {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    let why = h.status();
                    h.set_on(false);
                    movement::stop(&bot);
                    let after = inv::count_id(&bot, "melon");
                    info!(target: "script", "farmed: {} broken, melons {before} -> {after} - {why}", h.broken());
                    Ok(())
                }
            },
            ["signs"] => {
                let base = cfg.base.map(|b| azalea::BlockPos::new(b.x, b.y, b.z));
                match base {
                    None => Err("no base in config".into()),
                    Some(b) => {
                        for w in ["tools", "repair", "food"] {
                            info!(target: "script", "chest signed {w}: {:?}", crate::chests::signed(&bot, b, w));
                        }
                        Ok(())
                    }
                }
            }
            ["store-slices", keep] => {
                let keep: usize = keep.parse().unwrap_or(1);
                let base = cfg.base.map(|b| azalea::BlockPos::new(b.x, b.y, b.z));
                match base.and_then(|b| crate::chests::signed(&bot, b, "food")) {
                    None => Err("no chest signed food found near the base".into()),
                    Some(chest) => {
                        let stacks = inv::bags(&bot).iter().filter(|(_, s)| inv::stack_id(s) == "melon_slice").count();
                        let n = stacks.saturating_sub(keep);
                        crate::chests::store(&bot, chest, |s| inv::stack_id(s) == "melon_slice", n).await.map(|moved| {
                            info!(target: "script", "slices: {stacks} stacks, stored {moved}, kept {}", stacks - moved);
                        })
                    }
                }
            }
            ["quit"] => {
                info!(target: "script", "quitting");
                ctx.quit.store(true, Ordering::SeqCst);
                bot.disconnect();
                return;
            }
            _ => Err(format!("unknown step {step:?}")),
        };
        let now = bot.position().unwrap_or_default();
        match result {
            Ok(()) => info!(target: "script", "  ok - at {:.1} {:.1} {:.1}", now.x, now.y, now.z),
            Err(e) => warn!(target: "script", "  FAILED: {e} - at {:.1} {:.1} {:.1}", now.x, now.y, now.z),
        }
    }
    info!(target: "script", "script done - holding position");
}
