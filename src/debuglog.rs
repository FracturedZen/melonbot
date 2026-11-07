//! Per-module debug logs, always on: every event from our own code goes to
//! `bots/<name>/debug/<module>.log` at full detail (debug and up), whatever the console shows - the
//! Java client's melonhack-debug/ folder, which is where every real cause has come from ("read the
//! log first"). The module is the event's target: explicit ones ("goto", "melon-farmer",
//! "auto-eat", "chat", "script") or the last part of the module path ("melonbot::bot" -> "bot").
//! Each line carries the wall time and the bot's tick, so gaps and repeats can be measured.
//!
//! In the swarm (several bots in one process) an event inside a `bot` span (`info_span!("bot",
//! name = ...)`) goes to that bot's own folder instead: `<per_bot_root>/<name>/debug/<module>.log`.

use std::{
    collections::HashMap,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use parking_lot::Mutex;
use tracing::{Event, Level, Subscriber, field::Field, span};
use tracing_subscriber::{Layer, layer::Context, registry::LookupSpan};

/// The bot's tick count, stamped on every line (set by the handler each tick).
pub static TICK: AtomicU64 = AtomicU64::new(0);

pub struct DebugLogs {
    dir: PathBuf,
    /// The swarm: bots/ under which each bot named by a `bot` span gets its own debug/ folder.
    per_bot_root: Option<PathBuf>,
    files: Mutex<HashMap<(Option<String>, String), File>>,
}

impl DebugLogs {
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir, per_bot_root: None, files: Mutex::new(HashMap::new()) })
    }

    /// Bot events (inside a `bot` span) to `<root>/<name>/debug/`; the rest to `dir`.
    pub fn per_bot(mut self, root: PathBuf) -> Self {
        self.per_bot_root = Some(root);
        self
    }

    fn path(&self, bot: Option<&str>, module: &str) -> PathBuf {
        match (bot, &self.per_bot_root) {
            (Some(b), Some(root)) => root.join(b).join("debug").join(format!("{module}.log")),
            _ => self.dir.join(format!("{module}.log")),
        }
    }
}

/// The bot a `bot` span names, kept on the span.
struct BotName(String);

struct NameField(Option<String>);

impl tracing::field::Visit for NameField {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "name" {
            self.0 = Some(format!("{value:?}").trim_matches('"').to_string());
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "name" {
            self.0 = Some(value.to_string());
        }
    }
}

/// The explicit targets our modules log under (one file each).
pub const MODULE_TARGETS: &[&str] = &[
    "goto", "auto-eat", "melon-farmer", "chat", "script", "damage", "discord", "routine", "chests", "printer", "kill-aura", "inventory", "panel",
    "swarm", "controller", "accounts", "nuker", "combat", "perimeter", "seller", "buyer", "trade", "mapart",
];

/// Our own events: melonbot::* and the module targets - never azalea's or a library's.
pub fn is_ours(target: &str) -> bool {
    target.starts_with("melonbot") || MODULE_TARGETS.contains(&target)
}

/// Files are named after the module: "melonbot::bot" -> "bot", "goto" -> "goto".
fn module_name(target: &str) -> Option<String> {
    if let Some(rest) = target.strip_prefix("melonbot") {
        let last = rest.rsplit("::").next().unwrap_or("");
        return Some(if last.is_empty() { "main".into() } else { last.into() });
    }
    MODULE_TARGETS.contains(&target).then(|| target.to_string())
}

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for DebugLogs {
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        if attrs.metadata().name() != "bot" {
            return;
        }
        let mut f = NameField(None);
        attrs.record(&mut f);
        if let (Some(n), Some(span)) = (f.0, ctx.span(id)) {
            span.extensions_mut().insert(BotName(n));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > Level::DEBUG {
            return;
        }
        let Some(name) = module_name(meta.target()) else { return };
        let bot = if self.per_bot_root.is_some() {
            ctx.event_scope(event).and_then(|scope| scope.from_root().find_map(|s| s.extensions().get::<BotName>().map(|b| b.0.clone())))
        } else {
            None
        };
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        let line = format!(
            "{} [{:>7}] {:5} {}\n",
            chrono_like_now(),
            TICK.load(Ordering::Relaxed),
            meta.level(),
            fields.0
        );
        let mut files = self.files.lock();
        let key = (bot, name);
        let file = match files.get_mut(&key) {
            Some(f) => f,
            None => {
                let path = self.path(key.0.as_deref(), &key.1);
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let Ok(f) = OpenOptions::new().create(true).append(true).open(path) else { return };
                files.entry(key).or_insert(f)
            }
        };
        let _ = file.write_all(line.as_bytes());
    }
}

/// "2026-10-15 19:41:02.123" in UTC, without pulling in a date crate.
fn chrono_like_now() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        now.subsec_millis()
    )
}
