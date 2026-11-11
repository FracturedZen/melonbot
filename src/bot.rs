//! The event handler. Phase 1: chat into the log, back out of the queue, the player guard, deaths -
//! the parts of the Java AiPilot the melon routine leans on, without the model.

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use azalea::prelude::*;
use parking_lot::Mutex;
use regex::{Regex, RegexBuilder};
use tracing::{info, warn};

use crate::{
    config::{self, Config},
    discord::Discord,
    modules::{ModuleConfig, autoeat::AutoEat, harvest::Harvester, killaura::KillAura},
    panel::Panel,
};

/// azalea wants Default + Clone; the real state is filled in by [`State::new`] before the start.
#[derive(Clone, Component, Default)]
pub struct State {
    shared: Option<Arc<Shared>>,
}

/// One bot: its settings, its modules, and the handler's own bookkeeping. Shared with the test
/// script and the routine.
pub struct Shared {
    pub cfg: Config,
    /// The standalone's module settings (modules.json).
    pub modules: ModuleConfig,
    /// bots/<name>/ - progress files live here.
    pub dir: std::path::PathBuf,
    /// The melon routine runs after every join (ai-pilot routine=Melon + auto-continue).
    routine_on: bool,
    routine_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub routine: Mutex<crate::routine::RoutineState>,
    pub autoeat: Option<AutoEat>,
    pub harvest: Option<Harvester>,
    pub killaura: Option<KillAura>,
    /// A test script to run once after the first join (--script).
    script: Mutex<Option<String>>,
    /// Leave for good at the next disconnect (the script quit, or the player guard). exit() only
    /// once the disconnect has gone through: azalea reads the account on Disconnect and panics
    /// if exit() already tore the client down.
    pub quit: AtomicBool,
    pub discord: Discord,
    queue_trigger: Option<Regex>,
    inner: Mutex<Inner>,
    /// The control panel (the Java panel's page and API), when it could bind its port.
    pub panel: Option<Arc<Panel>>,
    /// Stopped from the panel: not started again by a join until "Continue".
    routine_stopped: AtomicBool,
    joined_at: Mutex<Option<Instant>>,
    pub autoeat_on: AtomicBool,
    pub killaura_on: AtomicBool,
    pub deaths: AtomicU32,
    /// Health lost in the last half minute, newest last (the perimeter's auto-log reads it).
    pub hurts: Mutex<Vec<Hurt>>,
    /// A swarm bot: its link to the swarm (orders, controller, modules). None for a bot of its own.
    pub swarm: Option<Arc<crate::swarm::BotLink>>,
    /// MelonBoard reporting (the melon farmer, on config.json's board_server only).
    pub board: Option<crate::board::Board>,
    /// The seller's sell list and the buyer's buy settings, live: from config.json, changed by the
    /// panel's Sell and Shop tabs.
    pub shop: Mutex<crate::seller::Shop>,
}

/// Health lost: when, how much, and from what ("arrow", "fall", ...).
#[derive(Clone, Debug)]
pub struct Hurt {
    pub at: Instant,
    pub lost: f32,
    pub from: Vec<String>,
}

/// Hurts kept this long.
const HURTS_KEPT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Inner {
    /// Dropped on the queue/lobby server: the queue command goes out 3 s later, then every 60 s.
    in_queue: bool,
    queue_sent: u32,
    next_queue_at: Option<Instant>,
    /// Our entity id on the server: it changes when the server moves us to another world.
    entity_id: Option<i32>,
    /// Logged out because a player came into view: stays out (no reconnect) until relaunched.
    logged_out: bool,
    ticks: u64,
    own: HashSet<String>,
    own_read_at: Option<Instant>,
    last_status: Option<Instant>,
    /// Our own name, read at join: azalea panics if asked for the account after a disconnect.
    me: String,
    last_health: Option<f32>,
    last_summary: Option<Instant>,
    /// The game tick of the last hourly summary.
    summary_ticks: u64,
}

/// Queue commands sent before giving up and leaving it to auto-reconnect (as on the Java side).
const MAX_QUEUE_SENDS: u32 = 30;
/// In the world at least 54 of the hour's 60 minutes (20 ticks a second), or no hourly summary.
const SUMMARY_MIN_TICKS: u64 = 54 * 60 * 20;

