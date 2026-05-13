use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::buffer::SharedBuffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcState {
    Pending,
    Running,
    Exited,
    Crashed,
    Killed,
}

impl ProcState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProcState::Pending => "pending",
            ProcState::Running => "running",
            ProcState::Exited => "exited",
            ProcState::Crashed => "crashed",
            ProcState::Killed => "killed",
        }
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
    pub fn spawn(self: &Arc<Self>, shell_cmd: &str, cwd: &Path, env: &[(String, String)]) -> Result<()> {
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

        // Inherit env, then overlay.
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
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
        *self.state.lock() = ProcState::Running;
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
                            ProcState::Exited
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

    pub fn stop(&self, grace: Duration) {
        {
            let mut st = self.state.lock();
            if matches!(*st, ProcState::Exited | ProcState::Crashed | ProcState::Killed) {
                return;
            }
            *st = ProcState::Killed;
        }
        let pid = *self.pid.lock();
        if let Some(pid) = pid {
            // SIGINT to the process group.
            unsafe {
                libc::kill(-pid, libc::SIGINT);
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
