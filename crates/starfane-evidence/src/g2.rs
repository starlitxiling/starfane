//! G2 deterministic evidence verification (contracts §5).

use crate::error::EvidenceError;
use crate::g1::non_ws_char_count;
use crate::source::{resolve_safe_log_path, CachingSource, GitSource, SourceProvider};
use starfane_proto::{
    CodeRef, EvidenceIssue, EvidenceQuote, EvidenceRef, FindingId, G2Outcome, ReviewReport,
};
use std::collections::BTreeMap;
use std::path::Path;

/// Configuration for G2 deterministic verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct G2Config {
    /// Allowed ±line drift before a match is considered relocated (default: 3).
    pub drift_tolerance: u32,
    /// Minimum non-whitespace characters required in a quote (default: 8).
    pub min_quote_chars: usize,
    /// Whether to verify `location.symbol` on word boundaries (default: true).
    pub check_symbols: bool,
}

impl Default for G2Config {
    fn default() -> Self {
        G2Config {
            drift_tolerance: 3,
            min_quote_chars: 8,
            check_symbols: true,
        }
    }
}

/// Normalize whitespace: trim each line, collapse internal whitespace runs to a single
/// ASCII space, and drop empty lines (contracts §5).
pub fn normalize_ws(s: &str) -> String {
    normalize_lines(s).join("\n")
}

/// Returns normalized non-empty lines.
fn normalize_lines(s: &str) -> Vec<String> {
    s.lines()
        .map(normalize_single_line)
        .filter(|l| !l.is_empty())
        .collect()
}

/// Returns `(1-based original line number, normalized line)` for every non-empty line,
/// plus the total raw line count of `s`.
fn normalize_indexed_lines(s: &str) -> (Vec<(u32, String)>, u32) {
    let mut total: u32 = 0;
    let mut out = Vec::new();
    for (idx, line) in s.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        total = line_no;
        let norm = normalize_single_line(line);
        if !norm.is_empty() {
            out.push((line_no, norm));
        }
    }
    (out, total)
}

fn normalize_single_line(line: &str) -> String {
    let mut out = String::new();
    for (i, part) in line.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(part);
    }
    out
}

/// Find all 1-based starting line numbers where `quote_lines` matches `file_lines`
/// as a contiguous normalized subsequence.
///
/// Exact line-by-line matches are preferred; for single-line quotes with zero exact matches,
/// falls back to verbatim substring matches within a normalized line.
fn find_code_quote_matches(file_lines: &[(u32, String)], quote_lines: &[String]) -> Vec<u32> {
    let k = quote_lines.len();
    if k == 0 || file_lines.len() < k {
        return Vec::new();
    }

    let mut exact_matches = Vec::new();
    for i in 0..=(file_lines.len() - k) {
        let matched = (0..k).all(|j| file_lines[i + j].1 == quote_lines[j]);
        if matched {
            exact_matches.push(file_lines[i].0);
        }
    }

    if !exact_matches.is_empty() || k > 1 {
        return exact_matches;
    }

    // Single-line verbatim substring fallback (e.g. quoting an expression without surrounding braces).
    let needle = &quote_lines[0];
    file_lines
        .iter()
        .filter(|(_, text)| text.contains(needle.as_str()))
        .map(|(line_no, _)| *line_no)
        .collect()
}

