//! The botswarm: the controller mod's swarm idea (core/swarm in the Java standalone) on Rust bots, with
//! everything controlled from the control panel.
//!
//! One process for every swarm bot (`melonbot swarm`, started by start-botswarm.cmd), apart from
//! the melon farmer's (start-melon-farmer.cmd) so a swarm crash never stops the melon routine. The swarm is
//! an azalea swarm: bots join and leave while it runs and share one world in memory.
//!
//! Who commands it: the swarm panel (http://127.0.0.1:7000/) and any Java client running the controller mod as
//! Controller - every bot keeps a controller-protocol link to it (controller.rs) and follows its orders, unless
//! the panel gives a newer one or a bot has its own (orders.rs). Formations are the panel's
//! (formation.rs).
//!
//! Files: swarm/swarm.json (these settings), swarm/modules.json (module settings for every bot, the
//! standalone's shape), swarm/bots/<name>/ (each account: login, bot.json, debug logs).
//!
//! The same runtime also runs the perimeter bots (`melonbot perimeter`, perimeter/mod.rs) in a
//! process of their own: Role::Perimeter, their own folder and panel, the botswarm's accounts.

pub mod accounts;
pub mod combat;
mod control;
pub mod eclink;
pub mod errand;
pub mod formation;
pub mod controller;
pub mod nuker;
pub mod orders;
pub mod perimeter;
pub mod printer;
mod tick;

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use azalea::{
    Client, Vec3,
    account::{Account, microsoft::MicrosoftAccountOpts},
    app::{App, Plugin, Update},
    auto_reconnect::{InternalReconnectAfter, rejoin_after_delay, start_rejoin_on_disconnect},
    ecs::{
        entity::Entity,
        prelude::{Commands, Component, IntoScheduleConfigs, Query, ResMut, Resource, With},
    },
    entity::{LocalEntity, indexing::EntityUuidIndex},
    prelude::*,
    swarm::prelude::*,
};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::{Instrument, error, info, warn};

use azalea::ecs as bevy_ecs; // for #[derive(Resource)]

use crate::{
    bot::State,
    config::Config,
    modules::{ModuleConfig, autoeat::AutoEat, killaura::KillAura},
    panel::Panel,
};

pub use tick::tick;

static SWARM: OnceLock<Arc<SwarmCtx>> = OnceLock::new();

pub fn ctx() -> Option<&'static Arc<SwarmCtx>> {
    SWARM.get()
}

/// swarm/swarm.json.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmConfig {
    pub server: String,
    /// The swarm panel's port. Kept out of 6970-6991, where the Java panels and the bots' own sit
    /// (a Discord status watcher can scan that range for them).
    pub panel_port: u16,
    pub controller_enabled: bool,
    pub controller_host: String,
    /// The controller mod's port setting (25566 by default).
    pub controller_port: u16,
    /// Follow the controller player's position stream (the controller mod's websocket-track) for "follow me".
    pub controller_ws: bool,
    /// The controller mod's ws-port-offset: the stream is on port + this.
    pub controller_ws_offset: u16,
    /// Players never attacked, besides your own accounts (kept in this panel).
    pub friends: Vec<String>,
    pub formation: formation::Formation,
    /// Top flying speed, blocks a second.
    pub max_speed: f64,
    /// Bots sent to one point stand on a ring this far apart.
    pub fan_out_spacing: f64,
    /// Seconds between bots when the swarm starts several at once.
    pub join_delay_secs: f64,
    pub queue_command: String,
    pub queue_trigger: String,
    /// The food chest set from the panel (the controller player's spot when it was set). None =
    /// the nearest chest under a sign saying `food_sign`, else what they carry.
    pub food_chest: Option<crate::config::Pos>,
    pub food_sign: String,
    /// Fetch food when what is carried is worth fewer hunger points than this.
    pub food_low_points: i32,
    /// The tools chest: under a sign saying this, within `tools_radius` of the bot.
    pub tools_sign: String,
    pub tools_radius: f64,
    /// Where collected blocks go (the nuker's collect mode).
    pub storage_sign: String,
    /// The most swarm bots online at once (10 or less).
    pub max_bots: usize,
    /// A bot fights whatever hits it, Defend or not.
    pub fight_back: bool,
    /// The printer's placement by name when the controller has several enabled (None: the one
    /// nearest the controller player).
    pub printer_placement: Option<String>,
    /// Tools and weapons out of the ender chest (eclink.rs) - never for print jobs.
    pub ec_link: bool,
    /// The ender chest set from the panel (the controller player's spot when it was set); the
    /// nearest one any bot has seen is used too.
    pub ender_chest: Option<crate::config::Pos>,
    /// At or below this health everything borrowed goes back in the chest (a death drops it).
    pub ec_panic_health: f32,
    /// How far from where it works a bot looks for an ender chest: remembered ones and the spiral
    /// search (2,000 blocks by default).
    pub ec_search_range: f64,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        let c = Config::default();
        Self {
            server: c.server,
            panel_port: 7000,
            controller_enabled: true,
            controller_host: "127.0.0.1".into(),
            controller_port: 25566,
            controller_ws: true,
            controller_ws_offset: 1,
            friends: Vec::new(),
            formation: formation::Formation::default(),
            max_speed: crate::movement::trip::DEFAULT_MAX_BPS,
            fan_out_spacing: 2.0,
            join_delay_secs: 4.0,
            queue_command: c.queue_command,
            queue_trigger: c.queue_trigger,
            food_chest: None,
            food_sign: "food".into(),
            food_low_points: 12,
            tools_sign: "tools".into(),
            tools_radius: 64.0,
            storage_sign: "storage".into(),
            max_bots: 10,
            fight_back: true,
            printer_placement: None,
            ec_link: true,
            ender_chest: None,
            ec_panic_health: 6.0,
            ec_search_range: 2000.0,
        }
    }
}

