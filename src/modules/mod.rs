//! Modules: the standalone's, rebuilt one at a time, reading their settings from the same JSON
//! the Java client keeps (`standalone.json`, copied to `bots/<name>/modules.json` by `import`) -
//! same module names, same setting names, so one config describes a bot on either client.

pub mod autoeat;
pub mod harvest;
pub mod killaura;

use std::{fs, path::Path};

use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct ModuleConfig(Value);

impl ModuleConfig {
    pub fn load(dir: &Path) -> Self {
        let v = fs::read_to_string(dir.join("modules.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        Self(v)
    }

    /// From settings already in memory (the swarm merges its layers into one).
    pub fn from_value(v: Value) -> Self {
        Self(v)
    }

    pub fn enabled(&self, module: &str) -> bool {
        self.0[module]["enabled"].as_bool().unwrap_or(false)
    }

    /// A setting by its standalone name, or the default when absent or unreadable.
    pub fn get<T: DeserializeOwned>(&self, module: &str, setting: &str, default: T) -> T {
        let v = self.0[module]["settings"][setting].clone();
        if let Ok(t) = serde_json::from_value(v.clone()) {
            return t;
        }
        // The Java client writes every number as a double ("port": 6971.0): a whole one is read
        // as an integer too. Without this every integer setting silently fell back to its default
        // (the control panel came up on 6970 instead of its own 6971).
        if let Some(f) = v.as_f64()
            && f.fract() == 0.0
            && let Ok(t) = serde_json::from_value(Value::from(f as i64))
        {
            return t;
        }
        default
    }
}

#[cfg(test)]
mod tests {
    use super::ModuleConfig;

    #[test]
    fn whole_doubles_read_as_integers() {
        let c = ModuleConfig(serde_json::json!({"control-panel": {"enabled": true, "settings": {"port": 6971.0, "scale": 1.5}}}));
        assert_eq!(c.get::<u16>("control-panel", "port", 6970), 6971);
        assert_eq!(c.get::<f64>("control-panel", "scale", 0.0), 1.5);
        // Not whole: the default, never a truncation.
        assert_eq!(c.get::<u32>("control-panel", "scale", 7), 7);
        assert!(c.enabled("control-panel"));
    }
}
