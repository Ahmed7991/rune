//! The opt-in **exact awaiting-input** hooks installer (P2.x).
//!
//! Read-only signals (the `sessions/<pid>.json` busy/idle heartbeat) can tell
//! *working* from *waiting*, but not *waiting-for-a-permission* from
//! *waiting-for-your-next-prompt*, and they carry no activity hint. This module
//! closes that gap — **with the user's explicit consent** (a Settings toggle) —
//! by appending three hooks to `~/.claude/settings.json`:
//!
//!   * **Stop**           → the turn finished (your turn).
//!   * **Notification**   → a permission request (blocked) or an idle nudge.
//!   * **UserPromptSubmit** → you just gave it a task (it's working; the prompt
//!                            is the activity hint).
//!
//! Each points at [`rune-hook.js`](../assets/rune-hook.js), which records a tiny
//! per-session state file under `~/.claude/rune-state/` that the queue reads.
//!
//! ## Safety contract
//! The user's `settings.json` already carries their own hooks (a `Stop` notify,
//! a `PostToolUse` logger) and a `statusLine`. So the installer:
//!   * **backs up** `settings.json` (timestamped) before touching it;
//!   * **append-merges** — it pushes rune's entry onto the existing
//!     `hooks.<Event>` arrays and never overwrites them (clobbering the user's
//!     `Stop` notify is the one real hazard);
//!   * is **reversible** — uninstall removes only rune's commands (identified by
//!     the full canonical script path in their `command`), leaving every other
//!     hook in place. For a canonically-formatted file (as Claude Code itself
//!     writes — 2-space pretty + trailing newline) the result is byte-for-byte
//!     identical (`preserve_order` keeps key order intact); a hand-reformatted
//!     file keeps all of its data but is normalized to standard JSON formatting.
//!     The timestamped backup is the verbatim recovery net either way.
//!   * **refuses** to write a `settings.json` it can't parse (or whose `hooks`
//!     is the wrong shape) rather than risk corrupting it;
//!   * writes atomically (temp + rename).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

use gtk4::glib;
use serde_json::{json, Value};

use crate::claude;

/// The hook script, embedded so the toggle is fully self-contained: turning the
/// feature on writes this to disk; no separate install step is required.
const HOOK_SCRIPT: &str = include_str!("../assets/rune-hook.js");

/// (settings.json event name, role arg passed to the script).
const EVENTS: [(&str, &str); 3] = [
    ("Stop", "stop"),
    ("Notification", "notify"),
    ("UserPromptSubmit", "prompt"),
];

pub fn settings_path() -> PathBuf {
    glib::home_dir().join(".claude/settings.json")
}

fn hooks_dir() -> PathBuf {
    glib::home_dir().join(".claude/rune-hooks")
}

fn script_path() -> PathBuf {
    hooks_dir().join("rune-hook.js")
}

pub fn state_dir() -> PathBuf {
    glib::home_dir().join(".claude/rune-state")
}

// ───────────────────────────────────────────────────────────────────────────
// State written by the hook script, read by the queue
// ───────────────────────────────────────────────────────────────────────────

/// The exact phase a session is in, as reported by the hooks. Only ever produced
/// when the installer is on; otherwise the queue uses its busy/idle heuristic.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Phase {
    /// You gave it a task and it's working (or about to).
    Working,
    /// Paused on a permission request — it can't proceed without you.
    AwaitingPermission,
    /// Idle, nudging for your next input.
    AwaitingInput,
    /// The turn finished.
    Finished,
}

impl Phase {
    fn parse(s: &str) -> Option<Phase> {
        match s {
            "working" => Some(Phase::Working),
            "awaiting_permission" => Some(Phase::AwaitingPermission),
            "awaiting_input" => Some(Phase::AwaitingInput),
            "finished" => Some(Phase::Finished),
            _ => None,
        }
    }
}

/// One session's hook-reported state, read from `rune-state/<id>.json`.
#[derive(Clone, Debug)]
pub struct HookState {
    pub phase: Phase,
    /// A short activity hint (the task it's on, or the tool it wants) — already
    /// truncated by the writer. `None` when empty.
    pub hint: Option<String>,
    /// When the hook fired, epoch milliseconds (comparable to the heartbeat's
    /// `statusUpdatedAt`).
    pub updated_at_ms: i64,
}