/// What the swarm knows of the controller.
#[derive(Default)]
pub struct ControllerView {
    /// The controller player from its position stream: where, yaw, dimension, when.
    pub pos: Option<(Vec3, f32, String, Instant)>,
    pub ws_connected: bool,
    pub ws_error: String,
    /// The latest of each sync the controller sent (kind -> line, when), for the panel and the
    /// modules that use them.
    pub syncs: BTreeMap<String, (String, Instant)>,
}

/// A player some swarm bot can see.
#[derive(Clone, Debug)]
pub struct Sighting {
    pub name: String,
    pub pos: Vec3,
    pub yaw: f32,
    pub at: Instant,
    pub by: String,
}

enum OwnerCmd {
    Start(String),
    Stop(String),
}

/// What this process runs: the botswarm, or the perimeter bots - the same runtime and the same
/// accounts (swarm/bots/), each account in one of them at a time (in-use.json).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Swarm,
    Perimeter,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Swarm => "botswarm",
            Role::Perimeter => "perimeter bots",
        }
    }

    /// The settings file in the process's folder.
    fn config_file(self) -> &'static str {
        match self {
            Role::Swarm => "swarm.json",
            Role::Perimeter => "perimeter.json",
        }
    }
}

pub struct SwarmCtx {
    pub dir: PathBuf,
    pub role: Role,
    /// The folder whose bots/ holds the accounts: swarm/ for both roles (the
    /// perimeter bots share the swarm's list).
    pub accounts_root: PathBuf,
    /// The perimeter job, in Role::Perimeter.
    pub perimeter: Option<Arc<perimeter::Perimeter>>,
    pub cfg: RwLock<SwarmConfig>,
    /// swarm/modules.json: every bot's module settings (the swarm-wide layer).
    pub modules: RwLock<Value>,
    pub bots: RwLock<BTreeMap<String, Arc<BotLink>>>,
    pub panel: Option<Arc<Panel>>,
    pub controller: Mutex<ControllerView>,
    /// The panel's swarm-wide order.
    pub order: Mutex<orders::Stamped>,
    /// When the formation started: every bot measures the motion from the same moment.
    pub formation_epoch: Mutex<Instant>,
    /// Players the swarm can see, by lower-case name.
    pub sightings: Mutex<HashMap<String, Sighting>>,
    pub login: Mutex<Option<accounts::Login>>,
    login_task: Mutex<Option<JoinHandle<()>>>,
    cmd_tx: mpsc::UnboundedSender<OwnerCmd>,
    cmd_rx: Mutex<Option<mpsc::UnboundedReceiver<OwnerCmd>>>,
    /// Test mode: offline accounts (a local offline-mode server).
    pub offline: bool,
    warned: Mutex<HashSet<String>>,
    /// Chests found under signs, remembered (swarm/places.json).
    pub places: Mutex<errand::Places>,
    /// Players who hit one of ours lately.
    pub attackers: Mutex<combat::Attackers>,
    /// The panel's combat orders (Defend, attack target).
    pub combat: Mutex<combat::Orders>,
    /// The nuker's cell claims.
    pub nuker_board: Mutex<nuker::Board>,
    /// The Java instance running the controller mod as Controller, and what is copied from it.
    pub controller_instance: Mutex<Option<combat::ControllerInstance>>,
    /// Which swarm bot (its entity) was last hit by what, and when.
    pub hit_by: Mutex<HashMap<Entity, (Entity, Instant)>>,
    /// The schematic the swarm prints (from the controller's Litematica).
    pub print_job: RwLock<Option<Arc<printer::Job>>>,
    /// The printer's cell claims.
    pub print_board: Mutex<nuker::Board>,
    /// Every placement the controller's Litematica has: (name, enabled, dimension).
    pub printer_offers: Mutex<Vec<(String, bool, String)>>,
    /// The controller printer's supply chests and its map-art colour chests.
    pub print_chests: Mutex<(Vec<azalea::BlockPos>, Vec<(String, azalea::BlockPos)>)>,
    /// What each supply chest held when a bot last looked in (item id -> count).
    pub chest_contents: Mutex<HashMap<azalea::BlockPos, HashMap<String, i32>>>,
    /// Ender chests that were gone or would not open, skipped until then.
    pub bad_ender_chests: Mutex<Vec<(azalea::BlockPos, Instant)>>,
    /// The search for an ender chest (one bot at a time).
    pub ec_search: Mutex<eclink::Search>,
}

