//! G1 structural & depth-tier schema validation (contracts §1.6 & §5).

use starfane_proto::{
    CodeRef, EvidenceQuote, EvidenceRef, FalsifierSubmission, FalsifierVerdict, FinalReport,
    FindingId, G1Issue, G1Result, HandoffIntent, HandoffPacket, PlanDoc, RcaReport, ReproKind,
    ReviewDepth, ReviewReport, Severity, TestOutcome, TriageReport, VerificationMethod,
};

/// Minimum number of non-whitespace characters required in an evidence quote (contracts §5).
pub const DEFAULT_MIN_QUOTE_CHARS: usize = 8;

/// Maximum characters allowed in a handoff summary (contracts §1.6).
const MAX_HANDOFF_SUMMARY_CHARS: usize = 500;

/// Maximum characters allowed in a `TestRun::output_tail` (contracts §1.5).
const MAX_TEST_OUTPUT_TAIL_CHARS: usize = 2000;

pub(crate) fn non_ws_char_count(s: &str) -> usize {
    s.chars().filter(|c| !c.is_whitespace()).count()
}

fn is_blank(s: &str) -> bool {
    s.trim().is_empty()
}

fn validate_code_ref(path: &str, r: &CodeRef, out: &mut Vec<G1Issue>) {
    if is_blank(&r.file) {
        out.push(G1Issue::new(
            format!("{path}.file"),
            "file must be non-empty",
        ));
    }
    if r.line == 0 {
        out.push(G1Issue::new(format!("{path}.line"), "line must be >= 1"));
    }
}

fn validate_quote(path: &str, q: &EvidenceQuote, min_chars: usize, out: &mut Vec<G1Issue>) {
    if is_blank(&q.file) {
        out.push(G1Issue::new(
            format!("{path}.file"),
            "file must be non-empty",
        ));
    }
    if q.line_start == 0 {
        out.push(G1Issue::new(
            format!("{path}.line_start"),
            "line_start must be >= 1",
        ));
    }
    if q.line_start > q.line_end {
        out.push(G1Issue::new(
            format!("{path}.line_start"),
            format!(
                "line_start ({}) must be <= line_end ({})",
                q.line_start, q.line_end
            ),
        ));
    }
    let chars = non_ws_char_count(&q.quote);
    if chars < min_chars {
        out.push(G1Issue::new(
            format!("{path}.quote"),
            format!("quote must have at least {min_chars} non-whitespace characters (got {chars})"),
        ));
    }
}

fn validate_evidence_ref(path: &str, r: &EvidenceRef, min_chars: usize, out: &mut Vec<G1Issue>) {
    match r {
        EvidenceRef::Code(q) => validate_quote(path, q, min_chars, out),
        EvidenceRef::Log {
            path: log_path,
            line,
            quote,
        } => {
            if is_blank(log_path) {
                out.push(G1Issue::new(
                    format!("{path}.path"),
                    "log path must be non-empty",
                ));
            }
            if let Some(l) = line {
                if *l == 0 {
                    out.push(G1Issue::new(
                        format!("{path}.line"),
                        "log line must be >= 1 when specified",
                    ));
                }
            }
            let chars = non_ws_char_count(quote);
            if chars < min_chars {
                out.push(G1Issue::new(
                    format!("{path}.quote"),
                    format!(
                        "log quote must have at least {min_chars} non-whitespace characters (got {chars})"
                    ),
                ));
            }
        }
    }
}

/// Validate `plan.v1` structural requirements (contracts §1.6).
pub fn validate_g1_plan(doc: &PlanDoc) -> G1Result {
    let mut res = G1Result::default();
    if is_blank(&doc.goal) {
        res.report
            .push(G1Issue::new("goal", "goal must be non-empty"));
    }
    if is_blank(&doc.design_md) {
        res.report
            .push(G1Issue::new("design_md", "design_md must be non-empty"));
    }
    if doc.tasks.is_empty() {
        res.report
            .push(G1Issue::new("tasks", "tasks must contain at least 1 task"));
    } else {
        for (i, t) in doc.tasks.iter().enumerate() {
            if is_blank(&t.id) {
                res.report.push(G1Issue::new(
                    format!("tasks[{i}].id"),
                    "task id must be non-empty",
                ));
            }
            if is_blank(&t.title) {
                res.report.push(G1Issue::new(
                    format!("tasks[{i}].title"),
                    "task title must be non-empty",
                ));
            }
        }
    }
    if doc.acceptance.is_empty() || doc.acceptance.iter().all(|s| is_blank(s)) {
        res.report.push(G1Issue::new(
            "acceptance",
            "acceptance must contain at least 1 non-empty criterion",
        ));
    }
    if doc.test_plan.is_empty() || doc.test_plan.iter().all(|s| is_blank(s)) {
        res.report.push(G1Issue::new(
            "test_plan",
            "test_plan must contain at least 1 non-empty item",
        ));
    }
    res
}