/// Read a session's hook state, or `None` if hooks aren't installed / the file is
/// absent or unreadable (the queue then falls back to its heuristic).
pub fn read_state(session_id: &str) -> Option<HookState> {
    // Same trust boundary as `claude --resume`: never build a path from anything
    // but a canonical session id.
    if !claude::is_valid_session_id(session_id) {
        return None;
    }
    let path = state_dir().join(format!("{session_id}.json"));
    let text = std::fs::read_to_string(&path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let phase = Phase::parse(v.get("phase")?.as_str()?)?;
    let hint = v
        .get("hint")
        .and_then(|h| h.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let updated_at_ms = v.get("updated_at_ms").and_then(|u| u.as_i64()).unwrap_or(0);
    Some(HookState {
        phase,
        hint,
        updated_at_ms,
    })
}

/// A cheap hash over the hook state of the given live sessions, used by the poll
/// to decide whether the dashboard needs rebuilding. Captures phase + freshness +
/// hint without reading any transcript, so it can run every tick. Order-stable.
pub fn state_fingerprint(live_ids: &[String]) -> u64 {
    // Fast path: no state dir → feature off → constant fingerprint, no churn.
    if !state_dir().exists() {
        return 0;
    }
    let mut ids: Vec<&String> = live_ids.iter().collect();
    ids.sort();
    let mut hasher = DefaultHasher::new();
    for id in ids {
        if let Some(st) = read_state(id) {
            id.hash(&mut hasher);
            (st.phase as u8).hash(&mut hasher);
            st.updated_at_ms.hash(&mut hasher);
            st.hint.hash(&mut hasher);
        }
    }
    hasher.finish()
}

// ───────────────────────────────────────────────────────────────────────────
// Install / uninstall
// ───────────────────────────────────────────────────────────────────────────

/// Whether rune's hooks are currently present in `settings.json`.
pub fn is_installed() -> bool {
    let marker = script_path().to_string_lossy().into_owned();
    match std::fs::read_to_string(settings_path()) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(root) => has_rune_hooks(&root, &marker),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// Install the hooks (with consent). Writes the hook script, backs up
/// `settings.json` (timestamped), append-merges rune's three hook entries, and
/// writes the result atomically. Returns the backup path on success (for the UI
/// to show), or a human-readable error. Refuses to touch an unparseable file.
pub fn install() -> Result<PathBuf, String> {
    // 1. Lay down the script + the state dir first, so by the time the hooks fire
    //    their target exists.
    let dir = hooks_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    write_atomic(&script_path(), HOOK_SCRIPT.as_bytes())
        .map_err(|e| format!("write hook script: {e}"))?;
    let sdir = state_dir();
    std::fs::create_dir_all(&sdir).map_err(|e| format!("create {}: {e}", sdir.display()))?;

    // 2. Read + parse settings.json (missing → start from {}; unparseable → stop,
    //    we will not risk corrupting a file we don't understand).
    let path = settings_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut root: Value = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&existing)
            .map_err(|e| format!("{} is not valid JSON ({e}); not modifying it", path.display()))?
    };
    if !root.is_object() {
        return Err(format!("{} is not a JSON object; not modifying it", path.display()));
    }
    // Refuse a settings.json whose `hooks` is the wrong shape rather than crash on
    // it: the contract is to leave a file we don't understand untouched (and a
    // panic here would abort the whole cockpit, since this runs in a GTK handler).
    if root.get("hooks").is_some_and(|h| !h.is_object()) {
        return Err(format!(
            "{}: \"hooks\" is not a JSON object; not modifying it",
            path.display()
        ));
    }

    // 3. Timestamped backup of the current file (only if it exists).
    let mut backup = path.clone();
    if !existing.is_empty() {
        backup = path.with_extension(format!("json.rune-bak.{}", now_ms()));
        // Write the backup through the hardened path (0600, symlink-safe) rather
        // than `fs::copy`, which would follow a symlink planted at the dest.
        write_atomic(&backup, existing.as_bytes()).map_err(|e| format!("back up settings.json: {e}"))?;
        harden_and_prune_backup(&backup);
    }

    // 4. Append-merge our entries, then write atomically.
    merge_rune_hooks(&mut root, &script_path().to_string_lossy());
    write_settings(&path, &root)?;
    Ok(backup)
}

/// Remove rune's hooks and clean up. Removes only rune's own commands (matched by
/// the full script path), leaving every other hook untouched; for a
/// canonically-formatted file this restores it byte-for-byte. The timestamped
/// backups are intentionally left in place as a verbatim safety net.
pub fn uninstall() -> Result<(), String> {
    let path = settings_path();
    if let Ok(text) = std::fs::read_to_string(&path) {
        if !text.trim().is_empty() {
            let mut root: Value = serde_json::from_str(&text).map_err(|e| {
                format!("{} is not valid JSON ({e}); not modifying it", path.display())
            })?;
            remove_rune_hooks(&mut root, &script_path().to_string_lossy());
            write_settings(&path, &root)?;
        }
    }
    // Best-effort cleanup of rune's own dirs (never the user's backups).
    let _ = std::fs::remove_dir_all(hooks_dir());
    let _ = std::fs::remove_dir_all(state_dir());
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Pure JSON merge helpers (unit-tested without disk)
// ───────────────────────────────────────────────────────────────────────────

/// One rune hook entry for an event: a single command hook pointing at the
/// script with its role. Identified later by the full script path in `command`.
fn rune_entry(script: &str, role: &str) -> Value {
    json!({
        "hooks": [
            { "type": "command", "command": format!("node {} {role}", shell_quote(script)), "timeout": 5 }
        ]
    })
}

/// Single-quote a path for a shell `command` string, escaping any embedded quote.
/// (Home dirs don't normally contain quotes, but an unescaped `$`/space/quote
/// would otherwise silently break the hook.)
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Is this individual command-hook object rune's? Identity is the FULL canonical
/// script path (`marker`), not a loose basename — so a user's own command that
/// merely references some other `rune-hook.js` is never mistaken for ours.
fn command_is_rune(hook: &Value, marker: &str) -> bool {
    hook.get("command")
        .and_then(|c| c.as_str())
        .is_some_and(|c| c.contains(marker))
}

/// Does a `hooks.<Event>` array entry contain any rune command?
fn entry_has_rune(entry: &Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| arr.iter().any(|h| command_is_rune(h, marker)))
}

fn has_rune_hooks(root: &Value, marker: &str) -> bool {
    root.get("hooks")
        .and_then(|h| h.as_object())
        .is_some_and(|hooks| {
            hooks.values().any(|arr| {
                arr.as_array()
                    .is_some_and(|a| a.iter().any(|e| entry_has_rune(e, marker)))
            })
        })
}

/// Append rune's entry to each event's array (creating `hooks` / the arrays as
/// needed), skipping any event that already carries a rune entry — so a re-run is
/// idempotent and the user's own entries are never touched. Never panics: a
/// non-object `hooks` (which `install` already rejects) is left untouched.
fn merge_rune_hooks(root: &mut Value, script: &str) {
    let Some(obj) = root.as_object_mut() else {
        return;
    };
    let Some(hooks) = obj
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        return; // an existing non-object `hooks` — defended (install returns Err first)
    };
    for (event, role) in EVENTS {
        let arr = hooks.entry(event).or_insert_with(|| json!([]));
        let Some(arr) = arr.as_array_mut() else {
            continue; // a non-array under this event was hand-set; leave it be
        };
        if arr.iter().any(|e| entry_has_rune(e, script)) {
            continue; // already installed for this event
        }
        arr.push(rune_entry(script, role));
    }
}