/// Find all 1-based starting line numbers where `quote_lines` matches `file_lines`
/// as a substring of a line (for single-line log quotes) or contiguous lines (for multi-line).
fn find_log_quote_matches(file_lines: &[(u32, String)], quote_lines: &[String]) -> Vec<u32> {
    let k = quote_lines.len();
    if k == 0 || file_lines.len() < k {
        return Vec::new();
    }

    let mut matches = Vec::new();
    if k == 1 {
        let needle = &quote_lines[0];
        for (line_no, text) in file_lines {
            if text.contains(needle.as_str()) {
                matches.push(*line_no);
            }
        }
        return matches;
    }

    for i in 0..=(file_lines.len() - k) {
        let first_ok = file_lines[i].1.contains(quote_lines[0].as_str());
        let middle_ok = (1..(k - 1)).all(|j| file_lines[i + j].1 == quote_lines[j]);
        let last_ok = file_lines[i + k - 1]
            .1
            .contains(quote_lines[k - 1].as_str());
        if first_ok && middle_ok && last_ok {
            matches.push(file_lines[i].0);
        }
    }
    matches
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Word-boundary search for `symbol` in `content` without regex.
fn contains_symbol_word_boundary(content: &str, symbol: &str) -> bool {
    let sym = symbol.trim();
    if sym.is_empty() {
        return true;
    }
    let starts_ident = sym.chars().next().is_some_and(is_ident_char);
    let ends_ident = sym.chars().last().is_some_and(is_ident_char);

    for (start, matched) in content.match_indices(sym) {
        let left_ok = if starts_ident {
            content[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !is_ident_char(c))
        } else {
            true
        };
        let end = start + matched.len();
        let right_ok = if ends_ident {
            content[end..]
                .chars()
                .next()
                .is_none_or(|c| !is_ident_char(c))
        } else {
            true
        };
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

/// Evaluate matches against claimed line and drift tolerance.
fn classify_line_match(
    file: &str,
    claimed: u32,
    matches: &[u32],
    tol: u32,
    is_log: bool,
) -> Option<EvidenceIssue> {
    if matches.is_empty() {
        return Some(if is_log {
            EvidenceIssue::LogQuoteNotFound {
                path: file.to_string(),
            }
        } else {
            EvidenceIssue::QuoteNotFound {
                file: file.to_string(),
                line_start: claimed,
            }
        });
    }

    let win_lo = claimed.saturating_sub(tol).max(1);
    let win_hi = claimed.saturating_add(tol);
    if matches.iter().any(|&m| m >= win_lo && m <= win_hi) {
        return None;
    }

    if matches.len() == 1 {
        Some(EvidenceIssue::QuoteRelocated {
            file: file.to_string(),
            claimed,
            actual: matches[0],
        })
    } else {
        Some(EvidenceIssue::QuoteAmbiguous {
            file: file.to_string(),
            occurrences: matches.len() as u32,
        })
    }
}

/// Helper to read a file through `SourceProvider`, treating errors (e.g. path traversal) as missing.
fn read_source_or_missing(src: &dyn SourceProvider, commit: &str, file: &str) -> Option<String> {
    src.read_at(commit, file).unwrap_or_default()
}

/// Verify code quotes, code refs (`trigger_path` / `checked_guards`), and optional symbol
/// against `src` at `commit` (contracts §5).
pub fn verify_g2_quotes(
    src: &dyn SourceProvider,
    commit: &str,
    quotes: &[EvidenceQuote],
    refs: &[CodeRef],
    symbol: Option<&str>,
    cfg: &G2Config,
) -> G2Outcome {
    let cached = CachingSource::new(src);
    let issues = verify_g2_quotes_inner(&cached, commit, quotes, refs, symbol, None, cfg);
    G2Outcome::from_issues(issues)
}

fn verify_g2_quotes_inner(
    src: &dyn SourceProvider,
    commit: &str,
    quotes: &[EvidenceQuote],
    refs: &[CodeRef],
    symbol: Option<&str>,
    symbol_file: Option<&str>,
    cfg: &G2Config,
) -> Vec<EvidenceIssue> {
    let mut issues = Vec::new();

    // 1. Verify each EvidenceQuote
    for q in quotes {
        if non_ws_char_count(&q.quote) < cfg.min_quote_chars {
            issues.push(EvidenceIssue::QuoteTooShort {
                file: q.file.clone(),
            });
            continue;
        }

        let Some(content) = read_source_or_missing(src, commit, &q.file) else {
            issues.push(EvidenceIssue::FileMissing {
                file: q.file.clone(),
            });
            continue;
        };

        let (file_lines, line_count) = normalize_indexed_lines(&content);
        if q.line_start == 0 || q.line_start > line_count {
            issues.push(EvidenceIssue::LineOutOfRange {
                file: q.file.clone(),
                line: q.line_start,
                line_count,
            });
            continue;
        }
        if q.line_end < q.line_start || q.line_end > line_count {
            issues.push(EvidenceIssue::LineOutOfRange {
                file: q.file.clone(),
                line: q.line_end,
                line_count,
            });
            continue;
        }

        let quote_lines = normalize_lines(&q.quote);
        if quote_lines.is_empty() {
            issues.push(EvidenceIssue::QuoteTooShort {
                file: q.file.clone(),
            });
            continue;
        }

        let matches = find_code_quote_matches(&file_lines, &quote_lines);
        if let Some(issue) =
            classify_line_match(&q.file, q.line_start, &matches, cfg.drift_tolerance, false)
        {
            issues.push(issue);
        }
    }

    // 2. Verify each CodeRef (trigger_path, checked_guards, etc.)
    for r in refs {
        let Some(content) = read_source_or_missing(src, commit, &r.file) else {
            issues.push(EvidenceIssue::FileMissing {
                file: r.file.clone(),
            });
            continue;
        };
        let line_count = content.lines().count() as u32;
        if r.line == 0 || r.line > line_count {
            issues.push(EvidenceIssue::LineOutOfRange {
                file: r.file.clone(),
                line: r.line,
                line_count,
            });
        }
    }

    // 3. Optional symbol word-boundary check
    if cfg.check_symbols {
        if let Some(sym) = symbol.map(str::trim).filter(|s| !s.is_empty()) {
            let target_file = symbol_file
                .or_else(|| quotes.first().map(|q| q.file.as_str()))
                .or_else(|| refs.first().map(|r| r.file.as_str()));

            if let Some(file) = target_file {
                if let Some(content) = read_source_or_missing(src, commit, file) {
                    if !contains_symbol_word_boundary(&content, sym) {
                        issues.push(EvidenceIssue::SymbolNotFound {
                            symbol: sym.to_string(),
                        });
                    }
                }
            } else {
                issues.push(EvidenceIssue::SymbolNotFound {
                    symbol: sym.to_string(),
                });
            }
        }
    }

    issues
}

/// Verify all findings in a `ReviewReport` against the git repository at `report.head` (contracts §5).
pub fn verify_g2_review(
    repo_dir: &Path,
    report: &ReviewReport,
    cfg: &G2Config,
) -> Result<BTreeMap<FindingId, G2Outcome>, EvidenceError> {
    let git = GitSource::new(repo_dir);
    git.verify_commit(&report.head)?;

    let cached = CachingSource::new(&git);
    let mut results = BTreeMap::new();

    for (i, finding) in report.findings.iter().enumerate() {
        let key = if finding.id.0.is_empty() {
            FindingId::new(format!("findings[{i}]"))
        } else {
            finding.id.clone()
        };

        let mut issues = Vec::new();

        // Validate finding.location file and line range.
        match read_source_or_missing(&cached, &report.head, &finding.location.file) {
            None => {
                issues.push(EvidenceIssue::FileMissing {
                    file: finding.location.file.clone(),
                });
            }
            Some(loc_content) => {
                let line_count = loc_content.lines().count() as u32;
                if finding.location.line_start == 0 || finding.location.line_start > line_count {
                    issues.push(EvidenceIssue::LineOutOfRange {
                        file: finding.location.file.clone(),
                        line: finding.location.line_start,
                        line_count,
                    });
                } else if finding.location.line_end < finding.location.line_start
                    || finding.location.line_end > line_count
                {
                    issues.push(EvidenceIssue::LineOutOfRange {
                        file: finding.location.file.clone(),
                        line: finding.location.line_end,
                        line_count,
                    });
                }
            }
        }

        // Combine trigger_path and checked_guards into refs for existence verification.
        let mut refs =
            Vec::with_capacity(finding.trigger_path.len() + finding.checked_guards.len());
        refs.extend(finding.trigger_path.iter().cloned());
        refs.extend(finding.checked_guards.iter().cloned());

        let quote_issues = verify_g2_quotes_inner(
            &cached,
            &report.head,
            &finding.evidence,
            &refs,
            finding.location.symbol.as_deref(),
            Some(finding.location.file.as_str()),
            cfg,
        );
        issues.extend(quote_issues);

        results.insert(key, G2Outcome::from_issues(issues));
    }

    Ok(results)
}

/// Verify only the `EvidenceRef::Log` variants in `refs` against files rooted at `base_dir`
/// (contracts §5, used by `triage.v1` and `rca.v1`).
pub fn verify_g2_log_refs(refs: &[EvidenceRef], base_dir: &Path, cfg: &G2Config) -> G2Outcome {
    let mut issues = Vec::new();

    for r in refs {
        let EvidenceRef::Log { path, line, quote } = r else {
            continue;
        };

        if non_ws_char_count(quote) < cfg.min_quote_chars {
            issues.push(EvidenceIssue::QuoteTooShort { file: path.clone() });
            continue;
        }

        let Some(resolved) = resolve_safe_log_path(base_dir, path) else {
            issues.push(EvidenceIssue::LogMissing { path: path.clone() });
            continue;
        };

        let Ok(bytes) = std::fs::read(&resolved) else {
            issues.push(EvidenceIssue::LogMissing { path: path.clone() });
            continue;
        };
        let content = String::from_utf8_lossy(&bytes);

        let (file_lines, line_count) = normalize_indexed_lines(&content);
        let quote_lines = normalize_lines(quote);
        if quote_lines.is_empty() {
            issues.push(EvidenceIssue::QuoteTooShort { file: path.clone() });
            continue;
        }

        let matches = find_log_quote_matches(&file_lines, &quote_lines);
        match line {
            Some(claimed) => {
                if *claimed == 0 || *claimed > line_count {
                    issues.push(EvidenceIssue::LineOutOfRange {
                        file: path.clone(),
                        line: *claimed,
                        line_count,
                    });
                    continue;
                }
                if let Some(issue) =
                    classify_line_match(path, *claimed, &matches, cfg.drift_tolerance, true)
                {
                    issues.push(issue);
                }
            }
            None => {
                if matches.is_empty() {
                    issues.push(EvidenceIssue::LogQuoteNotFound { path: path.clone() });
                }
            }
        }
    }

    G2Outcome::from_issues(issues)
}

/// Combined G2 verifier for `triage.v1` and `rca.v1` (handles both `Code` and `Log` evidence
/// plus `CodeRef` trigger paths) (contracts §5).
pub fn verify_g2_evidence(
    src: &dyn SourceProvider,
    commit: &str,
    base_dir: &Path,
    refs: &[EvidenceRef],
    code_refs: &[CodeRef],
    cfg: &G2Config,
) -> G2Outcome {
    let code_quotes: Vec<EvidenceQuote> = refs
        .iter()
        .filter_map(|r| match r {
            EvidenceRef::Code(q) => Some(q.clone()),
            EvidenceRef::Log { .. } => None,
        })
        .collect();

    let mut issues = Vec::new();
    let code_outcome = verify_g2_quotes(src, commit, &code_quotes, code_refs, None, cfg);
    issues.extend(code_outcome.issues);

    let log_outcome = verify_g2_log_refs(refs, base_dir, cfg);
    issues.extend(log_outcome.issues);

    G2Outcome::from_issues(issues)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ws_trims_collapses_and_drops_blank_lines() {
        let input = "  \n\tlet   x  =   1;\r\n\n   foo( a,   b )  \n   ";
        assert_eq!(normalize_ws(input), "let x = 1;\nfoo( a, b )");
    }

    #[test]
    fn symbol_word_boundary_distinguishes_substrings() {
        let code = "fn parse_header(buf: &[u8]) {\n    let x = parse(buf);\n}\n";
        assert!(contains_symbol_word_boundary(code, "parse_header"));
        assert!(contains_symbol_word_boundary(code, "parse"));
        assert!(!contains_symbol_word_boundary(code, "header"));
        assert!(!contains_symbol_word_boundary(code, "parse_head"));
    }
}