/// Validate `handoff.v1` structural requirements (contracts §1.6).
pub fn validate_g1_handoff(p: &HandoffPacket) -> G1Result {
    let mut res = G1Result::default();
    if p.to.is_empty() {
        res.report.push(G1Issue::new(
            "to",
            "handoff target must specify session_id, pane, agent, or stage",
        ));
    }
    if is_blank(&p.summary) {
        res.report
            .push(G1Issue::new("summary", "summary must be non-empty"));
    } else if p.summary.chars().count() > MAX_HANDOFF_SUMMARY_CHARS {
        res.report.push(G1Issue::new(
            "summary",
            format!("summary must be at most {MAX_HANDOFF_SUMMARY_CHARS} characters"),
        ));
    }
    if is_blank(&p.body_md) {
        res.report
            .push(G1Issue::new("body_md", "body_md must be non-empty"));
    }

    match (&p.intent, &p.self_test) {
        (HandoffIntent::Review, None) => {
            res.report.push(G1Issue::new(
                "self_test",
                "self_test is required when intent is review",
            ));
        }
        (_, Some(st)) => {
            if is_blank(&st.cmd) {
                res.report.push(G1Issue::new(
                    "self_test.cmd",
                    "self_test.cmd must be non-empty",
                ));
            }
            if let Some(tail) = &st.output_tail {
                if tail.chars().count() > MAX_TEST_OUTPUT_TAIL_CHARS {
                    res.report.push(G1Issue::new(
                        "self_test.output_tail",
                        format!(
                            "output_tail must be at most {MAX_TEST_OUTPUT_TAIL_CHARS} characters"
                        ),
                    ));
                }
            }
        }
        (_, None) => {}
    }
    res
}