/// Remove rune's commands (matched by full path), leaving every other command in
/// place — even one a user hand-nested inside the same entry as rune's. An entry
/// is dropped only when its `hooks` array becomes empty, and `hooks` only when it
/// becomes empty, restoring the pre-install shape.
fn remove_rune_hooks(root: &mut Value, marker: &str) {
    let Some(obj) = root.as_object_mut() else {
        return;
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };
    let mut empty_events: Vec<String> = Vec::new();
    for (event, val) in hooks.iter_mut() {
        let Some(arr) = val.as_array_mut() else {
            continue;
        };
        // Strip rune's command from inside each entry…
        for entry in arr.iter_mut() {
            if let Some(inner) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                inner.retain(|h| !command_is_rune(h, marker));
            }
        }
        // …then drop only the entries whose hook list is now empty (i.e. they
        // held nothing but rune's command). Entries without our shape are left be.
        arr.retain(|entry| match entry.get("hooks").and_then(|h| h.as_array()) {
            Some(inner) => !inner.is_empty(),
            None => true,
        });
        if arr.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks.remove(&event);
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Small utilities
// ───────────────────────────────────────────────────────────────────────────

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A settings.json backup can hold `permissions`/`env` (secrets), and `fs::copy`
/// inherits the source's (often group/world-readable) mode. Tighten the copy to
/// owner-only and prune old rune backups so they don't pile up forever.
fn harden_and_prune_backup(backup: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(backup, std::fs::Permissions::from_mode(0o600));
    }
    // Keep the few most-recent rune backups (lexically sortable: the suffix is a
    // zero-padded-enough epoch-ms counter that grows monotonically).
    const KEEP: usize = 3;
    let Some(dir) = backup.parent() else { return };
    let mut baks: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("settings.json.rune-bak."))
            })
            .collect(),
        Err(_) => return,
    };
    if baks.len() <= KEEP {
        return;
    }
    baks.sort();
    for old in &baks[..baks.len() - KEEP] {
        let _ = std::fs::remove_file(old);
    }
}

