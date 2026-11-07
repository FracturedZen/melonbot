//! The mapart queue: the maps sent in, kept in bots/<name>/queue.json (their files in maps/) so a
//! restart loses none. A file that passed the check waits for its name (the Name it form on
//! Discord); named, its maps join the line, and a map is built when every one before it is done.
//! A piece of several maps (2x2...) is one entry per map, from its north-west map along each row.
//!
//! The locked map gets an anvil name from the given name: 1X1 "Castle"; for a bigger piece the
//! size, the name, and the map's column and row - 2X2 "Castle" c1r1, c2r1, c1r2, c2r2.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Finished, failed and cancelled maps kept for the record, newest last.
const HISTORY: usize = 50;
/// The longest name an anvil gives.
pub const ANVIL_MAX: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    /// Checked, waiting for its name.
    Pending,
    Queued,
    Building,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// Its own: the file's group, and for a map of a bigger piece its place ("<group>-1-0").
    pub id: String,
    /// The file it is a map of - one per file sent ("<message id>", "<message id>.2" for a
    /// message's second file): named and answered together.
    #[serde(default)]
    pub group: String,
    /// The Discord message it came in (answers go to it).
    #[serde(default)]
    pub msg: String,
    /// The file's name as sent.
    pub name: String,
    /// The name given on the form.
    #[serde(default)]
    pub given: String,
    /// Which map of the piece it is (column, row from the north-west), of how many each way.
    #[serde(default)]
    pub map: (usize, usize),
    #[serde(default = "one_by_one")]
    pub of: (usize, usize),
    /// Where it is kept, relative to the bot's folder ("maps/<id>-<name>").
    pub file: String,
    /// Who sent it (their Discord name).
    pub by: String,
    pub at_ms: u64,
    pub carpets: usize,
    pub state: JobState,
    /// Why it failed, or how it ended.
    #[serde(default)]
    pub note: String,
}

fn one_by_one() -> (usize, usize) {
    (1, 1)
}

/// The anvil name: 1X1 "Castle", or 2X2 "Castle" c1r2 for column 1, row 2 of a bigger piece.
pub fn anvil_name(given: &str, map: (usize, usize), of: (usize, usize)) -> String {
    if of.0 * of.1 <= 1 {
        format!("1X1 \"{given}\"")
    } else {
        format!("{}X{} \"{given}\" c{}r{}", of.0, of.1, map.0 + 1, map.1 + 1)
    }
}

/// How long a given name may be so every map's anvil name fits.
pub fn name_room(of: (usize, usize)) -> usize {
    ANVIL_MAX.saturating_sub(anvil_name("", (of.0.saturating_sub(1), of.1.saturating_sub(1)), of).chars().count())
}

impl Job {
    pub fn waiting(&self) -> bool {
        matches!(self.state, JobState::Queued | JobState::Building)
    }

    pub fn anvil_name(&self) -> String {
        anvil_name(&self.given, self.map, self.of)
    }

    /// Its anvil name once named; till then the file's name (and its place in the piece).
    pub fn title(&self) -> String {
        if !self.given.is_empty() {
            return self.anvil_name();
        }
        if self.of.0 * self.of.1 <= 1 {
            return self.name.clone();
        }
        format!("{} c{}r{}", self.name, self.map.0 + 1, self.map.1 + 1)
    }
}

pub struct Queue {
    path: PathBuf,
    pub jobs: Vec<Job>,
}

