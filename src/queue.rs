//! The cross-project "needs-you" queue — the cockpit headline.
//!
//! Joins three read-only signals into one actionable list of every live Claude
//! session on the machine, regardless of project or whether rune has a tab open
//! for it:
//!   * `status::live_sessions()` → which sessions are running, their cwd, and
//!     whether each is *working* (busy) or *waiting on you* (idle);
//!   * `sessions::session_summary()` → a human title + estimated cost from the
//!     session's transcript.
//!
//! Honest ceiling: with read-only signals alone we can tell *working* from
//! *waiting*, not *waiting-for-a-permission* from *waiting-for-your-next-prompt*
//! — that exactness needs the opt-in hooks installer (P2.x). So "idle" is framed
//! as "your turn", with a "just finished" hint for sessions that went idle very
//! recently. **When the hooks ARE installed**, each session's
//! `rune-state/<id>.json` upgrades that classification to exact (adding the
//! `Blocked` = paused-at-a-permission state) and carries an activity hint.

use crate::claude;
use crate::hooks::{self, HookState, Phase};
use crate::sessions;
use crate::status::{self, Status};

/// How recently a session must have gone idle to read as "just finished" rather
/// than "your turn" (it's been waiting a while). Milliseconds.
const JUST_FINISHED_WINDOW_MS: i64 = 90_000;

/// What a queue row is asking of you.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Need {
    /// Paused on a permission request — it can't proceed without your yes/no.
    /// Only ever produced from the opt-in hooks (the heuristic can't see it).
    Blocked,
    /// Idle and went idle recently — it likely just finished a task.
    JustFinished,
    /// Idle and has been waiting — your turn.
    YourTurn,
    /// Claude is still working — informational, no action needed.
    Working,
}

impl Need {
    /// Sort rank: things that need you come before things that don't, most
    /// urgent first — a permission block stalls all progress, so it leads.
    fn rank(self) -> u8 {
        match self {
            Need::Blocked => 0,
            Need::JustFinished => 1,
            Need::YourTurn => 2,
            Need::Working => 3,
        }
    }
}

pub struct QueueEntry {
    pub session_id: String,
    pub project_path: String,
    pub title: String,
    pub need: Need,
    pub updated_at: i64,
    pub cost_usd: f64,
    /// A short activity hint from the hooks (the task it's on, or the tool it
    /// wants permission for). `None` when hooks are off or there's nothing to say.
    pub hint: Option<String>,
}

/// Build the queue from the current live sessions, ordered with the ones that
/// need you first (just-finished, then waiting), working sessions last; within a
/// group, most-recently-updated first.
pub fn build_queue() -> Vec<QueueEntry> {
    let now_ms = now_epoch_ms();
    let mut entries: Vec<QueueEntry> = status::live_sessions()
        .into_iter()
        // Same trust-boundary guard the resume browser + config-restore enforce:
        // a session id only ever reaches `claude --resume`/path-formatting if it
        // is a real UUID. Claude always writes canonical ids, so this is
        // defense-in-depth, kept symmetric with the other spawn paths.
        .filter(|ls| claude::is_valid_session_id(&ls.session_id))
        .map(|ls| {
            let meta = sessions::session_summary(&ls.cwd, &ls.session_id);
            let (title, cost_usd) = match meta {
                Some(m) => (m.title, m.cost_usd),
                // A live session with no transcript yet (just spawned) — still
                // worth showing; fall back to a short id.
                None => (short_id(&ls.session_id), 0.0),
            };
            // When the opt-in hooks are on, this upgrades the busy/idle guess to
            // an exact phase + activity hint; otherwise it's `None` and we use the
            // heuristic. (No file = no effect — cheap and self-gating.)
            let hook = hooks::read_state(&ls.session_id);
            let (need, hint) = classify(ls.status, ls.updated_at, now_ms, hook.as_ref());
            QueueEntry {
                session_id: ls.session_id,
                project_path: ls.cwd,
                title,
                need,
                updated_at: ls.updated_at,
                cost_usd,
                hint,
            }
        })
        .collect();

    entries.sort_by(|a, b| {
        a.need
            .rank()
            .cmp(&b.need.rank())
            .then(b.updated_at.cmp(&a.updated_at))
    });
    entries
}

/// How many sessions are waiting on you right now (the badge count). A blocked
/// session needs you most of all.
pub fn waiting_count(entries: &[QueueEntry]) -> usize {
    entries
        .iter()
        .filter(|e| matches!(e.need, Need::Blocked | Need::YourTurn | Need::JustFinished))
        .count()
}

/// How many sessions are still working (for the summary line / tooltip).
pub fn working_count(entries: &[QueueEntry]) -> usize {
    entries.iter().filter(|e| e.need == Need::Working).count()
}

/// How many sessions are paused on a permission prompt (the exact, hooks-only
/// state). Surfaced at a glance because it's the single most urgent thing.
pub fn blocked_count(entries: &[QueueEntry]) -> usize {
    entries.iter().filter(|e| e.need == Need::Blocked).count()
}

