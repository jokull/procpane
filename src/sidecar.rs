use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// procpane.toml — the sidecar that holds everything turbo.json doesn't.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskOverlay>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOverlay {
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub depends_on: BTreeMap<String, DependsOnCondition>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub stop_signal: Option<StopSignal>,
    #[serde(default, with = "humantime_opt")]
    pub stop_grace_period: Option<Duration>,
    #[serde(default)]
    pub env_from: Vec<String>,
}

/// Healthcheck — at most one kind per task in this MVP.
/// (Compose allows lists; we keep it flat for clarity.)
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    #[serde(default)]
    pub tcp: Option<u16>,
    #[serde(default)]
    pub http: Option<String>,
    #[serde(default)]
    pub log: Option<String>,
    #[serde(default)]
    pub exit: Option<i32>,
    /// Seconds between probes (default 1s)
    #[serde(default, with = "humantime_opt")]
    pub interval: Option<Duration>,
    /// Per-probe timeout (default 2s)
    #[serde(default, with = "humantime_opt")]
    pub timeout: Option<Duration>,
    /// Grace period before first probe runs (default 0)
    #[serde(default, with = "humantime_opt")]
    pub start_period: Option<Duration>,
}

impl Healthcheck {
    pub fn interval(&self) -> Duration {
        self.interval.unwrap_or(Duration::from_secs(1))
    }
    pub fn timeout(&self) -> Duration {
        self.timeout.unwrap_or(Duration::from_secs(2))
    }
    pub fn start_period(&self) -> Duration {
        self.start_period.unwrap_or(Duration::from_millis(0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DependsOnCondition {
    /// Process has been spawned.
    Started,
    /// Healthcheck has reported healthy.
    Healthy,
    /// One-shot task ran to completion successfully.
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum StopSignal {
    #[serde(rename = "SIGINT")]
    Int,
    #[serde(rename = "SIGTERM")]
    Term,
    #[serde(rename = "SIGHUP")]
    Hup,
    #[serde(rename = "SIGQUIT")]
    Quit,
}

impl StopSignal {
    pub fn as_libc(&self) -> i32 {
        match self {
            StopSignal::Int => libc::SIGINT,
            StopSignal::Term => libc::SIGTERM,
            StopSignal::Hup => libc::SIGHUP,
            StopSignal::Quit => libc::SIGQUIT,
        }
    }
}

mod humantime_opt {
    use serde::{de::Error, Deserialize, Deserializer};
    use std::time::Duration;

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<Duration>, D::Error> {
        let opt: Option<String> = Option::deserialize(de)?;
        match opt {
            None => Ok(None),
            Some(s) => humantime::parse_duration(&s)
                .map(Some)
                .map_err(D::Error::custom),
        }
    }
}

impl Sidecar {
    /// Load `procpane.toml` (required) and merge `procpane.local.toml` (optional).
    /// Missing root file → empty sidecar; missing local file → silently skipped.
    pub fn load(root: &Path) -> Result<Self> {
        let main_path = root.join("procpane.toml");
        let local_path = root.join("procpane.local.toml");

        let mut sc = if main_path.is_file() {
            let text = std::fs::read_to_string(&main_path)
                .with_context(|| format!("read {}", main_path.display()))?;
            toml::from_str::<Sidecar>(&text)
                .with_context(|| format!("parse {}", main_path.display()))?
        } else {
            Sidecar::default()
        };

        if local_path.is_file() {
            let text = std::fs::read_to_string(&local_path)
                .with_context(|| format!("read {}", local_path.display()))?;
            let local: Sidecar = toml::from_str(&text)
                .with_context(|| format!("parse {}", local_path.display()))?;
            sc.merge(local);
        }

        Ok(sc)
    }

    fn merge(&mut self, other: Sidecar) {
        for (id, overlay) in other.tasks {
            self.tasks
                .entry(id)
                .and_modify(|cur| cur.merge(&overlay))
                .or_insert(overlay);
        }
    }

    /// Look up overlay by task id. Tries canonical `pkg#task` first,
    /// then short `shortpkg#task` if provided.
    pub fn overlay(&self, canonical: &str, short: Option<&str>) -> Option<&TaskOverlay> {
        if let Some(o) = self.tasks.get(canonical) {
            return Some(o);
        }
        if let Some(s) = short {
            return self.tasks.get(s);
        }
        None
    }
}

impl TaskOverlay {
    fn merge(&mut self, other: &TaskOverlay) {
        if other.hostname.is_some() {
            self.hostname = other.hostname.clone();
        }
        if other.healthcheck.is_some() {
            self.healthcheck = other.healthcheck.clone();
        }
        for (k, v) in &other.depends_on {
            self.depends_on.insert(k.clone(), *v);
        }
        if !other.profiles.is_empty() {
            self.profiles = other.profiles.clone();
        }
        if other.stop_signal.is_some() {
            self.stop_signal = other.stop_signal;
        }
        if other.stop_grace_period.is_some() {
            self.stop_grace_period = other.stop_grace_period;
        }
        if !other.env_from.is_empty() {
            self.env_from = other.env_from.clone();
        }
    }

    pub fn stop_signal(&self) -> i32 {
        self.stop_signal.map(|s| s.as_libc()).unwrap_or(libc::SIGINT)
    }

    pub fn stop_grace(&self) -> Duration {
        self.stop_grace_period.unwrap_or(Duration::from_secs(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_overlay() {
        let toml_text = r#"
[tasks."web#dev"]
hostname = "web.test"
profiles = ["minimal"]
stop_signal = "SIGTERM"
stop_grace_period = "30s"
env_from = ["STRIPE_KEY"]

[tasks."web#dev".healthcheck]
log = "ready in"
interval = "500ms"

[tasks."web#dev".depends_on]
api = "healthy"
migrate = "completed"
"#;
        let sc: Sidecar = toml::from_str(toml_text).unwrap();
        let o = sc.tasks.get("web#dev").unwrap();
        assert_eq!(o.hostname.as_deref(), Some("web.test"));
        assert_eq!(o.profiles, vec!["minimal"]);
        assert_eq!(o.stop_signal, Some(StopSignal::Term));
        assert_eq!(o.stop_grace_period, Some(Duration::from_secs(30)));
        assert_eq!(o.env_from, vec!["STRIPE_KEY"]);
        let hc = o.healthcheck.as_ref().unwrap();
        assert_eq!(hc.log.as_deref(), Some("ready in"));
        assert_eq!(hc.interval, Some(Duration::from_millis(500)));
        assert_eq!(
            o.depends_on.get("api").copied(),
            Some(DependsOnCondition::Healthy)
        );
        assert_eq!(
            o.depends_on.get("migrate").copied(),
            Some(DependsOnCondition::Completed)
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let bad = r#"
[tasks."web#dev"]
bogus = true
"#;
        let r: Result<Sidecar, _> = toml::from_str(bad);
        assert!(r.is_err());
    }
}
