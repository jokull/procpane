use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Minimal turbo.json — only fields we honor or report on.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TurboJson {
    #[serde(default)]
    pub tasks: BTreeMap<String, TaskDef>,
    /// Legacy field name from older turbo.
    #[serde(default)]
    pub pipeline: BTreeMap<String, TaskDef>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskDef {
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub persistent: bool,
    #[serde(default)]
    pub interactive: bool,
    #[serde(default)]
    pub with: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub pass_through_env: Vec<String>,
    // Caching-related fields — parsed and ignored.
    #[serde(default)]
    pub cache: Option<bool>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
}

impl TurboJson {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read {}", path.display()))?;
        let mut v: Self = serde_json::from_str(&text)
            .with_context(|| format!("parse {}", path.display()))?;
        // Merge legacy pipeline into tasks if needed.
        if v.tasks.is_empty() && !v.pipeline.is_empty() {
            v.tasks = std::mem::take(&mut v.pipeline);
        }
        Ok(v)
    }

    pub fn task(&self, name: &str) -> Option<&TaskDef> {
        self.tasks.get(name)
    }
}

/// A dependency reference in `dependsOn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepRef {
    /// `^build` — run `build` of every upstream workspace dep first.
    Topological(String),
    /// `build` — run `build` in the same package.
    Same(String),
    /// `pkg#build` — explicit package + task.
    Explicit { package: String, task: String },
}

pub fn parse_dep(raw: &str) -> Result<DepRef> {
    if let Some(rest) = raw.strip_prefix('^') {
        if rest.is_empty() {
            return Err(anyhow!("invalid dep ref: {raw}"));
        }
        Ok(DepRef::Topological(rest.to_string()))
    } else if let Some((pkg, task)) = raw.split_once('#') {
        if pkg.is_empty() || task.is_empty() {
            return Err(anyhow!("invalid dep ref: {raw}"));
        }
        Ok(DepRef::Explicit {
            package: pkg.to_string(),
            task: task.to_string(),
        })
    } else {
        Ok(DepRef::Same(raw.to_string()))
    }
}