/// One swarm account, running or not.
pub struct BotLink {
    pub name: String,
    pub file: Mutex<accounts::BotFile>,
    pub client: Mutex<Option<Client>>,
    /// Meant to be on (started; online or reconnecting).
    pub running: AtomicBool,
    pub starting: AtomicBool,
    pub online: AtomicBool,
    /// The panel row, rebuilt by the bot's tick.
    pub row: Mutex<Value>,
    /// Panel commands for this bot, run on its tick.
    pub inbox: Mutex<VecDeque<Value>>,
    pub controller_link: Arc<controller::Link>,
    pub orders: Mutex<orders::BotOrders>,
    /// The order it followed last tick (for grouping: who shares a Come Here or a formation).
    pub current: Mutex<Option<orders::Stamped>>,
    /// Modules switched by the controller (MODULE lines), applied when it listens.
    pub controller_modules: Mutex<BTreeMap<String, bool>>,
    pub mods: Mutex<Mods>,
    pub last_error: Mutex<String>,
    pub joined_at: Mutex<Option<Instant>>,
    /// Its ECS entity while stopped (azalea reuses it when the account joins again).
    stopped_entity: Mutex<Option<Entity>>,
    /// Off fetching food or tools, or storing (errand.rs).
    pub errand: Mutex<Option<errand::Errand>>,
    /// Not before: the next food / tools / storage errand (after one failed or found nothing).
    pub food_retry_at: Mutex<Option<Instant>>,
    pub tools_retry_at: Mutex<Option<Instant>>,
    pub store_retry_at: Mutex<Option<Instant>>,
    pub nuker: Mutex<nuker::BotNuker>,
    /// The controller's NUKER_SYNC for this bot (its strip of the box).
    pub nuker_sync: Mutex<Option<nuker::Cfg>>,
    /// The controller's Defend / attack target, as sent down this bot's link.
    pub controller_combat: Mutex<combat::Orders>,
    pub combat_status: Mutex<String>,
    /// Where it last died, and when: its items lie there for 5 minutes.
    pub died_at: Mutex<Option<(Vec3, Instant)>>,
    /// Up out of reach to heal: where it holds.
    pub retreat: Mutex<Option<Vec3>>,
    pub printer: Mutex<printer::BotPrinter>,
    /// The controller's PRINTER_SYNC for this bot: its strip of the build, and strict.
    pub printer_sync: Mutex<Option<(Option<nuker::Box3>, bool)>>,
    pub restock_retry_at: Mutex<Option<Instant>>,
    /// The ender chest link and what is borrowed from it.
    pub eclink: Mutex<eclink::EcLink>,
    /// Its digging in the perimeter job (Role::Perimeter).
    pub perimeter: Mutex<perimeter::dig::BotDig>,
}

/// A stopped bot. azalea schedules a rejoin for every disconnected client; this undoes it for the
/// ones stopped on the panel. (Giving the entity its own AutoReconnectDelay - azalea's documented
/// way - does not work here: that type is a resource, and on an entity it hid the bot from
/// azalea's own queries - 16 Oct test: the stopped bot rejoined after 30 s, and its handler got
/// no more events.)
#[derive(Component)]
pub struct StayOut;

fn keep_stopped_out(mut commands: Commands, stopped: Query<Entity, (With<StayOut>, With<InternalReconnectAfter>)>) {
    for e in &stopped {
        info!(target: "swarm", "kept the stopped bot {e:?} out (its rejoin cancelled)");
        commands.entity(e).remove::<InternalReconnectAfter>();
    }
}

/// Every swarm bot's UUID points at its own entity. azalea finds the entity to reconnect by UUID,
/// and in a shared world another bot that sees this one come back (under a new network id) adds a
/// second entity for it and takes the UUID - then the next reconnect makes a fresh entity with no
/// handler state: on the server, but its code never runs (16 Oct test: a bot stopped and started
/// came back from a server restart as "Created new entity", offline in the panel).
fn keep_own_uuids(mut index: ResMut<EntityUuidIndex>, bots: Query<(Entity, &Account), With<LocalEntity>>) {
    for (e, account) in &bots {
        let uuid = account.uuid();
        if index.get(&uuid) != Some(e) {
            tracing::debug!(target: "swarm", "{} ({e:?}) had lost its uuid in the index - put back", account.username());
            index.insert(uuid, e);
        }
    }
}

pub struct SwarmPlugin;

impl Plugin for SwarmPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (keep_stopped_out, keep_own_uuids).after(start_rejoin_on_disconnect).before(rejoin_after_delay),
        )
        .add_systems(Update, combat::record_attackers)
        .add_plugins(eclink::EcLinkPlugin);
    }
}