/// The queue detector - none without a queue command to send (or with no pattern: an empty one
/// would match every line of chat).
fn queue_regex(cfg: &Config) -> Option<Regex> {
    if cfg.queue_command.trim().is_empty() || cfg.queue_trigger.trim().is_empty() {
        return None;
    }
    RegexBuilder::new(&cfg.queue_trigger)
        .case_insensitive(true)
        .build()
        .inspect_err(|e| warn!("queue_trigger is not a valid regex ({e}) - queue detection off"))
        .ok()
}

impl State {
    pub fn new(cfg: Config, dir: std::path::PathBuf, modules: ModuleConfig, discord: Discord, script: Option<String>, no_routine: bool) -> Self {
        let queue_trigger = queue_regex(&cfg);
        let in_pen = cfg.in_pen();
        // A hand-made seller or buyer config has no modules.json; default auto-eat on for it so it
        // eats from the `food`-signed chest. Otherwise it follows modules.json as before.
        let autoeat = AutoEat::from_config(&modules).or_else(|| in_pen.then(AutoEat::default_on));
        if autoeat.is_none() {
            warn!("auto-eat is off in modules.json - nothing will eat");
        }
        let harvest = cfg.farm.map(|f| Harvester::new(f, &modules));
        let modules_kept = modules.clone();
        // A bot of its own hits with what it holds: the farmer's hand is its axe's.
        let killaura = KillAura::from_config(&modules).map(KillAura::hand_only);
        // As the Java client: routine=Melon starts by itself after a join when auto-continue is on.
        // The seller and the buyer (cfg.job) start the same way. Never alongside a test script, and
        // never with --no-routine.
        let routine_on = !no_routine
            && script.is_none()
            && (in_pen
                || (modules.get::<String>("ai-pilot", "routine", String::new()) == "Melon"
                    && modules.get("ai-pilot", "auto-continue", false)));
        let job = if cfg.is_seller() {
            "seller"
        } else if cfg.is_buyer() {
            "buyer"
        } else if cfg.is_mapart() {
            "mapart builder"
        } else {
            "melon routine"
        };
        info!("{job} {}", if routine_on { "ON - starts after each join" } else { "off" });
        // The control panel on the instance's port (6971 for the melon farmer), as the Java client served it.
        let panel = if modules.enabled("control-panel") || modules.get::<u16>("control-panel", "port", 0) > 0 {
            Panel::start(modules.get("control-panel", "port", 6970))
        } else {
            None
        };
        // The leaderboard, as the Java client reported it: only on its server, never a test server.
        let board = (crate::board::tracks(&cfg.server, &cfg.board_server) && !in_pen).then(|| crate::board::Board::load(&dir));
        let shop = Mutex::new(crate::seller::Shop::from_config(&cfg));
        Self {
            shared: Some(Arc::new(Shared {
                cfg,
                modules: modules_kept,
                dir,
                routine_on,
                routine_task: Mutex::new(None),
                routine: Mutex::new(Default::default()),
                autoeat,
                harvest,
                killaura,
                script: Mutex::new(script),
                quit: AtomicBool::new(false),
                discord,
                queue_trigger,
                inner: Mutex::new(Inner::default()),
                panel,
                routine_stopped: AtomicBool::new(false),
                joined_at: Mutex::new(None),
                autoeat_on: AtomicBool::new(true),
                killaura_on: AtomicBool::new(true),
                deaths: AtomicU32::new(0),
                hurts: Mutex::new(Vec::new()),
                swarm: None,
                board,
                shop,
            })),
        }
    }

