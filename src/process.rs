use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::buffer::SharedBuffer;

/// Env vars carried over from procpane's parent shell even when the task has
/// opted into `env_from` (and therefore wants a scrubbed env). These are the
/// vars a Unix process generally expects to find — locale, terminal, user
/// identity, temp directory. *No* third-party-credentials-shaped names.
const SAFE_ENV_KEYS: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_TIME",
    "LC_NUMERIC",
    "LC_MONETARY",
    "TZ",
    "TMPDIR",
    "COLORTERM",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "PAGER",
    "EDITOR",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcState {
    /// Waiting on dependencies; not yet spawned.
    Pending,
    /// Spawned, process running, healthcheck not yet satisfied.
    /// (For tasks with no healthcheck, transitions to `Healthy` immediately.)
    Starting,
    /// Process running and healthcheck has passed (persistent tasks).
    Healthy,
    /// One-shot task ran to completion successfully (exit 0).
    Completed,
    /// Process exited with non-zero status, unexpectedly.
    Crashed,
    /// Killed by us (SIGINT/SIGTERM/SIGKILL).
    Killed,
}

impl ProcState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProcState::Pending => "pending",
            ProcState::Starting => "starting",
            ProcState::Healthy => "healthy",
            ProcState::Completed => "completed",
            ProcState::Crashed => "crashed",
            ProcState::Killed => "killed",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ProcState::Completed | ProcState::Crashed | ProcState::Killed
        )
    }
}

pub struct Proc {
    pub name: String,
    pub buffer: SharedBuffer,
    pub state: parking_lot::Mutex<ProcState>,
    pub pid: parking_lot::Mutex<Option<i32>>,
    pub exit_code: parking_lot::Mutex<Option<i32>>,
    pub started_at: parking_lot::Mutex<Option<Instant>>,
    pub persistent: bool,
    child: parking_lot::Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>,
    killer: parking_lot::Mutex<Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    master: parking_lot::Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
}

impl Proc {
    pub fn new(name: String, buffer: SharedBuffer, persistent: bool) -> Arc<Self> {
        Arc::new(Self {
            name,
            buffer,
            state: parking_lot::Mutex::new(ProcState::Pending),
            pid: parking_lot::Mutex::new(None),
            exit_code: parking_lot::Mutex::new(None),
            started_at: parking_lot::Mutex::new(None),
            persistent,
            child: parking_lot::Mutex::new(None),
            killer: parking_lot::Mutex::new(None),
            master: parking_lot::Mutex::new(None),
        })
    }

    /// Spawn under a PTY, in its own process group. Returns once the child is
    /// spawned; reader thread streams into the ring buffer.
    ///
    /// `inherit_parent_env` controls whether the spawned process sees procpane's
    /// full parent environment. When `false`, only a minimal "safe" set
    /// (HOME, USER, LANG, etc.) is inherited and everything else is dropped —
    /// the caller's overlay (`env`) is the only source of project-specific
    /// values. This is the right mode for tasks that have declared an
    /// `env_from` allowlist; it makes the allowlist actually load-bearing
    /// instead of "allowlist on top of full bleed".
    pub fn spawn(
        self: &Arc<Self>,
        shell_cmd: &str,
        cwd: &Path,
        env: &[(String, String)],
        inherit_parent_env: bool,
    ) -> Result<()> {
        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        // Use sh -c to honor package.json script semantics.
        let mut cmd = CommandBuilder::new("sh");
        cmd.arg("-c");
        cmd.arg(shell_cmd);
        cmd.cwd(cwd);

        if inherit_parent_env {
            for (k, v) in std::env::vars() {
                cmd.env(k, v);
            }
        } else {
            // Wipe the auto-inherited parent env first, then only add the
            // minimal "safe" set. Without env_clear(), portable-pty merges
            // our additions on top of the full parent env — so the allowlist
            // would be a no-op.
            cmd.env_clear();
            for k in SAFE_ENV_KEYS {
                if let Ok(v) = std::env::var(k) {
                    cmd.env(k, v);
                }
            }
        }
        // Tell programs we are a terminal.
        cmd.env("TERM", "xterm-256color");
        cmd.env("FORCE_COLOR", "1");
        for (k, v) in env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd).context("spawn_command")?;
        let pid = child.process_id().map(|p| p as i32);
        let killer = child.clone_killer();

        *self.pid.lock() = pid;
        *self.state.lock() = ProcState::Starting;
        *self.started_at.lock() = Some(Instant::now());
        *self.killer.lock() = Some(killer);
        // Store child + master BEFORE spawning the reader thread to avoid a
        // fast-exit race where EOF arrives before storage.
        *self.child.lock() = Some(child);
        *self.master.lock() = Some(pair.master);
        let mut reader = self
            .master
            .lock()
            .as_ref()
            .unwrap()
            .try_clone_reader()
            .context("clone reader")?;
        let buf = self.buffer.clone();
        let proc_clone = Arc::clone(self);
        thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.lock().ingest(&chunk[..n]);
                    }
                    Err(_) => break,
                }
            }
            buf.lock().flush_partial();
            let mut guard = proc_clone.child.lock();
            if let Some(mut c) = guard.take() {
                if let Ok(status) = c.wait() {
                    let code = status.exit_code() as i32;
                    *proc_clone.exit_code.lock() = Some(code);
                    let mut st = proc_clone.state.lock();
                    if *st != ProcState::Killed {
                        *st = if code == 0 {
                            ProcState::Completed
                        } else {
                            ProcState::Crashed
                        };
                    }
                }
            }
        });

        // Drop slave so EOF propagates when child exits.
        drop(pair.slave);
        Ok(())
    }

    pub fn stop_with_signal(&self, signal: i32, grace: Duration) {
        {
            let mut st = self.state.lock();
            if st.is_terminal() {
                return;
            }
            *st = ProcState::Killed;
        }
        let pid = *self.pid.lock();
        if let Some(pid) = pid {
            // Send to the process group so descendants get reaped.
            unsafe {
                libc::kill(-pid, signal);
            }
        }
        let deadline = Instant::now() + grace;
        loop {
            if Instant::now() >= deadline {
                break;
            }
            let alive = self
                .pid
                .lock()
                .map(|p| unsafe { libc::kill(p, 0) == 0 })
                .unwrap_or(false);
            if !alive {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        // SIGKILL fallback to the process group.
        if let Some(pid) = pid {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        if let Some(mut k) = self.killer.lock().take() {
            let _ = k.kill();
        }
    }
}
