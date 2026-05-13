use anyhow::{anyhow, Context, Result};
use globset::{Glob, GlobSetBuilder};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::TurboJson;

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    /// Short alias (directory name, or unscoped tail of name)
    pub short: String,
    pub path: PathBuf,
    pub scripts: BTreeMap<String, String>,
    pub deps: Vec<String>,
}

#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
    pub turbo: TurboJson,
    pub packages: Vec<Package>,
    /// Detected package manager command: "pnpm" | "npm" | "yarn" | "bun"
    pub pkg_manager: String,
}

#[derive(Deserialize)]
struct RootPackageJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    workspaces: Option<WorkspacesField>,
    #[serde(default, rename = "packageManager")]
    package_manager: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WorkspacesField {
    Plain(Vec<String>),
    Object {
        #[serde(default)]
        packages: Vec<String>,
    },
}

#[derive(Deserialize)]
struct PnpmWorkspace {
    #[serde(default)]
    packages: Vec<String>,
}

#[derive(Deserialize)]
struct PackageJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "peerDependencies")]
    peer_dependencies: BTreeMap<String, String>,
}

impl Workspace {
    pub fn discover(start: &Path) -> Result<Self> {
        let root = find_root(start)?;
        let turbo_path = root.join("turbo.json");
        if !turbo_path.exists() {
            return Err(anyhow!("no turbo.json found at {}", turbo_path.display()));
        }
        let turbo = TurboJson::load(&turbo_path)?;

        let patterns = discover_workspace_patterns(&root)?;
        let pkg_manager = detect_package_manager(&root);
        let mut packages = Vec::new();

        // Root package is its own "package" for root-level tasks.
        if let Ok(root_pkg) = read_package(&root) {
            packages.push(root_pkg);
        }

        let mut builder = GlobSetBuilder::new();
        for p in &patterns {
            // Workspace globs match dirs; ensure trailing /package.json
            let g = Glob::new(p).with_context(|| format!("invalid workspace glob: {p}"))?;
            builder.add(g);
        }
        let globset = builder.build()?;

        for entry in walkdir::WalkDir::new(&root)
            .min_depth(1)
            .max_depth(6)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                name != "node_modules" && !name.starts_with('.')
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            let rel = match entry.path().strip_prefix(&root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if !globset.is_match(rel) {
                continue;
            }
            if entry.path().join("package.json").is_file() {
                if let Ok(pkg) = read_package(entry.path()) {
                    packages.push(pkg);
                }
            }
        }

        Ok(Self {
            root,
            turbo,
            packages,
            pkg_manager,
        })
    }

    pub fn package(&self, name: &str) -> Option<&Package> {
        self.packages
            .iter()
            .find(|p| p.name == name || p.short == name)
    }
}

fn find_root(start: &Path) -> Result<PathBuf> {
    let start = start.canonicalize().with_context(|| "canonicalize start")?;
    let mut cur = start.as_path();
    loop {
        if cur.join("turbo.json").is_file() {
            return Ok(cur.to_path_buf());
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return Err(anyhow!("no turbo.json found from {}", start.display())),
        }
    }
}

fn detect_package_manager(root: &Path) -> String {
    if let Ok(text) = std::fs::read_to_string(root.join("package.json")) {
        if let Ok(pj) = serde_json::from_str::<RootPackageJson>(&text) {
            if let Some(pm) = pj.package_manager.as_deref() {
                let name = pm.split_once('@').map(|(n, _)| n).unwrap_or(pm);
                return name.to_string();
            }
        }
    }
    if root.join("pnpm-lock.yaml").is_file() {
        return "pnpm".into();
    }
    if root.join("yarn.lock").is_file() {
        return "yarn".into();
    }
    if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
        return "bun".into();
    }
    "npm".into()
}

fn discover_workspace_patterns(root: &Path) -> Result<Vec<String>> {
    // pnpm-workspace.yaml takes precedence if present.
    let pnpm = root.join("pnpm-workspace.yaml");
    if pnpm.is_file() {
        let text = std::fs::read_to_string(&pnpm)?;
        let ws: PnpmWorkspace = serde_yaml::from_str(&text)
            .with_context(|| "parse pnpm-workspace.yaml")?;
        return Ok(ws.packages);
    }
    let pkg_path = root.join("package.json");
    if !pkg_path.is_file() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&pkg_path)?;
    let root_pkg: RootPackageJson = serde_json::from_str(&text)
        .with_context(|| "parse root package.json")?;
    let _ = root_pkg.name;
    let _ = root_pkg.package_manager;
    let patterns = match root_pkg.workspaces {
        Some(WorkspacesField::Plain(v)) => v,
        Some(WorkspacesField::Object { packages }) => packages,
        None => Vec::new(),
    };
    Ok(patterns)
}

fn read_package(dir: &Path) -> Result<Package> {
    let pj_path = dir.join("package.json");
    let text = std::fs::read_to_string(&pj_path)
        .with_context(|| format!("read {}", pj_path.display()))?;
    let pj: PackageJson = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", pj_path.display()))?;
    let dir_name = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let name = pj.name.clone().unwrap_or_else(|| dir_name.clone());
    // Short alias: strip @scope/ prefix if present; else fall back to dir name.
    let short = if let Some((_, tail)) = name.split_once('/') {
        tail.to_string()
    } else if name.starts_with('@') {
        dir_name.clone()
    } else {
        name.clone()
    };
    let mut deps = Vec::new();
    for d in pj
        .dependencies
        .keys()
        .chain(pj.dev_dependencies.keys())
        .chain(pj.peer_dependencies.keys())
    {
        deps.push(d.clone());
    }
    Ok(Package {
        name,
        short,
        path: dir.to_path_buf(),
        scripts: pj.scripts,
        deps,
    })
}
