// rune — a native (GTK4 + VTE, no webview) Linux control cockpit for Claude
// Code sessions. Independent project; not affiliated with Anthropic. Reads
// files written by the official `claude` CLI and spawns it as a subprocess.

mod claude;
mod config;
mod cost;
mod git;
mod hooks;
mod palette;
mod queue;
mod sessions;
mod status;
mod terminal;
mod ui;

use gtk4::prelude::*;
use gtk4::{glib, Application};

pub const APP_ID: &str = "io.github.ahmed7991.rune";

fn main() -> glib::ExitCode {
    // Pick the GSK renderer before GTK initializes (GSK reads GSK_RENDERER when
    // the first surface is realized). Default to `ngl`: GTK 4.16+ otherwise
    // auto-selects Vulkan, which crashes on Wayland + NVIDIA. A saved preference
    // overrides the default; an explicit `GSK_RENDERER=… rune` overrides both.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // Only an allowlisted value reaches the env — a hand-edited/synced config
        // can't push an arbitrary string into GSK_RENDERER (it's the same set the
        // Settings UI offers; anything else falls back to the safe default).
        const RENDERERS: [&str; 4] = ["ngl", "gl", "vulkan", "cairo"];
        let renderer = config::Config::load()
            .settings
            .gsk_renderer
            .filter(|r| RENDERERS.contains(&r.as_str()))
            .unwrap_or_else(|| "ngl".to_string());
        std::env::set_var("GSK_RENDERER", renderer);
    }

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(ui::build_ui);
    // Pass no args to GTK — we don't expose any GTK command-line options, and
    // this keeps cargo's args from being parsed as app options.
    app.run_with_args::<&str>(&[])
}
