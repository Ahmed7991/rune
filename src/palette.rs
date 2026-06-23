//! Per-project identity colours.
//!
//! Each project gets one stable accent colour, threaded across the whole UI —
//! the sidebar rail swatch, the cross-project "needs-you" queue rows, the
//! project launch cards, and the recent-session rows — so a project is
//! recognisable at a glance everywhere it appears. This is the "one identity
//! colour per project, used everywhere" of the Mission Control mockup.
//!
//! The colour is derived from the project *path*, not its position in the list,
//! so it stays put when projects are reordered or removed. The CSS class for a
//! swatch is `swatch-<index>`; the colour classes are generated from
//! [`PALETTE`] in `ui::install_css`, so the two never drift apart.

/// The Mission Control swatch palette — distinct hues chosen to read on the deep
/// charcoal/navy background. Index 0 is the cyan accent. The first seven are the
/// hues used in the locked `c2-mission.html` mockup; the rest widen the set so
/// a handful of projects rarely collide onto the same colour.
pub const PALETTE: [&str; 10] = [
    "#36d3fa", // cyan (the signature accent)
    "#ffb13b", // amber
    "#8ea2ff", // periwinkle
    "#43e08a", // green
    "#ff7ea6", // pink
    "#d6a25c", // tan
    "#c98cff", // purple
    "#5fd0e0", // teal
    "#f4845f", // coral
    "#b6d94c", // lime
];

/// Stable palette index for a project path (FNV-1a over its bytes). Keyed off
/// the path, so reordering or removing other projects never shifts a project's
/// colour. Collisions are possible (more projects than palette entries) but
/// harmless — the swatch is an aid, never the sole identifier.
pub fn color_index(project_path: &str) -> usize {
    // FNV-1a, 64-bit. A tiny, well-distributed hash; no external dependency and
    // (unlike DefaultHasher) stable across Rust versions and runs.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // offset basis
    for byte in project_path.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    (hash % PALETTE.len() as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn index_is_stable_and_in_bounds() {
        let p = "/home/user/projects/my-app";
        let first = color_index(p);
        assert_eq!(first, color_index(p), "same path must map to the same colour");
        assert!(first < PALETTE.len(), "index must be a valid palette slot");
    }

    #[test]
    fn realistic_project_set_spreads_across_the_palette() {
        // A real sidebar's worth of projects shouldn't all collapse onto one or
        // two colours — the hash should fan them out.
        let paths = [
            "/p/web-app",
            "/p/api-service",
            "/p/cli-tool",
            "/p/mobile-app",
            "/p/docs-site",
            "/p/data-pipeline",
            "/p/design-system",
        ];
        let distinct: HashSet<usize> = paths.iter().map(|p| color_index(p)).collect();
        assert!(
            distinct.len() >= 4,
            "expected the 7 sample projects to use >=4 distinct colours, got {}",
            distinct.len()
        );
    }

    #[test]
    fn trailing_slash_is_the_callers_job_to_normalize() {
        // Documents intent: paths are hashed verbatim, so callers normalize
        // (strip trailing '/') before asking — same path in, same colour out.
        assert_ne!(color_index("/a/p"), color_index("/a/p/"));
    }
}
