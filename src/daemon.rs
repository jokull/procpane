use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use petgraph::graph::NodeIndex;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::buffer::{self, SharedBuffer};
use crate::graph::TaskGraph;
use crate::healthcheck::{run_healthcheck_loop, HealthcheckKind};
use crate::process::{Proc, ProcState};
use crate::proto::{GrepMatch, LineRecord, ProcStatus, Request, Response};
use crate::proxy::{self, PortRegistry, PROXY_PORT};
use crate::secrets;
use crate::sidecar::DependsOnCondition;
use crate::{ca, workspace::Workspace};

pub const PREBUILD_ID: &str = "procpane#prebuild";

pub struct Daemon {
    pub state_dir: PathBuf,
    pub socket_path: PathBuf,
    pub procs: BTreeMap<String, Arc<Proc>>,
    pub buffers: BTreeMap<String, SharedBuffer>,
    pub stop_tx: Arc<Mutex<Option<tokio::sync::watch::Sender<bool>>>>,
    pub started_at: Instant,
    /// Per-task hostname declared in `procpane.toml` (for status output).
    pub hostnames: BTreeMap<String, String>,
    /// Per-task allocated TCP port (when hostname is set; PORT env injected).
    pub allocated_ports: BTreeMap<String, u16>,
    /// Per-task shutdown signal (libc::SIG*). Default SIGINT.
    pub stop_signals: BTreeMap<String, i32>,
    /// Per-task grace period before SIGKILL. Default 5s.
    pub stop_grace: BTreeMap<String, Duration>,
}