impl Queue {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("queue.json");
        let jobs = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Self { path, jobs }
    }

    fn save(&self) {
        match serde_json::to_string_pretty(&self.jobs) {
            Ok(t) => {
                if let Err(e) = fs::write(&self.path, t + "\n") {
                    warn!(target: "mapart", "could not save {}: {e}", self.path.display());
                }
            }
            Err(e) => warn!(target: "mapart", "could not write the queue: {e}"),
        }
    }

    /// The maps still to build (the one being built first), in order.
    pub fn waiting(&self) -> Vec<&Job> {
        let mut w: Vec<&Job> = self.jobs.iter().filter(|j| j.waiting()).collect();
        w.sort_by_key(|j| (j.state != JobState::Building, j.at_ms));
        w
    }

    /// A checked file's maps, waiting for their name.
    pub fn add_pending(&mut self, maps: Vec<Job>) {
        self.jobs.extend(maps.into_iter().map(|j| Job { state: JobState::Pending, ..j }));
        self.trim();
        self.save();
    }

    /// The size of the piece in `group`, if it is waiting for its name.
    pub fn pending(&self, group: &str) -> Option<(usize, usize)> {
        self.jobs.iter().find(|j| j.group == group && j.state == JobState::Pending).map(|j| j.of)
    }

    /// Name the file in `group` and put its maps in line. Their places (first, last), counting the
    /// one being built.
    pub fn name(&mut self, group: &str, given: &str, at_ms: u64) -> Result<(usize, usize), String> {
        let Some(of) = self.pending(group) else { return Err("it has been named already, or it is gone".into()) };
        if given.is_empty() {
            return Err("the name is empty".into());
        }
        if given.chars().count() > name_room(of) {
            return Err(format!("the name is too long - at most {} characters, so the map's name fits on an anvil", name_room(of)));
        }
        let before = self.waiting().len();
        let mut n = 0;
        for j in self.jobs.iter_mut().filter(|j| j.group == group && j.state == JobState::Pending) {
            j.state = JobState::Queued;
            j.given = given.to_string();
            j.at_ms = at_ms;
            n += 1;
        }
        self.save();
        Ok((before + 1, before + n))
    }

    /// Cancel the `n`th map in line (1-based), unless it is being built.
    pub fn cancel(&mut self, n: usize) -> Result<Job, String> {
        let id = match self.waiting().get(n.wrapping_sub(1)) {
            None => return Err(format!("there is no #{n} in the queue")),
            Some(j) if j.state == JobState::Building => return Err(format!("#{n} ({}) is being built already", j.title())),
            Some(j) => j.id.clone(),
        };
        let j = self.jobs.iter_mut().find(|j| j.id == id).ok_or("gone")?;
        j.state = JobState::Cancelled;
        j.note = "cancelled".into();
        let out = j.clone();
        self.save();
        Ok(out)
    }

    /// The map to build now: the one being built (after a restart), else the oldest queued -
    /// marked as being built.
    pub fn next(&mut self) -> Option<Job> {
        let id = self.waiting().first().map(|j| j.id.clone())?;
        let j = self.jobs.iter_mut().find(|j| j.id == id)?;
        j.state = JobState::Building;
        let out = j.clone();
        self.save();
        Some(out)
    }

    pub fn finish(&mut self, id: &str, state: JobState, note: &str) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
            j.state = state;
            j.note = note.to_string();
        }
        self.trim();
        self.save();
    }

    /// Only HISTORY finished ones are kept; the waiting and the unnamed ones always.
    fn trim(&mut self) {
        let kept = |j: &Job| j.waiting() || j.state == JobState::Pending;
        let done = self.jobs.iter().filter(|j| !kept(j)).count();
        let mut drop = done.saturating_sub(HISTORY);
        self.jobs.retain(|j| {
            if drop > 0 && !kept(j) {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(group: &str, map: (usize, usize), of: (usize, usize)) -> Job {
        Job {
            id: if of == (1, 1) { group.into() } else { format!("{group}-{}-{}", map.0, map.1) },
            group: group.into(),
            msg: group.into(),
            name: format!("{group}.nbt"),
            given: String::new(),
            map,
            of,
            file: format!("maps/{group}.nbt"),
            by: "someone".into(),
            at_ms: 0,
            carpets: 16384,
            state: JobState::Queued,
            note: String::new(),
        }
    }

    #[test]
    fn maps_join_the_line_once_named_and_are_built_in_order() {
        let dir = std::env::temp_dir().join(format!("mapart-queue-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut q = Queue::load(&dir);
        q.jobs.clear();
        q.add_pending(vec![job("a", (0, 0), (1, 1))]);
        q.add_pending((0..2).flat_map(|r| (0..2).map(move |c| job("b", (c, r), (2, 2)))).collect());
        assert!(q.waiting().is_empty(), "nothing is in line before it is named");
        assert_eq!(q.pending("b"), Some((2, 2)));
        assert_eq!(q.name("b", "Castle", 10), Ok((1, 4)));
        assert_eq!(q.name("a", "Tree", 20), Ok((5, 5)));
        assert!(q.name("a", "Again", 30).is_err(), "named once");
        let titles: Vec<String> = q.waiting().iter().map(|j| j.title()).collect();
        assert_eq!(titles, vec!["2X2 \"Castle\" c1r1", "2X2 \"Castle\" c2r1", "2X2 \"Castle\" c1r2", "2X2 \"Castle\" c2r2", "1X1 \"Tree\""]);
        assert_eq!(q.next().unwrap().id, "b-0-0");
        assert!(q.cancel(1).unwrap_err().contains("being built"));
        assert_eq!(q.cancel(5).unwrap().id, "a");
        // After a restart the one being built carries on first.
        let mut again = Queue::load(&dir);
        assert_eq!(again.waiting().len(), 4);
        assert_eq!(again.next().unwrap().id, "b-0-0");
        again.finish("b-0-0", JobState::Done, "stored");
        assert_eq!(again.next().unwrap().id, "b-1-0");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_name_fits_on_an_anvil() {
        assert_eq!(name_room((1, 1)), 50 - "1X1 \"\"".len());
        assert_eq!(name_room((2, 2)), 50 - "2X2 \"\" c2r2".len());
        let mut q = Queue { path: std::env::temp_dir().join(format!("mapart-q2-{}.json", std::process::id())), jobs: Vec::new() };
        q.add_pending(vec![job("a", (0, 0), (1, 1))]);
        assert!(q.name("a", &"x".repeat(45), 1).unwrap_err().contains("at most 44"));
        assert!(q.name("a", "", 1).is_err());
        let _ = fs::remove_file(&q.path);
    }
}
