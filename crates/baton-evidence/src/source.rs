//! Source file providers for G2 verification (git tree access with path safety).

use crate::error::EvidenceError;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Reads source files at a specific git commit.
pub trait SourceProvider {
    /// Return file content at `commit`; `Ok(None)` when the file does not exist in `commit`.
    fn read_at(&self, commit: &str, path: &str) -> Result<Option<String>, EvidenceError>;
}

/// Validates a git commit reference to prevent option injection and malformed refs.
pub(crate) fn validate_commit_ref(commit: &str) -> Result<&str, EvidenceError> {
    let trimmed = commit.trim();
    if trimmed.is_empty() {
        return Err(EvidenceError::InvalidCommit {
            commit: commit.to_string(),
            reason: "commit reference is empty",
        });
    }
    if trimmed.starts_with('-') {
        return Err(EvidenceError::InvalidCommit {
            commit: commit.to_string(),
            reason: "commit reference must not start with '-'",
        });
    }
    if trimmed
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == ':' || c == '\0')
    {
        return Err(EvidenceError::InvalidCommit {
            commit: commit.to_string(),
            reason: "commit reference contains forbidden characters",
        });
    }
    if trimmed.contains("..") {
        return Err(EvidenceError::InvalidCommit {
            commit: commit.to_string(),
            reason: "commit reference must not contain '..'",
        });
    }
    Ok(trimmed)
}

/// Validates and normalizes a repo-relative file path for `git show <commit>:<path>`.
/// Rejects absolute paths, path traversal (`..`), backslashes, NUL/control chars, and leading `-`.
pub(crate) fn validate_repo_path(path: &str) -> Result<String, EvidenceError> {
    if path.is_empty() {
        return Err(EvidenceError::InvalidPath {
            path: path.to_string(),
            reason: "path is empty",
        });
    }
    if path.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(EvidenceError::InvalidPath {
            path: path.to_string(),
            reason: "path contains control or NUL characters",
        });
    }
    if path.contains('\\') {
        return Err(EvidenceError::InvalidPath {
            path: path.to_string(),
            reason: "path must use '/' separators",
        });
    }
    if path.starts_with('/') {
        return Err(EvidenceError::InvalidPath {
            path: path.to_string(),
            reason: "path must be repo-relative, not absolute",
        });
    }
    if path.starts_with('-') {
        return Err(EvidenceError::InvalidPath {
            path: path.to_string(),
            reason: "path must not start with '-'",
        });
    }

    let mut clean_parts: Vec<&str> = Vec::new();
    for comp in Path::new(path).components() {
        match comp {
            Component::Normal(seg) => {
                let s = seg.to_str().ok_or_else(|| EvidenceError::InvalidPath {
                    path: path.to_string(),
                    reason: "non-UTF-8 path segment",
                })?;
                if s.is_empty() {
                    continue;
                }
                clean_parts.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(EvidenceError::InvalidPath {
                    path: path.to_string(),
                    reason: "path traversal ('..') is forbidden",
                });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(EvidenceError::InvalidPath {
                    path: path.to_string(),
                    reason: "absolute path is forbidden",
                });
            }
        }
    }

    if clean_parts.is_empty() {
        return Err(EvidenceError::InvalidPath {
            path: path.to_string(),
            reason: "path resolves to empty",
        });
    }

    Ok(clean_parts.join("/"))
}

/// Reads files from a local git repository using `git -C <repo_dir> show <commit>:<path>`.
#[derive(Debug, Clone)]
pub struct GitSource {
    pub repo_dir: PathBuf,
}

impl GitSource {
    pub fn new(repo_dir: impl Into<PathBuf>) -> Self {
        GitSource {
            repo_dir: repo_dir.into(),
        }
    }

    /// Verify that `repo_dir` is a git repo and `commit` resolves to an existing commit.
    pub fn verify_commit(&self, commit: &str) -> Result<(), EvidenceError> {
        let commit = validate_commit_ref(commit)?;
        let spec = format!("{commit}^{{commit}}");
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_dir)
            .args(["rev-parse", "--verify", "--quiet", &spec])
            .output()
            .map_err(|e| EvidenceError::Io {
                context: format!("git rev-parse in {}", self.repo_dir.display()),
                message: e.to_string(),
            })?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("commit {commit:?} not found")
            };
            Err(EvidenceError::GitFailed {
                command: format!(
                    "git -C {} rev-parse --verify {spec}",
                    self.repo_dir.display()
                ),
                stderr: detail,
            })
        }
    }
}

impl SourceProvider for GitSource {
    fn read_at(&self, commit: &str, path: &str) -> Result<Option<String>, EvidenceError> {
        let commit = validate_commit_ref(commit)?;
        let clean_path = validate_repo_path(path)?;
        let spec = format!("{commit}:{clean_path}");

        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_dir)
            .args(["show", &spec])
            .output()
            .map_err(|e| EvidenceError::Io {
                context: format!("git show {spec}"),
                message: e.to_string(),
            })?;

        if output.status.success() {
            return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("does not exist in")
            || lower.contains("exists on disk, but not in")
            || lower.contains("path '") && lower.contains("not in '")
        {
            return Ok(None);
        }

        Err(EvidenceError::GitFailed {
            command: format!("git -C {} show {spec}", self.repo_dir.display()),
            stderr,
        })
    }
}

type SourceCacheMap = HashMap<(String, String), Result<Option<String>, EvidenceError>>;

/// Per-call memoizing wrapper around any [`SourceProvider`] to avoid repeated `git show` spawns.
pub(crate) struct CachingSource<'a> {
    inner: &'a dyn SourceProvider,
    cache: RefCell<SourceCacheMap>,
}

impl<'a> CachingSource<'a> {
    pub(crate) fn new(inner: &'a dyn SourceProvider) -> Self {
        CachingSource {
            inner,
            cache: RefCell::new(HashMap::new()),
        }
    }
}

impl SourceProvider for CachingSource<'_> {
    fn read_at(&self, commit: &str, path: &str) -> Result<Option<String>, EvidenceError> {
        let key = (commit.to_string(), path.to_string());
        if let Some(cached) = self.cache.borrow().get(&key) {
            return cached.clone();
        }
        let res = self.inner.read_at(commit, path);
        self.cache.borrow_mut().insert(key, res.clone());
        res
    }
}

/// Resolves a log file path safely against `base_dir`.
///
/// - Rejects empty paths, NUL/control characters, and any `..` traversal component.
/// - Relative paths are joined with `base_dir` and verified (via canonicalization)
///   to remain inside `base_dir` so symlinks cannot escape the repository.
/// - Clean absolute paths are allowed per contracts §1.5 (`path 相对仓库根目录，或使用绝对路径`),
///   provided they contain no `..` traversal components.
pub(crate) fn resolve_safe_log_path(base_dir: &Path, path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.chars().any(|c| c == '\0' || c.is_control()) {
        return None;
    }

    let raw = Path::new(path);
    for comp in raw.components() {
        if matches!(comp, Component::ParentDir) {
            return None;
        }
    }

    if raw.is_absolute() {
        let canon = std::fs::canonicalize(raw).ok()?;
        if canon.is_file() {
            Some(canon)
        } else {
            None
        }
    } else {
        let canon_base = std::fs::canonicalize(base_dir).ok()?;
        let joined = canon_base.join(raw);
        let canon_file = std::fs::canonicalize(&joined).ok()?;
        if canon_file.starts_with(&canon_base) && canon_file.is_file() {
            Some(canon_file)
        } else {
            None
        }
    }
}