impl Daemon {
    pub async fn run(
        ws: Workspace,
        requested: Vec<String>,
        state_dir: PathBuf,
        no_prebuild: bool,
    ) -> Result<()> {
        std::fs::create_dir_all(&state_dir)?;
        let socket_path = state_dir.join("sock");
        // Remove stale socket if present.
        let _ = std::fs::remove_file(&socket_path);

        let graph = TaskGraph::build(&ws, &requested)?;
        if graph.graph.node_count() == 0 {
            return Err(anyhow!("no tasks resolved"));
        }

        // Pre-flight: every secret in env_from must be present in Keychain.
        let service = secrets::service_name(&ws.root);
        let mut missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for idx in graph.graph.node_indices() {
            let n = &graph.graph[idx];
            if !n.def.persistent {
                continue;
            }
            for key in &n.overlay.env_from {
                match secrets::get(&service, key) {
                    Ok(Some(_)) => {}
                    Ok(None) => missing.entry(n.id()).or_default().push(key.clone()),
                    Err(e) => {
                        return Err(anyhow!("keychain check failed for {}: {e}", n.id()));
                    }
                }
            }
        }
        if !missing.is_empty() {
            let mut all_keys: Vec<String> = missing
                .values()
                .flat_map(|v| v.iter().cloned())
                .collect();
            all_keys.sort();
            all_keys.dedup();
            eprintln!(
                "✗ Missing {} required env vars: {}",
                all_keys.len(),
                all_keys.join(", ")
            );
            eprintln!("  Set them with:  procpane env set <KEY>");
            eprintln!("  Or receive from a teammate:  procpane env receive <code>");
            return Err(anyhow!("missing required secrets"));
        }

        // Partition graph nodes: persistent (procpane runs) vs non-persistent (turbo prebuild).
        let mut prebuild_ids: Vec<String> = Vec::new();
        let mut persistent_indices: Vec<NodeIndex> = Vec::new();
        for idx in graph.graph.node_indices() {
            let n = &graph.graph[idx];
            if n.def.persistent {
                persistent_indices.push(idx);
            } else if n.script.is_some() {
                prebuild_ids.push(n.id());
            }
        }
        if persistent_indices.is_empty() {
            return Err(anyhow!(
                "no persistent tasks to run; for non-persistent tasks use `turbo run` directly"
            ));
        }

        // One Proc per persistent node, plus optional turbo-prebuild proc.
        let mut procs: BTreeMap<String, Arc<Proc>> = BTreeMap::new();
        let mut buffers: BTreeMap<String, SharedBuffer> = BTreeMap::new();
        let mut node_to_id: BTreeMap<NodeIndex, String> = BTreeMap::new();
        for idx in &persistent_indices {
            let n = &graph.graph[*idx];
            let id = n.id();
            let buf = buffer::new_shared(buffer::DEFAULT_CAPACITY);
            let proc = Proc::new(id.clone(), buf.clone(), n.def.persistent);
            buffers.insert(id.clone(), buf);
            procs.insert(id.clone(), proc);
            node_to_id.insert(*idx, id);
        }
        let do_prebuild = !no_prebuild && !prebuild_ids.is_empty();
        if do_prebuild {
            let buf = buffer::new_shared(buffer::DEFAULT_CAPACITY);
            let proc = Proc::new(PREBUILD_ID.to_string(), buf.clone(), false);
            buffers.insert(PREBUILD_ID.to_string(), buf);
            procs.insert(PREBUILD_ID.to_string(), proc);
        }

        // Collect per-task overlay-derived metadata for daemon-side use.
        let mut hostnames: BTreeMap<String, String> = BTreeMap::new();
        let mut stop_signals: BTreeMap<String, i32> = BTreeMap::new();
        let mut stop_grace: BTreeMap<String, Duration> = BTreeMap::new();
        // Pre-allocate a port per hostnamed task. Tasks read PORT from env at
        // spawn time; proxy routes by SNI.
        let mut allocated_ports: BTreeMap<String, u16> = BTreeMap::new();
        for idx in &persistent_indices {
            let n = &graph.graph[*idx];
            let id = n.id();
            if let Some(h) = &n.overlay.hostname {
                hostnames.insert(id.clone(), h.clone());
                let port = proxy::allocate_port()?;
                allocated_ports.insert(id.clone(), port);
            }
            stop_signals.insert(id.clone(), n.overlay.stop_signal());
            stop_grace.insert(id, n.overlay.stop_grace());
        }

        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let daemon = Arc::new(Daemon {
            state_dir: state_dir.clone(),
            socket_path: socket_path.clone(),
            procs: procs.clone(),
            buffers: buffers.clone(),
            stop_tx: Arc::new(Mutex::new(Some(stop_tx.clone()))),
            started_at: Instant::now(),
            hostnames: hostnames.clone(),
            allocated_ports: allocated_ports.clone(),
            stop_signals,
            stop_grace,
        });

        // Start the TLS reverse proxy if any task declared a hostname AND the
        // local CA is installed. Without the CA, warn but keep going (the user
        // may want to inspect status / logs without HTTPS).
        let port_registry = PortRegistry::new();
        if !hostnames.is_empty() {
            if ca::is_installed() {
                let host_list: Vec<String> = hostnames.values().cloned().collect();
                match proxy::build_tls_config(&host_list) {
                    Ok(tls_cfg) => {
                        let bind: std::net::SocketAddr =
                            ([127, 0, 0, 1], PROXY_PORT).into();
                        let reg = Arc::clone(&port_registry);
                        let mut prx_stop = stop_rx.clone();
                        // Pre-register hostnames → allocated backend ports so
                        // the proxy can route even before tasks turn healthy
                        // (returns 503 until backend accepts).
                        for (id, host) in &hostnames {
                            if let Some(p) = allocated_ports.get(id) {
                                reg.register(host, *p);
                            }
                        }
                        tokio::spawn(async move {
                            if let Err(e) = proxy::run_proxy(tls_cfg, reg, bind, prx_stop.clone()).await {
                                tracing::error!(?e, "reverse proxy stopped");
                            }
                            let _ = prx_stop.changed().await;
                        });
                    }
                    Err(e) => {
                        eprintln!("procpane: skipping HTTPS proxy ({e})");
                    }
                }
            } else {
                eprintln!(
                    "procpane: tasks declare hostnames but the local CA is not installed."
                );
                eprintln!("  Run `procpane trust install` to enable https://*.test URLs.");
            }
        }
        let _ = port_registry; // silence unused if no hostnames

        // Install signal handler so Ctrl-C or SIGTERM triggers shutdown.
        {
            let stop_tx = stop_tx.clone();
            ctrlc::set_handler(move || {
                let _ = stop_tx.send(true);
            })
            .ok();
        }

        // Spawn scheduler task.
        let sched_daemon = Arc::clone(&daemon);
        let graph_arc = Arc::new(graph);
        let mut sched_stop = stop_rx.clone();
        let pm = ws.pkg_manager.clone();
        let root = ws.root.clone();
        let persistent_set: HashSet<NodeIndex> = persistent_indices.into_iter().collect();
        let sched_stop_rx = stop_rx.clone();
        let scheduler = tokio::spawn(async move {
            run_scheduler(
                sched_daemon,
                graph_arc,
                persistent_set,
                do_prebuild,
                prebuild_ids,
                pm,
                root,
                &mut sched_stop,
                sched_stop_rx,
            )
            .await;
        });

        // Listen on Unix socket.
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;

        eprintln!(
            "procpane daemon listening at {} (pid {})",
            socket_path.display(),
            std::process::id()
        );

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        break;
                    }
                }
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _addr)) => {
                            let d = Arc::clone(&daemon);
                            tokio::spawn(async move {
                                if let Err(e) = handle_client(d, stream).await {
                                    tracing::warn!(?e, "client error");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(?e, "accept failed");
                            break;
                        }
                    }
                }
            }
        }

        // Shutdown all procs, honoring per-task stop signal & grace.
        eprintln!("procpane shutting down…");
        for (id, p) in &daemon.procs {
            let signal = daemon
                .stop_signals
                .get(id)
                .copied()
                .unwrap_or(libc::SIGINT);
            let grace = daemon
                .stop_grace
                .get(id)
                .copied()
                .unwrap_or(Duration::from_secs(5));
            p.stop_with_signal(signal, grace);
        }
        scheduler.abort();
        let _ = std::fs::remove_file(&socket_path);
        Ok(())
    }
}