    /// A swarm bot: the queue, the status line and the shared handler as a bot of its own has them, but
    /// no routine, no panel of its own (the swarm panel shows it), no Discord, no player guard
    /// (cfg.player_guard is off) - and its modules run from the swarm (swarm::tick).
    pub fn new_swarm(cfg: Config, dir: std::path::PathBuf, modules: ModuleConfig, link: Arc<crate::swarm::BotLink>) -> Self {
        let queue_trigger = queue_regex(&cfg);
        let shop = Mutex::new(crate::seller::Shop::from_config(&cfg));
        Self {
            shared: Some(Arc::new(Shared {
                cfg,
                modules,
                dir,
                routine_on: false,
                routine_task: Mutex::new(None),
                routine: Mutex::new(Default::default()),
                autoeat: None,
                harvest: None,
                killaura: None,
                script: Mutex::new(None),
                quit: AtomicBool::new(false),
                discord: Discord::start("", ""),
                queue_trigger,
                inner: Mutex::new(Inner::default()),
                panel: None,
                routine_stopped: AtomicBool::new(false),
                joined_at: Mutex::new(None),
                autoeat_on: AtomicBool::new(true),
                killaura_on: AtomicBool::new(true),
                deaths: AtomicU32::new(0),
                hurts: Mutex::new(Vec::new()),
                swarm: Some(link),
                board: None,
                shop,
            })),
        }
    }

    /// The swarm bot this is (for its log span); None for a bot of its own.
    pub fn swarm_name(&self) -> Option<String> {
        self.shared.as_ref().and_then(|s| s.swarm.as_ref()).map(|l| l.name.clone())
    }

    pub fn panel(&self) -> Option<Arc<Panel>> {
        self.shared.as_ref().and_then(|s| s.panel.clone())
    }

    pub fn logged_out_for_player(&self) -> bool {
        self.shared.as_ref().is_some_and(|s| s.inner.lock().logged_out)
    }

    pub fn board(&self) -> Option<&crate::board::Board> {
        self.shared.as_ref().and_then(|s| s.board.as_ref())
    }
}

impl Shared {
    pub fn in_queue(&self) -> bool {
        self.inner.lock().in_queue
    }

    pub fn routine_phase(&self) -> String {
        self.routine.lock().phase.clone()
    }

