mod buffer;
mod cli;
mod client;
mod config;
mod daemon;
mod graph;
mod healthcheck;
mod lock;
mod process;
mod proto;
mod sidecar;
mod workspace;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

use crate::cli::{Cli, Cmd, ProcOp};
use crate::client as cli_client;
use crate::proto::{Request, Response};
use crate::workspace::Workspace;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Cli::parse();
    let start = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    match args.cmd {
        Cmd::Up {
            tasks,
            foreground,
            no_prebuild,
        } => run_cmd(start, tasks, foreground, no_prebuild),
        Cmd::WaitFor { name, timeout } => wait_for_cmd(start, name, timeout),
        Cmd::Status { json } => status_cmd(start, json),
        Cmd::Stop => stop_cmd(start),
        Cmd::Proc { name, op } => proc_cmd(start, name, op),
        Cmd::Grep {
            pattern,
            after,
            before,
            json,
        } => grep_cmd(start, pattern, before, after, json),
        Cmd::DaemonInner {
            tasks,
            root,
            no_prebuild,
        } => daemon_inner(root, tasks, no_prebuild),
    }
}

fn resolve_root(start: PathBuf) -> Result<PathBuf> {
    let mut cur = start
        .canonicalize()
        .with_context(|| "canonicalize cwd")?;
    loop {
        if cur.join("turbo.json").is_file() {
            return Ok(cur);
        }
        let parent = cur.parent().map(|p| p.to_path_buf());
        match parent {
            Some(p) if p != cur => cur = p,
            _ => return Err(anyhow!("no turbo.json found from given cwd")),
        }
    }
}

fn run_cmd(start: PathBuf, tasks: Vec<String>, foreground: bool, no_prebuild: bool) -> Result<()> {
    let root = resolve_root(start.clone())?;
    let state_dir = daemon::state_dir(&root);
    std::fs::create_dir_all(&state_dir)?;
    let lock_path = state_dir.join("lock");
    let socket_path = state_dir.join("sock");

    if let Some(pid) = lock::PidLock::read_pid(&lock_path) {
        if lock::is_alive(pid) {
            return Err(anyhow!(
                "procpane already running here (pid {pid}). Use `procpane stop` first."
            ));
        }
        let _ = std::fs::remove_file(&lock_path);
    }

    if foreground {
        return daemon_inner(root, tasks, no_prebuild);
    }

    // Fork-style detach via re-exec.
    let self_exe = std::env::current_exe().context("current_exe")?;
    let mut cmd = std::process::Command::new(&self_exe);
    cmd.arg("daemon-inner").arg("--root").arg(&root);
    if no_prebuild {
        cmd.arg("--no-prebuild");
    }
    cmd.args(&tasks);
    // Detach: new session, redirect stdio to /dev/null.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            // New session — detaches from controlling terminal.
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn daemon")?;
    let _ = child;

    // Wait for socket.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        cli_client::wait_for_socket(&socket_path, Duration::from_secs(10)).await
    })?;
    println!("procpane started ({})", socket_path.display());
    Ok(())
}

fn daemon_inner(root: PathBuf, tasks: Vec<String>, no_prebuild: bool) -> Result<()> {
    let state_dir = daemon::state_dir(&root);
    std::fs::create_dir_all(&state_dir)?;
    let lock_path = state_dir.join("lock");
    let _lock = lock::PidLock::acquire(&lock_path)?;

    let ws = Workspace::discover(&root)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(daemon::Daemon::run(ws, tasks, state_dir, no_prebuild))
}

fn status_cmd(start: PathBuf, json: bool) -> Result<()> {
    let root = resolve_root(start)?;
    let socket = daemon::state_dir(&root).join("sock");
    let rt = tokio::runtime::Runtime::new()?;
    let resp = rt.block_on(cli_client::call(&socket, Request::Status))?;
    match resp {
        Response::Status { procs } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&procs)?);
            } else {
                println!(
                    "{:<32} {:<10} {:>8} {:>6} {:>10}  {}",
                    "NAME", "STATE", "PID", "AGE", "LINES", "HOSTNAME"
                );
                for p in procs {
                    println!(
                        "{:<32} {:<10} {:>8} {:>5}s {:>10}  {}",
                        p.name,
                        p.state,
                        p.pid.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
                        p.age_secs,
                        p.line_count,
                        p.hostname.unwrap_or_default(),
                    );
                }
            }
        }
        Response::Error { message } => return Err(anyhow!(message)),
        _ => return Err(anyhow!("unexpected response")),
    }
    Ok(())
}