/// Write `contents` to `path` via a private temp file + rename. A crash mid-write
/// can't truncate the target; the result is **owner-only (0600)** — these files
/// can hold secrets (settings.json's `env`/`permissions`) — and a pre-planted
/// symlink or file at the temp path can't redirect or capture the write:
/// `create_new` (O_EXCL) refuses an existing path, and the temp name is
/// unpredictable (pid + nanos) so it can't be pre-created to block or hijack us.
fn write_atomic(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("rune-tmp.{}.{stamp}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(&tmp)?.write_all(contents)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Serialize a settings.json value and write it atomically, with the trailing
/// newline conventional for text/JSON files (the user's file has one, so a
/// round-trip stays byte-identical instead of stripping it).
fn write_settings(path: &std::path::Path, root: &Value) -> Result<(), String> {
    let pretty =
        serde_json::to_string_pretty(root).map_err(|e| format!("serialize settings.json: {e}"))?;
    write_atomic(path, format!("{pretty}\n").as_bytes())
        .map_err(|e| format!("write settings.json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "/home/u/.claude/rune-hooks/rune-hook.js";

    /// A representative settings.json: a Stop notify + a PostToolUse
    /// logger + a statusLine + assorted keys.
    fn sample_settings() -> Value {
        json!({
            "permissions": { "allow": ["Bash(git commit:*)"] },
            "model": "opus[1m]",
            "hooks": {
                "PostToolUse": [
                    { "matcher": "WebFetch", "hooks": [
                        { "type": "command", "command": "/home/u/.claude/hooks/log.sh", "timeout": 10 }
                    ]}
                ],
                "Stop": [
                    { "hooks": [
                        { "type": "command", "command": "canberra-gtk-play -i complete; notify-send X", "timeout": 10 }
                    ]}
                ]
            },
            "statusLine": { "type": "command", "command": "node \"/home/u/.claude/statusline.js\"" },
            "theme": "dark"
        })
    }

    #[test]
    fn merge_appends_without_clobbering_existing_stop() {
        let mut root = sample_settings();
        merge_rune_hooks(&mut root, SCRIPT);

        let hooks = root["hooks"].as_object().unwrap();
        // Stop now has the user's entry FIRST and rune's appended — never replaced.
        let stop = hooks["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert!(stop[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("canberra-gtk-play"));
        assert!(entry_has_rune(&stop[1], SCRIPT));
        // The two new events were created with a rune entry each.
        assert!(entry_has_rune(&hooks["Notification"].as_array().unwrap()[0], SCRIPT));
        assert!(entry_has_rune(&hooks["UserPromptSubmit"].as_array().unwrap()[0], SCRIPT));
        // The role argument is per-event.
        assert!(stop[1]["hooks"][0]["command"].as_str().unwrap().ends_with(" stop"));
        assert!(hooks["Notification"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with(" notify"));
        // Untouched keys survive.
        assert_eq!(root["model"], "opus[1m]");
        assert!(hooks["PostToolUse"].as_array().unwrap().len() == 1);
        assert_eq!(
            root["statusLine"]["command"],
            "node \"/home/u/.claude/statusline.js\""
        );
    }

    #[test]
    fn uninstall_restores_byte_identical() {
        // preserve_order means a merge then remove must reproduce the exact file,
        // key order and all.
        let original = sample_settings();
        let mut root = original.clone();
        merge_rune_hooks(&mut root, SCRIPT);
        assert!(has_rune_hooks(&root, SCRIPT));
        remove_rune_hooks(&mut root, SCRIPT);
        assert!(!has_rune_hooks(&root, SCRIPT));
        assert_eq!(
            serde_json::to_string_pretty(&root).unwrap(),
            serde_json::to_string_pretty(&original).unwrap()
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let mut once = sample_settings();
        merge_rune_hooks(&mut once, SCRIPT);
        let mut twice = once.clone();
        merge_rune_hooks(&mut twice, SCRIPT);
        assert_eq!(
            serde_json::to_string_pretty(&once).unwrap(),
            serde_json::to_string_pretty(&twice).unwrap()
        );
        // Exactly one rune entry on Stop, not two.
        let rune_on_stop = twice["hooks"]["Stop"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| entry_has_rune(e, SCRIPT))
            .count();
        assert_eq!(rune_on_stop, 1);
    }

    #[test]
    fn round_trip_from_empty_object() {
        let original = json!({});
        let mut root = original.clone();
        merge_rune_hooks(&mut root, SCRIPT);
        assert!(root["hooks"]["Stop"].as_array().unwrap().len() == 1);
        remove_rune_hooks(&mut root, SCRIPT);
        assert_eq!(
            serde_json::to_string_pretty(&root).unwrap(),
            serde_json::to_string_pretty(&original).unwrap()
        );
    }

    #[test]
    fn uninstall_keeps_a_user_command_co_located_in_runes_entry() {
        // A user who hand-nests their own command into the same entry object as
        // rune's must NOT lose it on uninstall (we remove rune's command, not the
        // whole entry).
        let mut root = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [
                        { "type": "command", "command": "my-own-thing" },
                        { "type": "command", "command": format!("node '{SCRIPT}' stop") }
                    ]}
                ]
            }
        });
        remove_rune_hooks(&mut root, SCRIPT);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        let inner = stop[0]["hooks"].as_array().unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["command"], "my-own-thing");
    }

    #[test]
    fn loose_basename_match_does_not_claim_a_foreign_hook() {
        // A user's own command that merely references some OTHER path ending in
        // rune-hook.js is not rune's (identity is the full canonical path).
        let foreign = json!({ "type": "command", "command": "node '/somewhere/else/rune-hook.js' x" });
        assert!(!command_is_rune(&foreign, SCRIPT));
        let ours = json!({ "type": "command", "command": format!("node '{SCRIPT}' stop") });
        assert!(command_is_rune(&ours, SCRIPT));
    }

    #[test]
    fn merge_leaves_a_non_object_hooks_untouched() {
        // install() rejects this shape; merge must at least never panic on it.
        let mut root = json!({ "hooks": ["weird"] });
        merge_rune_hooks(&mut root, SCRIPT);
        assert_eq!(root, json!({ "hooks": ["weird"] }));
    }

    /// The real end-to-end safety property on the *actual* `~/.claude/settings.json`:
    /// install must append rune's hooks while preserving the user's own (the Stop
    /// notify), and uninstall must restore the file byte-for-byte. Run with:
    ///   cargo test --quiet real_settings_round_trip -- --ignored --nocapture
    /// Safe: it takes an out-of-band backup and restores the file before exiting.
    #[test]
    #[ignore = "mutates the real ~/.claude/settings.json (round-trips it back)"]
    fn real_settings_round_trip() {
        let path = settings_path();
        let before = std::fs::read_to_string(&path).expect("read settings.json");
        std::fs::write("/tmp/rune-settings-safety.json", &before).expect("safety copy");

        assert!(!is_installed(), "expected hooks NOT installed at start");
        let backup = install().expect("install");

        let installed = std::fs::read_to_string(&path).unwrap();
        assert!(is_installed(), "is_installed() false after install");
        assert!(installed.contains("rune-hook.js"), "rune hook command missing");
        // The user's own Stop notify must survive untouched alongside rune's entry.
        assert!(
            installed.contains("canberra-gtk-play"),
            "clobbered the user's Stop hook!"
        );
        for ev in ["Notification", "UserPromptSubmit"] {
            assert!(installed.contains(ev), "missing {ev} hook");
        }

        uninstall().expect("uninstall");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "settings.json not restored byte-for-byte");

        // Don't litter ~/.claude with the timestamped backup this test created.
        let _ = std::fs::remove_file(&backup);
        eprintln!("round-trip OK; transient backup was {}", backup.display());
    }

    #[test]
    fn phase_parse() {
        assert_eq!(Phase::parse("working"), Some(Phase::Working));
        assert_eq!(Phase::parse("awaiting_permission"), Some(Phase::AwaitingPermission));
        assert_eq!(Phase::parse("awaiting_input"), Some(Phase::AwaitingInput));
        assert_eq!(Phase::parse("finished"), Some(Phase::Finished));
        assert_eq!(Phase::parse("nonsense"), None);
    }
}
