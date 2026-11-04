//! `melonbot conform`: the proof that print::place predicts the game. One offline bot on a local
//! vanilla server (creative, op) places every block state in a list - each in two set-ups, a cell
//! boxed in stone (it clicks a neighbour) and a cell with only a floor (it clicks the air) - and
//! reads back what the server made. TEST ONLY: needs a local offline-mode server whose console
//! reads `console` (e.g. `tail -f console.txt | java -jar server.jar nogui`).

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use azalea::{
    BlockPos, Client, Vec3,
    block::BlockState,
    inventory::ItemStack,
    prelude::*,
    protocol::packets::game::ServerboundSetCreativeModeSlot,
    registry::builtin::{BlockKind, ItemKind},
};
use parking_lot::Mutex;
use std::str::FromStr;

use crate::print::{act, items, place, schem};

#[derive(Clone)]
struct Job {
    name: String,
    cases: Vec<String>,
    console: PathBuf,
    out: PathBuf,
    done: Arc<AtomicBool>,
    report: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Component, Default)]
struct CState {
    job: Option<Job>,
    started: Arc<AtomicBool>,
}

pub async fn run(server: String, name: String, cases: PathBuf, console: PathBuf, out: PathBuf) -> Result<ExitCode> {
    let list: Vec<String> = fs::read_to_string(&cases)
        .with_context(|| format!("reading {}", cases.display()))?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect();
    println!("conform: {} states on {server} as {name}", list.len());
    let job = Job { name: name.clone(), cases: list, console, out, done: Arc::new(AtomicBool::new(false)), report: Arc::new(Mutex::new(Vec::new())) };
    let state = CState { job: Some(job.clone()), started: Arc::new(AtomicBool::new(false)) };
    let _ = ClientBuilder::new()
        .set_handler(handle)
        .set_state(state)
        .add_plugins(crate::movement::MovementPlugin)
        .start(Account::offline(&name), server.as_str())
        .await;
    Ok(if job.done.load(Ordering::SeqCst) { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

async fn handle(bot: Client, event: Event, state: CState) -> eyre::Result<()> {
    let Some(job) = state.job else { return Ok(()) };
    if let Event::Login = event
        && !state.started.swap(true, Ordering::SeqCst)
    {
        tokio::spawn(async move {
            let ok = match cases(&bot, &job).await {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("conform stopped: {e:#}");
                    false
                }
            };
            job.done.store(true, Ordering::SeqCst);
            bot.disconnect();
            tokio::time::sleep(Duration::from_millis(500)).await;
            std::process::exit(if ok { 0 } else { 1 });
        });
    }
    Ok(())
}

fn console(job: &Job, cmd: &str) {
    if let Ok(mut f) = OpenOptions::new().append(true).create(true).open(&job.console) {
        let _ = writeln!(f, "{cmd}");
    }
}

fn at(bot: &Client, p: BlockPos) -> Option<BlockState> {
    bot.world().ok()?.read().get_block_state(p)
}

async fn ticks(n: u64) {
    tokio::time::sleep(Duration::from_millis(50 * n)).await;
}

/// Wait until the cell and its surroundings read as set up (the server's block updates arrived).
async fn wait_for(bot: &Client, check: impl Fn(&Client) -> bool) -> bool {
    for _ in 0..60 {
        if check(bot) {
            return true;
        }
        ticks(1).await;
    }
    false
}

async fn cases(bot: &Client, job: &Job) -> Result<()> {
    ticks(40).await;
    console(job, &format!("op {}", job.name));
    console(job, &format!("gamemode creative {}", job.name));
    ticks(20).await;
    let stone = BlockState::from(BlockKind::Stone);
    let (mut pass, mut fail, mut skip) = (0, 0, 0);
    for (i, line) in job.cases.iter().enumerate() {
        let Some(required) = schem::parse_state(line) else {
            job.report.lock().push(format!("SKIP {line}: not a state this client knows"));
            skip += 1;
            continue;
        };
        let Some(item) = items::item_for(required) else {
            job.report.lock().push(format!("SKIP {line}: nothing places it"));
            skip += 1;
            continue;
        };
        let Ok(item_kind) = ItemKind::from_str(&item) else {
            job.report.lock().push(format!("SKIP {line}: no item {item}"));
            skip += 1;
            continue;
        };
        for boxed in [true, false] {
            // One column per case, 4 apart, at y 100 over the flat world.
            let x = (i as i32) * 4 - 400;
            let cell = BlockPos::new(x, 100, 0);
            let stand = Vec3::new(x as f64 + 0.5, 101.0, 2.5);
            let _ = crate::movement::fly_to(bot, stand, "conform").await;
            console(job, &format!("fill {} 99 -1 {} 102 1 air", x - 1, x + 1));
            if boxed {
                console(job, &format!("fill {} 99 -1 {} 101 1 stone", x - 1, x + 1));
                console(job, &format!("setblock {x} 100 0 air"));
            } else {
                console(job, &format!("setblock {x} 99 0 stone"));
            }
            // The floor set-up follows the boxed one in the same column: wait until the box is gone
            // too, or the bot chooses against the box it still sees (doors and beds read "no room").
            let ready = wait_for(bot, |b| {
                let side = at(b, cell + BlockPos::new(0, 0, -1));
                let above = at(b, cell + BlockPos::new(0, 1, 0));
                at(b, cell).is_some_and(|s| s.is_air())
                    && at(b, cell + BlockPos::new(0, -1, 0)) == Some(stone)
                    && if boxed { side == Some(stone) && above == Some(stone) } else { side.is_some_and(|s| s.is_air()) && above.is_some_and(|s| s.is_air()) }
            })
            .await;
            if !ready {
                job.report.lock().push(format!("SKIP {line} ({}): the set-up never showed", if boxed { "boxed" } else { "floor" }));
                skip += 1;
                continue;
            }
            // One: a stack over the item's own limit (16 signs) is refused outright.
            bot.write_packet(ServerboundSetCreativeModeSlot { slot_num: 45, item_stack: ItemStack::new(item_kind, 1) });
            ticks(2).await;
            let eye = bot.position().unwrap_or(stand) + Vec3::new(0.0, 1.62, 0.0);
            let setup = if boxed { "boxed" } else { "floor" };
            let choice = {
                let w = bot.world()?;
                let w = w.read();
                place::choose(&w, eye, cell, required, 4.4)
            };
            let choice = match choice {
                Ok(c) => c,
                Err(why) => {
                    job.report.lock().push(format!("NONE {line} ({setup}): {why}"));
                    skip += 1;
                    continue;
                }
            };
            let predicted = {
                let w = bot.world()?;
                let w = w.read();
                place::simulate(&w, required, &choice.click).map(|(_, s)| s)
            };
            // The look a tick ahead: the server reads the yaw of "nearest looking directions" from
            // the head, which only follows the body on the player's tick (LivingEntity.getViewYRot).
            if choice.needs_look {
                crate::movement::look_now(bot, choice.click.yaw, choice.click.pitch);
                ticks(3).await;
            }
            act::click(bot, &choice);
            ticks(8).await;
            // A double slab (or more candles) takes another click on what went down.
            for _ in 0..3 {
                let now = at(bot, cell).unwrap_or(BlockState::AIR);
                let short = BlockKind::from(now) == BlockKind::from(required)
                    && (required.to_trait().get_property("type") == Some("double") && now.to_trait().get_property("type") != Some("double"));
                if !short {
                    break;
                }
                let again = {
                    let w = bot.world()?;
                    let w = w.read();
                    place::choose(&w, eye, cell, required, 4.4)
                };
                let Ok(again) = again else { break };
                act::click(bot, &again);
                ticks(8).await;
            }
            let got = at(bot, cell).unwrap_or(BlockState::AIR);
            let partner = {
                let w = bot.world()?;
                let w = w.read();
                place::chest_partner_present(&w, cell, required)
            };
            let same = BlockKind::from(got) == BlockKind::from(required) || items::placed_form(BlockKind::from(required)) == Some(BlockKind::from(got));
            let c = &choice.click;
            let how = format!(
                "click {} {} {} face {:?} hit {:.2} {:.2} {:.2} yaw {} pitch {} sneak {} look {}",
                c.clicked.x - x, c.clicked.y - 100, c.clicked.z, c.face, c.hit.x - x as f64, c.hit.y - 100.0, c.hit.z, c.yaw, c.pitch, c.sneak, choice.needs_look
            );
            if same && !place::orientation_differs(got, required, partner) {
                pass += 1;
                job.report.lock().push(format!("PASS {line} ({setup})"));
            } else {
                fail += 1;
                job.report.lock().push(format!("FAIL {line} ({setup}): server made {got:?}, predicted {predicted:?}; {how}"));
            }
        }
        if i % 20 == 19 {
            println!("conform: {}/{} states - pass {pass}, fail {fail}, none/skip {skip}", i + 1, job.cases.len());
            write_report(job, pass, fail, skip);
        }
    }
    write_report(job, pass, fail, skip);
    println!("conform: done - pass {pass}, fail {fail}, none/skip {skip}; report in {}", job.out.display());
    Ok(())
}

/// The swarm printer's end-to-end test build (9 x 4 x 9): every kind of work the printer does -
/// plain blocks, looks (stairs, furnace, observer, sign), clicked faces (logs, slabs, trapdoor),
/// look-a-tick-ahead (wall torch, lever), a chest pair, and state fixes (repeater delay, note,
/// an open fence gate) - over a stone floor, with a brick column to print in layers.
pub fn test_build() -> Vec<((i32, i32, i32), BlockState)> {
    let s = |t: &str| schem::parse_state(t).unwrap_or_else(|| panic!("{t}"));
    let mut b = Vec::new();
    for x in 0..9 {
        for z in 0..9 {
            b.push(((x, 0, z), s("stone")));
        }
    }
    for (p, t) in [
        ((1, 1, 1), "stone"),
        ((1, 2, 1), "stone"),
        ((2, 2, 1), "wall_torch[facing=east]"),
        ((2, 1, 1), "lever[face=wall,facing=east,powered=false]"),
        ((4, 1, 2), "oak_stairs[facing=north,half=bottom]"),
        ((6, 1, 4), "oak_stairs[facing=east,half=bottom]"),
        ((4, 1, 6), "oak_stairs[facing=south,half=top]"),
        ((2, 1, 4), "stone_brick_stairs[facing=west,half=top]"),
        ((3, 1, 3), "oak_log[axis=x]"),
        ((5, 1, 3), "oak_log[axis=z]"),
        ((5, 1, 5), "oak_log[axis=y]"),
        ((3, 1, 5), "oak_slab[type=top]"),
        ((7, 1, 7), "oak_slab[type=double]"),
        ((6, 1, 6), "repeater[delay=3,facing=west]"),
        ((7, 1, 1), "note_block[note=5]"),
        ((1, 1, 7), "oak_fence_gate[facing=south,open=true]"),
        ((7, 1, 3), "oak_trapdoor[facing=east,half=top]"),
        ((2, 1, 7), "observer[facing=up]"),
        ((5, 1, 8), "chest[facing=south,type=left]"),
        ((4, 1, 8), "chest[facing=south,type=right]"),
        ((6, 1, 2), "furnace[facing=west]"),
        ((3, 1, 7), "oak_sign[rotation=6]"),
        ((8, 1, 8), "stone_button[face=floor,facing=north]"),
        ((8, 1, 0), "bricks"),
        ((8, 2, 0), "bricks"),
        ((8, 3, 0), "bricks"),
        ((8, 3, 1), "brick_slab[type=bottom]"),
    ] {
        b.push((p, s(t)));
    }
    b
}

/// `melonbot verify`: an observer flies over a placed schematic and lists every block that is not
/// what it should be - kind, orientation, the clicked states, counts.
pub async fn verify(server: String, name: String, file: PathBuf, origin: (i32, i32, i32), out: PathBuf) -> Result<ExitCode> {
    let s = schem::load(&file)?;
    let p = crate::print::litematica::Placement {
        schematic: file.clone(),
        name: "verify".into(),
        origin,
        rotation: Default::default(),
        mirror: Default::default(),
        enabled: true,
        subs: vec![],
        dimension: String::new(),
        file: file.clone(),
        saved: None,
    };
    let target = Arc::new(crate::print::litematica::target(&s, &p));
    let done = Arc::new(AtomicBool::new(false));
    let report = Arc::new(Mutex::new(Vec::<String>::new()));
    let job = Job { name: name.clone(), cases: vec![], console: PathBuf::new(), out: out.clone(), done: done.clone(), report: report.clone() };
    let state = VState { job: Some((job, target)), started: Arc::new(AtomicBool::new(false)) };
    let _ = ClientBuilder::new().set_handler(vhandle).set_state(state).add_plugins(crate::movement::MovementPlugin).start(Account::offline(&name), server.as_str()).await;
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Component, Default)]
struct VState {
    job: Option<(Job, Arc<crate::print::litematica::Target>)>,
    started: Arc<AtomicBool>,
}

async fn vhandle(bot: Client, event: Event, state: VState) -> eyre::Result<()> {
    let Some((job, target)) = state.job else { return Ok(()) };
    if let Event::Login = event
        && !state.started.swap(true, Ordering::SeqCst)
    {
        tokio::spawn(async move {
            let mid = Vec3::new((target.min.x + target.max.x) as f64 / 2.0 + 0.5, target.max.y as f64 + 4.0, (target.min.z + target.max.z) as f64 / 2.0 + 0.5);
            let _ = crate::movement::fly_to(&bot, mid, "verify").await;
            ticks(60).await;
            let mut bad = Vec::new();
            let mut unread = 0;
            {
                let w = bot.world().expect("world");
                let w = w.read();
                let mut keys: Vec<&BlockPos> = target.blocks.keys().collect();
                keys.sort_by_key(|p| (p.y, p.x, p.z));
                for at in keys {
                    let req = target.blocks[at];
                    let Some(got) = w.get_block_state(*at) else {
                        unread += 1;
                        continue;
                    };
                    let partner = place::chest_partner_present(&w, *at, req);
                    let same = BlockKind::from(got) == BlockKind::from(req) || items::placed_form(BlockKind::from(req)) == Some(BlockKind::from(got));
                    let states = ["delay", "mode", "open", "inverted", "note", "candles", "pickles", "eggs", "layers"]
                        .iter()
                        .filter(|k| req.to_trait().get_property(k) != got.to_trait().get_property(k))
                        .filter(|k| !(**k == "open" && BlockKind::from(req).to_str().contains("iron_")))
                        .copied()
                        .collect::<Vec<_>>();
                    let lever = BlockKind::from(req) == BlockKind::Lever && req.to_trait().get_property("powered") != got.to_trait().get_property("powered");
                    if !same || place::orientation_differs(got, req, partner) || !states.is_empty() || lever {
                        bad.push(format!("{} {} {}: wants {}, has {}", at.x, at.y, at.z, schem::state_string(req), schem::state_string(got)));
                    }
                }
            }
            let text = format!("{} blocks, {} wrong, {} unread\n{}\n", target.blocks.len(), bad.len(), unread, bad.join("\n"));
            let _ = fs::write(&job.out, &text);
            println!("verify: {} blocks, {} wrong, {} unread (report in {})", target.blocks.len(), bad.len(), unread, job.out.display());
            bot.disconnect();
            tokio::time::sleep(Duration::from_millis(500)).await;
            std::process::exit(0);
        });
    }
    Ok(())
}

fn write_report(job: &Job, pass: usize, fail: usize, skip: usize) {
    let r = job.report.lock();
    let mut text = format!("pass {pass}, fail {fail}, none/skip {skip}\n");
    // Failures first.
    for l in r.iter().filter(|l| l.starts_with("FAIL")) {
        text.push_str(l);
        text.push('\n');
    }
    for l in r.iter().filter(|l| !l.starts_with("FAIL")) {
        text.push_str(l);
        text.push('\n');
    }
    let _ = fs::write(&job.out, text);
}