/// The modules a swarm bot runs, rebuilt from its merged settings when they change.
#[derive(Default)]
pub struct Mods {
    pub autoeat: Option<AutoEat>,
    pub killaura: Option<KillAura>,
    /// The nuker as the panel (swarm settings + the bot's own) has it; the controller's sync
    /// takes over while the controller has it switched on.
    pub nuker: Option<nuker::Cfg>,
    pub printer: Option<printer::Cfg>,
    /// kill-aura's mob list (Defend fights the same mobs, kill-aura on or not).
    pub killaura_targets: HashSet<String>,
    pub dirty: bool,
}

/// The modules swarm bots run so far, and so the ones the panel offers settings for.
pub const SWARM_MODULES: &[&str] = &["auto-eat", "kill-aura", nuker::MODULE, printer::MODULE];

impl BotLink {
    fn new(name: &str, file: accounts::BotFile) -> Self {
        Self {
            name: name.to_string(),
            file: Mutex::new(file),
            client: Mutex::new(None),
            running: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            online: AtomicBool::new(false),
            row: Mutex::new(json!({})),
            inbox: Mutex::new(VecDeque::new()),
            controller_link: Arc::new(controller::Link::default()),
            orders: Mutex::new(orders::BotOrders::default()),
            current: Mutex::new(None),
            controller_modules: Mutex::new(BTreeMap::new()),
            mods: Mutex::new(Mods { dirty: true, ..Default::default() }),
            last_error: Mutex::new(String::new()),
            joined_at: Mutex::new(None),
            stopped_entity: Mutex::new(None),
            errand: Mutex::new(None),
            food_retry_at: Mutex::new(None),
            tools_retry_at: Mutex::new(None),
            store_retry_at: Mutex::new(None),
            nuker: Mutex::new(nuker::BotNuker::default()),
            nuker_sync: Mutex::new(None),
            controller_combat: Mutex::new(combat::Orders::default()),
            combat_status: Mutex::new(String::new()),
            died_at: Mutex::new(None),
            retreat: Mutex::new(None),
            printer: Mutex::new(printer::BotPrinter::default()),
            printer_sync: Mutex::new(None),
            restock_retry_at: Mutex::new(None),
            eclink: Mutex::new(eclink::EcLink::default()),
            perimeter: Mutex::new(perimeter::dig::BotDig::default()),
        }
    }

    /// The printer this bot runs now, if any: the merged settings, with the controller's strip
    /// (and its strict) while the controller has the printer on and the bot listens.
    pub fn printer_cfg(&self) -> Option<printer::Cfg> {
        let mut cfg = self.mods.lock().printer.clone()?;
        if self.listens()
            && self.controller_modules.lock().get(printer::MODULE) == Some(&true)
            && let Some((region, strict)) = *self.printer_sync.lock()
        {
            cfg.region = region;
            cfg.strict = strict;
        }
        Some(cfg)
    }

    /// Defend and the attack target this bot follows: the panel's, and the controller's when it
    /// listens to it.
    pub fn combat_orders(&self, ctx: &SwarmCtx) -> combat::Orders {
        let panel = ctx.combat.lock().clone();
        if !self.listens() {
            return panel;
        }
        let c = self.controller_combat.lock().clone();
        combat::Orders {
            defend: panel.defend || c.defend,
            defend_from: if panel.defend { "panel" } else { c.defend_from },
            target: panel.target.clone().or(c.target.clone()),
            target_from: if panel.target.is_some() { "panel" } else { c.target_from },
        }
    }

    pub fn killaura_targets(&self, _ctx: &SwarmCtx) -> HashSet<String> {
        self.mods.lock().killaura_targets.clone()
    }

    pub fn set_combat_status(&self, s: String) {
        *self.combat_status.lock() = s;
    }

    /// The nuker this bot runs now, if any: the controller's sync while the controller has it on
    /// (and the bot listens), else the panel's.
    pub fn nuker_cfg(&self) -> Option<nuker::Cfg> {
        if self.listens()
            && self.controller_modules.lock().get(nuker::MODULE) == Some(&true)
            && let Some(sync) = self.nuker_sync.lock().clone()
        {
            return Some(sync);
        }
        self.mods.lock().nuker.clone()
    }

    pub fn listens(&self) -> bool {
        self.file.lock().listen_controller
    }

    pub fn dir(&self, ctx: &SwarmCtx) -> PathBuf {
        accounts::bots_dir(&ctx.accounts_root).join(&self.name)
    }
}

impl SwarmCtx {
    pub fn bot(&self, name: &str) -> Option<Arc<BotLink>> {
        self.bots.read().get(name).cloned()
    }

    /// The account is known to the swarm (made on first sight; the panel lists it).
    pub fn ensure_bot(&self, name: &str) -> Arc<BotLink> {
        if let Some(b) = self.bot(name) {
            return b;
        }
        let file = accounts::load_all(&self.accounts_root).into_iter().find(|(n, _)| n == name).map(|(_, f)| f).unwrap_or_default();
        let link = Arc::new(BotLink::new(name, file));
        self.bots.write().insert(name.to_string(), link.clone());
        link
    }

    pub fn feed(&self, kind: &str, text: &str) {
        if let Some(p) = &self.panel {
            p.feed(kind, text);
        }
    }

