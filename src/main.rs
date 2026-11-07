//! melonbot: headless Rust Minecraft bots, on azalea (MC 26.2).
//!
//! The first one is the melon farmer, ported from the MelonHack standalone's melon routine one phase at a
//! time. Phase 1 (this): log in as the right account, stay connected (auto-reconnect, back out of the
//! queue), Discord, and the player guard - log out on any stranger and stay out.
//!
//! Each bot lives in `bots/<name>/`: config.json (from `melonbot import`), its Microsoft login
//! cache, and logs/.

mod board;
mod border;
mod bot;
mod buyer;
mod chests;
mod config;
mod conform;
mod control;
mod damage;
mod debuglog;
mod discord;
mod entities;
mod inv;
mod mapart;
mod modules;
mod movement;
mod panel;
mod print;
mod progress;
mod printer;
mod routine;
mod schematic;
mod sites;
mod script;
mod seller;
mod swarm;
mod trade;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use azalea::{account::microsoft::MicrosoftAccountOpts, prelude::*};
use clap::{Parser, Subcommand};
use tracing::{error, info};

use crate::{bot::State, config::Config, discord::Discord};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Make bots/<account>/config.json from a Java MelonHack instance (its folder or its .minecraft).
    Import {
        instance: PathBuf,
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
    /// Read a schematic and print what the routine would build from it.
    Schem { file: PathBuf },
    /// Run one bot: bots/<name>/config.json.
    Run {
        name: String,
        /// A test script run once after joining, e.g. "wait 5; goto base; status; quit".
        #[arg(long)]
        script: Option<String>,
        /// Do not start the melon routine even if modules.json says routine=Melon.
        #[arg(long)]
        no_routine: bool,
    },
    /// Run the botswarm: every swarm account in one process, its panel on http://127.0.0.1:7000/
    /// (swarm/swarm.json). Not the melon farmer - that one is `run <folder>`.
    Swarm {
        /// The swarm's folder.
        #[arg(long, default_value = "swarm")]
        dir: PathBuf,
        /// Another server than swarm.json's (a local test server: 127.0.0.1:25570).
        #[arg(long)]
        server: Option<String>,
        /// TEST ONLY: offline accounts by these names (comma separated), for an offline-mode server.
        #[arg(long, value_delimiter = ',')]
        offline: Vec<String>,
    },
    /// Run the perimeter bots: clear a box from build height to bedrock (perimeter/), their panel on
    /// http://127.0.0.1:7100/. The botswarm's accounts (swarm/bots/) - an account runs in one or the
    /// other, never both.
    Perimeter {
        /// The perimeter bots' folder (settings, progress, logs).
        #[arg(long, default_value = "perimeter")]
        dir: PathBuf,
        /// The botswarm's folder, whose accounts they use.
        #[arg(long, default_value = "swarm")]
        accounts: PathBuf,
        /// Another server than perimeter.json's (a local test server: 127.0.0.1:25570).
        #[arg(long)]
        server: Option<String>,
        /// TEST ONLY: offline accounts by these names (comma separated), for an offline-mode server.
        #[arg(long, value_delimiter = ',')]
        offline: Vec<String>,
    },
    /// TEST ONLY: place every block state in a list on a local offline vanilla server and check
    /// what the server made against the printer's prediction.
    Conform {
        #[arg(long, default_value = "127.0.0.1:25570")]
        server: String,
        #[arg(long, default_value = "Conform")]
        name: String,
        /// One block state a line ("minecraft:oak_stairs[facing=north,half=top]").
        #[arg(long)]
        cases: PathBuf,
        /// The file the server's console reads (tail -f into its stdin).
        #[arg(long)]
        console: PathBuf,
        #[arg(long, default_value = "conform-report.txt")]
        out: PathBuf,
    },
    /// TEST ONLY: write the swarm printer's end-to-end test build as a Sponge schematic.
    TestBuild { out: PathBuf },
    /// Fly an observer over a placed schematic and list every block that is not what it should be.
    Verify {
        #[arg(long, default_value = "127.0.0.1:25570")]
        server: String,
        #[arg(long, default_value = "Verifier")]
        name: String,
        #[arg(long)]
        schem: PathBuf,
        /// The placement's origin: x,y,z (no rotation or mirror).
        #[arg(long, value_delimiter = ',', num_args = 3)]
        origin: Vec<i32>,
        #[arg(long, default_value = "verify-report.txt")]
        out: PathBuf,
    },
}

