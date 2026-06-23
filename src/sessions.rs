//! Browse a project's past Claude sessions for the resume picker.
//!
//! Reads `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl` (read-only) and pulls
//! out, per session: a human title (latest `ai-title`, else the first real user
//! prompt, else a short id), last-active time (file mtime), and a prompt count.

use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

use crate::claude;

/// How many most-recent sessions we surface per project. Bounds the work the
/// picker does so a project with a long history still opens snappily.
pub const MAX_SESSIONS: usize = 60;

/// How many recent sessions the global Ctrl-K switcher surfaces across *all*
/// projects at once. Tighter than [`MAX_SESSIONS`] because the switcher parses
/// these transcripts synchronously on each open, so this caps the per-keypress
/// work to stay snappy.
pub const SWITCHER_RECENT: usize = 30;

pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub modified: SystemTime,
    pub prompt_count: usize,
    pub cost_usd: f64,
    /// The model of the session's most recent (non-synthetic) assistant turn,
    /// raw id (e.g. `claude-opus-4-8`); empty if it has no assistant turn yet.
    pub model: String,
    /// Tokens in context on that last turn (input + cache read + cache write) —
    /// the basis for the context-fill estimate. 0 if no assistant turn yet.
    pub context_tokens: u64,
}

/// The selected project's sessions, newest-active first (capped to MAX_SESSIONS).
pub fn list_sessions(project_path: &str) -> Vec<SessionMeta> {
    let dir = claude::project_transcript_dir(project_path);
    let mut files = candidate_files(&dir);
    files.sort_by(|a, b| b.1.cmp(&a.1)); // newest first
    files.truncate(MAX_SESSIONS);
    files
        .into_iter()
        .map(|(path, modified, id)| meta_from_file(&path, modified, id))
        .collect()
}

/// Metadata for one *known* session under a project (used by the cross-project
/// queue, which already knows the live session id + its cwd). `None` if no
/// transcript exists for it yet.
pub fn session_summary(project_path: &str, session_id: &str) -> Option<SessionMeta> {
    let path = claude::project_transcript_dir(project_path).join(format!("{session_id}.jsonl"));
    let modified = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()?;
    Some(meta_from_file(&path, modified, session_id.to_string()))
}

/// The most-recently-active sessions across *several* projects, newest first,
/// capped at `limit`. Powers the global quick-switcher. We gather every
/// project's candidate files first and sort by mtime *before* parsing, so only
/// `limit` transcripts ever get read regardless of how many sessions a project
/// has accumulated. Note: this bounds the *count* parsed, not their total bytes
/// — a handful of very large recent transcripts still costs proportionally
/// (a synchronous read on the caller's thread), so keep `limit` modest.
pub fn recent_sessions(projects: &[String], limit: usize) -> Vec<(String, SessionMeta)> {
    let mut all: Vec<(String, std::path::PathBuf, SystemTime, String)> = Vec::new();
    for project in projects {
        let dir = claude::project_transcript_dir(project);
        for (path, modified, id) in candidate_files(&dir) {
            all.push((project.clone(), path, modified, id));
        }
    }
    all.sort_by(|a, b| b.2.cmp(&a.2)); // newest first
    all.truncate(limit);
    all.into_iter()
        .map(|(project, path, modified, id)| (project, meta_from_file(&path, modified, id)))
        .collect()
}

/// Cheap count of a project's past sessions (uuid-named transcripts) — a
/// directory scan, no file parsing. Powers the dashboard project cards.
pub fn session_count(project_path: &str) -> usize {
    candidate_files(&claude::project_transcript_dir(project_path)).len()
}

/// A project's estimated total cost: the sum of every transcript's cost. This
/// parses *all* of a project's transcripts, so it can be hundreds of MB of JSON
/// — call it off the UI thread. Takes the resolved dir (not a project path) so
/// it never touches GLib and is safe on a worker thread.
pub fn dir_total_cost(dir: &Path) -> f64 {
    candidate_files(dir)
        .iter()
        .map(|(path, _, _)| parse_meta(path).cost_usd)
        .sum()
}

/// A cheap fingerprint of a project's transcript dir — combines each
/// transcript's mtime + size — so the dashboard can tell whether the expensive
/// total-cost parse needs redoing or a cached value still holds. Only a dir scan
/// + metadata, no file parsing. Order-independent (read_dir order is undefined).
pub fn dir_fingerprint(dir: &Path) -> u64 {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut fp: u64 = 0;
    let mut count: u64 = 0;
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !claude::is_valid_session_id(id) {
            continue;
        }
        let Ok(md) = entry.metadata() else {
            continue;
        };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        fp ^= mtime.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ md.len().wrapping_mul(0xD1B5_4A32_D192_ED03);
        count = count.wrapping_add(1);
    }
    fp ^ count.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// Every uuid-named transcript in a project dir, as `(path, mtime, id)`. Cheap:
/// just a directory scan + metadata, no file parsing.
fn candidate_files(dir: &Path) -> Vec<(std::path::PathBuf, SystemTime, String)> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        // Only real session transcripts (uuid-named), not stray files.
        if !claude::is_valid_session_id(&id) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((path, modified, id));
    }
    files
}

/// What a single transcript yields beyond its id/mtime.
struct ParsedMeta {
    title: String,
    prompt_count: usize,
    cost_usd: f64,
    /// Most recent non-synthetic assistant model, raw id; empty if none.
    model: String,
    /// Context tokens on that last turn; 0 if none.
    context_tokens: u64,
}