    pub fn start(&self, name: &str) {
        let max = self.cfg.read().max_bots;
        let on = self.bots.read().values().filter(|b| b.name != name && (b.running.load(Ordering::SeqCst) || b.starting.load(Ordering::SeqCst))).count();
        if on >= max {
            let why = format!("the {} are at their limit of {max} bots - stop one first (or raise the limit)", self.role.name());
            if let Some(b) = self.bot(name) {
                *b.last_error.lock() = why.clone();
            }
            self.feed("say", &format!("{name} not started: {why}"));
            return;
        }
        let _ = self.cmd_tx.send(OwnerCmd::Start(name.to_string()));
    }

    pub fn stop(&self, name: &str) {
        let _ = self.cmd_tx.send(OwnerCmd::Stop(name.to_string()));
    }

    pub fn save_cfg(&self) {
        let text = serde_json::to_string_pretty(&*self.cfg.read()).unwrap_or_default();
        if let Err(e) = fs::write(self.dir.join(self.role.config_file()), text + "\n") {
            warn!(target: "swarm", "could not save {}: {e}", self.role.config_file());
        }
    }

    pub fn save_modules(&self) {
        let text = serde_json::to_string_pretty(&*self.modules.read()).unwrap_or_default();
        if let Err(e) = fs::write(self.dir.join("modules.json"), text + "\n") {
            warn!(target: "swarm", "could not save modules.json: {e}");
        }
    }

    /// Every bot's modules are rebuilt on its next tick (a setting changed).
    pub fn modules_changed(&self) {
        for b in self.bots.read().values() {
            b.mods.lock().dirty = true;
        }
    }

    /// One bot's module settings: the swarm's, then what the controller switched (if it listens),
    /// then its own overrides.
    pub fn merged_modules(&self, link: &BotLink) -> Value {
        let mut v = self.modules.read().clone();
        if !v.is_object() {
            v = json!({});
        }
        // kill-aura's mob list follows the controller instance's (copied live).
        if link.listens()
            && let Some(t) = self.controller_instance.lock().as_ref().and_then(|c| c.killaura_targets.clone())
        {
            v["kill-aura"]["settings"]["targets"] = json!(t);
        }
        // The printer's settings are the controller instance's printer's, under the swarm's own.
        if link.listens()
            && let Some(Value::Object(theirs)) = self.controller_instance.lock().as_ref().and_then(|c| c.printer.clone()).map(|p| p["settings"].clone())
        {
            let ours = v[printer::MODULE]["settings"].as_object().cloned().unwrap_or_default();
            let mut merged = theirs;
            merged.extend(ours);
            v[printer::MODULE]["settings"] = Value::Object(merged);
        }
        if link.listens() {
            for (m, on) in link.controller_modules.lock().iter() {
                v[m.as_str()]["enabled"] = Value::Bool(*on);
            }
        }
        let overrides = link.file.lock().overrides.clone();
        if let Some(o) = overrides.as_object() {
            for (m, mv) in o {
                if let Some(on) = mv.get("enabled").and_then(Value::as_bool) {
                    v[m.as_str()]["enabled"] = Value::Bool(on);
                }
                if let Some(s) = mv.get("settings").and_then(Value::as_object) {
                    for (k, val) in s {
                        v[m.as_str()]["settings"][k.as_str()] = val.clone();
                    }
                }
            }
        }
        v
    }

    /// Say something odd once (the controller may repeat a thing the Rust bots cannot do yet).
    pub fn warn_once(&self, text: &str) {
        controller::warn_once(&self.warned, text);
    }
}

// ------------------------------------------------------------------ running it

#[derive(Clone, Default, Resource)]
struct SwarmState;

