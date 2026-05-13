use anyhow::{anyhow, Result};
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::BTreeMap;

use crate::config::{parse_dep, DepRef, TaskDef};
use crate::workspace::Workspace;

#[derive(Debug, Clone)]
pub struct TaskNode {
    pub package: String,
    pub task: String,
    pub def: TaskDef,
    pub script: Option<String>,
    pub cwd: std::path::PathBuf,
    /// Sibling co-launches (`with`): launch alongside, no edge in DAG.
    pub with: Vec<String>, // resolved "pkg#task" ids
}

impl TaskNode {
    pub fn id(&self) -> String {
        format!("{}#{}", self.package, self.task)
    }
}

pub struct TaskGraph {
    pub graph: DiGraph<TaskNode, ()>,
    pub by_id: BTreeMap<String, NodeIndex>,
}

impl TaskGraph {
    /// Build a graph for the user-requested tasks, expanding bare names across packages.
    /// `requested` is a list of either `task` (every package that has it) or `pkg#task`.
    pub fn build(ws: &Workspace, requested: &[String]) -> Result<Self> {
        let mut graph: DiGraph<TaskNode, ()> = DiGraph::new();
        let mut by_id: BTreeMap<String, NodeIndex> = BTreeMap::new();
        let mut pending: Vec<(String, String)> = Vec::new(); // (pkg, task)

        // Seed pending from request.
        for req in requested {
            if let Some((pkg, task)) = req.split_once('#') {
                // Resolve short alias to canonical pkg name.
                let canon = ws
                    .package(pkg)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| pkg.to_string());
                pending.push((canon, task.to_string()));
            } else {
                let mut matched = false;
                for p in &ws.packages {
                    if p.scripts.contains_key(req) || ws.turbo.task(req).is_some() {
                        // Only add if package can actually run this task (has script) OR
                        // turbo has a config for it and the package has the script.
                        if p.scripts.contains_key(req) {
                            pending.push((p.name.clone(), req.to_string()));
                            matched = true;
                        }
                    }
                }
                if !matched {
                    return Err(anyhow!("no packages have script '{req}'"));
                }
            }
        }

        while let Some((pkg, task)) = pending.pop() {
            let id = format!("{pkg}#{task}");
            if by_id.contains_key(&id) {
                continue;
            }
            let package = ws
                .package(&pkg)
                .ok_or_else(|| anyhow!("unknown package: {pkg}"))?;
            let short_id = format!("{}#{}", package.short, task);
            // Resolve task def: prefer pkg#task entry (canonical or short), fall back to bare task name.
            let def = ws
                .turbo
                .task(&id)
                .or_else(|| ws.turbo.task(&short_id))
                .or_else(|| ws.turbo.task(&task))
                .cloned()
                .unwrap_or_default();
            let script = package.scripts.get(&task).cloned();

            // Resolve `with` siblings — schedule but do not add edge.
            let mut with_ids = Vec::new();
            for w in &def.with {
                let (wp, wt) = match w.split_once('#') {
                    Some((p, t)) => {
                        let canon = ws
                            .package(p)
                            .map(|x| x.name.clone())
                            .unwrap_or_else(|| p.to_string());
                        (canon, t.to_string())
                    }
                    None => (pkg.clone(), w.clone()),
                };
                pending.push((wp.clone(), wt.clone()));
                with_ids.push(format!("{wp}#{wt}"));
            }

            // Add dependsOn (graph edges).
            let mut new_pending: Vec<(String, String)> = Vec::new();
            for dep_raw in &def.depends_on {
                let dep = parse_dep(dep_raw)?;
                match dep {
                    DepRef::Same(t) => new_pending.push((pkg.clone(), t)),
                    DepRef::Explicit { package, task } => new_pending.push((package, task)),
                    DepRef::Topological(t) => {
                        // For every workspace dep of `pkg`, schedule `dep#t` if such pkg exists.
                        for d in &package.deps {
                            if let Some(p) = ws.package(d) {
                                if p.scripts.contains_key(&t) {
                                    new_pending.push((p.name.clone(), t.clone()));
                                }
                            }
                        }
                    }
                }
            }
            // Capture the dep ids before consuming new_pending into pending queue.
            let dep_ids: Vec<String> = new_pending
                .iter()
                .map(|(p, t)| format!("{p}#{t}"))
                .collect();
            for x in new_pending {
                pending.push(x);
            }

            let node = TaskNode {
                package: pkg.clone(),
                task: task.clone(),
                def,
                script,
                cwd: package.path.clone(),
                with: with_ids,
            };
            let idx = graph.add_node(node);
            by_id.insert(id.clone(), idx);

            // Defer wiring edges until all nodes inserted. Store via a placeholder map.
            // We'll do this in a second pass after the loop by walking by_id.
            // For now, attach a hint: re-traverse later.
            // Simpler: insert edges incrementally if both ends exist; otherwise wire in a final pass.
            let _ = dep_ids;
        }

        // Second pass: wire edges from each node's depends_on.
        let ids: Vec<String> = by_id.keys().cloned().collect();
        for id in ids {
            let idx = by_id[&id];
            // Snapshot to drop the borrow before mutating graph.
            let node = graph[idx].clone();
            for dep_raw in &node.def.depends_on {
                let dep = parse_dep(dep_raw)?;
                let dep_targets: Vec<String> = match dep {
                    DepRef::Same(t) => vec![format!("{}#{}", node.package, t)],
                    DepRef::Explicit { package, task } => vec![format!("{package}#{task}")],
                    DepRef::Topological(t) => ws
                        .package(&node.package)
                        .map(|p| {
                            p.deps
                                .iter()
                                .filter_map(|d| {
                                    ws.package(d).and_then(|wp| {
                                        if wp.scripts.contains_key(&t) {
                                            Some(format!("{}#{}", wp.name, t))
                                        } else {
                                            None
                                        }
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                };
                for dep_id in dep_targets {
                    if let Some(&dep_idx) = by_id.get(&dep_id) {
                        graph.add_edge(dep_idx, idx, ());
                    }
                }
            }
        }

        // Persistent-task safety: a non-persistent task may not depend on a persistent one.
        for idx in graph.node_indices() {
            let node = &graph[idx];
            if !node.def.persistent {
                let mut walker = graph
                    .neighbors_directed(idx, petgraph::Direction::Incoming)
                    .detach();
                while let Some(dep_idx) = walker.next_node(&graph) {
                    if graph[dep_idx].def.persistent {
                        return Err(anyhow!(
                            "non-persistent task {} cannot depend on persistent {}",
                            node.id(),
                            graph[dep_idx].id()
                        ));
                    }
                }
            }
        }

        Ok(Self { graph, by_id })
    }

    pub fn persistent_count(&self) -> usize {
        self.graph
            .node_indices()
            .filter(|i| self.graph[*i].def.persistent)
            .count()
    }
}
