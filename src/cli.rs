use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "procpane", version, about = "Agent-friendly process runner for turbo.json monorepos")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,

    /// Override repo root (defaults to nearest turbo.json ancestor)
    #[arg(long, global = true)]
    pub cwd: Option<std::path::PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Launch tasks in the background and return immediately
    Run {
        /// Task names (`dev`) or qualified ids (`web#dev`). One or more.
        #[arg(required = true)]
        tasks: Vec<String>,
        /// Run in the foreground (do not detach)
        #[arg(long)]
        foreground: bool,
    },
    /// List running processes
    Status {
        /// JSON output
        #[arg(long)]
        json: bool,
    },
    /// Stop the running daemon
    Stop,
    /// Per-process operations
    Proc {
        name: String,
        #[command(subcommand)]
        op: ProcOp,
    },
    /// Cross-task grep
    Grep {
        pattern: String,
        #[arg(short = 'A', long, default_value_t = 0)]
        after: usize,
        #[arg(short = 'B', long, default_value_t = 0)]
        before: usize,
        #[arg(long)]
        json: bool,
    },
    /// Internal: daemon entry point (invoked by `run`).
    #[command(hide = true)]
    DaemonInner {
        tasks: Vec<String>,
        #[arg(long)]
        root: std::path::PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProcOp {
    /// Tail the last N lines
    Tail {
        #[arg(short, long, default_value_t = 50)]
        n: usize,
        #[arg(long)]
        json: bool,
    },
    /// Grep this process's buffer
    Grep {
        pattern: String,
        #[arg(short = 'A', long, default_value_t = 0)]
        after: usize,
        #[arg(short = 'B', long, default_value_t = 0)]
        before: usize,
        #[arg(long)]
        json: bool,
    },
    /// Lines since cursor (incremental polling)
    Since {
        cursor: u64,
        #[arg(long)]
        json: bool,
    },
}