/// Validate `review.v1` against G1 depth-tier rules (contracts §5).
///
/// Allows valid zero-finding reviews when `checked_areas` has at least one non-empty entry.
pub fn validate_g1_review(r: &ReviewReport) -> G1Result {
    let mut res = G1Result::default();

    if is_blank(&r.base) {
        res.report
            .push(G1Issue::new("base", "base commit must be non-empty"));
    }
    if is_blank(&r.head) {
        res.report
            .push(G1Issue::new("head", "head commit must be non-empty"));
    }

    let cap = r.depth.max_findings();
    if r.findings.len() > cap {
        res.report.push(G1Issue::new(
            "findings",
            format!(
                "depth {} allows at most {cap} findings (got {})",
                r.depth.as_str(),
                r.findings.len()
            ),
        ));
    }

    if r.findings.is_empty() {
        if r.checked_areas.is_empty() || r.checked_areas.iter().all(|s| is_blank(s)) {
            res.report.push(G1Issue::new(
                "checked_areas",
                "zero-finding review must include at least 1 checked_areas entry",
            ));
        }
        return res;
    }

    for (i, f) in r.findings.iter().enumerate() {
        let prefix = format!("findings[{i}]");

        // Report-level D0 severity constraint (contracts §5 table).
        if r.depth == ReviewDepth::D0 && !f.severity.is_actionable() {
            res.report.push(G1Issue::new(
                format!("{prefix}.severity"),
                "depth D0 only allows blocker or major findings",
            ));
        }

        let mut issues: Vec<G1Issue> = Vec::new();

        // Rules for all depths:
        if is_blank(&f.claim) {
            issues.push(G1Issue::new(
                format!("{prefix}.claim"),
                "claim must be non-empty",
            ));
        }
        if !(0.0..=1.0).contains(&f.confidence) {
            issues.push(G1Issue::new(
                format!("{prefix}.confidence"),
                format!("confidence must be in [0.0, 1.0] (got {})", f.confidence),
            ));
        }
        if is_blank(&f.location.file) {
            issues.push(G1Issue::new(
                format!("{prefix}.location.file"),
                "location.file must be non-empty",
            ));
        }
        if f.location.line_start == 0 {
            issues.push(G1Issue::new(
                format!("{prefix}.location.line_start"),
                "location.line_start must be >= 1",
            ));
        }
        if f.location.line_start > f.location.line_end {
            issues.push(G1Issue::new(
                format!("{prefix}.location.line_start"),
                format!(
                    "location.line_start ({}) must be <= line_end ({})",
                    f.location.line_start, f.location.line_end
                ),
            ));
        }

        if f.evidence.is_empty() {
            issues.push(G1Issue::new(
                format!("{prefix}.evidence"),
                "evidence must contain at least 1 quote",
            ));
        } else {
            for (j, q) in f.evidence.iter().enumerate() {
                validate_quote(
                    &format!("{prefix}.evidence[{j}]"),
                    q,
                    DEFAULT_MIN_QUOTE_CHARS,
                    &mut issues,
                );
            }
        }

        // D0+: trigger_path >= 1
        if f.trigger_path.is_empty() {
            issues.push(G1Issue::new(
                format!("{prefix}.trigger_path"),
                "trigger_path must contain at least 1 CodeRef",
            ));
        } else {
            for (j, cr) in f.trigger_path.iter().enumerate() {
                validate_code_ref(&format!("{prefix}.trigger_path[{j}]"), cr, &mut issues);
            }
        }

        // D1+: checked_guards >= 1 and observable_impact non-empty
        if r.depth >= ReviewDepth::D1 {
            if f.checked_guards.is_empty() {
                issues.push(G1Issue::new(
                    format!("{prefix}.checked_guards"),
                    "depth D1+ requires at least 1 checked_guards entry",
                ));
            } else {
                for (j, cg) in f.checked_guards.iter().enumerate() {
                    validate_code_ref(&format!("{prefix}.checked_guards[{j}]"), cg, &mut issues);
                }
            }
            if f.observable_impact.as_deref().map(is_blank).unwrap_or(true) {
                issues.push(G1Issue::new(
                    format!("{prefix}.observable_impact"),
                    "depth D1+ requires non-empty observable_impact",
                ));
            }
        } else {
            // Still validate any checked_guards provided at D0.
            for (j, cg) in f.checked_guards.iter().enumerate() {
                validate_code_ref(&format!("{prefix}.checked_guards[{j}]"), cg, &mut issues);
            }
        }

        // D2+: trigger_path >= 2 and preconditions non-empty
        if r.depth >= ReviewDepth::D2 {
            if f.trigger_path.len() == 1 {
                issues.push(G1Issue::new(
                    format!("{prefix}.trigger_path"),
                    "depth D2+ requires at least 2 hops in trigger_path",
                ));
            }
            if f.preconditions.as_deref().map(is_blank).unwrap_or(true) {
                issues.push(G1Issue::new(
                    format!("{prefix}.preconditions"),
                    "depth D2+ requires non-empty preconditions",
                ));
            }
        }

        // D3: blocker findings require repro.kind != None or verification_method == ReproTest
        if r.depth == ReviewDepth::D3 && f.severity == Severity::Blocker {
            let has_repro = f
                .repro
                .as_ref()
                .is_some_and(|rp| rp.kind != ReproKind::None);
            let is_repro_test = f.verification_method == VerificationMethod::ReproTest;
            if !has_repro && !is_repro_test {
                issues.push(G1Issue::new(
                    format!("{prefix}.repro"),
                    "depth D3 blocker requires repro.kind != none or verification_method == repro_test",
                ));
            }
        }

        if !issues.is_empty() {
            let key = if f.id.0.is_empty() {
                FindingId::new(prefix)
            } else {
                f.id.clone()
            };
            res.findings.insert(key, issues);
        }
    }

    res
}

/// Validate `triage.v1` structural requirements (contracts §1.6).
pub fn validate_g1_triage(t: &TriageReport) -> G1Result {
    let mut res = G1Result::default();
    if is_blank(&t.symptom) {
        res.report
            .push(G1Issue::new("symptom", "symptom must be non-empty"));
    }
    if t.hypotheses.is_empty() || t.hypotheses.len() > 2 {
        res.report.push(G1Issue::new(
            "hypotheses",
            format!(
                "triage must include 1..=2 hypotheses (got {})",
                t.hypotheses.len()
            ),
        ));
    }
    for (i, h) in t.hypotheses.iter().enumerate() {
        let prefix = format!("hypotheses[{i}]");
        if is_blank(&h.claim) {
            res.report.push(G1Issue::new(
                format!("{prefix}.claim"),
                "hypothesis claim must be non-empty",
            ));
        }
        if !(0.0..=1.0).contains(&h.confidence) {
            res.report.push(G1Issue::new(
                format!("{prefix}.confidence"),
                format!("confidence must be in [0.0, 1.0] (got {})", h.confidence),
            ));
        }
        if is_blank(&h.how_to_confirm) {
            res.report.push(G1Issue::new(
                format!("{prefix}.how_to_confirm"),
                "how_to_confirm must be non-empty",
            ));
        }
        if h.evidence_refs.is_empty() {
            res.report.push(G1Issue::new(
                format!("{prefix}.evidence_refs"),
                "hypothesis must include at least 1 evidence_ref",
            ));
        } else {
            for (j, ev) in h.evidence_refs.iter().enumerate() {
                validate_evidence_ref(
                    &format!("{prefix}.evidence_refs[{j}]"),
                    ev,
                    DEFAULT_MIN_QUOTE_CHARS,
                    &mut res.report,
                );
            }
        }
    }
    res
}

