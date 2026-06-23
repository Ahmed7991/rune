//! The small slice of Claude Code's on-disk surface the shell needs in
//! Increment 1: where transcripts live (to decide resume-vs-new) and a fresh
//! session UUID. Read-only — we never write into `~/.claude`.

use std::path::PathBuf;

use gtk4::glib;

/// Claude encodes a session's cwd into its transcript directory name by
/// replacing every non-`[A-Za-z0-9]` character with `-`. This is lossy and
/// non-invertible (`/`, `_`, `.`, space all collapse to `-`), so we only ever
/// encode *forward* (path → dir) and never try to decode a dir name back.
///
/// Verified on disk: `/home/user/projects/my-app`
/// → `-home-user-projects-my-app`.
pub fn encode_cwd(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `~/.claude/projects/<encoded-cwd>/` — where a project's transcripts live.
pub fn project_transcript_dir(project_path: &str) -> PathBuf {
    glib::home_dir()
        .join(".claude/projects")
        .join(encode_cwd(project_path))
}

/// True if Claude already has a transcript for this session under this project.
/// If so we resume it; otherwise we start a brand-new session under our
/// pre-allocated UUID (which lets Claude's auto `ai-title` fire).
pub fn transcript_exists(project_path: &str, session_id: &str) -> bool {
    project_transcript_dir(project_path)
        .join(format!("{session_id}.jsonl"))
        .exists()
}

/// A fresh RFC-4122 v4 UUID for `claude --session-id`. Uses glib's generator so
/// we don't pull in the `uuid` crate.
pub fn new_session_id() -> String {
    glib::uuid_string_random().to_string()
}

/// True if `s` has the canonical UUID shape `8-4-4-4-12` hex. We only ever hand
/// session ids to `claude --session-id/--resume`; validating at the trust
/// boundary keeps a hand-edited/garbage config from reaching the CLI (which
/// rejects non-UUIDs) — and is belt-and-braces against shell metacharacters.
pub fn is_valid_session_id(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b.iter().enumerate().all(|(i, &c)| match i {
        8 | 13 | 18 | 23 => c == b'-',
        _ => c.is_ascii_hexdigit(),
    })
}
