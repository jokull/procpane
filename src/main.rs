mod buffer;
mod ca;
mod cli;
mod client;
mod config;
mod daemon;
mod graph;
mod healthcheck;
mod lock;
mod process;
mod proto;
mod proxy;
mod secrets;
mod share;
mod sidecar;
mod workspace;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;

use crate::cli::{Cli, Cmd, EnvOp, ProcOp, TrustOp};
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
        Cmd::Env { op } => env_cmd(start, op),
        Cmd::Trust { op } => trust_cmd(op),
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

    // Wait for socket, then poll until every task reaches a stable state
    // (healthy / completed / terminal) or we hit a budget. Show the README
    // table on the way through.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        cli_client::wait_for_socket(&socket_path, Duration::from_secs(10)).await
    })?;
    rt.block_on(async {
        wait_and_print_status(&socket_path, Duration::from_secs(30)).await;
    });
    Ok(())
}

async fn wait_and_print_status(socket: &std::path::Path, budget: Duration) {
    let deadline = std::time::Instant::now() + budget;
    let mut last_render: Option<String> = None;
    loop {
        let resp = match cli_client::call(socket, Request::Status).await {
            Ok(r) => r,
            Err(_) => break,
        };
        let procs = match resp {
            Response::Status { procs } => procs,
            _ => break,
        };
        let stable = procs.iter().all(|p| {
            matches!(
                p.state.as_str(),
                "healthy" | "completed" | "crashed" | "killed"
            )
        });
        let rendered = render_up_table(&procs);
        if last_render.as_deref() != Some(rendered.as_str()) {
            // Erase prior frame.
            if let Some(prior) = &last_render {
                let lines = prior.matches('\n').count();
                for _ in 0..lines {
                    eprint!("\x1b[1A\x1b[2K");
                }
            }
            eprint!("{rendered}");
            last_render = Some(rendered);
        }
        if stable {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn render_up_table(procs: &[proto::ProcStatus]) -> String {
    let mut out = String::new();
    let mut healthy = 0usize;
    for p in procs {
        let mark = match p.state.as_str() {
            "healthy" | "completed" => {
                healthy += 1;
                "✓"
            }
            "crashed" | "killed" => "✗",
            _ => "⏳",
        };
        let host = p.hostname.as_deref().unwrap_or("");
        let host_disp = if host.is_empty() {
            String::new()
        } else {
            format!("  https://{host}")
        };
        out.push_str(&format!(
            "{mark} {name:<28} {state:<10}{host}\n",
            name = p.name,
            state = p.state,
            host = host_disp
        ));
    }
    let alive = procs.iter().filter(|p| !matches!(p.state.as_str(), "killed" | "crashed")).count();
    out.push_str(&format!("{healthy}/{alive} tasks healthy.\n"));
    out
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

fn trust_cmd(op: TrustOp) -> Result<()> {
    match op {
        TrustOp::Install { pretty_urls } => {
            ca::ensure_ca()?;
            let cert_path = ca::ca_cert_path()?;
            println!("Installing CA into /Library/Keychains/System.keychain");
            println!("  cert: {}", cert_path.display());
            println!("  This will prompt for `sudo` (Touch ID works if pam_tid is enabled).");
            let status = std::process::Command::new("sudo")
                .arg("security")
                .arg("add-trusted-cert")
                .arg("-d") // user trust → root trust (with -k System.keychain)
                .arg("-r")
                .arg("trustRoot")
                .arg("-k")
                .arg("/Library/Keychains/System.keychain")
                .arg(&cert_path)
                .status()
                .map_err(|e| anyhow!("failed to invoke `sudo security`: {e}"))?;
            if !status.success() {
                return Err(anyhow!("`security add-trusted-cert` failed"));
            }
            println!("✓ CA installed. https://*.test:8443 is now trusted.");
            if pretty_urls {
                println!("\n--pretty-urls: pf-based :443→:8443 redirect is not yet implemented.");
                println!("For now, use https://web.test:8443 (note the explicit port).");
            }
            Ok(())
        }
        TrustOp::Uninstall => {
            let cert_path = ca::ca_cert_path()?;
            if cert_path.is_file() {
                let status = std::process::Command::new("sudo")
                    .arg("security")
                    .arg("delete-certificate")
                    .arg("-c")
                    .arg(ca::CA_COMMON_NAME)
                    .arg("-t")
                    .arg("/Library/Keychains/System.keychain")
                    .status();
                match status {
                    Ok(s) if s.success() => println!("✓ removed from System keychain"),
                    Ok(_) => eprintln!("(no entry in System keychain, or sudo declined)"),
                    Err(e) => eprintln!("sudo invocation failed: {e}"),
                }
            }
            if let Ok(dir) = ca::ca_dir() {
                let _ = std::fs::remove_dir_all(&dir);
                println!("✓ removed {}", dir.display());
            }
            Ok(())
        }
        TrustOp::Status => {
            if ca::is_installed() {
                println!("✓ CA files present: {}", ca::ca_dir()?.display());
            } else {
                println!("✗ CA not generated. Run `procpane trust install` first.");
            }
            Ok(())
        }
    }
}

fn env_cmd(start: PathBuf, op: EnvOp) -> Result<()> {
    let root = resolve_root(start)?;
    let service = secrets::service_name(&root);
    match op {
        EnvOp::Set { key, value } => {
            validate_key(&key)?;
            let v = match value {
                Some(v) => v,
                None => rpassword::prompt_password(format!("Value for {key}: "))
                    .map_err(|e| anyhow!("prompt failed: {e}"))?,
            };
            if v.is_empty() {
                return Err(anyhow!("empty value; not storing"));
            }
            secrets::set(&service, &key, &v)?;
            println!("✓ stored {key}");
            Ok(())
        }
        EnvOp::Get { key } => {
            validate_key(&key)?;
            match secrets::get(&service, &key)? {
                Some(v) => {
                    print!("{v}");
                    Ok(())
                }
                None => Err(anyhow!("{key} not set")),
            }
        }
        EnvOp::List { json } => {
            let keys = secrets::list_accounts(&service)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&keys)?);
            } else if keys.is_empty() {
                println!("(no secrets stored for this repo)");
            } else {
                for k in keys {
                    println!("{k}");
                }
            }
            Ok(())
        }
        EnvOp::Unset { key } => {
            validate_key(&key)?;
            if secrets::delete(&service, &key)? {
                println!("✓ removed {key}");
            } else {
                println!("(no such key: {key})");
            }
            Ok(())
        }
        EnvOp::Receive => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(share::receive(&service))
        }
        EnvOp::Send { code, keys } => {
            let keys = if keys.is_empty() {
                secrets::list_accounts(&service)?
            } else {
                for k in &keys {
                    validate_key(k)?;
                }
                keys
            };
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(share::send(&service, code, keys))
        }
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        return Err(anyhow!("empty key"));
    }
    // Env-var-shaped: ASCII alnum + underscore, not starting with digit.
    let ok = key.chars().enumerate().all(|(i, c)| {
        c == '_'
            || c.is_ascii_alphabetic()
            || (i > 0 && c.is_ascii_digit())
    });
    if !ok {
        return Err(anyhow!(
            "key must match [A-Za-z_][A-Za-z0-9_]* (got: {key})"
        ));
    }
    Ok(())
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