/// Validate `rca.v1` structural requirements (contracts §1.6).
pub fn validate_g1_rca(r: &RcaReport) -> G1Result {
    let mut res = G1Result::default();
    if is_blank(&r.root_cause.claim) {
        res.report.push(G1Issue::new(
            "root_cause.claim",
            "root_cause.claim must be non-empty",
        ));
    }
    if r.root_cause.trigger_path.is_empty() {
        res.report.push(G1Issue::new(
            "root_cause.trigger_path",
            "root_cause.trigger_path must contain at least 1 CodeRef",
        ));
    } else {
        for (i, cr) in r.root_cause.trigger_path.iter().enumerate() {
            validate_code_ref(
                &format!("root_cause.trigger_path[{i}]"),
                cr,
                &mut res.report,
            );
        }
    }
    if r.root_cause.evidence.is_empty() {
        res.report.push(G1Issue::new(
            "root_cause.evidence",
            "root_cause.evidence must contain at least 1 EvidenceRef",
        ));
    } else {
        for (i, ev) in r.root_cause.evidence.iter().enumerate() {
            validate_evidence_ref(
                &format!("root_cause.evidence[{i}]"),
                ev,
                DEFAULT_MIN_QUOTE_CHARS,
                &mut res.report,
            );
        }
    }

    if is_blank(&r.repro_before.cmd) {
        res.report.push(G1Issue::new(
            "repro_before.cmd",
            "repro_before.cmd must be non-empty",
        ));
    }
    if r.repro_before.outcome != TestOutcome::Fail
        && r.repro_note.as_deref().map(is_blank).unwrap_or(true)
    {
        res.report.push(G1Issue::new(
            "repro_before.outcome",
            "repro_before.outcome must be fail unless repro_note explains why not reproducible",
        ));
    }

    if let Some(fix) = &r.fix {
        if is_blank(&fix.branch) {
            res.report
                .push(G1Issue::new("fix.branch", "fix.branch must be non-empty"));
        }
        if fix.files.is_empty() {
            res.report.push(G1Issue::new(
                "fix.files",
                "fix.files must list at least 1 modified file",
            ));
        }
        match &r.verification_after {
            None => {
                res.report.push(G1Issue::new(
                    "verification_after",
                    "verification_after is required when fix is present",
                ));
            }
            Some(ver) => {
                if is_blank(&ver.cmd) {
                    res.report.push(G1Issue::new(
                        "verification_after.cmd",
                        "verification_after.cmd must be non-empty",
                    ));
                }
                if ver.outcome != TestOutcome::Pass {
                    res.report.push(G1Issue::new(
                        "verification_after.outcome",
                        "verification_after.outcome must be pass when fix is present",
                    ));
                }
            }
        }
    }

    for (i, ro) in r.ruled_out.iter().enumerate() {
        if is_blank(&ro.hypothesis) {
            res.report.push(G1Issue::new(
                format!("ruled_out[{i}].hypothesis"),
                "ruled_out hypothesis must be non-empty",
            ));
        }
        if ro.evidence.is_empty() {
            res.report.push(G1Issue::new(
                format!("ruled_out[{i}].evidence"),
                "ruled_out entry must include at least 1 evidence ref",
            ));
        } else {
            for (j, ev) in ro.evidence.iter().enumerate() {
                validate_evidence_ref(
                    &format!("ruled_out[{i}].evidence[{j}]"),
                    ev,
                    DEFAULT_MIN_QUOTE_CHARS,
                    &mut res.report,
                );
            }
        }
    }

    res
}

/// Validate `report.v1` structural requirements (contracts §1.6).
pub fn validate_g1_report(r: &FinalReport) -> G1Result {
    let mut res = G1Result::default();
    if is_blank(&r.summary_md) {
        res.report
            .push(G1Issue::new("summary_md", "summary_md must be non-empty"));
    }
    res
}

/// Validate a G3 falsifier submission (contracts §1.6 & §5).
pub fn validate_g1_falsifier(s: &FalsifierSubmission) -> Vec<G1Issue> {
    let mut issues = Vec::new();
    if is_blank(&s.rationale) {
        issues.push(G1Issue::new("rationale", "rationale must be non-empty"));
    }
    if s.verdict == FalsifierVerdict::Disproved {
        if s.counter_evidence.is_empty() {
            issues.push(G1Issue::new(
                "counter_evidence",
                "disproved verdict requires at least 1 counter_evidence quote",
            ));
        } else {
            for (i, q) in s.counter_evidence.iter().enumerate() {
                validate_quote(
                    &format!("counter_evidence[{i}]"),
                    q,
                    DEFAULT_MIN_QUOTE_CHARS,
                    &mut issues,
                );
            }
        }
    }
    issues
}
