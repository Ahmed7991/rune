//! Persistent app state: `~/.config/rune/config.json`.
//!
//! Holds the ordered project list, the open tabs (so sessions restore +
//! resume across restarts), and the last-selected project. On first run the
//! project list is seeded from xshell's settings if present, so an xshell user
//! lands with their projects already in the sidebar.

use std::collections::HashMap;
use std::path::PathBuf;

use gtk4::glib;
use serde::{Deserialize, Serialize};

/// One restorable tab = a Claude session bound to a project. `session_id` is the
/// manager-owned UUID; on restore we `claude --resume <id>` (or fall back to
/// `--session-id <id>` if no transcript exists yet).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct OpenTab {
    pub project_path: String,
    pub session_id: String,
    #[serde(default)]
    pub title: String,
}

/// User-tunable app settings (the §3.7 settings panel). All optional so an old
/// config (or a hand-edit) stays valid and unset fields fall back to defaults.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Pango font for the terminals, e.g. "Monospace 12". `None` = VTE default.
    pub terminal_font: Option<String>,
    /// Prefer the dark GTK theme for the app chrome.
    pub prefer_dark: Option<bool>,
    /// `GSK_RENDERER` to export before GTK starts (applies on next launch).
    /// `None` = use the built-in default (`ngl`), which dodges the GTK
    /// Vulkan-on-NVIDIA crash; set it to override (gl / vulkan / cairo / …).
    pub gsk_renderer: Option<String>,
}

/// A per-project launch preset: how `claude` should be started for this project.
/// All optional so an empty/old config is valid and an unset field means "use the
/// global default" (don't pass the flag).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectPreset {
    /// `--model` alias (`opus` / `sonnet` / `haiku` / `fable`). `None` = the
    /// model from the user's own settings.
    pub model: Option<String>,
    /// `--permission-mode` (`acceptEdits` / `plan` / `bypassPermissions`).
    /// `None` = the default (ask before changes).
    pub permission_mode: Option<String>,
}

impl ProjectPreset {
    /// Whether this preset is entirely default (nothing to launch with / store).
    pub fn is_default(&self) -> bool {
        self.model.is_none() && self.permission_mode.is_none()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// Ordered project paths shown in the sidebar.
    pub projects: Vec<String>,
    /// Tabs open at last save, in tab-strip order.
    pub open_tabs: Vec<OpenTab>,
    /// Last-selected sidebar project (so "+ New session" targets it on launch).
    pub selected_project: Option<String>,
    /// App settings (theme/font/renderer).
    pub settings: Settings,
    /// Per-project launch presets, keyed by normalized project path.
    pub project_presets: HashMap<String, ProjectPreset>,
}

impl Config {
    /// The launch preset for a project, keyed by its normalized path. Matching is
    /// exact-cwd (trailing slash aside), consistent with the rail's identity model
    /// — a session whose cwd is a *subdirectory* of a rail project gets the
    /// default preset, not the parent's.
    pub fn preset_for(&self, project_path: &str) -> ProjectPreset {
        self.project_presets
            .get(&normalize_project_path(project_path))
            .cloned()
            .unwrap_or_default()
    }

    /// Store (or clear, when default) a project's launch preset.
    pub fn set_preset(&mut self, project_path: &str, preset: ProjectPreset) {
        let key = normalize_project_path(project_path);
        if preset.is_default() {
            self.project_presets.remove(&key);
        } else {
            self.project_presets.insert(key, preset);
        }
    }
}

pub fn config_dir() -> PathBuf {
    glib::user_config_dir().join("rune")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Config {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => match serde_json::from_str::<Config>(&s) {
                Ok(cfg) => cfg,
                Err(e) => {
                    // Don't silently overwrite an unparseable config on the next
                    // save — preserve it as .corrupt so the user can recover, and
                    // re-bootstrap the sidebar from xshell (same as a first run).
                    eprintln!(
                        "rune: {} is unreadable ({e}); kept as .corrupt, starting fresh",
                        path.display()
                    );
                    let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
                    Config {
                        projects: seed_projects_from_xshell(),
                        ..Config::default()
                    }
                }
            },
            Err(_) => Config {
                // First run: no config yet — seed projects from xshell if present.
                projects: seed_projects_from_xshell(),
                ..Config::default()
            },
        }
    }

    pub fn save(&self) {
        let dir = config_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("rune: cannot create {}: {e}", dir.display());
            return;
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("rune: failed to serialize config: {e}");
                return;
            }
        };
        let path = config_path();
        // Keep one prior-good copy so any bad write stays reversible (written
        // through the hardened path: 0600, symlink-safe, not `fs::copy` which
        // would follow a symlink planted at the .bak destination).
        if let Ok(old) = std::fs::read(&path) {
            let _ = write_private(&path.with_extension("json.bak"), &old);
        }
        // Write to a private temp then rename, so a crash mid-write can't truncate
        // the existing config and a planted symlink can't redirect the write.
        if let Err(e) = write_private(&path, json.as_bytes()) {
            eprintln!("rune: failed to write {}: {e}", path.display());
        }
    }
}

/// Write `contents` to `path` atomically and **owner-only (0600)**, refusing to
/// follow or be hijacked by a pre-planted symlink/file at the temp path
/// (`create_new` = O_EXCL + an unpredictable temp name). Mirrors
/// `hooks::write_atomic`.
fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
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

/// Normalize a project path for storage + dedup: strip a trailing `/` so
/// `/x/p` and `/x/p/` are one project (and match the slash-free directory name
/// Claude derives from its own normalized cwd).
pub fn normalize_project_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Best-effort: read project paths out of xshell's store so an existing user
/// starts with their sidebar populated. Never fails the app — returns empty on
/// any problem.
fn seed_projects_from_xshell() -> Vec<String> {
    let path = glib::home_dir().join(".local/share/com.xshell.app/settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(arr) = value.get("project_paths").and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for item in arr {
        if let Some(s) = item.as_str() {
            let path = normalize_project_path(s);
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_round_trips_and_normalizes_the_key() {
        let mut cfg = Config::default();
        let preset = ProjectPreset {
            model: Some("opus".into()),
            permission_mode: Some("plan".into()),
        };
        // A trailing slash on store must match a lookup without it (and vice versa).
        cfg.set_preset("/home/u/proj/", preset.clone());
        assert_eq!(cfg.preset_for("/home/u/proj"), preset);
        assert_eq!(cfg.preset_for("/home/u/proj/"), preset);
    }

    #[test]
    fn unknown_project_gets_the_default_preset() {
        let cfg = Config::default();
        assert!(cfg.preset_for("/nope").is_default());
    }

    #[test]
    fn setting_a_default_preset_clears_the_entry() {
        let mut cfg = Config::default();
        cfg.set_preset("/p", ProjectPreset { model: Some("opus".into()), permission_mode: None });
        assert_eq!(cfg.project_presets.len(), 1);
        cfg.set_preset("/p", ProjectPreset::default());
        assert!(cfg.project_presets.is_empty(), "an all-default preset should be removed, not stored");
    }
}
