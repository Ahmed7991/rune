<div align="center">

# rune

**A native cockpit for your Claude Code sessions — see every project and agent in one place, and steer them.**

![rune — the "Mission Control" home dashboard: a dark header with the wordmark + search, a project rail with per-project colour swatches and live status badges, a cross-project needs-you command board with status pills and actions, project cards with per-project cost, and recent sessions with model / branch / context chips](./docs/screenshot.png)

</div>

> *Independent project. Not affiliated with, endorsed by, or a product of Anthropic. rune reads files written by the official `claude` CLI and spawns it as a subprocess. "Claude" and "Claude Code" are trademarks of Anthropic, PBC.*

---

## What it is

rune is a desktop control room for [Claude Code](https://docs.anthropic.com/en/docs/claude-code). Instead of one terminal window per project, you get a single cockpit that shows **every** Claude session across all your projects, tells you which ones **need you** right now, and lets you launch, resume, and reply to them — without losing your place.

- **Native, no webview** — **GTK4 + VTE**, written in **Rust**. VTE owns the PTY in-process, so terminal input is correct on **Wayland and Xorg** alike: real keyboard/mouse, Arabic/RTL, copy-paste, no click-through.
- **A cockpit, not just a terminal grid** — a cross-project "needs-you" queue, live per-session status, estimated cost, and one-click launch / resume / reply.
- **Local, and read-only by default** — it reads the files the `claude` CLI already writes under `~/.claude`; it never proxies the API. The one thing that *writes* (opt-in hooks) is consent-gated, backed up first, and reversible.

## Features

- **Mission-Control dashboard** — opens to a cross-project overview: a greeting glance ("*N agents want you*"), the **needs-you command board** (one row per live session with a status pill — your-turn / working / finished — and an action), **project cards** (click → new session, each with its estimated total cost), and **recent sessions** (click → resume) showing a model chip, git branch, and a context-fill bar.
- **Project rail** — a sidebar with a per-project identity colour, a live status badge, and an accent stripe on the active project. **Pinned** projects sit above an auto-discovered **Recent** section (any project with on-disk Claude history from the last 14 days), and **clicking a project opens its full session list** in the main pane. Add / remove / reorder / pin from the right-click menu.
- **Tabbed `claude` terminals** — one session per tab, spawned in the correct project directory, with correct input and copy-paste (`Ctrl+Shift+C/V` or right-click).
- **Browse & resume** — list a project's past sessions (real titles, last-active, prompt count) and resume any of them in a click.
- **Live status + notifications** — a per-tab dot (working / idle / not-running) read live from `~/.claude`, plus a desktop notification when a background session finishes.
- **Cross-project "needs-you" queue** — every live Claude session on the machine (not just rune's tabs), classified and one header button away from anywhere.
- **Ctrl-K quick-switcher** — a fuzzy palette over every project and recent session.
- **Exact awaiting-input (opt-in)** — a consent-gated Settings toggle installs hooks into `~/.claude/settings.json` (backed up first, append-merged so your own hooks are untouched, fully reversible) so each row's state is *exact* — working (with the **task it's on** as a live hint), finished, your-turn — instead of guessed.
- **Per-project launch presets** — each project remembers the **model** (Opus / Sonnet / Haiku / Fable) and **permission mode** (ask / accept-edits / plan / bypass) rune starts its sessions with. Right-click a project → **Launch settings…**.
- **Reply from the dashboard** — a session rune hosts that's waiting on you gets a **Reply** action: type a message and it's fed straight into that session's terminal.
- **Persistence** — your open tabs and projects come back (and resume) on the next launch. Config lives in `~/.config/rune/config.json`.

## Install

You need the official [`claude`](https://docs.anthropic.com/en/docs/claude-code) CLI on your `PATH` — rune drives it.

**Debian / Ubuntu — the `.deb`** (apt pulls the GTK4 / VTE / rsvg runtime deps):

```bash
bash packaging/build-deb.sh                          # → dist/rune_<version>_amd64.deb
sudo apt install ./dist/rune_<version>_amd64.deb     # then launch "rune" from your app grid
```

*Prebuilt `.deb`s are attached to each [release](https://github.com/Ahmed7991/rune/releases). Verified end-to-end on a clean Debian 13.*

**Any distro — build from source** (needs Rust + the GTK4/VTE dev headers):

```bash
sudo apt install libgtk-4-dev libvte-2.91-gtk4-dev build-essential pkg-config   # or your distro's equivalent
cargo build --release && ./target/release/rune
bash packaging/install.sh          # optional: no-root install (binary + .desktop + icon under ~/.local)
```

Pick a project → **＋** (header) to start a `claude` session in a new tab; tabs persist and resume on the next launch.

> **Scope & caveats.** Linux + a GTK4/VTE desktop (developed on GNOME, x86-64); Claude-only. Built on Ubuntu 24.04, validated on Debian 13. rune reads undocumented `~/.claude` internals (sessions, transcripts) and degrades gracefully if they change. NVIDIA + Wayland is handled automatically — rune defaults `GSK_RENDERER` to `ngl` (GTK ≥ 4.16's default Vulkan renderer crashes on some NVIDIA/Wayland combos); override it in Settings or `GSK_RENDERER=… rune`. This is an early release — expect rough edges, and please file issues.

## License

[MIT](./LICENSE) © 2026 Ahmed7991