/// Parse one transcript into a `SessionMeta`, applying the short-id title
/// fallback when no `ai-title`/first-prompt is found.
fn meta_from_file(path: &Path, modified: SystemTime, id: String) -> SessionMeta {
    let p = parse_meta(path);
    let title = if p.title.is_empty() {
        short_id(&id)
    } else {
        p.title
    };
    SessionMeta {
        id,
        title,
        modified,
        prompt_count: p.prompt_count,
        cost_usd: p.cost_usd,
        model: p.model,
        context_tokens: p.context_tokens,
    }
}

/// A transcript is normally small (KB–low-MB). Refuse to slurp a pathologically
/// large one: this read runs synchronously on the UI thread, so a hostile/synced
/// multi-GB `.jsonl` could otherwise freeze or OOM-kill the cockpit (security
/// review). 25 MB is far above any real session; an oversized file just yields
/// empty meta (fallback title + $0).
pub const MAX_TRANSCRIPT_BYTES: u64 = 25 * 1024 * 1024;

/// Read a transcript only if it's within the size cap, else `None`.
pub fn read_transcript_capped(path: &Path) -> Option<String> {
    if std::fs::metadata(path).ok()?.len() > MAX_TRANSCRIPT_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Scan one transcript: latest `ai-title`, first real user prompt, prompt
/// count, estimated USD cost (summed across assistant turns), and the most
/// recent assistant turn's model + context-token count.
fn parse_meta(path: &Path) -> ParsedMeta {
    let Some(text) = read_transcript_capped(path) else {
        return ParsedMeta {
            title: String::new(),
            prompt_count: 0,
            cost_usd: 0.0,
            model: String::new(),
            context_tokens: 0,
        };
    };
    let mut ai_title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut prompts = 0usize;
    let mut cost_usd = 0.0f64;
    let mut last_model = String::new();
    let mut last_context = 0u64;
    // One assistant turn is written as several JSONL lines (one per content
    // block), each repeating the same cumulative usage — price each id once.
    let mut priced_msg_ids: HashSet<String> = HashSet::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    ai_title = Some(truncate(t, 80)); // bounded + sanitized, latest wins
                }
            }
            Some("user") => {
                if let Some(text) = extract_user_text(&v) {
                    let t = text.trim();
                    // Skip tool results (non-text content) and the command /
                    // system wrappers that aren't real prompts.
                    if !t.is_empty() && !t.starts_with('<') && !t.starts_with("Caveat:") {
                        prompts += 1;
                        if first_prompt.is_none() {
                            first_prompt = Some(truncate(t, 80));
                        }
                    }
                }
            }
            Some("assistant") => {
                let message = v.get("message");
                let model = message.and_then(|m| m.get("model")).and_then(|m| m.as_str());
                // Synthetic records (compaction summaries, etc.) aren't real
                // model turns — skip them for cost, model, and context alike.
                if model == Some("<synthetic>") {
                    continue;
                }
                let usage = message.and_then(|m| m.get("usage"));
                // Skip repeat lines of a turn we've already priced.
                let first_for_id = match message.and_then(|m| m.get("id")).and_then(|m| m.as_str())
                {
                    Some(id) => priced_msg_ids.insert(id.to_string()),
                    None => true, // no id (rare) — count it
                };
                if first_for_id {
                    if let (Some(model), Some(usage)) = (model, usage) {
                        cost_usd += crate::cost::turn_cost_usd(model, usage);
                    }
                }
                // The latest assistant turn wins for the model + context chips
                // (every repeat line of a turn carries the same values, so the
                // last line we see is the latest turn).
                if let Some(model) = model {
                    last_model = model.to_string();
                }
                if let Some(usage) = usage {
                    last_context = context_tokens(usage);
                }
            }
            _ => {}
        }
    }

    ParsedMeta {
        title: ai_title.or(first_prompt).unwrap_or_default(),
        prompt_count: prompts,
        cost_usd,
        model: last_model,
        context_tokens: last_context,
    }
}

/// Tokens occupying the context window on one assistant turn: the prompt the
/// model actually saw = fresh input + cache reads + this turn's cache writes.
fn context_tokens(usage: &serde_json::Value) -> u64 {
    // Read as f64 (like the cost path) so a float-encoded count isn't silently
    // dropped, then round back to a token count.
    let t = |key: &str| usage.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    (t("input_tokens") + t("cache_read_input_tokens") + t("cache_creation_input_tokens")).round()
        as u64
}

/// Pull plain text out of a user record's `message.content`, which is either a
/// string or an array of typed blocks.
fn extract_user_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    for block in content.as_array()? {
        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    // Strip non-whitespace control chars + bidi/zero-width overrides first: a
    // hostile/synced transcript title reaches desktop notifications and labels, so
    // an embedded terminal escape or an RTL-override could otherwise inject or
    // spoof. Keep normal whitespace; it's collapsed to one line next.
    let cleaned: String = s
        .chars()
        .filter(|c| {
            c.is_whitespace()
                || (!c.is_control()
                    && !matches!(c,
                        '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        })
        .collect();
    let flat: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max).collect();
        format!("{}…", cut.trim_end())
    }
}

fn short_id(id: &str) -> String {
    format!("session {}", id.get(..8).unwrap_or(id))
}

/// Same as [`relative_time`] but from an epoch-millisecond timestamp (Claude's
/// `statusUpdatedAt`). Reuses the file-mtime path so the phrasing stays in sync.
pub fn relative_time_ms(epoch_ms: i64) -> String {
    if epoch_ms <= 0 {
        return String::new();
    }
    let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(epoch_ms as u64);
    relative_time(t)
}

/// "just now" / "5m ago" / "3h ago" / "2d ago" / "4mo ago" from a file mtime.
pub fn relative_time(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        _ => format!("{}mo ago", secs / 2_592_000),
    }
}