fn wait_for_cmd(start: PathBuf, name: String, timeout: String) -> Result<()> {
    let dur = humantime::parse_duration(&timeout)
        .map_err(|e| anyhow!("invalid --timeout: {e}"))?;
    let root = resolve_root(start)?;
    let socket = daemon::state_dir(&root).join("sock");
    if !socket.exists() {
        return Err(anyhow!("no procpane daemon running here"));
    }
    let rt = tokio::runtime::Runtime::new()?;
    let deadline = std::time::Instant::now() + dur;
    let interval = Duration::from_millis(250);
    let exit_code: i32 = rt.block_on(async {
        loop {
            let resp = match cli_client::call(
                &socket,
                Request::GetTask { name: name.clone() },
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("wait-for: {e}");
                    return 1;
                }
            };
            let task = match resp {
                Response::Task { task } => task,
                Response::Error { message } => {
                    eprintln!("wait-for: {message}");
                    return 1;
                }
                _ => {
                    eprintln!("wait-for: unexpected response");
                    return 1;
                }
            };
            match task.state.as_str() {
                "healthy" | "completed" => {
                    println!("{name} is {}", task.state);
                    return 0;
                }
                "crashed" | "killed" => {
                    eprintln!(
                        "wait-for: {name} reached terminal state {} (exit {:?})",
                        task.state, task.exit_code
                    );
                    return 1;
                }
                _ => {}
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("wait-for: timeout after {timeout} (last state: {})", task.state);
                return 2;
            }
            tokio::time::sleep(interval).await;
        }
    });
    if exit_code == 0 {
        Ok(())
    } else {
        std::process::exit(exit_code);
    }
}

fn stop_cmd(start: PathBuf) -> Result<()> {
    let root = resolve_root(start)?;
    let socket = daemon::state_dir(&root).join("sock");
    if !socket.exists() {
        println!("no running daemon");
        return Ok(());
    }
    let rt = tokio::runtime::Runtime::new()?;
    let resp = rt.block_on(cli_client::call(&socket, Request::Stop))?;
    match resp {
        Response::Ok => {
            println!("stopping…");
            // Wait briefly for socket to disappear.
            for _ in 0..50 {
                if !socket.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        }
        Response::Error { message } => Err(anyhow!(message)),
        _ => Err(anyhow!("unexpected response")),
    }
}

fn proc_cmd(start: PathBuf, name: String, op: ProcOp) -> Result<()> {
    let root = resolve_root(start)?;
    let socket = daemon::state_dir(&root).join("sock");
    let rt = tokio::runtime::Runtime::new()?;
    match op {
        ProcOp::Tail { n, json } => {
            let resp = rt.block_on(cli_client::call(
                &socket,
                Request::Tail { name, lines: n },
            ))?;
            print_lines(resp, json)
        }
        ProcOp::Since { cursor, json } => {
            let resp = rt.block_on(cli_client::call(
                &socket,
                Request::Since { name, cursor },
            ))?;
            print_lines(resp, json)
        }
        ProcOp::Grep {
            pattern,
            before,
            after,
            json,
        } => {
            let resp = rt.block_on(cli_client::call(
                &socket,
                Request::Grep {
                    name: Some(name),
                    pattern,
                    before,
                    after,
                },
            ))?;
            print_grep(resp, json)
        }
    }
}

fn grep_cmd(start: PathBuf, pattern: String, before: usize, after: usize, json: bool) -> Result<()> {
    let root = resolve_root(start)?;
    let socket = daemon::state_dir(&root).join("sock");
    let rt = tokio::runtime::Runtime::new()?;
    let resp = rt.block_on(cli_client::call(
        &socket,
        Request::Grep {
            name: None,
            pattern,
            before,
            after,
        },
    ))?;
    print_grep(resp, json)
}

fn print_lines(resp: Response, json: bool) -> Result<()> {
    match resp {
        Response::Lines { lines, next_cursor } => {
            if json {
                #[derive(serde::Serialize)]
                struct Out<'a> {
                    next_cursor: u64,
                    lines: &'a [proto::LineRecord],
                }
                let o = Out {
                    next_cursor,
                    lines: &lines,
                };
                println!("{}", serde_json::to_string_pretty(&o)?);
            } else {
                for l in lines {
                    println!("{}", l.text);
                }
                eprintln!("--- next_cursor={next_cursor} ---");
            }
            Ok(())
        }
        Response::Error { message } => Err(anyhow!(message)),
        _ => Err(anyhow!("unexpected response")),
    }
}

fn print_grep(resp: Response, json: bool) -> Result<()> {
    match resp {
        Response::GrepMatches { matches } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&matches)?);
            } else {
                for m in matches {
                    for c in &m.context_before {
                        println!("{}- {}", m.task, c);
                    }
                    println!("{}> {}", m.task, m.text);
                    for c in &m.context_after {
                        println!("{}- {}", m.task, c);
                    }
                }
            }
            Ok(())
        }
        Response::Error { message } => Err(anyhow!(message)),
        _ => Err(anyhow!("unexpected response")),
    }
}
