//! Per-session cost estimate from a transcript's `message.usage`.
//!
//! Token counts are exact (from the transcript); the dollar figure is an
//! *estimate* — it applies public per-model token rates, which can change.
//! Cache writes are priced by TTL when the `cache_creation` split is present
//! (5-minute = 1.25× input, 1-hour = 2× input); cache reads are 0.1× input.
//! Rates per 1M tokens, current as of 2026-06.

struct Rates {
    input: f64,
    output: f64,
}

/// USD per 1M tokens, matched by model family. Unknown ids fall back to the
/// current Opus tier (almost all local sessions are Opus).
fn rates_for(model: &str) -> Rates {
    if model.starts_with("claude-fable") || model.starts_with("claude-mythos") {
        Rates { input: 10.0, output: 50.0 }
    } else if model.starts_with("claude-sonnet") {
        Rates { input: 3.0, output: 15.0 }
    } else if model.starts_with("claude-haiku") {
        Rates { input: 1.0, output: 5.0 }
    } else {
        Rates { input: 5.0, output: 25.0 } // opus 4.x + fallback
    }
}

/// Estimated USD cost of one assistant turn from its `message.model` and
/// `message.usage` JSON object. (Caller must price each `message.id` once —
/// Claude writes one turn as several transcript lines that all repeat the same
/// cumulative usage.)
pub fn turn_cost_usd(model: &str, usage: &serde_json::Value) -> f64 {
    let r = rates_for(model);
    // A turn with multiple internal iterations reports only the LAST iteration
    // in the top-level usage; the true work is the sum of the iteration objects
    // (they share the same shape). Fall back to top-level usage otherwise.
    match usage.get("iterations").and_then(|v| v.as_array()) {
        Some(iters) if !iters.is_empty() => iters.iter().map(|it| price_usage(&r, it)).sum(),
        _ => price_usage(&r, usage),
    }
}

/// Price one usage object (a top-level usage or a single `iterations` entry).
fn price_usage(r: &Rates, usage: &serde_json::Value) -> f64 {
    let tokens = |key: &str| usage.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);

    let input = tokens("input_tokens");
    let output = tokens("output_tokens");
    let cache_read = tokens("cache_read_input_tokens");

    // Price cache writes by TTL when the breakdown is present; otherwise treat
    // the flat figure as a 5-minute write.
    let (write_5m, write_1h) = match usage.get("cache_creation") {
        Some(c) => (
            c.get("ephemeral_5m_input_tokens")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            c.get("ephemeral_1h_input_tokens")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
        ),
        None => (tokens("cache_creation_input_tokens"), 0.0),
    };

    (input * r.input
        + output * r.output
        + cache_read * r.input * 0.1
        + write_5m * r.input * 1.25
        + write_1h * r.input * 2.0)
        / 1_000_000.0
}

/// "$12.30" / "<$0.01" / "$0.00" for display.
pub fn format_usd(usd: f64) -> String {
    if usd <= 0.0 {
        // Cost is never legitimately negative; this also normalizes the negative
        // zero that an *empty* transcript sum produces (`[].sum::<f64>()` seeds
        // its fold with -0.0), which would otherwise render as a stray "$-0.00".
        "$0.00".to_string()
    } else if usd < 0.005 {
        "<$0.01".to_string()
    } else {
        format!("${usd:.2}")
    }
}

/// A short, human label for a model id, for the session-row chip:
/// `claude-opus-4-8` → "Opus 4.8", `claude-haiku-4-5-20251001` → "Haiku 4.5",
/// `claude-fable-5` → "Fable 5", and the legacy version-first shape
/// `claude-3-5-sonnet-…` → "Sonnet 3.5". A vendor prefix
/// (`us.anthropic.claude-…`) and a `[1m]` window annotation are stripped first.
/// Unknown shapes degrade to the capitalized family, then the raw id. Empty in →
/// empty out (caller skips the chip).
pub fn model_label(model: &str) -> String {
    if model.is_empty() {
        return String::new();
    }
    // Drop any "[1m]"-style window annotation, then any vendor prefix up to and
    // including "claude-" (handles bedrock-style "us.anthropic.claude-…").
    let base = model.split('[').next().unwrap_or(model);
    let core = match base.find("claude-") {
        Some(i) => &base[i + "claude-".len()..],
        None => base,
    };
    let tokens: Vec<&str> = core.split('-').filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return base.to_string();
    }
    // Don't assume family-then-version: the family is the first non-numeric
    // token (works for both "opus-4-8" and the legacy "3-5-sonnet"). The version
    // is the short numeric tokens anywhere in the id (a long run like 20251001 is
    // a date stamp and is skipped), newest catalog first.
    let family = tokens
        .iter()
        .find(|t| !t.bytes().all(|b| b.is_ascii_digit()))
        .copied();
    let version: Vec<&str> = tokens
        .iter()
        .filter(|t| t.len() <= 2 && t.bytes().all(|b| b.is_ascii_digit()))
        .take(2)
        .copied()
        .collect();
    match family {
        Some(fam) => {
            let mut out = capitalize(fam);
            if !version.is_empty() {
                out.push(' ');
                out.push_str(&version.join("."));
            }
            out
        }
        None => version.join("."), // all-numeric id (unusual) — just the version
    }
}

