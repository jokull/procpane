use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    Status,
    Stop,
    Tail { name: String, lines: usize },
    Grep {
        name: Option<String>,
        pattern: String,
        before: usize,
        after: usize,
    },
    Since { name: String, cursor: u64 },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong,
    Status { procs: Vec<ProcStatus> },
    Lines { lines: Vec<LineRecord>, next_cursor: u64 },
    GrepMatches { matches: Vec<GrepMatch> },
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcStatus {
    pub name: String,
    pub state: String, // pending | running | exited | crashed | killed
    pub pid: Option<i32>,
    pub age_secs: u64,
    pub line_count: u64,
    pub exit_code: Option<i32>,
    pub persistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineRecord {
    pub seq: u64,
    pub ts_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepMatch {
    pub task: String,
    pub seq: u64,
    pub ts_ms: u64,
    pub text: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}
