//! The Discord webhook: one line per post, posted in order off the game loop. Port of the Java
//! AiDiscord: never coordinates (NoCoords), never pings anyone, the same line not twice in an hour,
//! and a 429 is waited out rather than dropped.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use regex::Regex;
use std::sync::LazyLock;
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Clone)]
pub struct Discord {
    tx: Option<mpsc::UnboundedSender<String>>,
    pending: Arc<AtomicUsize>,
    recent: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Discord {
    /// No url: every send is a no-op (still logged).
    pub fn start(url: &str, name: &str) -> Self {
        let pending = Arc::new(AtomicUsize::new(0));
        let recent = Arc::new(Mutex::new(HashMap::new()));
        if url.is_empty() {
            return Self { tx: None, pending, recent };
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let url = url.to_string();
        let name = name.to_string();
        let left = pending.clone();
        tokio::spawn(async move {
            let http = reqwest::Client::new();
            while let Some(text) = rx.recv().await {
                for _attempt in 0..5 {
                    let mut body = serde_json::json!({ "content": text, "allowed_mentions": { "parse": [] } });
                    if !name.is_empty() {
                        body["username"] = name.clone().into();
                    }
                    match http.post(&url).json(&body).send().await {
                        Ok(r) if r.status().as_u16() == 429 => {
                            let wait = r
                                .json::<serde_json::Value>()
                                .await
                                .ok()
                                .and_then(|v| v["retry_after"].as_f64())
                                .unwrap_or(2.0);
                            tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.5, 60.0))).await;
                        }
                        Ok(r) if !r.status().is_success() => {
                            warn!("discord: post failed with {}", r.status());
                            break;
                        }
                        Ok(_) => break,
                        Err(e) => {
                            warn!("discord: post failed: {e}");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                }
                left.fetch_sub(1, Ordering::SeqCst);
            }
        });
        Self { tx: Some(tx), pending, recent }
    }

    /// Queue one line. The same text again within the hour is not news (the Java side spammed a
    /// channel for six hours on 10 Oct without this).
    pub fn send(&self, text: &str) {
        let text = scrub(text.trim());
        if text.is_empty() {
            return;
        }
        info!("discord: {text}");
        {
            let mut recent = self.recent.lock();
            let now = Instant::now();
            recent.retain(|_, at| now.duration_since(*at) < Duration::from_secs(3600));
            if recent.contains_key(&text) {
                return;
            }
            recent.insert(text.clone(), now);
        }
        if let Some(tx) = &self.tx {
            self.pending.fetch_add(1, Ordering::SeqCst);
            if tx.send(text).is_err() {
                self.pending.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// Wait (up to `max`) for queued posts to go out - before the process exits.
    pub async fn flush(&self, max: Duration) {
        let until = Instant::now() + max;
        while self.pending.load(Ordering::SeqCst) > 0 && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

pub const HIDDEN: &str = "[hidden]";

/// A count for a Discord line in a form the scrubber keeps: 666, 1.9k, 395.2k, 1.13M. scrub()
/// hides every 4+ digit number, so the hourly summary read "[hidden] melons mined" (16 Oct).
pub fn count(n: i64) -> String {
    let a = n.unsigned_abs() as f64;
    let sign = if n < 0 { "-" } else { "" };
    if a < 1000.0 {
        n.to_string()
    } else if a < 999_950.0 {
        // Under the point where {:.1} would round up to "1000.0k" - four digits again.
        format!("{sign}{:.1}k", a / 1000.0)
    } else {
        format!("{sign}{:.2}M", a / 1_000_000.0)
    }
}

static GROUP: LazyLock<Regex> = LazyLock::new(|| {
    let num = r"-?\d+(?:\.\d+)?";
    let sep = r"(?:\s*[,/]\s*|\s+(?:(?:and|to|by)\s+)?)";
    Regex::new(&format!("{num}{sep}{num}(?:{sep}{num})?")).unwrap()
});
static LABELLED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b[xyz]\s*[=:]?\s*-?\d{3,}(?:\.\d+)?").unwrap());
static BIG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-?\d{4,}(?:\.\d+)?").unwrap());
static THREE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{3,}").unwrap());
static FRACTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.\d+").unwrap());

/// Port of NoCoords.scrub: number groups with any 3+ digit part, labelled x/y/z values and any 4+
/// digit number become "[hidden]". "health 3/20" stays.
pub fn scrub(s: &str) -> String {
    let grouped = GROUP.replace_all(s, |c: &regex::Captures| {
        let whole = &c[0];
        if THREE.is_match(&FRACTION.replace_all(whole, "")) { HIDDEN.to_string() } else { whole.to_string() }
    });
    let labelled = LABELLED.replace_all(&grouped, HIDDEN);
    BIG.replace_all(&labelled, HIDDEN).into_owned()
}

#[cfg(test)]
mod tests {
    use super::scrub;

    #[test]
    fn hides_coordinates_keeps_small_numbers() {
        assert_eq!(scrub("died at 1230 63 -3456"), "died at [hidden]");
        assert_eq!(scrub("hunger 3/20, 2 totems"), "hunger 3/20, 2 totems");
        assert_eq!(scrub("x: 1230"), "[hidden]");
        assert_eq!(scrub("mined 1859 melons"), "mined [hidden] melons");
    }

    #[test]
    fn counts_survive_the_scrub() {
        use super::count;
        assert_eq!(count(666), "666");
        assert_eq!(count(1859), "1.9k");
        assert_eq!(count(395_157), "395.2k");
        assert_eq!(count(999_960), "1.00M");
        assert_eq!(count(1_132_740), "1.13M");
        let line = format!(
            "hourly: {} melons mined, {} placed this hour; builds done 0, floor pieces 0, melons mined {}, placed {}",
            count(7_200),
            count(666),
            count(387_954),
            count(395_157)
        );
        assert_eq!(scrub(&line), line);
    }
}