fn bots_dir() -> PathBuf {
    PathBuf::from("bots")
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(code) => code,
        Err(e) => {
            error!("{e:#}");
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    match Cli::parse().cmd {
        Cmd::Import { instance, force } => {
            let cfg = Config::import(&instance)?;
            if cfg.account.is_empty() {
                bail!("{} has no account in melonhack/ai/profile.properties - which account is it?", instance.display());
            }
            let dir = bots_dir().join(&cfg.account);
            if dir.join("config.json").exists() && !force {
                bail!("{} already exists (--force to overwrite)", dir.join("config.json").display());
            }
            cfg.save(&dir)?;
            // Module settings in the standalone's own shape (auto-eat, melon-farmer, ...).
            let mc = if instance.join(".minecraft").is_dir() { instance.join(".minecraft") } else { instance.clone() };
            std::fs::copy(mc.join("melonhack").join("standalone.json"), dir.join("modules.json"))
                .context("copying standalone.json to modules.json")?;
            // The routine's progress (builds done, sites, melons mined), so the Rust bot carries on.
            let progress = mc.join("melonhack").join("ai").join("melon-routine.json");
            // Never over the Rust bot's own progress: that is newer than the Java copy.
            if progress.exists() && !dir.join("melon-routine.json").exists() {
                std::fs::copy(&progress, dir.join("melon-routine.json")).context("copying melon-routine.json")?;
            }
            println!(
                "wrote {} - account {}, discord {}, base {}, farm {}, tools chest {}, repair chest {}",
                dir.join("config.json").display(),
                cfg.account,
                if cfg.discord_webhook.is_empty() { "none" } else { "set" },
                yes(cfg.base.is_some()),
                yes(cfg.farm.is_some()),
                yes(cfg.tools_chest.is_some()),
                yes(cfg.repair_chest.is_some()),
            );
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Schem { file } => {
            match schematic::load(&file) {
                Ok(p) => println!("{}: {}x{}x{} (w x h x d), {} melons in {} layers", p.name, p.w, p.h, p.d, p.total, p.layers.len()),
                Err(e) => println!("(the melon routine cannot read it: {e:#})"),
            }
            // What the swarm printer reads from it: every block, every state.
            let s = print::schem::load(&file)?;
            let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
            for r in &s.regions {
                println!("  region '{}' at {:?} size {:?}: {} blocks", r.name, r.pos, r.size, r.blocks.len());
                for (_, st) in &r.blocks {
                    *kinds.entry(st.to_trait().id().to_string()).or_default() += 1;
                }
            }
            let mut top: Vec<_> = kinds.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            println!("  {} blocks, {} kinds: {}", s.block_count(), top.len(), top.iter().take(12).map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "));
            if !s.unknown.is_empty() {
                println!("  NOT KNOWN to this client (left out): {:?}", s.unknown);
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Run { name, script, no_routine } => run(&bots_dir().join(&name), script, no_routine).await,
        Cmd::Conform { server, name, cases, console, out } => conform::run(server, name, cases, console, out).await,
        Cmd::TestBuild { out } => {
            let b = conform::test_build();
            print::schem::write_sponge(&out, &b)?;
            println!("wrote {} ({} blocks)", out.display(), b.len());
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Verify { server, name, schem, origin, out } => {
            let o = (origin.first().copied().unwrap_or(0), origin.get(1).copied().unwrap_or(0), origin.get(2).copied().unwrap_or(0));
            conform::verify(server, name, schem, o, out).await
        }
        Cmd::Swarm { dir, server, offline } => {
            let _log_guard = init_logging(&dir, true)?;
            std::thread::spawn(deadlock_watch);
            info!("melonbot {} starting the botswarm from {}", env!("CARGO_PKG_VERSION"), dir.display());
            swarm::run(dir.clone(), dir, swarm::Role::Swarm, server, offline).await
        }
        Cmd::Perimeter { dir, accounts, server, offline } => {
            let _log_guard = init_logging(&dir, true)?;
            std::thread::spawn(deadlock_watch);
            info!("melonbot {} starting the perimeter bots from {} (accounts in {})", env!("CARGO_PKG_VERSION"), dir.display(), accounts.display());
            swarm::run(dir, accounts, swarm::Role::Perimeter, server, offline).await
        }
    }
}

fn yes(b: bool) -> &'static str {
    if b { "found" } else { "MISSING" }
}

/// A bot set up before its account was picked (config.json's "account" empty): sign in once with
/// whichever Microsoft account it is to be, keep the login under that account's name, and write the
/// name into config.json - every start after this one is an ordinary cached login, checked
/// against it. An account another bot already runs as is refused.
async fn first_sign_in(dir: &Path) -> Result<String> {
    use azalea::auth::{self, cache::CachedAccount};
    let http = reqwest::Client::new();
    let code = auth::get_ms_link_code(&http, None, None).await.context("asking Microsoft for a sign-in code")?;
    info!("{} has no account yet - waiting for the sign-in", dir.display());
    println!(
        "\nThis bot has no account yet. Open {}?otc={}\nand sign in with the Microsoft account it is to play as (code {}).\n",
        code.verification_uri, code.user_code, code.user_code
    );
    let msa = auth::get_ms_auth_token(&http, code, None).await.context("waiting for the sign-in")?;
    let mc = auth::get_minecraft_token(&http, &msa.data.access_token).await.context("Minecraft sign-in")?;
    let profile = auth::get_profile(&http, &mc.minecraft_access_token).await.context("reading the Minecraft profile")?;
    let name = profile.name.clone();
    if let Some(other) = account_taken(dir, &name) {
        bail!("{name} is already the account of {other} - two bots on one account kick each other off. Sign in with another one.");
    }
    auth::cache::set_account_in_cache(&dir.join("azalea-auth.json"), &name, CachedAccount { cache_key: name.clone(), msa, xbl: mc.xbl, mca: mc.mca, profile })
        .await
        .map_err(|e| anyhow::anyhow!("saving the login: {e}"))?;
    Config::update_file(dir, &[("account", serde_json::json!(name))])?;
    info!("signed in as {name} - saved as this bot's account in {}", dir.join("config.json").display());
    Ok(name)
}

/// Where else `name` is already a bot's account: another bots/ folder, or a swarm account
/// (swarm/bots/<name>/, which the botswarm and the perimeter bots run).
fn account_taken(dir: &Path, name: &str) -> Option<String> {
    for e in std::fs::read_dir(bots_dir()).into_iter().flatten().flatten() {
        let other = e.path();
        if other.file_name() != dir.file_name() && Config::load(&other).is_ok_and(|c| c.account.eq_ignore_ascii_case(name)) {
            return Some(other.display().to_string());
        }
    }
    let swarm = PathBuf::from("swarm").join("bots").join(name);
    swarm.is_dir().then(|| format!("the swarm ({})", swarm.display()))
}

async fn run(dir: &Path, script: Option<String>, no_routine: bool) -> Result<ExitCode> {
    let mut cfg = Config::load(dir)?;
    let _log_guard = init_logging(dir, false)?;
    std::thread::spawn(deadlock_watch);
    // The mapart bot's Discord side takes maps from the start - signed into Minecraft or not.
    if cfg.is_mapart() {
        mapart::start(dir, &cfg);
    }
    if cfg.account.is_empty() {
        cfg.account = first_sign_in(dir).await?;
    }
    info!("melonbot {} starting: {} on {}", env!("CARGO_PKG_VERSION"), cfg.account, cfg.server);

    // First run: azalea prints a microsoft.com/link URL with a code to sign in with; after that the
    // tokens are cached (and refreshed) in this bot's own folder.
    let account = Account::microsoft_with_opts(
        &cfg.account,
        MicrosoftAccountOpts { cache_file: Some(dir.join("azalea-auth.json")), ..Default::default() },
    )
    .await
    .context("Microsoft login")?;
    if !account.username().eq_ignore_ascii_case(&cfg.account) {
        bail!(
            "signed in as {}, but this bot is {} - delete {} and sign in with the right account",
            account.username(),
            cfg.account,
            dir.join("azalea-auth.json").display()
        );
    }
    config::register_own_account(account.username());

    let discord = Discord::start(&cfg.discord_webhook, &cfg.discord_name);
    let server = cfg.server.clone();
    let state = State::new(cfg, dir.to_path_buf(), modules::ModuleConfig::load(dir), discord.clone(), script, no_routine);
    let exit = ClientBuilder::new()
        .set_handler(bot::handle)
        .set_state(state.clone())
        .add_plugins((movement::MovementPlugin, damage::DamagePlugin, chests::ChestsPlugin, board::BoardPlugin, trade::TradePlugin, border::BorderPlugin))
        .reconnect_after(Duration::from_secs(30))
        .start(account, server.as_str())
        .await;
    discord.flush(Duration::from_secs(10)).await;
    if let Some(p) = state.panel() {
        p.shut();
    }
    info!("stopped: {exit:?}");
    Ok(if state.logged_out_for_player() { ExitCode::from(3) } else { ExitCode::SUCCESS })
}

/// Every 10 s: any threads deadlocked on our (or azalea's) locks go to the log with their
/// backtraces - a hung bot reports itself instead of going quiet.
fn deadlock_watch() {
    loop {
        std::thread::sleep(Duration::from_secs(10));
        let dead = parking_lot::deadlock::check_deadlock();
        if dead.is_empty() {
            continue;
        }
        error!("DEADLOCK: {} cycle(s)", dead.len());
        for (i, threads) in dead.iter().enumerate() {
            for t in threads {
                error!("deadlock #{i} thread {:?}:
{:?}", t.thread_id(), t.backtrace());
            }
        }
    }
}

/// Info to stdout and to bots/<name>/logs/melonbot.log.<date> (one file a day); and ALWAYS, at
/// full detail, one debug log per module in bots/<name>/debug/ (see debuglog.rs). The swarm
/// (`per_bot`): swarm/logs/ and swarm/debug/, and each bot's own in swarm/bots/<name>/debug/.
fn init_logging(dir: &Path, per_bot: bool) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{EnvFilter, filter::filter_fn, fmt, prelude::*};
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs)?;
    let (file, guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(&logs, "melonbot.log"));
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,azalea=warn,azalea_client=warn"));
    // Our own events only (melonbot::* and the explicit module targets), never azalea's.
    let ours = filter_fn(|m| debuglog::is_ours(m.target()));
    // A panic goes to the log files too, not only the console window (16 Oct: the melon farmer went
    // quiet mid-print and nothing on disk said why).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        error!("PANIC: {info}\n{}", std::backtrace::Backtrace::force_capture());
        default_hook(info);
    }));
    tracing_subscriber::registry()
        .with(fmt::layer().with_filter(filter()))
        .with(fmt::layer().with_ansi(false).with_writer(file).with_filter(filter()))
        .with({
            let logs = debuglog::DebugLogs::new(dir.join("debug"))?;
            if per_bot { logs.per_bot(dir.join("bots")) } else { logs }
        }.with_filter(ours))
        .init();
    Ok(guard)
}
