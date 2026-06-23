//! The VTE terminal that hosts one `claude` session.
//!
//! All of the spawn + clipboard logic here was validated by the input spike on
//! real Wayland **and** Xorg (correct mouse/keyboard/Arabic, no click-through,
//! copy/paste). The only additions over the spike: resume-vs-new selection, a
//! full child environment (so PATH survives), and a shell wrapper that keeps
//! the tab usable after `claude` exits.

use gtk4::prelude::*;
use gtk4::{gdk, gio, glib, EventControllerKey, GestureClick, PopoverMenu, PropagationPhase};
use vte4::{Format, PtyFlags, Terminal, TerminalExt, TerminalExtManual};

use crate::claude;
use crate::config::ProjectPreset;

/// `--model` aliases rune offers as presets (an allowlist — only these are ever
/// interpolated into the launch command). The values are interpolated UNQUOTED,
/// so every token MUST be `[A-Za-z]`-only (enforced by a test below); a token
/// with a shell metacharacter would reintroduce an injection hazard.
pub(crate) const MODEL_ALIASES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];
/// `--permission-mode` values rune offers (allowlist). `default` is implicit, so
/// it's never passed. Same `[A-Za-z]`-only invariant as `MODEL_ALIASES`.
pub(crate) const PERMISSION_MODES: [&str; 3] = ["acceptEdits", "plan", "bypassPermissions"];

/// Build the extra `claude` flags for a project's launch preset. Only values that
/// match a hardcoded allowlist are interpolated, so a hand-edited/hostile config
/// value is dropped rather than injected — and the allowlisted tokens are all
/// `[a-zA-Z]` with no shell metacharacters, so the interpolation is safe.
fn preset_flags(preset: &ProjectPreset) -> String {
    let mut flags = String::new();
    if let Some(m) = preset.model.as_deref() {
        if MODEL_ALIASES.contains(&m) {
            flags.push_str(&format!(" --model {m}"));
        }
    }
    if let Some(pm) = preset.permission_mode.as_deref() {
        if PERMISSION_MODES.contains(&pm) {
            flags.push_str(&format!(" --permission-mode {pm}"));
        }
    }
    flags
}

/// Build and return a VTE terminal running `claude` for `project_path`, launched
/// with the project's preset (`--model` / `--permission-mode`). Resumes
/// `session_id` if a transcript exists, else starts it new.
pub fn spawn_session(project_path: &str, session_id: &str, preset: &ProjectPreset) -> Terminal {
    let terminal = Terminal::new();
    terminal.set_scrollback_lines(100_000);
    terminal.set_hexpand(true);
    terminal.set_vexpand(true);

    wire_clipboard(&terminal);

    // Build the child command. Everything that must be true of claude's
    // environment is done in-shell, because VTE *merges* the spawn envv onto
    // the inherited parent env (it does not replace it) — so omitting a var
    // can't unset it. Three steps:
    //  1. unset Claude Code's own control/IPC markers we may have inherited
    //     (rune is often launched via `cargo run` from inside a claude tab),
    //     so the spawned claude doesn't think it's a nested child session;
    //  2. export the attribution + clean-repaint hints;
    //  3. resume or start the session, then drop to an interactive shell so a
    //     finished session doesn't take the tab down with it.
    // The session id is the positional arg $1 — never interpolated into the
    // script text — so a malformed/hostile id is data, never shell code.
    // `-l` gives the login PATH (~/.local/bin, where `claude` lives).
    let flag = if claude::transcript_exists(project_path, session_id) {
        "--resume"
    } else {
        "--session-id"
    };
    // Preset flags are allowlisted constant tokens (see `preset_flags`); the
    // session id stays the positional `$1` (data, never code).
    let extra = preset_flags(preset);
    let command = format!(
        "unset CLAUDECODE AI_AGENT ${{!CLAUDE_CODE_*}} 2>/dev/null; \
         export TERM_PROGRAM=rune CLAUDE_CODE_NO_FLICKER=1 CLAUDE_CODE_FORCE_SYNC_OUTPUT=1; \
         claude {flag} \"$1\"{extra}; exec bash -i"
    );

    let project_for_err = project_path.to_string();
    // Explicit ABSOLUTE working directory — a relative "." would resolve to the
    // launcher dir / $HOME, not the project. The child
    // inherits rune's env; the script above scrubs and sets what matters.
    // argv tail: $0=bash, $1=session_id (data only).
    terminal.spawn_async(
        PtyFlags::DEFAULT,
        Some(project_path),
        &["/bin/bash", "-lc", command.as_str(), "bash", session_id],
        &[],
        glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        gio::Cancellable::NONE,
        move |res| {
            if let Err(err) = res {
                eprintln!("rune: failed to spawn claude in {project_for_err}: {err}");
            }
        },
    );

    terminal
}