/// `melonbot swarm` (Role::Swarm, `accounts_root` = `dir`) or `melonbot perimeter`
/// (Role::Perimeter, its own `dir`, the swarm's accounts). `offline` names test accounts for a
/// local offline server.
pub async fn run(dir: PathBuf, accounts_root: PathBuf, role: Role, server: Option<String>, offline: Vec<String>) -> Result<ExitCode> {
    fs::create_dir_all(&dir)?;
    fs::create_dir_all(accounts::bots_dir(&accounts_root))?;
    let mut cfg: SwarmConfig = match fs::read_to_string(dir.join(role.config_file())).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        Some(c) => c,
        // The perimeter panel on its own port, clear of the swarm's 7000-7020; no controller.
        None if role == Role::Perimeter => SwarmConfig { panel_port: 7100, controller_enabled: false, ..Default::default() },
        None => SwarmConfig::default(),
    };
    if let Some(s) = server {
        cfg.server = s;
    }
    let modules = load_modules(&dir);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let page = if role == Role::Perimeter { perimeter::control::PAGE } else { control::PAGE };
    let panel = Panel::start_with(cfg.panel_port, page, false);
    let ctx = Arc::new(SwarmCtx {
        dir: dir.clone(),
        role,
        accounts_root: accounts_root.clone(),
        perimeter: (role == Role::Perimeter).then(|| Arc::new(perimeter::Perimeter::load(&dir))),
        cfg: RwLock::new(cfg.clone()),
        modules: RwLock::new(modules),
        bots: RwLock::new(BTreeMap::new()),
        panel,
        controller: Mutex::new(ControllerView::default()),
        order: Mutex::new(orders::Stamped::idle()),
        formation_epoch: Mutex::new(Instant::now()),
        sightings: Mutex::new(HashMap::new()),
        login: Mutex::new(None),
        login_task: Mutex::new(None),
        cmd_tx,
        cmd_rx: Mutex::new(Some(cmd_rx)),
        offline: !offline.is_empty(),
        warned: Mutex::new(HashSet::new()),
        places: Mutex::new(errand::Places::load(&dir)),
        attackers: Mutex::new(combat::Attackers::default()),
        combat: Mutex::new(combat::Orders::default()),
        nuker_board: Mutex::new(nuker::Board::default()),
        controller_instance: Mutex::new(None),
        hit_by: Mutex::new(HashMap::new()),
        print_job: RwLock::new(None),
        print_board: Mutex::new(nuker::Board::default()),
        printer_offers: Mutex::new(Vec::new()),
        print_chests: Mutex::new((Vec::new(), Vec::new())),
        chest_contents: Mutex::new(HashMap::new()),
        bad_ender_chests: Mutex::new(Vec::new()),
        ec_search: Mutex::new(eclink::Search::default()),
    });
    let _ = SWARM.set(ctx.clone());
    ctx.save_cfg();
    ctx.save_modules();
    if ctx.offline {
        for n in &offline {
            let link = ctx.ensure_bot(n);
            link.file.lock().autostart = true;
            if let Some(p) = &ctx.perimeter {
                p.chosen.write().insert(n.clone());
            }
        }
    }
    for (name, _) in accounts::load_all(&accounts_root) {
        ctx.ensure_bot(&name);
    }
    info!(
        target: "swarm",
        "{} starting: {} account(s) on {}{}, panel http://127.0.0.1:{}/",
        role.name(),
        ctx.bots.read().len(),
        cfg.server,
        if ctx.offline { " (OFFLINE test accounts)" } else { "" },
        ctx.panel.as_ref().map_or(0, |p| p.port)
    );
    match role {
        Role::Swarm => {
            tokio::spawn(controller::watch_controller(ctx.clone()));
            tokio::spawn(combat::watch_controller_instance(ctx.clone()));
            tokio::spawn(printer::watch_job(ctx.clone()));
            tokio::spawn(control::run(ctx.clone()));
        }
        Role::Perimeter => {
            tokio::spawn(perimeter::control::run(ctx.clone()));
        }
    }
    tokio::spawn(keep_in_use(ctx.clone()));

    let exit = SwarmBuilder::new()
        .set_handler(handle)
        .set_swarm_handler(swarm_handle)
        .set_swarm_state(SwarmState)
        .add_plugins((crate::movement::MovementPlugin, crate::damage::DamagePlugin, crate::chests::ChestsPlugin, crate::border::BorderPlugin, SwarmPlugin))
        .reconnect_after(Duration::from_secs(30))
        .start(cfg.server.as_str())
        .await;
    info!(target: "swarm", "{} stopped: {exit:?}", role.name());
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------ one process per account

/// swarm/bots/<name>/in-use.json: which process runs the account. The botswarm and the perimeter
/// bots share the accounts, and an account is in one of them at a time, never
/// both. Rewritten every 10 s while it runs; an older one is from a process that is gone.
const IN_USE_FILE: &str = "in-use.json";
const IN_USE_STALE_MS: u64 = 45_000;

fn in_use_path(ctx: &SwarmCtx, name: &str) -> PathBuf {
    accounts::bots_dir(&ctx.accounts_root).join(name).join(IN_USE_FILE)
}

/// The other process running this account now, if one is (its role's name).
pub fn in_use_elsewhere(ctx: &SwarmCtx, name: &str) -> Option<String> {
    let v: Value = serde_json::from_str(&fs::read_to_string(in_use_path(ctx, name)).ok()?).ok()?;
    let fresh = v["at"].as_u64().is_some_and(|at| orders::now_ms().saturating_sub(at) < IN_USE_STALE_MS);
    let ours = v["pid"].as_u64() == Some(std::process::id() as u64);
    (fresh && !ours).then(|| v["by"].as_str().unwrap_or("another melonbot").to_string())
}

/// A live process rewrites its mark every 10 s: one left as it was this long is a dead process's.
const IN_USE_DEAD_SECS: u64 = 15;

/// in_use_elsewhere, for a start: a fresh mark that is not rewritten within IN_USE_DEAD_SECS was
/// left by a process that is gone (4 Nov: started 2 s after the old one was killed, both perimeter
/// bots "did not start: <name> is running in the perimeter bots" - the dead process's marks; the
/// launcher's restart after a crash, 30 s on, met the same).
async fn in_use_by_live(ctx: &SwarmCtx, name: &str) -> Option<String> {
    let at = || fs::read_to_string(in_use_path(ctx, name)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["at"].as_u64());
    in_use_elsewhere(ctx, name)?;
    let first = at();
    info!(target: "swarm", "{name} is marked in use by another process - seeing whether that one is still alive");
    tokio::time::sleep(Duration::from_secs(IN_USE_DEAD_SECS)).await;
    let other = in_use_elsewhere(ctx, name)?;
    (at() != first).then_some(other)
}

