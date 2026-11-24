//! Golden git repository test suite for `baton-evidence` (G1 + G2).
//!
//! Creates real temporary git repositories with commits and log files at test time,
//! exercising Verified, Relocated, Ambiguous, Fabricated, ±3-line drift, path traversal
//! rejection, verbatim log matching, and all G1 validators & depth rules (D0–D3).

use baton_evidence::{
    normalize_ws, validate_g1_falsifier, validate_g1_handoff, validate_g1_plan, validate_g1_rca,
    validate_g1_report, validate_g1_review, validate_g1_triage, verify_g2_evidence,
    verify_g2_log_refs, verify_g2_quotes, verify_g2_review, EvidenceError, G2Config, GitSource,
    SourceProvider,
};
use baton_proto::{
    AccessMode, AgentKind, ArtifactHeader, ArtifactKind, AuthorRef, Category, CodeRef,
    EvidenceIssue, EvidenceQuote, EvidenceRef, EvidenceVerdict, FalsifierSubmission,
    FalsifierVerdict, FinalReport, FindingId, FixRef, HandoffId, HandoffIntent, HandoffPacket,
    HandoffStatus, HandoffTarget, Hypothesis, HypothesisKind, LocationRange, PlanDoc, PlanTask,
    RcaReport, Repro, ReproKind, ReviewDepth, ReviewFinding, ReviewId, ReviewReport, RootCause,
    RuledOut, SessionId, Severity, TestOutcome, TestRun, TriageReport, VerificationMethod,
    PROTO_VERSION,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Temp git repository cleaned up automatically on drop.
struct GoldenRepo {
    dir: PathBuf,
    v1_commit: String,
    v2_commit: String,
}

impl GoldenRepo {
    fn create() -> Self {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("baton-evidence-golden-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).expect("create src dir");
        fs::create_dir_all(dir.join("logs")).expect("create logs dir");

        run_git(&dir, &["init", "-q"]);
        run_git(&dir, &["config", "user.name", "Baton Test"]);
        run_git(&dir, &["config", "user.email", "test@baton.local"]);

        // Commit 1: initial version of src/auth.rs and src/api.rs
        let auth_v1 = "\
// line 1: auth module
pub struct Token {
    pub raw: String,
    pub role: String,
}

pub fn parse_header(input: &str) -> Option<Token> {
    if input.is_empty() {
        return None;
    }
    let parts: Vec<&str> = input.split(':').collect();
    if parts.len() < 2 {
        return None;
    }
    Some(Token {
        raw: parts[0].to_string(),
        role: parts[1].to_string(),
    })
}

pub fn check_admin(tok: &Token) -> bool {
    tok.role == \"admin\"
}

pub fn audit_log(msg: &str) -> usize {
    let trimmed = msg.trim();
    if trimmed.is_empty() {
        return 0;
    }
    trimmed.len()
}

pub fn duplicate_block_a(x: i32) -> i32 {
    if x < 0 {
        return 0;
    }
    x + 1
}

pub fn duplicate_block_b(x: i32) -> i32 {
    if x < 0 {
        return 0;
    }
    x + 2
}
";
        let api_v1 = "\
use crate::auth::{parse_header, check_admin};

pub fn handle_request(header: &str) -> Result<String, &'static str> {
    // Guard: checks non-empty only, does not check length limit
    if header.trim().is_empty() {
        return Err(\"missing header\");
    }
    let tok = parse_header(header).ok_or(\"bad token\")?;
    if !check_admin(&tok) {
        return Err(\"forbidden\");
    }
    Ok(format!(\"ok:{}\", tok.raw))
}
";

        fs::write(dir.join("src/auth.rs"), auth_v1).expect("write auth v1");
        fs::write(dir.join("src/api.rs"), api_v1).expect("write api v1");
        run_git(&dir, &["add", "src/auth.rs", "src/api.rs"]);
        run_git(&dir, &["commit", "-q", "-m", "v1: initial auth and api"]);
        let v1_commit = run_git(&dir, &["rev-parse", "HEAD"]);

        // Commit 2: insert 6 lines at the top of src/auth.rs so everything shifts down by 6 lines!
        let auth_v2 = format!(
            "// shift 1\n// shift 2\n// shift 3\n// shift 4\n// shift 5\n// shift 6\n{auth_v1}"
        );
        fs::write(dir.join("src/auth.rs"), &auth_v2).expect("write auth v2");
        run_git(&dir, &["add", "src/auth.rs"]);
        run_git(&dir, &["commit", "-q", "-m", "v2: add header banner"]);
        let v2_commit = run_git(&dir, &["rev-parse", "HEAD"]);

        // Create runtime log files (untracked, as in real debugging sessions)
        let api_log = "\
2026-11-20T10:00:01Z INFO server started on 127.0.0.1:8080
2026-11-20T10:00:05Z WARN slow query took 1420ms table=sessions
2026-11-20T10:00:08Z ERROR panic in handle_request: index out of bounds: the len is 1 but the index is 2
    at src/api.rs:8:15
    at src/auth.rs:14:9
2026-11-20T10:00:09Z INFO healthcheck status=200
2026-11-20T10:00:10Z WARN connection pool exhausted active=64
2026-11-20T10:00:11Z INFO retrying upstream request attempt=1
2026-11-20T10:00:12Z INFO retrying upstream request attempt=1
2026-11-20T10:00:13Z ERROR timeout waiting for db pool after 5000ms
";
        fs::write(dir.join("logs/api.log"), api_log).expect("write api.log");

        GoldenRepo {
            dir,
            v1_commit,
            v2_commit,
        }
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for GoldenRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn sample_finding(
    id: &str,
    quote_file: &str,
    line_start: u32,
    line_end: u32,
    quote: &str,
) -> ReviewFinding {
    ReviewFinding {
        id: FindingId::new(id),
        category: Category::Correctness,
        severity: Severity::Major,
        confidence: 0.85,
        claim: "parse_header does not cap input length before splitting".into(),
        location: LocationRange {
            file: quote_file.into(),
            line_start,
            line_end,
            symbol: Some("parse_header".into()),
        },
        evidence: vec![EvidenceQuote {
            file: quote_file.into(),
            line_start,
            line_end,
            quote: quote.into(),
        }],
        trigger_path: vec![
            CodeRef {
                file: "src/api.rs".into(),
                line: 8,
                note: Some("handle_request calls parse_header".into()),
            },
            CodeRef {
                file: "src/auth.rs".into(),
                line: 11,
                note: Some("splits unbounded header".into()),
            },
        ],
        preconditions: Some("attacker sends multi-megabyte colon-separated header".into()),
        checked_guards: vec![CodeRef {
            file: "src/api.rs".into(),
            line: 5,
            note: Some("only checks header.trim().is_empty()".into()),
        }],
        observable_impact: Some("excessive memory allocation per request".into()),
        verification_method: VerificationMethod::CodeTrace,
        repro: None,
        suggested_fix: Some("reject headers longer than 4096 bytes".into()),
    }
}

fn sample_report(
    repo: &GoldenRepo,
    commit: &str,
    depth: ReviewDepth,
    findings: Vec<ReviewFinding>,
) -> ReviewReport {
    ReviewReport {
        header: ArtifactHeader::new(ArtifactKind::Review),
        depth,
        base: repo.v1_commit.clone(),
        head: commit.to_string(),
        scope_files: vec!["src/auth.rs".into(), "src/api.rs".into()],
        checked_areas: vec![],
        findings,
        summary: Some("golden test review".into()),
    }
}

// ===========================================================================
// GitSource & Path Safety Tests
// ===========================================================================

#[test]
fn git_source_reads_commits_and_blocks_traversal() {
    let repo = GoldenRepo::create();
    let src = GitSource::new(repo.path());

    // Valid read at v1 and v2
    let v1 = src
        .read_at(&repo.v1_commit, "src/auth.rs")
        .expect("read v1")
        .expect("file exists");
    assert!(v1.starts_with("// line 1: auth module"));

    let v2 = src
        .read_at(&repo.v2_commit, "./src/auth.rs")
        .expect("read v2 with ./")
        .expect("file exists");
    assert!(v2.starts_with("// shift 1"));

    // Missing file returns Ok(None)
    let missing = src
        .read_at(&repo.v1_commit, "src/does_not_exist.rs")
        .expect("missing file should not error");
    assert!(missing.is_none());

    // Path traversal & unsafe paths MUST be rejected with InvalidPath
    for bad in [
        "../etc/passwd",
        "src/../../etc/passwd",
        "/etc/passwd",
        "-p",
        "",
        "src\\auth.rs",
        "src/auth.rs\0bad",
    ] {
        let err = src.read_at(&repo.v1_commit, bad).unwrap_err();
        assert!(
            matches!(err, EvidenceError::InvalidPath { .. }),
            "expected InvalidPath for {bad:?}, got {err:?}"
        );
    }

    // Invalid commit refs (option injection, empty, '..') rejected
    for bad_commit in ["", "--all", "HEAD..HEAD~1", "HEAD:src/auth.rs"] {
        let err = src.read_at(bad_commit, "src/auth.rs").unwrap_err();
        assert!(
            matches!(err, EvidenceError::InvalidCommit { .. }),
            "expected InvalidCommit for {bad_commit:?}, got {err:?}"
        );
    }
}

// ===========================================================================
// G2 Code Verification Tests (Verified / Drift ±3 / Relocated / Ambiguous / Fabricated)
// ===========================================================================

#[test]
fn g2_verified_exact_whitespace_and_drift_boundary() {
    let repo = GoldenRepo::create();
    let cfg = G2Config::default();

    // Exact match (lines 11-13 in v1), plus extra whitespace & blank line inside quote
    let f_exact = sample_finding(
        "F-exact",
        "src/auth.rs",
        11,
        13,
        "   let   parts: Vec<&str> = input.split(':').collect();\n\n  if  parts.len() < 2 {\n    return None;\n",
    );

    // +3 drift (actual line 11, claimed line 14 -> within ±3 -> Verified!)
    let mut f_drift_plus3 = sample_finding(
        "F-plus3",
        "src/auth.rs",
        14,
        16,
        "let parts: Vec<&str> = input.split(':').collect();\nif parts.len() < 2 {",
    );
    f_drift_plus3.location.line_start = 14;
    f_drift_plus3.location.line_end = 16;

    // -3 drift (actual line 11, claimed line 8 -> within ±3 -> Verified!)
    let f_drift_minus3 = sample_finding(
        "F-minus3",
        "src/auth.rs",
        8,
        10,
        "let parts: Vec<&str> = input.split(':').collect();\nif parts.len() < 2 {",
    );

    let report = sample_report(
        &repo,
        &repo.v1_commit,
        ReviewDepth::D2,
        vec![f_exact, f_drift_plus3, f_drift_minus3],
    );
    let outcomes = verify_g2_review(repo.path(), &report, &cfg).expect("verify");

    for id in ["F-exact", "F-plus3", "F-minus3"] {
        let o = &outcomes[&FindingId::new(id)];
        assert_eq!(o.verdict, EvidenceVerdict::Verified, "{id}: {:?}", o.issues);
        assert!(o.issues.is_empty());
        assert!(o.accepted());
    }
}

#[test]
fn g2_relocated_when_unique_outside_tolerance() {
    let repo = GoldenRepo::create();
    let cfg = G2Config::default();

    // In v2, `src/auth.rs` shifted down by 6 lines: `let parts` moved from line 11 -> line 17.
    // Reviewer claimed line 11 (drift = 6 > 3), unique match in file -> Relocated!
    let f_reloc = sample_finding(
        "F-reloc",
        "src/auth.rs",
        11,
        13,
        "let parts: Vec<&str> = input.split(':').collect();\nif parts.len() < 2 {",
    );

    let report = sample_report(&repo, &repo.v2_commit, ReviewDepth::D2, vec![f_reloc]);
    let outcomes = verify_g2_review(repo.path(), &report, &cfg).expect("verify");
    let o = &outcomes[&FindingId::new("F-reloc")];

    assert_eq!(o.verdict, EvidenceVerdict::Relocated, "{:?}", o.issues);
    assert!(o.accepted());
    assert_eq!(
        o.issues,
        vec![EvidenceIssue::QuoteRelocated {
            file: "src/auth.rs".into(),
            claimed: 11,
            actual: 17,
        }]
    );
}

#[test]
fn g2_ambiguous_when_multiple_matches_outside_tolerance() {
    let repo = GoldenRepo::create();
    let cfg = G2Config::default();

    // `if x < 0 { return 0; }` appears in both duplicate_block_a (line 34) and duplicate_block_b (line 41).
    // Claimed at line 20 (outside ±3 of both 34 and 41) -> QuoteAmbiguous -> Fabricated!
    let mut f_ambig = sample_finding(
        "F-ambig",
        "src/auth.rs",
        20,
        22,
        "if x < 0 {\n    return 0;\n}",
    );
    f_ambig.location.symbol = Some("duplicate_block_a".into());

    let report = sample_report(&repo, &repo.v1_commit, ReviewDepth::D1, vec![f_ambig]);
    let outcomes = verify_g2_review(repo.path(), &report, &cfg).expect("verify");
    let o = &outcomes[&FindingId::new("F-ambig")];

    assert_eq!(o.verdict, EvidenceVerdict::Fabricated);
    assert!(!o.accepted());
    assert!(
        o.issues.contains(&EvidenceIssue::QuoteAmbiguous {
            file: "src/auth.rs".into(),
            occurrences: 2,
        }),
        "expected QuoteAmbiguous(2), got {:?}",
        o.issues
    );
}

#[test]
fn g2_fabricated_for_missing_file_quote_symbol_lines_traversal_and_trigger_path() {
    let repo = GoldenRepo::create();
    let cfg = G2Config::default();

    // 1. Non-existent quote
    let f_fake_quote = sample_finding(
        "F-noquote",
        "src/auth.rs",
        11,
        13,
        "let secret = bypass_authentication(input);",
    );

    // 2. Non-existent file
    let f_missing_file = sample_finding(
        "F-nofile",
        "src/nonexistent.rs",
        1,
        2,
        "pub fn ghost_handler() {}",
    );

    // 3. Path traversal inside finding -> treated as FileMissing -> Fabricated!
    let f_traversal = sample_finding(
        "F-traversal",
        "../../etc/passwd",
        1,
        1,
        "root:x:0:0:root:/root:/bin/bash",
    );

    // 4. Symbol does not exist (even though quote is valid)
    let mut f_bad_sym = sample_finding(
        "F-badsym",
        "src/auth.rs",
        11,
        12,
        "let parts: Vec<&str> = input.split(':').collect();",
    );
    f_bad_sym.location.symbol = Some("nonexistent_symbol".into());

    // 5. trigger_path points to out-of-range line
    let mut f_bad_trigger = sample_finding(
        "F-badtrigger",
        "src/auth.rs",
        11,
        12,
        "let parts: Vec<&str> = input.split(':').collect();",
    );
    f_bad_trigger.trigger_path = vec![CodeRef {
        file: "src/api.rs".into(),
        line: 999,
        note: Some("fabricated line number".into()),
    }];

    // 6. Quote too short (< 8 non-ws chars)
    let f_short = sample_finding("F-short", "src/auth.rs", 9, 9, "None");

    // 7. checked_guards points to missing file
    let mut f_bad_guard = sample_finding(
        "F-badguard",
        "src/auth.rs",
        11,
        12,
        "let parts: Vec<&str> = input.split(':').collect();",
    );
    f_bad_guard.checked_guards = vec![CodeRef {
        file: "src/phantom_guard.rs".into(),
        line: 1,
        note: None,
    }];

    let report = sample_report(
        &repo,
        &repo.v1_commit,
        ReviewDepth::D1,
        vec![
            f_fake_quote,
            f_missing_file,
            f_traversal,
            f_bad_sym,
            f_bad_trigger,
            f_short,
            f_bad_guard,
        ],
    );
    let outcomes = verify_g2_review(repo.path(), &report, &cfg).expect("verify");

    assert_eq!(
        outcomes[&FindingId::new("F-noquote")].verdict,
        EvidenceVerdict::Fabricated
    );
    assert!(outcomes[&FindingId::new("F-noquote")]
        .issues
        .iter()
        .any(|i| matches!(i, EvidenceIssue::QuoteNotFound { .. })));

    assert_eq!(
        outcomes[&FindingId::new("F-nofile")].verdict,
        EvidenceVerdict::Fabricated
    );
    assert!(outcomes[&FindingId::new("F-nofile")]
        .issues
        .iter()
        .any(|i| matches!(i, EvidenceIssue::FileMissing { .. })));

    assert_eq!(
        outcomes[&FindingId::new("F-traversal")].verdict,
        EvidenceVerdict::Fabricated
    );
    assert!(outcomes[&FindingId::new("F-traversal")]
        .issues
        .iter()
        .any(|i| matches!(i, EvidenceIssue::FileMissing { .. })));

    assert_eq!(
        outcomes[&FindingId::new("F-badsym")].verdict,
        EvidenceVerdict::Fabricated
    );
    assert_eq!(
        outcomes[&FindingId::new("F-badsym")].issues,
        vec![EvidenceIssue::SymbolNotFound {
            symbol: "nonexistent_symbol".into()
        }]
    );

    assert_eq!(
        outcomes[&FindingId::new("F-badtrigger")].verdict,
        EvidenceVerdict::Fabricated
    );
    assert!(outcomes[&FindingId::new("F-badtrigger")]
        .issues
        .iter()
        .any(|i| matches!(i, EvidenceIssue::LineOutOfRange { file, line: 999, .. } if file == "src/api.rs")));

    assert_eq!(
        outcomes[&FindingId::new("F-short")].verdict,
        EvidenceVerdict::Fabricated
    );
    assert!(outcomes[&FindingId::new("F-short")]
        .issues
        .iter()
        .any(|i| matches!(i, EvidenceIssue::QuoteTooShort { .. })));

    assert_eq!(
        outcomes[&FindingId::new("F-badguard")].verdict,
        EvidenceVerdict::Fabricated
    );
    assert!(outcomes[&FindingId::new("F-badguard")].issues.iter().any(
        |i| matches!(i, EvidenceIssue::FileMissing { file } if file == "src/phantom_guard.rs")
    ));
}

#[test]
fn g2_review_errors_on_unknown_commit() {
    let repo = GoldenRepo::create();
    let cfg = G2Config::default();
    let report = sample_report(&repo, "deadbeefdeadbeefdeadbeef", ReviewDepth::D1, vec![]);
    let err = verify_g2_review(repo.path(), &report, &cfg).unwrap_err();
    assert!(matches!(err, EvidenceError::GitFailed { .. }));
}

#[test]
fn g2_direct_verify_g2_quotes_works_with_symbol_and_refs() {
    let repo = GoldenRepo::create();
    let src = GitSource::new(repo.path());
    let cfg = G2Config::default();

    let quotes = vec![EvidenceQuote {
        file: "src/auth.rs".into(),
        line_start: 21,
        line_end: 23,
        quote: "pub fn check_admin(tok: &Token) -> bool {\n    tok.role == \"admin\"\n}".into(),
    }];
    let refs = vec![CodeRef {
        file: "src/api.rs".into(),
        line: 9,
        note: None,
    }];

    let ok = verify_g2_quotes(
        &src,
        &repo.v1_commit,
        &quotes,
        &refs,
        Some("check_admin"),
        &cfg,
    );
    assert_eq!(ok.verdict, EvidenceVerdict::Verified);

    let bad_sym = verify_g2_quotes(
        &src,
        &repo.v1_commit,
        &quotes,
        &refs,
        Some("check_adm"),
        &cfg,
    );
    assert_eq!(bad_sym.verdict, EvidenceVerdict::Fabricated);
}

// ===========================================================================
// G2 Log Reference & Combined Evidence Tests (triage.v1 / rca.v1)
// ===========================================================================

#[test]
fn g2_log_refs_verbatim_multiline_drift_relocated_ambiguous_and_traversal() {
    let repo = GoldenRepo::create();
    let cfg = G2Config::default();

    // 1. Verbatim single-line substring (no line number) -> Verified
    let ok_single = verify_g2_log_refs(
        &[EvidenceRef::Log {
            path: "logs/api.log".into(),
            line: None,
            quote: "panic in handle_request: index out of bounds".into(),
        }],
        repo.path(),
        &cfg,
    );
    assert_eq!(ok_single.verdict, EvidenceVerdict::Verified);

    // 2. Verbatim multi-line log stack trace with exact line 3 -> Verified
    let ok_multi = verify_g2_log_refs(
        &[EvidenceRef::Log {
            path: "logs/api.log".into(),
            line: Some(3),
            quote: "panic in handle_request: index out of bounds: the len is 1 but the index is 2\nat src/api.rs:8:15\nat src/auth.rs:14:9".into(),
        }],
        repo.path(),
        &cfg,
    );
    assert_eq!(
        ok_multi.verdict,
        EvidenceVerdict::Verified,
        "{:?}",
        ok_multi.issues
    );

    // 3. Unique log line outside ±3 window -> Relocated
    // Line 10 is `ERROR timeout waiting for db pool after 5000ms`, claimed at line 2 -> Relocated!
    let reloc = verify_g2_log_refs(
        &[EvidenceRef::Log {
            path: "logs/api.log".into(),
            line: Some(2),
            quote: "timeout waiting for db pool after 5000ms".into(),
        }],
        repo.path(),
        &cfg,
    );
    assert_eq!(reloc.verdict, EvidenceVerdict::Relocated);
    assert_eq!(
        reloc.issues,
        vec![EvidenceIssue::QuoteRelocated {
            file: "logs/api.log".into(),
            claimed: 2,
            actual: 10,
        }]
    );

    // 4. Duplicate log line outside ±3 window -> Ambiguous -> Fabricated
    // Lines 8 and 9 both have `retrying upstream request attempt=1`, claimed at line 1
    let ambig = verify_g2_log_refs(
        &[EvidenceRef::Log {
            path: "logs/api.log".into(),
            line: Some(1),
            quote: "retrying upstream request attempt=1".into(),
        }],
        repo.path(),
        &cfg,
    );
    assert_eq!(ambig.verdict, EvidenceVerdict::Fabricated);
    assert!(ambig.issues.contains(&EvidenceIssue::QuoteAmbiguous {
        file: "logs/api.log".into(),
        occurrences: 2,
    }));

    // 5. Fabricated log quote -> LogQuoteNotFound -> Fabricated
    let not_found = verify_g2_log_refs(
        &[EvidenceRef::Log {
            path: "logs/api.log".into(),
            line: None,
            quote: "FATAL segfault in nonexistent_worker".into(),
        }],
        repo.path(),
        &cfg,
    );
    assert_eq!(not_found.verdict, EvidenceVerdict::Fabricated);
    assert_eq!(
        not_found.issues,
        vec![EvidenceIssue::LogQuoteNotFound {
            path: "logs/api.log".into()
        }]
    );

    // 6. Path traversal in log path -> LogMissing -> Fabricated
    let traversal = verify_g2_log_refs(
        &[EvidenceRef::Log {
            path: "../../etc/passwd".into(),
            line: None,
            quote: "root:x:0:0:root".into(),
        }],
        repo.path(),
        &cfg,
    );
    assert_eq!(traversal.verdict, EvidenceVerdict::Fabricated);
    assert_eq!(
        traversal.issues,
        vec![EvidenceIssue::LogMissing {
            path: "../../etc/passwd".into()
        }]
    );

    // 7. Absolute log path inside temp dir works (per contracts §1.5)
    let abs_log = repo.path().join("logs/api.log");
    let ok_abs = verify_g2_log_refs(
        &[EvidenceRef::Log {
            path: abs_log.to_string_lossy().into_owned(),
            line: Some(7),
            quote: "connection pool exhausted active=64".into(),
        }],
        repo.path(),
        &cfg,
    );
    assert_eq!(ok_abs.verdict, EvidenceVerdict::Verified);
}

#[test]
fn g2_combined_evidence_for_triage_and_rca() {
    let repo = GoldenRepo::create();
    let src = GitSource::new(repo.path());
    let cfg = G2Config::default();

    let refs = vec![
        EvidenceRef::Code(EvidenceQuote {
            file: "src/api.rs".into(),
            line_start: 8,
            line_end: 8,
            quote: "let tok = parse_header(header).ok_or(\"bad token\")?;".into(),
        }),
        EvidenceRef::Log {
            path: "logs/api.log".into(),
            line: Some(3),
            quote: "panic in handle_request: index out of bounds".into(),
        },
    ];
    let code_refs = vec![CodeRef {
        file: "src/auth.rs".into(),
        line: 11,
        note: None,
    }];

    let out = verify_g2_evidence(&src, &repo.v1_commit, repo.path(), &refs, &code_refs, &cfg);
    assert_eq!(out.verdict, EvidenceVerdict::Verified);

    // If one ref is fabricated, combined outcome is Fabricated
    let mut bad_refs = refs;
    bad_refs.push(EvidenceRef::Log {
        path: "logs/missing.log".into(),
        line: None,
        quote: "something that does not exist".into(),
    });
    let out_bad = verify_g2_evidence(
        &src,
        &repo.v1_commit,
        repo.path(),
        &bad_refs,
        &code_refs,
        &cfg,
    );
    assert_eq!(out_bad.verdict, EvidenceVerdict::Fabricated);
}

// ===========================================================================
// G1 Depth-Tier & Artifact Validation Tests
// ===========================================================================

#[test]
fn g1_review_zero_findings_contract() {
    let repo = GoldenRepo::create();

    // Valid zero-finding review with checked_areas
    let mut r = sample_report(&repo, &repo.v1_commit, ReviewDepth::D1, vec![]);
    r.checked_areas = vec!["src/auth.rs: checked bounds on parts slice".into()];
    let res = validate_g1_review(&r);
    assert!(res.ok(), "valid zero-finding review failed: {res:?}");

    // Invalid zero-finding review without checked_areas
    r.checked_areas.clear();
    let res = validate_g1_review(&r);
    assert!(!res.ok());
    assert!(res.report.iter().any(|i| i.path == "checked_areas"));
}

#[test]
fn g1_review_depth_tier_progression_d0_through_d3() {
    let repo = GoldenRepo::create();

    // Minimal D0 finding (only 1 trigger_path, no checked_guards, no observable_impact, no preconditions)
    let mut f = sample_finding(
        "F-1",
        "src/auth.rs",
        11,
        12,
        "let parts: Vec<&str> = input.split(':').collect();",
    );
    f.trigger_path.truncate(1);
    f.checked_guards.clear();
    f.observable_impact = None;
    f.preconditions = None;
    f.severity = Severity::Blocker;

    // Passes D0
    let r0 = sample_report(&repo, &repo.v1_commit, ReviewDepth::D0, vec![f.clone()]);
    assert!(validate_g1_review(&r0).ok());

    // D0 rejects Minor/Nit findings
    let mut f_minor = f.clone();
    f_minor.severity = Severity::Minor;
    let r0_minor = sample_report(&repo, &repo.v1_commit, ReviewDepth::D0, vec![f_minor]);
    let g1_r0_minor = validate_g1_review(&r0_minor);
    assert!(!g1_r0_minor.ok());
    assert!(g1_r0_minor
        .report
        .iter()
        .any(|i| i.path.contains("severity")));

    // Fails D1 (missing checked_guards + observable_impact)
    let r1 = sample_report(&repo, &repo.v1_commit, ReviewDepth::D1, vec![f.clone()]);
    let g1_r1 = validate_g1_review(&r1);
    assert!(!g1_r1.ok());
    let f1_issues = &g1_r1.findings[&FindingId::new("F-1")];
    assert!(f1_issues.iter().any(|i| i.path.contains("checked_guards")));
    assert!(f1_issues
        .iter()
        .any(|i| i.path.contains("observable_impact")));

    // Add D1 requirements -> passes D1, fails D2 (needs >=2 trigger_path hops + preconditions)
    f.checked_guards.push(CodeRef {
        file: "src/api.rs".into(),
        line: 5,
        note: None,
    });
    f.observable_impact = Some("OOM crash".into());
    assert!(validate_g1_review(&sample_report(
        &repo,
        &repo.v1_commit,
        ReviewDepth::D1,
        vec![f.clone()]
    ))
    .ok());

    let g1_r2 = validate_g1_review(&sample_report(
        &repo,
        &repo.v1_commit,
        ReviewDepth::D2,
        vec![f.clone()],
    ));
    assert!(!g1_r2.ok());
    let f2_issues = &g1_r2.findings[&FindingId::new("F-1")];
    assert!(f2_issues.iter().any(|i| i.path.contains("trigger_path")));
    assert!(f2_issues.iter().any(|i| i.path.contains("preconditions")));

    // Add D2 requirements -> passes D2, fails D3 (blocker needs repro or ReproTest)
    f.trigger_path.push(CodeRef {
        file: "src/auth.rs".into(),
        line: 11,
        note: None,
    });
    f.preconditions = Some("untrusted HTTP client".into());
    assert!(validate_g1_review(&sample_report(
        &repo,
        &repo.v1_commit,
        ReviewDepth::D2,
        vec![f.clone()]
    ))
    .ok());

    let g1_r3_fail = validate_g1_review(&sample_report(
        &repo,
        &repo.v1_commit,
        ReviewDepth::D3,
        vec![f.clone()],
    ));
    assert!(!g1_r3_fail.ok());
    assert!(g1_r3_fail.findings[&FindingId::new("F-1")]
        .iter()
        .any(|i| i.path.contains("repro")));

    // Satisfying D3 blocker via Repro { kind: Input } passes D3
    f.repro = Some(Repro {
        kind: ReproKind::Input,
        content: Some("a:".repeat(100_000)),
    });
    assert!(validate_g1_review(&sample_report(
        &repo,
        &repo.v1_commit,
        ReviewDepth::D3,
        vec![f.clone()]
    ))
    .ok());

    // Or satisfying D3 blocker via verification_method == ReproTest passes D3
    f.repro = None;
    f.verification_method = VerificationMethod::ReproTest;
    assert!(validate_g1_review(&sample_report(
        &repo,
        &repo.v1_commit,
        ReviewDepth::D3,
        vec![f.clone()]
    ))
    .ok());

    // Cap enforcement: D0 max 5 findings -> 6 findings fails report-level G1
    let six: Vec<ReviewFinding> = (0..6)
        .map(|i| {
            let mut item = f.clone();
            item.id = FindingId::new(format!("F-{i}"));
            item
        })
        .collect();
    let g1_cap = validate_g1_review(&sample_report(&repo, &repo.v1_commit, ReviewDepth::D0, six));
    assert!(!g1_cap.ok());
    assert!(g1_cap.report.iter().any(|i| i.path == "findings"));
}

#[test]
fn g1_other_artifact_validators() {
    // 1. Plan
    let mut plan = PlanDoc {
        header: ArtifactHeader::new(ArtifactKind::Plan),
        goal: "Add mute command".into(),
        design_md: "Add mute state to router".into(),
        tasks: vec![PlanTask {
            id: "T1".into(),
            title: "Add mute handler".into(),
            files: vec!["src/router.rs".into()],
        }],
        acceptance: vec!["mute suppresses feishu".into()],
        test_plan: vec!["cargo test".into()],
        risks: vec![],
        native_strategy: None,
    };
    assert!(validate_g1_plan(&plan).ok());
    plan.tasks.clear();
    assert!(!validate_g1_plan(&plan).ok());

    // 2. Handoff (Review intent requires self_test)
    let mut handoff = HandoffPacket {
        v: PROTO_VERSION,
        id: HandoffId::new("H-1"),
        pipeline_id: None,
        from: AuthorRef {
            agent: AgentKind::Codex,
            session_id: SessionId::new("s1"),
            model: None,
        },
        to: HandoffTarget {
            agent: Some(AgentKind::Agy),
            ..Default::default()
        },
        intent: HandoffIntent::Review,
        mode: AccessMode::ReadOnly,
        repo: None,
        summary: "Implemented mute command".into(),
        body_md: "Details...".into(),
        self_test: None,
        todo: vec![],
        refs: vec![],
        status: HandoffStatus::Pending,
        created_ms: 1,
    };
    assert!(!validate_g1_handoff(&handoff).ok());
    handoff.self_test = Some(TestRun {
        cmd: "cargo test".into(),
        outcome: TestOutcome::Pass,
        output_tail: Some("ok".into()),
    });
    assert!(validate_g1_handoff(&handoff).ok());

    // 3. Triage (1..=2 hypotheses)
    let mut triage = TriageReport {
        header: ArtifactHeader::new(ArtifactKind::Triage),
        symptom: "login 500".into(),
        hypotheses: vec![Hypothesis {
            kind: HypothesisKind::Hypothesis,
            claim: "pool exhausted".into(),
            confidence: 0.7,
            evidence_refs: vec![EvidenceRef::Log {
                path: "logs/api.log".into(),
                line: Some(7),
                quote: "connection pool exhausted active=64".into(),
            }],
            how_to_confirm: "check pg_stat_activity".into(),
        }],
        mitigation: Some("increase max_conns".into()),
        probes: vec![],
        elapsed_secs: Some(120),
    };
    assert!(validate_g1_triage(&triage).ok());
    triage.hypotheses.clear();
    assert!(!validate_g1_triage(&triage).ok());

    // 4. RCA (repro_before fail + verification_after pass when fix present)
    let mut rca = RcaReport {
        header: ArtifactHeader::new(ArtifactKind::Rca),
        triage_id: None,
        root_cause: RootCause {
            claim: "unbounded header split".into(),
            trigger_path: vec![CodeRef {
                file: "src/auth.rs".into(),
                line: 11,
                note: None,
            }],
            evidence: vec![EvidenceRef::Code(EvidenceQuote {
                file: "src/auth.rs".into(),
                line_start: 11,
                line_end: 11,
                quote: "let parts: Vec<&str> = input.split(':').collect();".into(),
            })],
        },
        repro_before: TestRun {
            cmd: "cargo test repro_header".into(),
            outcome: TestOutcome::Fail,
            output_tail: None,
        },
        repro_note: None,
        fix: Some(FixRef {
            branch: "baton/P-1".into(),
            commit: None,
            files: vec!["src/auth.rs".into()],
        }),
        verification_after: Some(TestRun {
            cmd: "cargo test repro_header".into(),
            outcome: TestOutcome::Pass,
            output_tail: None,
        }),
        regression: vec![],
        ruled_out: vec![RuledOut {
            hypothesis: "DNS timeout".into(),
            evidence: vec![EvidenceRef::Log {
                path: "logs/api.log".into(),
                line: Some(1),
                quote: "server started on 127.0.0.1:8080".into(),
            }],
        }],
    };
    assert!(validate_g1_rca(&rca).ok());
    rca.verification_after = None;
    assert!(!validate_g1_rca(&rca).ok());

    // 5. FinalReport
    let mut final_report = FinalReport {
        header: ArtifactHeader::new(ArtifactKind::Report),
        summary_md: "All done".into(),
        changes: vec![],
        decisions: vec![],
        follow_ups: vec![],
        doc_path: None,
    };
    assert!(validate_g1_report(&final_report).ok());
    final_report.summary_md = "   ".into();
    assert!(!validate_g1_report(&final_report).ok());

    // 6. Falsifier (Disproved requires counter_evidence)
    let mut falsifier = FalsifierSubmission {
        review_id: ReviewId::new("R-1"),
        finding_id: FindingId::new("F-1"),
        verdict: FalsifierVerdict::Disproved,
        rationale: "guarded upstream in router".into(),
        counter_evidence: vec![],
        falsifier: AuthorRef::default(),
    };
    assert!(!validate_g1_falsifier(&falsifier).is_empty());
    falsifier.counter_evidence.push(EvidenceQuote {
        file: "src/api.rs".into(),
        line_start: 5,
        line_end: 5,
        quote: "if header.trim().is_empty() {".into(),
    });
    assert!(validate_g1_falsifier(&falsifier).is_empty());

    // 7. normalize_ws check
    assert_eq!(normalize_ws("  a\t  b \n\n  c  \r\n"), "a b\nc");
}

#[test]
fn g2_config_overrides_drift_and_symbol_check() {
    let repo = GoldenRepo::create();
    let src = GitSource::new(repo.path());

    // Claimed line 12 for actual line 11 (drift = 1)
    let quotes = vec![EvidenceQuote {
        file: "src/auth.rs".into(),
        line_start: 12,
        line_end: 13,
        quote: "let parts: Vec<&str> = input.split(':').collect();".into(),
    }];

    // With drift_tolerance = 0 and check_symbols = false, drift=1 becomes Relocated and missing symbol is ignored.
    let strict_cfg = G2Config {
        drift_tolerance: 0,
        min_quote_chars: 8,
        check_symbols: false,
    };
    let out = verify_g2_quotes(
        &src,
        &repo.v1_commit,
        &quotes,
        &[],
        Some("nonexistent_sym"),
        &strict_cfg,
    );
    assert_eq!(out.verdict, EvidenceVerdict::Relocated);
    assert_eq!(
        out.issues,
        vec![EvidenceIssue::QuoteRelocated {
            file: "src/auth.rs".into(),
            claimed: 12,
            actual: 11,
        }]
    );
}