/// Conventional terminal copy/paste: Ctrl+Shift+C/V (VTE binds no clipboard
/// keys by default) plus a right-click Copy/Paste menu. Lifted from the spike.
fn wire_clipboard(terminal: &Terminal) {
    let key = EventControllerKey::new();
    key.set_propagation_phase(PropagationPhase::Capture);
    key.connect_key_pressed(glib::clone!(
        #[weak]
        terminal,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, keyval, _code, state| {
            let ctrl_shift = state.contains(gdk::ModifierType::CONTROL_MASK)
                && state.contains(gdk::ModifierType::SHIFT_MASK);
            if ctrl_shift && (keyval == gdk::Key::C || keyval == gdk::Key::c) {
                terminal.copy_clipboard_format(Format::Text);
                return glib::Propagation::Stop;
            }
            if ctrl_shift && (keyval == gdk::Key::V || keyval == gdk::Key::v) {
                terminal.paste_clipboard();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    ));
    terminal.add_controller(key);

    let actions = gio::SimpleActionGroup::new();
    let copy = gio::SimpleAction::new("copy", None);
    copy.connect_activate(glib::clone!(
        #[weak]
        terminal,
        move |_, _| terminal.copy_clipboard_format(Format::Text)
    ));
    let paste = gio::SimpleAction::new("paste", None);
    paste.connect_activate(glib::clone!(
        #[weak]
        terminal,
        move |_, _| terminal.paste_clipboard()
    ));
    actions.add_action(&copy);
    actions.add_action(&paste);
    terminal.insert_action_group("term", Some(&actions));

    let menu = gio::Menu::new();
    menu.append(Some("Copy"), Some("term.copy"));
    menu.append(Some("Paste"), Some("term.paste"));
    let popover = PopoverMenu::from_model(Some(&menu));
    popover.set_parent(terminal);
    popover.set_has_arrow(false);
    popover.set_halign(gtk4::Align::Start);

    let right_click = GestureClick::new();
    right_click.set_button(gdk::BUTTON_SECONDARY);
    right_click.connect_pressed(glib::clone!(
        #[weak]
        popover,
        move |_, _n, x, y| {
            popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    ));
    terminal.add_controller(right_click);

    // A PopoverMenu set as a child must be unparented before its parent is
    // finalized, or GTK warns on every tab close. Drop it when the terminal goes.
    terminal.connect_destroy(glib::clone!(
        #[weak]
        popover,
        move |_| popover.unparent()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(model: Option<&str>, mode: Option<&str>) -> ProjectPreset {
        ProjectPreset {
            model: model.map(String::from),
            permission_mode: mode.map(String::from),
        }
    }

    #[test]
    fn default_preset_adds_no_flags() {
        assert_eq!(preset_flags(&preset(None, None)), "");
    }

    #[test]
    fn allowlisted_values_become_flags() {
        assert_eq!(preset_flags(&preset(Some("opus"), None)), " --model opus");
        assert_eq!(
            preset_flags(&preset(Some("sonnet"), Some("acceptEdits"))),
            " --model sonnet --permission-mode acceptEdits"
        );
    }

    #[test]
    fn non_allowlisted_values_are_dropped_not_injected() {
        // A hand-edited / hostile config value never reaches the command string.
        assert_eq!(preset_flags(&preset(Some("opus; rm -rf ~"), None)), "");
        assert_eq!(preset_flags(&preset(Some("$(reboot)"), None)), "");
        assert_eq!(preset_flags(&preset(None, Some("plan; curl evil"))), "");
        // "default" is not in the permission allowlist (it's implicit) → not passed.
        assert_eq!(preset_flags(&preset(None, Some("default"))), "");
    }

    #[test]
    fn allowlist_tokens_are_metacharacter_free() {
        // The whole non-injection argument rests on every allowlisted token being
        // shell-safe, since `preset_flags` interpolates it UNQUOTED. Enforce that
        // invariant so the allowlist can't be widened with an unsafe token (a
        // space, '=', '[', ';', '$', …) without this failing.
        for t in MODEL_ALIASES.iter().chain(PERMISSION_MODES.iter()) {
            assert!(
                !t.is_empty() && t.chars().all(|c| c.is_ascii_alphabetic()),
                "allowlist token {t:?} is not [A-Za-z]-only — unsafe to interpolate unquoted"
            );
        }
    }
}
