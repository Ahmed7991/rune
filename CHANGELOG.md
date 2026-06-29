# Changelog

All notable changes to **rune**. Format based on [Keep a Changelog](https://keepachangelog.com/).

## [0.1.2] — 2026-06-29

### Added
- **The project rail now auto-discovers your projects.** Above the projects you've
  pinned, the rail shows every project with recent on-disk Claude history (active
  in the last 14 days) under a **Recent** section — so a project you worked in but
  never added is still one click away (handy after a crash, when you don't remember
  which sessions were open). Right-click a discovered project to **pin** it.
- **Click a project to open its sessions.** Selecting a project now opens its full
  session history in the main pane — newest first, each one resumable, plus a
  one-click **New session**. Works for pinned and discovered projects alike.

## [0.1.1] — 2026-06-23

Security hardening, from a multi-agent adversarial review (no critical or high
issues found — no network/telemetry, no shell injection, no path traversal in
transcript reads, no markup injection).

### Security
- Config/settings writes are now atomic, **owner-only (0600)**, and symlink-safe
  (`create_new` + an unpredictable temp name) — `settings.json` (which can hold
  `env` secrets), the hook script, `config.json`, and their backups are no longer
  left world-readable and can't have their write redirected by a planted symlink
  on a shared/synced home.
- Transcript reads are **size-capped (25 MB)** so a malicious multi-GB `.jsonl`
  can't freeze or OOM the app.
- Session titles from transcripts are sanitized (control chars + bidi/zero-width
  overrides stripped, length-capped) before reaching desktop notifications/labels.
- `GSK_RENDERER` read from config is allowlisted before use.

## [0.1.0] — 2026-06-23

First public release — a native (GTK4 + VTE, no webview) Linux cockpit for Claude Code.

### Added
- **Mission-Control home dashboard** — a cross-project overview: a greeting glance, the **needs-you command board** (one row per live session, status pill + action), **project cards** with per-project cost, and **recent sessions** with model / branch / context chips.
- **Project rail** — per-project identity colours and live status badges; add / remove / reorder.
- **Tabbed `claude` terminals** — correct input on Wayland *and* Xorg, correct working directory, copy-paste.
- **Browse & resume** past sessions; **live per-tab status** + a desktop notification when a background session finishes.
- **Cross-project "needs-you" queue** and a **Ctrl-K** fuzzy quick-switcher.
- **Estimated cost** per session and per project, from transcript token usage.
- **Exact awaiting-input (opt-in)** — a consent-gated, reversible hooks installer that makes each session's state exact and surfaces the live task as a hint.
- **Per-project launch presets** — remembers the model + permission mode per project.
- **Reply from the dashboard** — feed a message to a hosted session without switching to it.
- **Settings** (font, theme, GPU renderer), tab/project **persistence**, a `.deb` package, and a bespoke app icon.

[0.1.2]: https://github.com/Ahmed7991/rune/releases/tag/v0.1.2
[0.1.1]: https://github.com/Ahmed7991/rune/releases/tag/v0.1.1
[0.1.0]: https://github.com/Ahmed7991/rune/releases/tag/v0.1.0
