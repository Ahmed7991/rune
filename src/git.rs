//! The project's current git branch — read straight from `.git/HEAD`, never by
//! spawning `git`. It's a couple of small file reads, so it's cheap enough to
//! call while building the dashboard (no subprocess, no blocking on a slow repo).
//! Read-only: we only ever read inside the project's own `.git`.

use std::path::{Path, PathBuf};

/// The project's current branch (e.g. `main`, or `feature/x`), a short commit
/// hash for a detached HEAD, or `None` when it isn't a git repo / can't be read.
pub fn current_branch(project_path: &str) -> Option<String> {
    let head = head_path(&Path::new(project_path).join(".git"))?;
    let text = std::fs::read_to_string(head).ok()?;
    let text = text.trim();

    if let Some(rest) = text.strip_prefix("ref:") {
        // "ref: refs/heads/main" → "main" (keep deeper paths like feature/x).
        let name = rest.trim().strip_prefix("refs/heads/").unwrap_or(rest.trim());
        return (!name.is_empty()).then(|| name.to_string());
    }
    // Detached HEAD — a raw 40-char hex sha. Show the short form.
    if text.len() >= 7 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(text[..7].to_string());
    }
    None
}

/// Resolve the file that holds HEAD. Usually `.git` is a directory and HEAD sits
/// inside it; in a linked worktree `.git` is instead a file
/// `gitdir: <abs path>` pointing at the real per-worktree git dir.
fn head_path(git: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(git).ok()?;
    if meta.is_dir() {
        return Some(git.join("HEAD"));
    }
    // ".git" is a file: "gitdir: /repo/.git/worktrees/foo".
    let text = std::fs::read_to_string(git).ok()?;
    let dir = text.trim().strip_prefix("gitdir:")?.trim();
    Some(Path::new(dir).join("HEAD"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // A unique temp dir per test (no external tempdir crate).
    fn scratch(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rune-git-test-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_a_normal_branch() {
        let proj = scratch("branch");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::write(proj.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(
            current_branch(proj.to_str().unwrap()),
            Some("main".to_string())
        );
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn keeps_slashes_in_branch_names() {
        let proj = scratch("slash");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::write(proj.join(".git/HEAD"), "ref: refs/heads/feature/login\n").unwrap();
        assert_eq!(
            current_branch(proj.to_str().unwrap()),
            Some("feature/login".to_string())
        );
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn detached_head_shows_short_hash() {
        let proj = scratch("detached");
        std::fs::create_dir_all(proj.join(".git")).unwrap();
        std::fs::write(
            proj.join(".git/HEAD"),
            "a1b2c3d4e5f600112233445566778899aabbccdd\n",
        )
        .unwrap();
        assert_eq!(
            current_branch(proj.to_str().unwrap()),
            Some("a1b2c3d".to_string())
        );
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn worktree_git_file_is_followed() {
        let proj = scratch("worktree");
        let real = scratch("worktree-gitdir");
        std::fs::write(real.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(
            proj.join(".git"),
            format!("gitdir: {}\n", real.display()),
        )
        .unwrap();
        assert_eq!(current_branch(proj.to_str().unwrap()), Some("wt".to_string()));
        let _ = std::fs::remove_dir_all(&proj);
        let _ = std::fs::remove_dir_all(&real);
    }

    #[test]
    fn not_a_repo_is_none() {
        let proj = scratch("norepo");
        assert_eq!(current_branch(proj.to_str().unwrap()), None);
        let _ = std::fs::remove_dir_all(&proj);
    }
}
