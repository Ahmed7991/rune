#!/usr/bin/env node
// rune-hook.js — installed by rune's opt-in "Exact awaiting-input" toggle, which
// appends three hooks (Stop / Notification / UserPromptSubmit) to the user's
// ~/.claude/settings.json, each pointing here with a role argument.
//
// It reads one Claude Code hook event on stdin and records a tiny per-session
// state file at ~/.claude/rune-state/<session_id>.json:
//     { session_id, phase, hint, updated_at_ms }
// rune's needs-you queue reads those files to tell "your turn" from "paused at a
// permission" exactly, and to show an honest activity hint (the task it's on, or
// the tool it wants to use).
//
// Contract: a hook must NEVER break Claude. This script always exits 0 and writes
// nothing on any error (missing/invalid input, unwritable dir, …). When it writes
// nothing, rune simply falls back to its read-only busy/idle heuristic.

'use strict';
const fs = require('fs');
const os = require('os');
const path = require('path');

function bail() { process.exit(0); }

const role = process.argv[2] || '';

let raw = '';
try {
  raw = fs.readFileSync(0, 'utf8'); // fd 0 = stdin; blocks until Claude closes it
} catch (_) { bail(); }

let ev;
try { ev = JSON.parse(raw); } catch (_) { bail(); }
if (!ev || typeof ev !== 'object') bail();

const sid = ev.session_id;
// Only ever touch a file named after a canonical session UUID — the same
// trust boundary rune enforces before `claude --resume`.
if (typeof sid !== 'string' || !/^[0-9a-fA-F-]{36}$/.test(sid)) bail();

function clean(s, n) {
  s = String(s == null ? '' : s).replace(/\s+/g, ' ').trim();
  return s.length > n ? s.slice(0, n - 1) + '…' : s;
}

let phase;
let hint = '';
if (role === 'prompt') {
  // The user just submitted — Claude is (about to be) working. The prompt itself
  // is the honest "what is this agent doing" hint.
  phase = 'working';
  hint = clean(ev.prompt, 80);
} else if (role === 'stop') {
  // The turn finished — it's your turn for the next prompt.
  phase = 'finished';
} else if (role === 'notify') {
  // A notification. Claude Code carries a STRUCTURED discriminator,
  // `notification_type` — use it; the free-text `message` is unreliable
  // (the default permission text doesn't contain the word "permission").
  //   permission_prompt / elicitation_dialog → blocked, needs an answer now
  //   idle_prompt                            → idle, your turn
  //   auth_success / elicitation_complete / elicitation_response → transient,
  //                                            not a "needs you" state → do nothing
  //
  // EMPIRICAL NOTE (Claude Code v2.1.185, 2026-06-22): a session sitting at a
  // *permission* dialog fires `idle_prompt` ("Claude is waiting for your input"),
  // NOT `permission_prompt` — so in practice a permission wait currently reads as
  // "your turn", not the distinct Blocked state. The `permission_prompt` branch
  // below is correct and forward-compatible: it lights up the moment a Claude
  // version actually emits that type.
  const nt = String(ev.notification_type || '');
  const msg = String(ev.message || '');
  if (nt) {
    if (nt === 'permission_prompt' || nt === 'elicitation_dialog') {
      phase = 'awaiting_permission';
      // Permission message is e.g. "Permission needed to run: Bash(npm test)".
      const m = msg.match(/(?:run|use):?\s*([A-Za-z0-9_.-]+)/i);
      hint = m ? m[1] : clean(msg, 80);
    } else if (nt === 'idle_prompt') {
      phase = 'awaiting_input';
    } else {
      bail(); // transient / informational — leave the existing state alone
    }
  } else {
    // Older Claude with no notification_type → fall back to the message text.
    if (/permission|approve|\ballow\b/i.test(msg)) {
      phase = 'awaiting_permission';
      const m = msg.match(/(?:run|use)(?: the)?:?\s*([A-Za-z0-9_.-]+)/i);
      hint = m ? m[1] : clean(msg, 80);
    } else {
      phase = 'awaiting_input';
    }
  }
} else {
  bail(); // unknown role — do nothing
}

const dir = path.join(os.homedir(), '.claude', 'rune-state');
try {
  fs.mkdirSync(dir, { recursive: true });
  const out = JSON.stringify({
    session_id: sid,
    phase: phase,
    hint: hint,
    updated_at_ms: Date.now(),
  });
  // Write to a unique temp file then rename, so rune never reads a half-written
  // file. The pid keeps two near-simultaneous events for one session from
  // clobbering each other's temp file.
  const tmp = path.join(dir, '.' + sid + '.' + process.pid + '.tmp');
  fs.writeFileSync(tmp, out);
  fs.renameSync(tmp, path.join(dir, sid + '.json'));
} catch (_) { /* unwritable — fall back to the heuristic, never break Claude */ }

bail();