    pub fn routine_running(&self) -> bool {
        self.routine_task.lock().as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Start the melon routine unless it runs already (after a join: 10 s later, as auto-continue).
    pub fn start_routine(self: &Arc<Self>, bot: &Client, after_join: bool) {
        self.routine_stopped.store(false, Ordering::SeqCst);
        let mut task = self.routine_task.lock();
        if task.as_ref().is_some_and(|t| !t.is_finished()) {
            return;
        }
        let (bot, ctx) = (bot.clone(), self.clone());
        *task = Some(tokio::spawn(async move {
            if after_join {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            if ctx.cfg.is_seller() {
                crate::seller::run(bot, ctx).await;
            } else if ctx.cfg.is_buyer() {
                crate::buyer::run(bot, ctx).await;
            } else if ctx.cfg.is_mapart() {
                crate::mapart::run(bot, ctx).await;
            } else {
                crate::routine::run(bot, ctx).await;
            }
        }));
    }

    /// Stop it where it is: the farmer off, the trip dropped, holding position.
    pub fn stop_routine(&self, bot: &Client) {
        self.routine_stopped.store(true, Ordering::SeqCst);
        if let Some(t) = self.routine_task.lock().take() {
            t.abort();
        }
        if let Some(h) = &self.harvest {
            h.set_on(false);
        }
        crate::movement::stop(bot);
        self.routine.lock().phase.clear();
        info!(target: "routine", "melon routine stopped from the control panel");
    }

    pub fn joined_secs(&self) -> u64 {
        self.joined_at.lock().map_or(0, |t| t.elapsed().as_secs())
    }

    /// Log out and stay out until relaunched (exit code 3: the start script does not restart).
    pub fn leave(&self, bot: &Client, why: &str) {
        info!("{why} - logging out until relaunched");
        self.inner.lock().logged_out = true;
        self.quit.store(true, Ordering::SeqCst);
        bot.disconnect();
    }

    /// A line in the panel's feed.
    pub fn feed(&self, kind: &str, text: &str) {
        if let Some(p) = &self.panel {
            p.feed(kind, text);
        }
    }
}

pub async fn handle(bot: Client, event: Event, state: State) -> eyre::Result<()> {
    let Some(s) = state.shared else { return Ok(()) };
    match event {
        Event::Init => info!("connecting to {}", s.cfg.server),
        Event::Login => {
            let mut inner = s.inner.lock();
            inner.me = bot.username().to_lowercase();
            // Health reads 1 until the server's first health packet: not a hit.
            inner.last_health = None;
            // First status line 5 s in: health reads 1 until the server's first health packet.
            inner.last_status = Some(Instant::now() - Duration::from_secs(55));
            inner.entity_id = None;
            // A new world after a queue command was sent: back on the server.
            if inner.in_queue && inner.queue_sent > 0 {
                info!("joined a new world after {} queue command(s) - back on the server", inner.queue_sent);
                inner.in_queue = false;
                inner.queue_sent = 0;
                inner.next_queue_at = None;
            } else {
                info!("joined the world");
            }
            drop(inner);
            *s.joined_at.lock() = Some(Instant::now());
            if let Some(l) = &s.swarm {
                *l.joined_at.lock() = Some(Instant::now());
            }
            s.feed("event", "joined the world");
            if let Some(b) = &s.board {
                b.on_join();
            }
            if s.routine_on && !s.routine_stopped.load(Ordering::SeqCst) {
                s.start_routine(&bot, true);
            }
            if let Some(script) = s.script.lock().take() {
                let (bot, ctx) = (bot.clone(), s.clone());
                tokio::spawn(async move {
                    // Let the world and the first teleport arrive before moving.
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    crate::script::run(bot, ctx, script).await;
                });
            }
        }
        Event::Chat(m) => {
            let line = m.message().to_string();
            info!(target: "chat", "{line}");
            if let Some(re) = &s.queue_trigger
                && re.is_match(&line)
            {
                let mut inner = s.inner.lock();
                if !inner.in_queue {
                    info!("in the queue - sending {:?} in 3 s, then every 60 s", s.cfg.queue_command);
                    inner.in_queue = true;
                    inner.queue_sent = 0;
                    inner.next_queue_at = Some(Instant::now() + Duration::from_secs(3));
                }
            }
        }
        Event::Tick => tick(&bot, &s),
        Event::Death(_) => {
            if let (Some(l), Ok(p)) = (&s.swarm, bot.position()) {
                *l.died_at.lock() = Some((p, Instant::now()));
            }
            let pos = bot.position().map(|p| format!("{:.0} {:.0} {:.0}", p.x, p.y, p.z)).unwrap_or_default();
            info!("DIED at {pos}");
            s.deaths.fetch_add(1, Ordering::Relaxed);
            s.feed("event", &format!("died at {pos}"));
            s.discord.send("died - respawning");
        }
        Event::Disconnect(reason) => {
            let why = reason.map(|r| r.to_string()).unwrap_or_default();
            // The panel (and the Discord watcher) see it offline at once.
            if let Some(p) = &s.panel {
                let mut st = p.state.lock();
                st["online"] = false.into();
                st["screen"] = why.clone().into();
            }
            *s.joined_at.lock() = None;
            s.feed("event", &format!("disconnected: {why}"));
            if let Some(b) = &s.board {
                b.on_leave();
            }
            if let Some(l) = &s.swarm {
                // Never exit(): that ends the whole swarm. A stopped bot has no reconnect delay.
                l.online.store(false, Ordering::Relaxed);
                *l.joined_at.lock() = None;
                info!("disconnected: {why}{}", if l.running.load(Ordering::SeqCst) { " - reconnecting in 30 s" } else { " - stopped" });
            } else if s.quit.load(Ordering::SeqCst) {
                info!("disconnected: {why} - leaving for good");
                bot.exit();
            } else if s.inner.lock().logged_out {
                info!("disconnected (logged out for a player): {why}");
            } else {
                info!("disconnected: {why} - reconnecting in 30 s");
            }
        }
        _ => {}
    }
    Ok(())
}

fn tick(bot: &Client, s: &Arc<Shared>) {
    let ticks = {
        let mut inner = s.inner.lock();
        inner.ticks += 1;
        inner.ticks
    };
    crate::debuglog::TICK.store(ticks, std::sync::atomic::Ordering::Relaxed);

    // The control panel: its orders, then a fresh state every 5 ticks (as the Java panel).
    if let Some(p) = &s.panel {
        let cmds: Vec<serde_json::Value> = p.commands.lock().drain(..).collect();
        for c in cmds {
            crate::control::handle(bot, s, &c);
        }
        if ticks.is_multiple_of(5) {
            let snap = crate::control::snapshot(bot, s);
            *p.state.lock() = snap;
        }
    }

    // Any health lost is logged with where and what the mover was doing - evidence, not theory.
    if let Ok(h) = bot.health() {
        let was = s.inner.lock().last_health.replace(h);
        if let Some(was) = was
            && h < was - 0.01
        {
            let pos = bot.position().map(|p| format!("{:.1} {:.1} {:.1}", p.x, p.y, p.z)).unwrap_or_default();
            let what = crate::damage::take_hits(bot);
            {
                let mut hurts = s.hurts.lock();
                hurts.retain(|x| x.at.elapsed() < HURTS_KEPT);
                hurts.push(Hurt { at: Instant::now(), lost: was - h, from: what.clone() });
            }
            warn!(
                "DAMAGE -{:.1} ({was:.1} -> {h:.1}) from {} at {pos}, {}",
                was - h,
                if what.is_empty() { "?".to_string() } else { what.join(", ") },
                crate::movement::status(bot)
            );
        }
    }

    // Modules: auto-eat first, and the farmer waits while a meal has the hand.
    let eating = match &s.autoeat {
        Some(ae) if s.autoeat_on.load(Ordering::Relaxed) => {
            ae.tick(bot, ticks);
            ae.eating()
        }
        _ => false,
    };
    if let Some(h) = &s.harvest {
        h.tick(bot, ticks, eating);
    }
    if let Some(k) = &s.killaura
        && s.killaura_on.load(Ordering::Relaxed)
    {
        k.tick(bot, eating, None);
    }

    // A swarm bot: its orders, controller link and modules.
    if let Some(l) = s.swarm.clone() {
        crate::swarm::tick(bot, s, &l, ticks);
    }

    if let Some(b) = &s.board {
        b.tick(bot);
    }

    // The hourly Discord summary while the routine runs - only for an hour it was online: the game
    // ticks since the last one must cover most of it (17 Oct: the PC slept, and on waking the bot
    // posted "0 melons mined, 0 placed" for hours it had not been in the world at all).
    if s.routine_on && !s.cfg.in_pen() {
        let due = {
            let mut inner = s.inner.lock();
            let due = inner.last_summary.is_some_and(|t| t.elapsed() >= Duration::from_secs(3600));
            let in_world = ticks.saturating_sub(inner.summary_ticks);
            if due || inner.last_summary.is_none() {
                inner.last_summary = Some(Instant::now());
                inner.summary_ticks = ticks;
            }
            due.then_some(in_world)
        };
        if let Some(in_world) = due {
            let (mined, placed) = {
                let mut r = s.routine.lock();
                (std::mem::take(&mut r.mined_hour), std::mem::take(&mut r.placed_hour))
            };
            use crate::discord::count;
            if in_world >= SUMMARY_MIN_TICKS {
                s.discord.send(&format!("hourly: {} melons mined, {} placed this hour; {}", count(mined), count(placed), crate::progress::summary(s)));
            } else {
                info!("hourly summary not posted: in the world for only {} min of the last hour ({mined} mined, {placed} placed)", in_world / 1200);
            }
        }
    }

    // Moved to another world without a new login - the proxy putting us in its queue server and
    // back: azalea sends Event::Login only the first time, so it is caught here, by our entity id
    // changing, and treated as a join (7 Nov: kicked to the queue at 21:30 and sent back 4 s
    // later, the mapper kept its in-queue flag, its builder had stopped with the world gone for a
    // moment, and it sat 8 hours with 60 maps in line).
    if let Ok(id) = bot.minecraft_id() {
        let moved = {
            let mut inner = s.inner.lock();
            let moved = inner.entity_id.replace(id.0).is_some_and(|was| was != id.0);
            if moved && inner.in_queue && inner.queue_sent > 0 {
                info!("moved to a new world after {} queue command(s) - back on the server", inner.queue_sent);
                inner.in_queue = false;
                inner.queue_sent = 0;
                inner.next_queue_at = None;
            }
            moved
        };
        if moved {
            info!("the server moved us to another world");
            *s.joined_at.lock() = Some(Instant::now());
            if s.routine_on && !s.routine_stopped.load(Ordering::SeqCst) {
                s.start_routine(bot, true);
            }
        }
    }

    // Back out of the queue.
    let send_queue = {
        let mut inner = s.inner.lock();
        match inner.next_queue_at {
            Some(at) if inner.in_queue && Instant::now() >= at => {
                if inner.queue_sent >= MAX_QUEUE_SENDS {
                    warn!("still in the queue after {MAX_QUEUE_SENDS} queue commands - leaving it to reconnect");
                    inner.next_queue_at = None;
                    false
                } else {
                    inner.queue_sent += 1;
                    inner.next_queue_at = Some(Instant::now() + Duration::from_secs(60));
                    true
                }
            }
            _ => false,
        }
    };
    if send_queue {
        info!("sending {:?}", s.cfg.queue_command);
        bot.chat(s.cfg.queue_command.clone());
    }

    // Half a second between looks for players, as on the Java side.
    if ticks.is_multiple_of(10) {
        guard_players(bot, s);
    }

    // A status line a minute, for reading the log afterwards.
    let due = {
        let mut inner = s.inner.lock();
        let due = inner.last_status.is_some_and(|t| t.elapsed() >= Duration::from_secs(60));
        if due {
            inner.last_status = Some(Instant::now());
        }
        due
    };
    if due {
        let pos = bot.position().map(|p| format!("{:.1} {:.1} {:.1}", p.x, p.y, p.z)).unwrap_or_else(|_| "?".into());
        let health = bot.health().unwrap_or(-1.0);
        let hunger = bot.hunger().map(|h| h.food as i32).unwrap_or(-1);
        let farm = s.harvest.as_ref().map(|h| h.status()).unwrap_or_else(|| "no plot".into());
        let farm = if s.routine_on { format!("{farm} | routine: {}", s.routine_phase()) } else { farm };
        let food = s.autoeat.as_ref().map_or(-1, |a| a.food_points(bot));
        let board = s.board.as_ref().map(|b| b.standing()).filter(|b| !b.is_empty()).map(|b| format!(" | board: {b}")).unwrap_or_default();
        info!(
            "status: at {pos}, health {health:.0}/20, hunger {hunger}/20, food carried {food} pts, {} empty slots | move: {} | farm: {farm}{board}",
            crate::inv::empty_slots(bot),
            crate::movement::status(bot)
        );
    }
}

/// Any player in view who is not one of your own accounts or a friend: log out now and stay out
/// until relaunched. Not in the queue lobby, not while dead.
fn guard_players(bot: &Client, s: &Shared) {
    if !s.cfg.player_guard {
        return;
    }
    {
        let inner = s.inner.lock();
        if inner.in_queue || inner.logged_out {
            return;
        }
    }
    if bot.health().is_ok_and(|h| h <= 0.0) {
        return;
    }
    // Read under one lock (azalea's nearby_players nests two and can deadlock).
    let players: Vec<(f64, crate::entities::Near)> = crate::entities::near(bot).into_iter().filter(|(_, e)| e.player).collect();
    if players.is_empty() {
        return;
    }
    let tab = bot.tab_list().unwrap_or_default();
    let (own, me) = {
        let mut inner = s.inner.lock();
        if inner.own_read_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
            inner.own = config::own_accounts();
            inner.own_read_at = Some(Instant::now());
        }
        (inner.own.clone(), inner.me.clone())
    };
    for (dist, p) in players.iter() {
        let Some(uuid) = p.uuid else { continue };
        let name = tab.get(&uuid).map(|i| i.profile.name.to_string()).unwrap_or_else(|| uuid.to_string());
        let n = name.to_lowercase();
        if n == me || own.contains(&n) || s.cfg.friends.iter().any(|f| f.eq_ignore_ascii_case(&n)) {
            continue;
        }
        info!("player {name} in view ({dist:.0} blocks)");
        s.discord.send("a player came into view - logged out, staying out until relaunched");
        s.feed("event", "a player came into view - logged out");
        s.leave(bot, "player in view");
        return;
    }
}
