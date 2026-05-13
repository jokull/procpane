use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct PidLock {
    path: PathBuf,
}

impl PidLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        if path.exists() {
            let mut existing = String::new();
            if let Ok(mut f) = File::open(path) {
                let _ = f.read_to_string(&mut existing);
            }
            if let Ok(pid) = existing.trim().parse::<i32>() {
                if is_alive(pid) {
                    return Err(anyhow!(
                        "procpane already running (pid {pid}); lock at {}",
                        path.display()
                    ));
                }
            }
            // Stale — remove it.
            let _ = std::fs::remove_file(path);
        }
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .with_context(|| format!("create lock {}", path.display()))?;
        writeln!(f, "{}", std::process::id())?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    pub fn read_pid(path: &Path) -> Option<i32> {
        let mut s = String::new();
        File::open(path).ok()?.read_to_string(&mut s).ok()?;
        s.trim().parse().ok()
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn is_alive(pid: i32) -> bool {
    // signal 0 → existence check
    unsafe { libc::kill(pid, 0) == 0 }
}