fn mark_in_use(ctx: &SwarmCtx, name: &str) {
    let v = json!({"by": ctx.role.name(), "pid": std::process::id(), "at": orders::now_ms()});
    if let Err(e) = fs::write(in_use_path(ctx, name), v.to_string()) {
        warn!(target: "swarm", "could not mark {name} in use: {e}");
    }
}

/// Ours (or a dead process's): gone. Another live process's is left alone.
fn clear_in_use(ctx: &SwarmCtx, name: &str) {
    if in_use_elsewhere(ctx, name).is_none() {
        let _ = fs::remove_file(in_use_path(ctx, name));
    }
}

async fn keep_in_use(ctx: Arc<SwarmCtx>) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let names: Vec<String> =
            ctx.bots.read().values().filter(|b| b.running.load(Ordering::SeqCst) || b.starting.load(Ordering::SeqCst)).map(|b| b.name.clone()).collect();
        for n in names {
            mark_in_use(&ctx, &n);
        }
    }
}

/// swarm/modules.json; on first start a copy of a bot of its own's (the first bots/<name>/ with
/// auto-eat or kill-aura set up), with the routine and the control panel switched off - those are
/// that bot's alone.
fn load_modules(dir: &Path) -> Value {
    if let Some(mut v) = fs::read_to_string(dir.join("modules.json")).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        for m in SWARM_MODULES {
            if v.get(*m).is_none() {
                v[*m] = json!({"enabled": false, "settings": {}});
            }
        }
        return v;
    }
    let mut v = dir.parent().and_then(|p| seed_modules(&p.join("bots"))).unwrap_or_else(|| json!({}));
    if let Some(o) = v.as_object_mut() {
        // Only what swarm bots run: the rest of the standalone's 80-odd modules mean nothing here.
        o.retain(|k, _| SWARM_MODULES.contains(&k.as_str()));
    }
    for m in SWARM_MODULES {
        if v.get(*m).is_none() {
            v[*m] = json!({"enabled": *m == "auto-eat", "settings": {}});
        }
    }
    v
}

/// The first bot of its own (bots/<name>/, by name) whose modules.json sets up auto-eat or kill-aura.
fn seed_modules(bots: &Path) -> Option<Value> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(bots).ok()?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.into_iter().find_map(|d| {
        let v = serde_json::from_str::<Value>(&fs::read_to_string(d.join("modules.json")).ok()?).ok()?;
        (v.get("auto-eat").is_some() || v.get("kill-aura").is_some()).then_some(v)
    })
}

async fn handle(bot: Client, event: Event, state: State) -> eyre::Result<()> {
    let span = match state.swarm_name() {
        Some(n) => tracing::info_span!("bot", name = %n),
        None => tracing::Span::none(),
    };
    crate::bot::handle(bot, event, state).instrument(span).await
}

async fn swarm_handle(swarm: Swarm, event: SwarmEvent, _state: SwarmState) -> eyre::Result<()> {
    if let SwarmEvent::Init = event {
        let Some(ctx) = SWARM.get().cloned() else { return Ok(()) };
        let Some(mut rx) = ctx.cmd_rx.lock().take() else { return Ok(()) };
        // The azalea swarm lives on this local task set: bots are added from here.
        let owner_ctx = ctx.clone();
        tokio::task::spawn_local(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    OwnerCmd::Start(name) => {
                        tokio::task::spawn_local(start_bot(swarm.clone(), owner_ctx.clone(), name));
                    }
                    OwnerCmd::Stop(name) => stop_bot(&owner_ctx, &name),
                }
            }
        });
        // Autostart, one at a time (as the swarm's join delay). The perimeter bots open the
        // accounts ticked in their own panel, not the swarm's autostart.
        let names: Vec<String> = match &ctx.perimeter {
            Some(p) => {
                let chosen = p.chosen.read().clone();
                ctx.bots.read().keys().filter(|n| chosen.contains(*n)).cloned().collect()
            }
            None => ctx.bots.read().values().filter(|b| b.file.lock().autostart).map(|b| b.name.clone()).collect(),
        };
        let delay = Duration::from_secs_f64(ctx.cfg.read().join_delay_secs.max(0.0));
        tokio::spawn(async move {
            for n in names {
                ctx.start(&n);
                tokio::time::sleep(delay).await;
            }
        });
    }
    Ok(())
}

