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
use crate::process::{Proc, ProcState};
use crate::proto::{GrepMatch, LineRecord, ProcStatus, Request, Response};
use crate::workspace::Workspace;

pub struct Daemon {
    pub state_dir: PathBuf,
    pub socket_path: PathBuf,
    pub procs: BTreeMap<String, Arc<Proc>>,
    pub buffers: BTreeMap<String, SharedBuffer>,
    pub stop_tx: Arc<Mutex<Option<tokio::sync::watch::Sender<bool>>>>,
    pub started_at: Instant,
}

impl Daemon {
    pub async fn run(ws: Workspace, requested: Vec<String>, state_dir: PathBuf) -> Result<()> {
        std::fs::create_dir_all(&state_dir)?;
        let socket_path = state_dir.join("sock");
        // Remove stale socket if present.
        let _ = std::fs::remove_file(&socket_path);

        let graph = TaskGraph::build(&ws, &requested)?;
        if graph.graph.node_count() == 0 {
            return Err(anyhow!("no tasks resolved"));
        }

        // Build proc registry — one Proc per node.
        let mut procs: BTreeMap<String, Arc<Proc>> = BTreeMap::new();
        let mut buffers: BTreeMap<String, SharedBuffer> = BTreeMap::new();
        let mut node_to_id: BTreeMap<NodeIndex, String> = BTreeMap::new();
        for idx in graph.graph.node_indices() {
            let n = &graph.graph[idx];
            let id = n.id();
            let buf = buffer::new_shared(buffer::DEFAULT_CAPACITY);
            let proc = Proc::new(id.clone(), buf.clone(), n.def.persistent);
            buffers.insert(id.clone(), buf);
            procs.insert(id.clone(), proc);
            node_to_id.insert(idx, id);
        }

        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let daemon = Arc::new(Daemon {
            state_dir: state_dir.clone(),
            socket_path: socket_path.clone(),
            procs: procs.clone(),
            buffers: buffers.clone(),
            stop_tx: Arc::new(Mutex::new(Some(stop_tx.clone()))),
            started_at: Instant::now(),
        });

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
        let scheduler = tokio::spawn(async move {
            run_scheduler(sched_daemon, graph_arc, &mut sched_stop).await;
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

        // Shutdown all procs.
        eprintln!("procpane shutting down…");
        for (_, p) in &daemon.procs {
            p.stop(Duration::from_secs(5));
        }
        scheduler.abort();
        let _ = std::fs::remove_file(&socket_path);
        Ok(())
    }
}

async fn run_scheduler(
    daemon: Arc<Daemon>,
    graph: Arc<TaskGraph>,
    stop_rx: &mut tokio::sync::watch::Receiver<bool>,
) {
    let mut completed: HashSet<NodeIndex> = HashSet::new();
    let mut spawned: HashSet<NodeIndex> = HashSet::new();

    loop {
        if *stop_rx.borrow() {
            return;
        }

        // Find nodes whose deps are all completed and which haven't been spawned.
        let mut to_spawn: Vec<NodeIndex> = Vec::new();
        for idx in graph.graph.node_indices() {
            if spawned.contains(&idx) {
                continue;
            }
            let mut ready = true;
            for dep in graph
                .graph
                .neighbors_directed(idx, petgraph::Direction::Incoming)
            {
                if !completed.contains(&dep) {
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
                    *proc.state.lock() = ProcState::Exited;
                    spawned.insert(*idx);
                    completed.insert(*idx);
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
            let env: Vec<(String, String)> = vec![("PATH".into(), path)];
            let cwd = node.cwd.clone();
            if let Err(e) = proc.spawn(&shell_cmd, &cwd, &env) {
                tracing::error!(?e, "failed to spawn {id}");
                *proc.state.lock() = ProcState::Crashed;
                completed.insert(*idx);
            }
            spawned.insert(*idx);
        }

        // Check for newly-completed non-persistent procs.
        for idx in graph.graph.node_indices() {
            if completed.contains(&idx) {
                continue;
            }
            let id = graph.graph[idx].id();
            if let Some(proc) = daemon.procs.get(&id) {
                let st = *proc.state.lock();
                if matches!(st, ProcState::Exited | ProcState::Crashed | ProcState::Killed) {
                    completed.insert(idx);
                }
            }
        }

        // Exit condition: every non-persistent task is complete and no persistent tasks remain alive.
        let mut any_persistent_alive = false;
        let mut all_non_persistent_done = true;
        for idx in graph.graph.node_indices() {
            let node = &graph.graph[idx];
            let id = node.id();
            let proc = daemon.procs.get(&id);
            let state = proc.map(|p| *p.state.lock()).unwrap_or(ProcState::Pending);
            if node.def.persistent {
                if matches!(state, ProcState::Running | ProcState::Pending) {
                    any_persistent_alive = true;
                }
            } else if !matches!(
                state,
                ProcState::Exited | ProcState::Crashed | ProcState::Killed
            ) {
                all_non_persistent_done = false;
            }
        }

        if all_non_persistent_done && !any_persistent_alive {
            // Nothing else will happen; trigger daemon shutdown.
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
            let now = Instant::now();
            let mut procs = Vec::new();
            for (id, p) in &daemon.procs {
                let started = *p.started_at.lock();
                let age = started.map(|t| now.duration_since(t).as_secs()).unwrap_or(0);
                let line_count = p.buffer.lock().line_count();
                procs.push(ProcStatus {
                    name: id.clone(),
                    state: p.state.lock().as_str().to_string(),
                    pid: *p.pid.lock(),
                    age_secs: age,
                    line_count,
                    exit_code: *p.exit_code.lock(),
                    persistent: p.persistent,
                });
            }
            Response::Status { procs }
        }
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
