//! Live per-session run-state, read (no hooks, no install) from
//! `~/.claude/sessions/<pid>.json`. Each running `claude` process writes one of
//! these with its `sessionId`, `cwd`, and a `status` heartbeat. We map sessionId
//! → state for the *live* processes only — stale files for exited pids are
//! skipped, so a session with no entry simply isn't running.

use std::collections::HashMap;
use std::path::Path;

use gtk4::glib;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Claude is working (generating / running tools).
    Busy,
    /// Claude is waiting — ready for your input.
    Idle,
}

/// One live `claude` process, joined from its `sessions/<pid>.json`. Carries the
/// `cwd` so the cross-project queue can map a session back to its project (and
/// its transcript) without depending on the sidebar, and `updated_at` so we can
/// surface "just finished" vs "waiting a while".
#[derive(Clone, Debug)]
pub struct LiveSession {
    pub session_id: String,
    pub cwd: String,
    pub status: Status,
    /// `statusUpdatedAt`, epoch milliseconds (when the status last changed).
    pub updated_at: i64,
}

/// Every live Claude session on the machine (across all projects), one entry per
/// sessionId (freshest heartbeat wins when a resumed session has both a live and
/// a stale pid file). Read-only; ignores leftover state files for exited pids.
pub fn live_sessions() -> Vec<LiveSession> {
    let dir = glib::home_dir().join(".claude/sessions");
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    // sessionId -> (statusUpdatedAt, LiveSession); keep the freshest per id.
    let mut best: HashMap<String, (i64, LiveSession)> = HashMap::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };

        // Skip non-interactive sessions (headless `claude -p` jobs): they run
        // autonomously and exit on their own, so they never "need you" and would
        // only add noise to the queue/badge. A session with no `kind` (older
        // Claude) is treated as interactive.
        if let Some(kind) = v.get("kind").and_then(|k| k.as_str()) {
            if kind != "interactive" {
                continue;
            }
        }

        // Only live processes — ignore leftover state files for exited pids.
        let pid = v.get("pid").and_then(|p| p.as_i64()).unwrap_or(0);
        if pid <= 0 || !Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) else {
            continue;
        };
        let status = match v.get("status").and_then(|s| s.as_str()) {
            Some("busy") => Status::Busy,
            _ => Status::Idle,
        };
        let updated = v
            .get("statusUpdatedAt")
            .and_then(|u| u.as_i64())
            .unwrap_or(0);
        // `cwd` is written by current Claude versions; fall back to the live
        // process's own working directory for older ones.
        let cwd = v
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(String::from)
            .or_else(|| {
                std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .unwrap_or_default();

        let session = LiveSession {
            session_id: sid.to_string(),
            cwd,
            status,
            updated_at: updated,
        };
        // Keep the freshest heartbeat per sessionId. On an exact tie we keep the
        // first-scanned entry — equally-fresh duplicates are interchangeable for
        // our purposes (status/cwd), and read_dir order is unspecified anyway, so
        // there is no meaningful "right" winner to preserve.
        match best.get(sid) {
            Some((best_updated, _)) if *best_updated >= updated => {}
            _ => {
                best.insert(sid.to_string(), (updated, session));
            }
        }
    }

    best.into_iter().map(|(_, (_, s))| s).collect()
}
