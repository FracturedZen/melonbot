//! melon-routine.json - the Java routine's own progress file (builds done, their sites, the floor
//! pieces, melons mined/placed), in its field names, copied over by `import` so the Rust bot
//! carries on where the Java one stopped. Unknown fields are kept (flattened), so nothing the Java
//! side wrote is lost.

use std::fs;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::bot::Shared;

/// One build's place in the world, so nothing is ever put on top of it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Site {
    pub file: String,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    pub w: i32,
    pub h: i32,
    pub d: i32,
    /// The layer the printer had reached (world y), to resume at; 0 = not started.
    #[serde(default)]
    pub layer: i32,
    /// Melons the last check of the world found missing (None = never checked).
    #[serde(default)]
    pub missing: Option<i32>,
    /// Finished by a check of the world - never by the printer's say-so.
    #[serde(default)]
    pub checked: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    #[serde(default)]
    pub done: Vec<String>,
    #[serde(default)]
    pub sites: Vec<Site>,
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub floors: Vec<[i32; 2]>,
    #[serde(default)]
    pub floor_current: Option<[i32; 2]>,
    #[serde(default)]
    pub floor_missing: Option<i32>,
    #[serde(default)]
    pub floor_requests: i32,
    /// Ceiling tiles finished (origin x, z), the one being built, the layer it reached (world y; 0 =
    /// not started) and what its last check of the world found missing.
    #[serde(default)]
    pub ceilings: Vec<[i32; 2]>,
    #[serde(default)]
    pub ceiling_current: Option<[i32; 2]>,
    #[serde(default)]
    pub ceiling_layer: i32,
    #[serde(default)]
    pub ceiling_missing: Option<i32>,
    #[serde(default)]
    pub next_build: Option<String>,
    #[serde(default)]
    pub mined: i64,
    #[serde(default)]
    pub placed: i64,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Progress {
    pub fn site(&self, file: &str) -> Option<&Site> {
        self.sites.iter().find(|s| s.file == file)
    }

    pub fn site_mut(&mut self, file: &str) -> Option<&mut Site> {
        self.sites.iter_mut().find(|s| s.file == file)
    }
}

fn path(ctx: &Shared) -> std::path::PathBuf {
    ctx.dir.join("melon-routine.json")
}

pub fn load(ctx: &Shared) -> Progress {
    match fs::read_to_string(path(ctx)) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
            warn!(target: "routine", "progress unreadable ({e}) - starting afresh");
            Progress::default()
        }),
        Err(_) => Progress::default(),
    }
}

pub fn save(ctx: &Shared, p: &Progress) {
    if let Err(e) = fs::write(path(ctx), serde_json::to_string_pretty(p).unwrap_or_default()) {
        warn!(target: "routine", "could not save progress: {e}");
    }
}

/// Load, change, save - one step.
pub fn update(ctx: &Shared, f: impl FnOnce(&mut Progress)) -> Progress {
    let mut p = load(ctx);
    f(&mut p);
    save(ctx, &p);
    p
}

pub fn add_mined(ctx: &Shared, n: i64) {
    if n > 0 {
        update(ctx, |p| p.mined += n);
    }
}

/// "builds done 1, floor pieces 0, melons mined 388.0k, placed 395.2k" for Discord lines - counts
/// via discord::count, which the coordinate scrubber leaves alone.
pub fn summary(ctx: &Shared) -> String {
    use crate::discord::count;
    let p = load(ctx);
    format!(
        "builds done {}, floor pieces {}, ceiling tiles {}, melons mined {}, placed {}",
        p.done.len(),
        p.floors.len(),
        p.ceilings.len(),
        count(p.mined),
        count(p.placed)
    )
}