async fn run_scheduler(
    daemon: Arc<Daemon>,
    graph: Arc<TaskGraph>,
    persistent_set: HashSet<NodeIndex>,
    do_prebuild: bool,
    prebuild_ids: Vec<String>,
    pkg_manager: String,
    root: std::path::PathBuf,
    stop_rx: &mut tokio::sync::watch::Receiver<bool>,
    health_stop_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut spawned: HashSet<NodeIndex> = HashSet::new();

    // Default depends_on condition for an edge: a persistent dep must be
    // `healthy`; a non-persistent dep must be `completed`.
    let default_condition = |dep_idx: NodeIndex| -> DependsOnCondition {
        let dep_node = &graph.graph[dep_idx];
        if dep_node.def.persistent {
            DependsOnCondition::Healthy
        } else {
            DependsOnCondition::Completed
        }
    };

    // Does this graph edge consider `dep_idx`'s current state satisfactory?
    let edge_satisfied = |dep_idx: NodeIndex, condition: DependsOnCondition| -> bool {
        let dep_id = graph.graph[dep_idx].id();
        let st = daemon
            .procs
            .get(&dep_id)
            .map(|p| *p.state.lock())
            .unwrap_or(ProcState::Pending);
        match condition {
            DependsOnCondition::Started => {
                // Anything beyond Pending.
                !matches!(st, ProcState::Pending)
            }
            DependsOnCondition::Healthy => matches!(st, ProcState::Healthy | ProcState::Completed),
            DependsOnCondition::Completed => matches!(st, ProcState::Completed),
        }
    };

    // Pre-mark non-persistent nodes as spawned when prebuild owns them; turbo
    // prebuild satisfies their downstream effects in one shot.
    if do_prebuild {
        for idx in graph.graph.node_indices() {
            if !persistent_set.contains(&idx) {
                spawned.insert(idx);
            }
        }
    }

    // Turbo prebuild phase.
    if do_prebuild {
        if let Some(proc) = daemon.procs.get(PREBUILD_ID).cloned() {
            let mut shell = format!("{pkg_manager} exec turbo run");
            for id in &prebuild_ids {
                shell.push(' ');
                shell.push_str(id);
            }
            let env: Vec<(String, String)> = Vec::new();
            if let Err(e) = proc.spawn(&shell, &root, &env) {
                tracing::error!(?e, "prebuild spawn failed");
                *proc.state.lock() = ProcState::Crashed;
                if let Some(tx) = daemon.stop_tx.lock().as_ref() {
                    let _ = tx.send(true);
                }
                return;
            }
            loop {
                if *stop_rx.borrow() {
                    return;
                }
                let st = *proc.state.lock();
                if st.is_terminal() {
                    if matches!(st, ProcState::Crashed | ProcState::Killed) {
                        eprintln!("procpane: turbo prebuild failed; aborting run");
                        if let Some(tx) = daemon.stop_tx.lock().as_ref() {
                            let _ = tx.send(true);
                        }
                        return;
                    }
                    // Synthetic non-persistent nodes for prebuild-handled tasks:
                    // mark their procs Completed so downstream gates clear.
                    for idx in graph.graph.node_indices() {
                        if !persistent_set.contains(&idx) {
                            let id = graph.graph[idx].id();
                            if let Some(p) = daemon.procs.get(&id) {
                                let mut st = p.state.lock();
                                if matches!(*st, ProcState::Pending) {
                                    *st = ProcState::Completed;
                                }
                            }
                        }
                    }
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                    _ = stop_rx.changed() => { if *stop_rx.borrow() { return; } }
                }
            }
        }
    }

    loop {
        if *stop_rx.borrow() {
            return;
        }

        // Find nodes whose deps are all satisfied (per-edge condition).
        let mut to_spawn: Vec<NodeIndex> = Vec::new();
        for idx in graph.graph.node_indices() {
            if spawned.contains(&idx) {
                continue;
            }
            let node = &graph.graph[idx];
            let mut ready = true;
            for dep in graph
                .graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
            {
                let dep_id = graph.graph[dep].id();
                // Look up per-task override from the dependent's overlay, by both
                // canonical id and short id (overlay map keys can use either).
                let cond = node
                    .overlay
                    .depends_on
                    .get(&dep_id)
                    .or_else(|| {
                        // Strip "@scope/" prefix on the dep package, if any.
                        graph.graph[dep]
                            .package
                            .split_once('/')
                            .and_then(|(_, tail)| {
                                let short_id = format!("{}#{}", tail, graph.graph[dep].task);
                                node.overlay.depends_on.get(&short_id)
                            })
                    })
                    .copied()
                    .unwrap_or_else(|| default_condition(dep));
                if !edge_satisfied(dep, cond) {
                    ready = false;
                    break;
                }
            }
            if ready {
                to_spawn.push(idx);
            }
        }

        for idx in &to_spawn {
            let node = &graph.graph[*idx];
            let id = node.id();
            let proc = match daemon.procs.get(&id) {
                Some(p) => p.clone(),
                None => continue,
            };
            // Determine shell command. If no script, skip (treat as no-op completed).
            let shell_cmd = match &node.script {
                Some(s) => s.clone(),
                None => {
                    *proc.state.lock() = ProcState::Completed;
                    spawned.insert(*idx);
                    continue;
                }
            };
            // Prepend node_modules/.bin to PATH walking from cwd up to root.
            let mut bin_paths: Vec<std::path::PathBuf> = Vec::new();
            let mut cur = Some(node.cwd.as_path());
            while let Some(d) = cur {
                let nb = d.join("node_modules").join(".bin");
                if nb.is_dir() {
                    bin_paths.push(nb);
                }
                cur = d.parent();
            }
            let cur_path = std::env::var("PATH").unwrap_or_default();
            let mut path = String::new();
            for p in &bin_paths {
                if !path.is_empty() {
                    path.push(':');
                }
                path.push_str(&p.to_string_lossy());
            }
            if !cur_path.is_empty() {
                if !path.is_empty() {
                    path.push(':');
                }
                path.push_str(&cur_path);
            }
            let mut env: Vec<(String, String)> = vec![("PATH".into(), path)];

            // If this task has a hostname, hand it the allocated port via PORT
            // and a public URL via the canonical-cased hostname env var.
            if let Some(host) = daemon.hostnames.get(&id) {
                if let Some(port) = daemon.allocated_ports.get(&id) {
                    env.push(("PORT".into(), port.to_string()));
                    env.push((
                        "PROCPANE_PUBLIC_URL".into(),
                        format!("https://{host}:{}", PROXY_PORT),
                    ));
                }
            }
            // Inject every *other* task's public URL too, so apps that talk to
            // siblings can resolve without hard-coding ports.
            for (other_id, other_host) in &daemon.hostnames {
                if other_id == &id {
                    continue;
                }
                // Build an env var name from the short task name:
                //   "@demo/api#dev" → API_URL
                //   "api#dev"       → API_URL
                let pkg_short = other_id
                    .split_once('#')
                    .map(|(p, _)| p)
                    .unwrap_or(other_id)
                    .rsplit('/')
                    .next()
                    .unwrap_or(other_id)
                    .to_uppercase()
                    .replace(['-', '.'], "_");
                let var_name = format!("{pkg_short}_URL");
                env.push((
                    var_name,
                    format!("https://{other_host}:{}", PROXY_PORT),
                ));
            }

            // Inject env_from secrets from Keychain. Pre-flight verified
            // presence; if a value disappeared between then and now we warn
            // and let the task start without it (rare race).
            let service = secrets::service_name(&root);
            for key in &node.overlay.env_from {
                match secrets::get(&service, key) {
                    Ok(Some(val)) => env.push((key.clone(), val)),
                    Ok(None) => tracing::warn!(task = %id, key, "env_from secret vanished after pre-flight"),
                    Err(e) => tracing::warn!(task = %id, key, error = ?e, "env_from fetch failed"),
                }
            }

            let cwd = node.cwd.clone();
            if let Err(e) = proc.spawn(&shell_cmd, &cwd, &env) {
                tracing::error!(?e, "failed to spawn {id}");
                *proc.state.lock() = ProcState::Crashed;
                spawned.insert(*idx);
                continue;
            }
            spawned.insert(*idx);

            // Kick off the healthcheck loop for this task.
            let hc_cfg = node.overlay.healthcheck.clone();
            let hostname = node.overlay.hostname.clone();
            let buffer = match daemon.buffers.get(&id) {
                Some(b) => b.clone(),
                None => continue,
            };
            let kind = match &hc_cfg {
                Some(hc) => match HealthcheckKind::from_sidecar(hc, hostname.as_deref()) {
                    Ok(k) => k,
                    Err(e) => {
                        tracing::warn!(task = %id, error = ?e, "invalid healthcheck; treating as none");
                        HealthcheckKind::None
                    }
                },
                None => HealthcheckKind::None,
            };
            let interval = hc_cfg.as_ref().map(|h| h.interval()).unwrap_or(Duration::from_secs(1));
            let probe_t = hc_cfg.as_ref().map(|h| h.timeout()).unwrap_or(Duration::from_secs(2));
            let start_p = hc_cfg.as_ref().map(|h| h.start_period()).unwrap_or(Duration::ZERO);
            let proc_for_hc = Arc::clone(&proc);
            let hc_stop = health_stop_rx.clone();
            tokio::spawn(async move {
                run_healthcheck_loop(
                    proc_for_hc,
                    buffer,
                    kind,
                    interval,
                    probe_t,
                    start_p,
                    hc_stop,
                )
                .await;
            });
        }

        // Exit condition: every non-persistent task has reached terminal state
        // AND no persistent task is alive (either all terminal, or none was
        // requested).
        let mut any_persistent_alive = false;
        let mut all_non_persistent_done = true;
        for idx in graph.graph.node_indices() {
            let node = &graph.graph[idx];
            let id = node.id();
            let state = daemon
                .procs
                .get(&id)
                .map(|p| *p.state.lock())
                .unwrap_or(ProcState::Pending);
            if node.def.persistent {
                if !state.is_terminal() {
                    any_persistent_alive = true;
                }
            } else if !state.is_terminal() {
                all_non_persistent_done = false;
            }
        }

        if all_non_persistent_done && !any_persistent_alive {
            if let Some(tx) = daemon.stop_tx.lock().as_ref() {
                let _ = tx.send(true);
            }
            return;
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() { return; }
            }
        }
    }
}