/// A short CSS-class suffix for the model family, so the chip can be tinted per
/// model (Opus = cyan, Sonnet = periwinkle, Haiku = green, Fable = gold).
pub fn model_family(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        "opus"
    } else if m.contains("sonnet") {
        "sonnet"
    } else if m.contains("haiku") {
        "haiku"
    } else if m.contains("fable") || m.contains("mythos") {
        "fable"
    } else {
        "generic"
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The standard Claude context window.
pub const STD_CONTEXT_TOKENS: u64 = 200_000;
/// The extended (1M-token beta) context window.
pub const BIG_CONTEXT_TOKENS: u64 = 1_000_000;

/// The context window to measure fill against, assumed *per model family*. The
/// transcript doesn't record the window, so we can't read it directly — but
/// keying it off the family (rather than the live token count) keeps the fill
/// bar monotonic: the families that offer the 1M beta (Opus/Sonnet/Fable, which
/// is what runs here) are shown against 1M, Haiku against its 200K window.
/// Measuring against the *count* instead would make the bar jump backwards as a
/// session grew past 200K (a smaller window suddenly reading "fuller"). The row
/// tooltip shows the exact token count + this assumed window so it's transparent.
pub fn context_window(model: &str) -> u64 {
    match model_family(model) {
        "opus" | "sonnet" | "fable" => BIG_CONTEXT_TOKENS,
        _ => STD_CONTEXT_TOKENS, // haiku + unknown → conservative 200K
    }
}

/// Context-window fill as a 0–100 percentage for `model`, or `None` when the
/// session has no assistant turn yet (no usage to read).
pub fn context_pct(context_tokens: u64, model: &str) -> Option<u8> {
    if context_tokens == 0 {
        return None;
    }
    let pct = (context_tokens as f64 / context_window(model) as f64 * 100.0).round();
    Some(pct.clamp(0.0, 100.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_labels() {
        assert_eq!(model_label("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_label("claude-sonnet-4-6"), "Sonnet 4.6");
        assert_eq!(model_label("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_label("claude-fable-5"), "Fable 5");
        assert_eq!(model_label("claude-opus-4-8[1m]"), "Opus 4.8"); // 1m suffix dropped
        // legacy version-first ids + a bedrock vendor prefix
        assert_eq!(model_label("claude-3-5-sonnet-20241022"), "Sonnet 3.5");
        assert_eq!(model_label("claude-3-opus-20240229"), "Opus 3");
        assert_eq!(model_label("us.anthropic.claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_label(""), "");
        assert_eq!(model_label("weird"), "Weird");
    }

    #[test]
    fn format_usd_normalizes_zero_and_negatives() {
        // an empty transcript sum is negative zero — must not read "$-0.00"
        let empty: f64 = std::iter::empty::<f64>().sum();
        assert_eq!(format_usd(empty), "$0.00");
        assert_eq!(format_usd(0.0), "$0.00");
        assert_eq!(format_usd(-1.0), "$0.00");
        assert_eq!(format_usd(0.004), "<$0.01");
        assert_eq!(format_usd(12.5), "$12.50");
    }

    #[test]
    fn model_families() {
        assert_eq!(model_family("claude-opus-4-8"), "opus");
        assert_eq!(model_family("claude-sonnet-4-6"), "sonnet");
        assert_eq!(model_family("claude-haiku-4-5"), "haiku");
        assert_eq!(model_family("claude-fable-5"), "fable");
        assert_eq!(model_family("something-else"), "generic");
    }

    #[test]
    fn context_window_by_family_and_pct() {
        // window is keyed off family, NOT the token count → monotonic bar
        assert_eq!(context_window("claude-opus-4-8"), BIG_CONTEXT_TOKENS);
        assert_eq!(context_window("claude-sonnet-4-6"), BIG_CONTEXT_TOKENS);
        assert_eq!(context_window("claude-haiku-4-5"), STD_CONTEXT_TOKENS);
        assert_eq!(context_window("unknown-model"), STD_CONTEXT_TOKENS);
        assert_eq!(context_pct(0, "claude-opus-4-8"), None);
        assert_eq!(context_pct(100_000, "claude-opus-4-8"), Some(10)); // of 1M
        assert_eq!(context_pct(782_911, "claude-opus-4-8"), Some(78)); // of 1M
        assert_eq!(context_pct(190_000, "claude-haiku-4-5"), Some(95)); // of 200K
        assert_eq!(context_pct(2_000_000, "claude-opus-4-8"), Some(100)); // pegged
    }
}