/// The reconciled need for one live session — the SAME classification the command
/// board uses (hook state preferred when fresher), but WITHOUT reading the
/// transcript (no title/cost). Cheap enough to run on every 1.5s poll, so the
/// header badge and the project rail can agree with the dashboard instead of
/// reading the raw busy/idle heartbeat (which can't see `Blocked`).
pub fn live_need(ls: &status::LiveSession) -> Need {
    let now = now_epoch_ms();
    let hook = hooks::read_state(&ls.session_id);
    classify(ls.status, ls.updated_at, now, hook.as_ref()).0
}

/// Whether a need is one that wants your attention (vs just working).
pub fn need_is_waiting(need: Need) -> bool {
    matches!(need, Need::Blocked | Need::YourTurn | Need::JustFinished)
}

/// Classify a session's need, preferring the hook state when it's the *fresher*
/// signal (so a stale state file can't override a newer heartbeat, and a live
/// "busy" heartbeat is never reported as blocked). Falls back to the busy/idle
/// heuristic when hooks are off or the heartbeat is newer. Also returns the
/// activity hint, if any.
fn classify(
    status: Status,
    updated_at: i64,
    now_ms: i64,
    hook: Option<&HookState>,
) -> (Need, Option<String>) {
    let heuristic = match status {
        Status::Busy => Need::Working,
        Status::Idle => {
            if updated_at > 0 && now_ms - updated_at <= JUST_FINISHED_WINDOW_MS {
                Need::JustFinished
            } else {
                Need::YourTurn
            }
        }
    };

    match hook {
        // The hook is at least as fresh as the heartbeat → trust its exact phase.
        Some(h) if h.updated_at_ms >= updated_at => {
            let need = match h.phase {
                Phase::AwaitingPermission => Need::Blocked,
                Phase::Working => Need::Working,
                Phase::AwaitingInput => Need::YourTurn,
                Phase::Finished => {
                    if now_ms - h.updated_at_ms <= JUST_FINISHED_WINDOW_MS {
                        Need::JustFinished
                    } else {
                        Need::YourTurn
                    }
                }
            };
            (need, h.hint.clone())
        }
        // Heartbeat is the fresher signal (or no hook installed) → heuristic. Even
        // then, surface the last hint we have (e.g. the task it's working on).
        _ => (heuristic, hook.and_then(|h| h.hint.clone())),
    }
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn short_id(id: &str) -> String {
    format!("session {}", id.get(..8).unwrap_or(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(phase: Phase, hint: Option<&str>, at: i64) -> HookState {
        HookState {
            phase,
            hint: hint.map(String::from),
            updated_at_ms: at,
        }
    }

    #[test]
    fn heuristic_without_hooks() {
        let now = 1_000_000;
        assert_eq!(classify(Status::Busy, now, now, None).0, Need::Working);
        // idle, went idle 5s ago → just finished
        assert_eq!(
            classify(Status::Idle, now - 5_000, now, None).0,
            Need::JustFinished
        );
        // idle, waiting 5 min → your turn
        assert_eq!(
            classify(Status::Idle, now - 300_000, now, None).0,
            Need::YourTurn
        );
    }

    #[test]
    fn fresh_hook_gives_exact_phase_and_hint() {
        let now = 1_000_000;
        // A permission block while the heartbeat still reads idle/stale → Blocked.
        let (need, hint) = classify(
            Status::Idle,
            now - 10_000,
            now,
            Some(&hook(Phase::AwaitingPermission, Some("Bash"), now)),
        );
        assert_eq!(need, Need::Blocked);
        assert_eq!(hint.as_deref(), Some("Bash"));

        // Working phase with the task as the hint.
        let (need, hint) = classify(
            Status::Idle,
            now - 10_000,
            now,
            Some(&hook(Phase::Working, Some("Fix login"), now)),
        );
        assert_eq!(need, Need::Working);
        assert_eq!(hint.as_deref(), Some("Fix login"));
    }

    #[test]
    fn stale_hook_never_overrides_a_fresher_heartbeat() {
        let now = 1_000_000;
        // The heartbeat says busy and is NEWER than an old "finished" hook — the
        // session resumed working, so we must report Working, not a stale finish.
        let (need, hint) = classify(
            Status::Busy,
            now, // heartbeat fresh
            now,
            Some(&hook(Phase::Finished, Some("earlier task"), now - 60_000)),
        );
        assert_eq!(need, Need::Working);
        // …but the last hint is still surfaced as context.
        assert_eq!(hint.as_deref(), Some("earlier task"));
    }

    #[test]
    fn finished_hook_ages_into_your_turn() {
        let now = 1_000_000;
        // Fresh finish → just finished.
        assert_eq!(
            classify(Status::Idle, now - 5_000, now, Some(&hook(Phase::Finished, None, now - 5_000))).0,
            Need::JustFinished
        );
        // Old finish → your turn.
        assert_eq!(
            classify(Status::Idle, now - 200_000, now, Some(&hook(Phase::Finished, None, now - 200_000))).0,
            Need::YourTurn
        );
    }

    /// Not a unit assertion — a manual inspector. Run it to eyeball the live
    /// queue against the real machine:
    ///   cargo test --quiet dump_live_queue -- --ignored --nocapture
    #[test]
    #[ignore = "reads live machine state"]
    fn dump_live_queue() {
        for e in super::build_queue() {
            eprintln!(
                "{:?}\t{}\t[{}]\t~${:.2}\t{}",
                e.need, e.title, e.project_path, e.cost_usd, e.session_id
            );
        }
    }
}
