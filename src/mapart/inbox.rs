//! Maps from a folder, for a mapart bot with no Discord (or beside it): a .litematic, .schem or
//! .nbt dropped in bots/<name>/inbox/ is checked as one sent on Discord is (art.rs) and queued
//! under its file's name - no Name it form here. One that fails the check goes to
//! inbox/refused/, the reason in a .txt beside it.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use tracing::{info, warn};

use super::{
    Mapart,
    art,
    queue::{Job, JobState, name_room},
};

pub const INBOX: &str = "inbox";
const REFUSED: &str = "refused";
/// The folder is looked in this often; a file is taken once its size and time stayed the same
/// from one look to the next (not while it is still being copied in).
const POLL: Duration = Duration::from_secs(5);
const KINDS: [&str; 3] = ["litematic", "schem", "nbt"];

pub async fn run(m: Arc<Mapart>, dir: PathBuf) {
    let inbox = dir.join(INBOX);
    if let Err(e) = fs::create_dir_all(&inbox) {
        warn!(target: "mapart", "no inbox folder ({}): {e}", inbox.display());
        return;
    }
    info!(target: "mapart", "maps dropped in {} are queued under their file's name", inbox.display());
    let mut seen: HashMap<PathBuf, (u64, Option<SystemTime>)> = HashMap::new();
    loop {
        let now = files(&inbox);
        for (path, stamp) in &now {
            if seen.get(path) == Some(stamp) {
                take(&m, &dir, path).await;
            }
        }
        seen = now.into_iter().collect();
        tokio::time::sleep(POLL).await;
    }
}

/// The map files in the inbox, with their size and time.
fn files(inbox: &Path) -> Vec<(PathBuf, (u64, Option<SystemTime>))> {
    let Ok(rd) = fs::read_dir(inbox) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            let path = e.path();
            let kind = path.extension()?.to_str()?.to_ascii_lowercase();
            KINDS.contains(&kind.as_str()).then(|| (path, (meta.len(), meta.modified().ok())))
        })
        .collect()
}

/// The name a file's maps get: its name without the extension, cut to what fits on an anvil.
pub fn given_name(file: &str, of: (usize, usize)) -> String {
    let stem = Path::new(file).file_stem().and_then(|s| s.to_str()).unwrap_or("").trim();
    let name: String = stem.chars().take(name_room(of)).collect();
    if name.trim().is_empty() { "map".into() } else { name.trim().to_string() }
}

async fn take(m: &Arc<Mapart>, dir: &Path, path: &Path) {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("map.nbt").to_string();
    let at = super::now_ms();
    let group = format!("inbox{at}");
    let file = format!("{group}-{}", super::safe_name(&name));
    let maps = dir.join("maps");
    let kept = maps.join(&file);
    if let Err(e) = fs::create_dir_all(&maps).and_then(|_| fs::rename(path, &kept)) {
        warn!(target: "mapart", "could not take {name} out of the inbox: {e}");
        return;
    }
    let checked = {
        let kept = kept.clone();
        tokio::task::spawn_blocking(move || art::read(&kept)).await.unwrap_or_else(|e| Err(format!("the check crashed ({e})")))
    };
    let a = match checked {
        Ok(a) => a,
        Err(why) => {
            let refused = dir.join(INBOX).join(REFUSED);
            let back = refused.join(&name);
            let _ = fs::create_dir_all(&refused).and_then(|_| fs::rename(&kept, &back));
            let _ = fs::write(refused.join(format!("{name}.txt")), format!("{why}\n"));
            warn!(target: "mapart", "refused {name} from the inbox: {why} (it is in {})", refused.display());
            return;
        }
    };
    let single = a.maps.len() == 1;
    let jobs: Vec<Job> = a
        .maps
        .iter()
        .map(|map| Job {
            id: if single { group.clone() } else { format!("{group}-{}-{}", map.col, map.row) },
            group: group.clone(),
            msg: String::new(),
            name: name.clone(),
            given: String::new(),
            map: (map.col, map.row),
            of: (a.cols, a.rows),
            file: format!("maps/{file}"),
            by: "the inbox".into(),
            at_ms: at,
            carpets: map.carpets(),
            state: JobState::Pending,
            note: String::new(),
        })
        .collect();
    let given = given_name(&name, (a.cols, a.rows));
    let mut q = m.queue.lock();
    q.add_pending(jobs);
    match q.name(&group, &given, at) {
        Ok((first, last)) => {
            let place = if first == last { format!("#{first}") } else { format!("#{first}-{last}") };
            info!(target: "mapart", "took {name} from the inbox: {} - queued {place} as \"{given}\"", a.summary());
        }
        Err(e) => warn!(target: "mapart", "took {name} from the inbox but could not queue it: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_named_after_itself() {
        assert_eq!(given_name("Castle.litematic", (1, 1)), "Castle");
        assert_eq!(given_name("  .nbt", (1, 1)), "map");
        let long = "x".repeat(80) + ".nbt";
        assert_eq!(given_name(&long, (2, 2)).chars().count(), name_room((2, 2)), "it fits on an anvil");
    }
}
