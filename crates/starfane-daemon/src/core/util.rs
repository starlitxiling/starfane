//! Small blocking helpers (git, text sanitizing). Call from `spawn_blocking`.

use std::path::Path;
use std::process::Command;

/// Run `git -C <dir> <args>`; trimmed stdout on success.
pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub(crate) fn git_root(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .filter(|s| !s.is_empty())
}

pub(crate) fn git_head(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty())
}

pub(crate) fn git_branch(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|s| !s.is_empty())
}

pub(crate) fn git_dirty(dir: &Path) -> Vec<String> {
    git(dir, &["status", "--porcelain"])
        .map(|s| {
            s.lines()
                .filter_map(|l| l.get(3..).map(|p| p.trim().to_string()))
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Create (or reuse) `<root>/.starfane/worktrees/<pid>` on branch `starfane/<pid>`.
/// Returns (path, branch, base commit).
pub(crate) fn worktree_add(root: &Path, pid: &str) -> Result<(String, String, String), String> {
    let path = starfane_proto::paths::project_dir(root)
        .join("worktrees")
        .join(pid);
    let branch = format!("starfane/{pid}");
    let base = git_head(root).ok_or_else(|| "cannot read HEAD of project root".to_string())?;
    if path.join(".git").exists() {
        return Ok((path.to_string_lossy().into_owned(), branch, base));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let p = path.to_string_lossy().into_owned();
    let created = git(root, &["worktree", "add", "-b", &branch, &p, &base]);
    if created.is_err() {
        // Branch may already exist from an earlier attempt.
        git(root, &["worktree", "add", &p, &branch])?;
    }
    Ok((p, branch, base))
}

/// Single-line, control-char-free, length-capped text (chars).
pub(crate) fn sanitize_line(text: &str, max_chars: usize) -> String {
    let folded: String = text
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !c.is_control())
        .collect();
    let collapsed = folded.split_whitespace().collect::<Vec<_>>().join(" ");
    starfane_proto::event::truncate_chars(&collapsed, max_chars)
}

/// Short display id: last 6 chars of a ULID-style id.
pub(crate) fn short_id(id: &str) -> String {
    let n = id.chars().count();
    id.chars().skip(n.saturating_sub(6)).collect()
}

pub(crate) fn project_name(root: &str) -> String {
    Path::new(root)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string())
}

/// Component-aware path prefix check (avoids `/proj` matching `/proj-v2` or empty base matching all).
pub(crate) fn path_starts_with(child: &str, base: &str) -> bool {
    !base.is_empty() && !child.is_empty() && Path::new(child).starts_with(Path::new(base))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_line("a\nb\t c\u{1b}[0m", 100), "a b c[0m");
        assert_eq!(sanitize_line("你好世界", 3), "你好…");
        assert_eq!(short_id("P-01JD0000000000000000ABCDEF"), "ABCDEF");
        assert_eq!(project_name("/Users/x/proj"), "proj");
        assert!(path_starts_with("/Users/x/proj/sub", "/Users/x/proj"));
        assert!(path_starts_with("/Users/x/proj/", "/Users/x/proj"));
        assert!(!path_starts_with("/Users/x/proj-v2", "/Users/x/proj"));
        assert!(!path_starts_with("/Users/x/proj", ""));
    }
}