async fn start_bot(swarm: Swarm, ctx: Arc<SwarmCtx>, name: String) {
    let Some(link) = ctx.bot(&name) else { return };
    if link.running.load(Ordering::SeqCst) || link.starting.swap(true, Ordering::SeqCst) {
        return;
    }
    let span = tracing::info_span!("bot", name = %name);
    async {
        if let Some(other) = in_use_by_live(&ctx, &name).await {
            fail(&ctx, &link, &format!("{name} is running in the {other} - stop it there first"));
            return;
        }
        mark_in_use(&ctx, &name);
        let account = if ctx.offline {
            Ok(Account::offline(&name))
        } else {
            let opts = MicrosoftAccountOpts { cache_file: Some(link.dir(&ctx).join("azalea-auth.json")), ..Default::default() };
            // A refresh that fails falls back to an interactive sign-in that would wait 15 minutes
            // on the console: give up sooner and say so on the panel.
            match tokio::time::timeout(Duration::from_secs(60), Account::microsoft_with_opts(&name, opts)).await {
                Ok(Ok(a)) => Ok(a),
                Ok(Err(e)) => Err(format!("sign-in failed ({e}) - Add account and sign in as {name} again")),
                Err(_) => Err(format!("the saved login has expired - Add account and sign in as {name} again")),
            }
        };
        let account = match account {
            Ok(a) if a.username().eq_ignore_ascii_case(&name) => a,
            Ok(a) => {
                fail(&ctx, &link, &format!("the saved login is for {}, not {name}", a.username()));
                return;
            }
            Err(e) => {
                fail(&ctx, &link, &e);
                return;
            }
        };
        let cfg = ctx.cfg.read().clone();
        let bot_cfg = Config {
            account: name.clone(),
            server: cfg.server.clone(),
            friends: cfg.friends.clone(),
            // Swarm bots never log out on players.
            player_guard: false,
            queue_command: cfg.queue_command.clone(),
            queue_trigger: cfg.queue_trigger.clone(),
            instance: if ctx.role == Role::Perimeter { "PERIMETER" } else { "BOTSWARM" }.into(),
            ..Config::default()
        };
        let modules = ModuleConfig::from_value(ctx.merged_modules(&link));
        let state = State::new_swarm(bot_cfg, link.dir(&ctx), modules, link.clone());
        info!(target: "swarm", "starting {name}");
        // Started again after a stop: the same entity comes back - let it reconnect again.
        if let Some(e) = link.stopped_entity.lock().take() {
            let mut ecs = swarm.ecs.write();
            match ecs.get_entity_mut(e) {
                Ok(mut em) => {
                    em.remove::<StayOut>();
                    info!(target: "swarm", "{name} may reconnect again (entity {e:?}, still marked stopped: {})", em.contains::<StayOut>());
                }
                Err(err) => warn!(target: "swarm", "{name}: its stopped entity {e:?} is gone ({err})"),
            }
        }
        let client = swarm.add(&account, state).await;
        *link.client.lock() = Some(client);
        link.last_error.lock().clear();
        link.running.store(true, Ordering::SeqCst);
        link.starting.store(false, Ordering::SeqCst);
        // The perimeter bots take no controller orders.
        if ctx.role == Role::Swarm {
            link.controller_link.start(ctx.clone(), name.clone());
        }
        ctx.feed("event", &format!("{name} started"));
    }
    .instrument(span)
    .await;
}

fn fail(ctx: &SwarmCtx, link: &BotLink, why: &str) {
    clear_in_use(ctx, &link.name);
    error!(target: "swarm", "{} did not start: {why}", link.name);
    *link.last_error.lock() = why.to_string();
    link.starting.store(false, Ordering::SeqCst);
}

fn stop_bot(ctx: &SwarmCtx, name: &str) {
    let Some(link) = ctx.bot(name) else { return };
    link.running.store(false, Ordering::SeqCst);
    link.online.store(false, Ordering::SeqCst);
    link.controller_link.stop();
    if let Some(c) = link.client.lock().take() {
        // This one stays out (no reconnect for its entity); the rest of the swarm carries on.
        c.ecs.write().entity_mut(c.entity).insert(StayOut);
        *link.stopped_entity.lock() = Some(c.entity);
        c.disconnect();
    }
    clear_in_use(ctx, name);
    info!(target: "swarm", "stopped {name}");
    ctx.feed("event", &format!("{name} stopped"));
}

/// Remove an account from the swarm: stopped, and its folder moved to swarm/removed/.
pub fn remove(ctx: &SwarmCtx, name: &str) -> Result<()> {
    stop_bot(ctx, name);
    ctx.bots.write().remove(name);
    let from = accounts::bots_dir(&ctx.accounts_root).join(name);
    let to = ctx.accounts_root.join("removed").join(format!("{name}-{}", orders::now_ms()));
    fs::create_dir_all(to.parent().unwrap())?;
    fs::rename(&from, &to).with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
    info!(target: "swarm", "removed {name} (its folder is in {})", to.display());
    Ok(())
}

pub fn start_login(ctx: &Arc<SwarmCtx>) {
    let mut task = ctx.login_task.lock();
    if task.as_ref().is_some_and(|t| !t.is_finished()) {
        return;
    }
    *ctx.login.lock() = Some(accounts::Login { status: "asking Microsoft for a code...".into(), ..Default::default() });
    *task = Some(tokio::spawn(accounts::device_login(ctx.clone())));
}

pub fn cancel_login(ctx: &SwarmCtx) {
    if let Some(t) = ctx.login_task.lock().take() {
        t.abort();
    }
    *ctx.login.lock() = None;
}
