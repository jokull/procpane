use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::proto::{GrepMatch, LineRecord};

pub const DEFAULT_CAPACITY: usize = 2048;

#[derive(Debug, Clone)]
pub struct Line {
    pub seq: u64,
    pub ts_ms: u64,
    pub text: String,
}

#[derive(Debug)]
pub struct RingBuffer {
    capacity: usize,
    lines: VecDeque<Line>,
    next_seq: u64,
    partial: String,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            lines: VecDeque::with_capacity(capacity),
            next_seq: 0,
            partial: String::new(),
        }
    }

    fn push_line(&mut self, text: String) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(Line { seq, ts_ms, text });
    }

    /// Ingest a chunk of raw bytes from a process. Strips ANSI, splits on
    /// `\n` and `\r` so progress-bar redraws become separate lines (per design).
    pub fn ingest(&mut self, bytes: &[u8]) {
        let cleaned = strip_ansi_escapes::strip(bytes);
        let s = String::from_utf8_lossy(&cleaned);
        for ch in s.chars() {
            match ch {
                '\n' | '\r' => {
                    if !self.partial.is_empty() {
                        let line = std::mem::take(&mut self.partial);
                        self.push_line(line);
                    }
                }
                _ => self.partial.push(ch),
            }
        }
    }

    pub fn flush_partial(&mut self) {
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.push_line(line);
        }
    }

    pub fn line_count(&self) -> u64 {
        self.next_seq
    }

    pub fn tail(&self, n: usize) -> Vec<LineRecord> {
        let start = self.lines.len().saturating_sub(n);
        self.lines
            .iter()
            .skip(start)
            .map(|l| LineRecord {
                seq: l.seq,
                ts_ms: l.ts_ms,
                text: l.text.clone(),
            })
            .collect()
    }

    /// Lines with seq >= cursor. Returns lines and the next cursor (last seq + 1).
    pub fn since(&self, cursor: u64) -> (Vec<LineRecord>, u64) {
        let mut out = Vec::new();
        let mut next = cursor;
        for l in &self.lines {
            if l.seq >= cursor {
                out.push(LineRecord {
                    seq: l.seq,
                    ts_ms: l.ts_ms,
                    text: l.text.clone(),
                });
                next = l.seq + 1;
            }
        }
        (out, next.max(cursor))
    }

    pub fn grep(
        &self,
        task: &str,
        re: &regex::Regex,
        before: usize,
        after: usize,
    ) -> Vec<GrepMatch> {
        let lines: Vec<&Line> = self.lines.iter().collect();
        let mut out = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if re.is_match(&line.text) {
                let b_start = i.saturating_sub(before);
                let a_end = (i + 1 + after).min(lines.len());
                let ctx_before = lines[b_start..i]
                    .iter()
                    .map(|l| l.text.clone())
                    .collect();
                let ctx_after = lines[i + 1..a_end]
                    .iter()
                    .map(|l| l.text.clone())
                    .collect();
                out.push(GrepMatch {
                    task: task.to_string(),
                    seq: line.seq,
                    ts_ms: line.ts_ms,
                    text: line.text.clone(),
                    context_before: ctx_before,
                    context_after: ctx_after,
                });
            }
        }
        out
    }
}

pub type SharedBuffer = Arc<Mutex<RingBuffer>>;

pub fn new_shared(capacity: usize) -> SharedBuffer {
    Arc::new(Mutex::new(RingBuffer::new(capacity)))
}