async fn handle_client(daemon: Arc<Daemon>, stream: UnixStream) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }
    let req: Request = serde_json::from_str(line.trim()).context("parse request")?;
    let resp = dispatch(&daemon, req);
    let mut out = serde_json::to_vec(&resp)?;
    out.push(b'\n');
    wr.write_all(&out).await?;
    wr.flush().await?;
    Ok(())
}

fn dispatch(daemon: &Daemon, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,
        Request::Stop => {
            if let Some(tx) = daemon.stop_tx.lock().as_ref() {
                let _ = tx.send(true);
            }
            Response::Ok
        }
        Request::Status => {
            let procs = daemon
                .procs
                .iter()
                .map(|(id, p)| build_proc_status(daemon, id, p))
                .collect();
            Response::Status { procs }
        }
        Request::GetTask { name } => match daemon.procs.get(&name) {
            Some(p) => Response::Task {
                task: build_proc_status(daemon, &name, p),
            },
            None => Response::Error {
                message: format!("unknown task: {name}"),
            },
        },
        Request::Tail { name, lines } => match daemon.buffers.get(&name) {
            Some(buf) => {
                let recs: Vec<LineRecord> = buf.lock().tail(lines);
                let next = buf.lock().line_count();
                Response::Lines {
                    lines: recs,
                    next_cursor: next,
                }
            }
            None => Response::Error {
                message: format!("unknown task: {name}"),
            },
        },
        Request::Since { name, cursor } => match daemon.buffers.get(&name) {
            Some(buf) => {
                let (recs, next) = buf.lock().since(cursor);
                Response::Lines {
                    lines: recs,
                    next_cursor: next,
                }
            }
            None => Response::Error {
                message: format!("unknown task: {name}"),
            },
        },
        Request::Grep {
            name,
            pattern,
            before,
            after,
        } => {
            let re = match regex::Regex::new(&pattern) {
                Ok(r) => r,
                Err(e) => {
                    return Response::Error {
                        message: format!("bad regex: {e}"),
                    }
                }
            };
            let mut matches: Vec<GrepMatch> = Vec::new();
            if let Some(name) = name {
                if let Some(buf) = daemon.buffers.get(&name) {
                    matches.extend(buf.lock().grep(&name, &re, before, after));
                } else {
                    return Response::Error {
                        message: format!("unknown task: {name}"),
                    };
                }
            } else {
                for (id, buf) in &daemon.buffers {
                    matches.extend(buf.lock().grep(id, &re, before, after));
                }
            }
            Response::GrepMatches { matches }
        }
    }
}

pub fn state_dir(root: &Path) -> PathBuf {
    root.join(".procpane")
}

fn build_proc_status(daemon: &Daemon, id: &str, p: &Proc) -> ProcStatus {
    let now = Instant::now();
    let started = *p.started_at.lock();
    let age = started.map(|t| now.duration_since(t).as_secs()).unwrap_or(0);
    let line_count = p.buffer.lock().line_count();
    ProcStatus {
        name: id.to_string(),
        state: p.state.lock().as_str().to_string(),
        pid: *p.pid.lock(),
        age_secs: age,
        line_count,
        exit_code: *p.exit_code.lock(),
        persistent: p.persistent,
        hostname: daemon.hostnames.get(id).cloned(),
    }
}
