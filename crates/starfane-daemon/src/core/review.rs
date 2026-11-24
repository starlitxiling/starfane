//! review.v1 pipeline: G1 → G2 → G3 Falsifier → adjudication → fix loop,
//! plus feedback and precision telemetry (architecture §7).

use super::delivery::SpawnPlan;
use super::engine::g2_config;
use super::state::{BindPurpose, StoredReview};
use super::tasks;
use super::{lock, to_json, util, Core};
use crate::notice::{Notice, NoticeKind};
use starfane_proto::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn effective_severity(f: &AdjudicatedFinding) -> Severity {
    match f.falsifier.as_ref().map(|s| s.verdict) {
        Some(FalsifierVerdict::Downgraded { to }) => to,
        _ => f.finding.severity,
    }
}

fn disposition_for(verdict: FalsifierVerdict) -> Disposition {
    match verdict {
        FalsifierVerdict::Confirmed | FalsifierVerdict::NeedsHuman => Disposition::Delivered,
        FalsifierVerdict::Downgraded { to } if to.is_actionable() => Disposition::Delivered,
        FalsifierVerdict::Downgraded { .. } => Disposition::Folded,
        FalsifierVerdict::Disproved => Disposition::Dropped,
    }
}

fn same_model(a: &AuthorRef, b: &AuthorRef) -> bool {
    a.agent == b.agent
        && match (&a.model, &b.model) {
            (Some(x), Some(y)) => x.eq_ignore_ascii_case(y),
            _ => true, // unknown model on the same agent counts as same
        }
}

impl Core {
    pub(crate) fn list_reviews(&self, pipeline: Option<&PipelineId>) -> Vec<ReviewRecord> {
        let mut v: Vec<ReviewRecord> = lock(&self.inner.state)
            .reviews
            .values()
            .filter(|r| pipeline.is_none_or(|p| r.pipeline_id.as_ref() == Some(p)))
            .map(|r| r.record.clone())
            .collect();
        v.sort_by_key(|r| std::cmp::Reverse(r.report.header.created_ms));
        v
    }

    pub(crate) async fn submit_review(
        &self,
        session: &SessionRecord,
        mut report: ReviewReport,
    ) -> Result<serde_json::Value, ApiError> {
        for f in report.findings.iter_mut() {
            if f.id.is_empty() {
                f.id = FindingId(id::new_id(id::prefix::FINDING));
            }
        }
        let owned = self.owned_stage(session, &[StageKind::TestReview]);
        let pipeline = owned.as_ref().map(|(p, _)| p.clone());
        let project_root = pipeline
            .as_ref()
            .map(|p| p.project.root.clone())
            .or_else(|| session.git_root.clone())
            .ok_or_else(|| {
                ApiError::bad_request("cannot determine the repository for this review")
            })?;
        let repo_dir = pipeline
            .as_ref()
            .and_then(|p| {
                lock(&self.inner.state)
                    .worktrees
                    .get(&p.id)
                    .map(|w| w.path.clone())
            })
            .unwrap_or_else(|| project_root.clone());

        let mut g1 = starfane_evidence::validate_g1_review(&report);
        // A G1 return happens at most once per pipeline round.
        let round = pipeline
            .as_ref()
            .and_then(|p| p.stage(StageKind::Implement).map(|s| s.round))
            .unwrap_or(0);
        let already_returned = pipeline.as_ref().is_some_and(|p| {
            lock(&self.inner.state).reviews.values().any(|r| {
                r.pipeline_id.as_ref() == Some(&p.id)
                    && r.record.returned_once
                    && !r.record.complete
                    && r.record.report.header.stage == Some(StageKind::TestReview)
            })
        });

        // G2 (only meaningful when the head commit exists).
        let g2_map: Result<BTreeMap<FindingId, G2Outcome>, String> = {
            let (dir, rep) = (PathBuf::from(&repo_dir), report.clone());
            tokio::task::spawn_blocking(move || {
                starfane_evidence::verify_g2_review(&dir, &rep, &g2_config())
                    .map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
        };
        let g2_map = match g2_map {
            Ok(m) => m,
            Err(e) => {
                g1.report
                    .push(G1Issue::new("head", format!("cannot verify evidence: {e}")));
                BTreeMap::new()
            }
        };

        if !g1.report.is_empty() && !already_returned {
            let rid = ReviewId(id::new_id(id::prefix::REVIEW));
            let mut payload = ArtifactPayload::Review(report.clone());
            let aid = self
                .store_artifact(session, &mut payload, Some(project_root.clone()))
                .await;
            if let ArtifactPayload::Review(r) = payload {
                report = r;
            }
            let record = ReviewRecord {
                v: PROTO_VERSION,
                id: rid.clone(),
                report,
                report_g1: g1.report.clone(),
                findings: Vec::new(),
                returned_once: true,
                complete: false,
            };
            self.store_review(StoredReview {
                record,
                project_root,
                repo_dir,
                reviewer: author_of(session),
                pipeline_id: pipeline.as_ref().map(|p| p.id.clone()),
                falsifier_session: None,
            });
            return to_json(SubmitOutcome {
                artifact_id: aid,
                accepted: false,
                g1,
                g2: g2_map.into_iter().map(|(k, v)| (k.0, v)).collect(),
                review_id: Some(rid),
                pending_falsifier: Vec::new(),
                next: Some(
                    "报告级 G1 未通过（只会退回这一次）：按 issues 补全后重新调用 submit_review"
                        .into(),
                ),
            });
        }
        // Close any earlier returned record for this round.
        if let Some(p) = &pipeline {
            let mut st = lock(&self.inner.state);
            for r in st.reviews.values_mut() {
                if r.pipeline_id.as_ref() == Some(&p.id)
                    && r.record.returned_once
                    && !r.record.complete
                {
                    r.record.complete = true;
                }
            }
        }

        let in_pipeline = pipeline.is_some();
        let findings: Vec<AdjudicatedFinding> = report
            .findings
            .iter()
            .map(|f| {
                let issues = g1.findings.get(&f.id).cloned().unwrap_or_default();
                let g2 = g2_map.get(&f.id).cloned();
                let disposition = if !issues.is_empty() {
                    Disposition::ReturnedG1
                } else if g2.as_ref().is_some_and(|o| !o.accepted()) {
                    Disposition::Fabricated
                } else if f.severity.is_actionable() && in_pipeline {
                    Disposition::PendingFalsifier
                } else if f.severity.is_actionable() {
                    Disposition::Delivered
                } else {
                    Disposition::Folded
                };
                AdjudicatedFinding {
                    finding: f.clone(),
                    g1: issues,
                    g2,
                    falsifier: None,
                    disposition,
                    feedback: None,
                }
            })
            .collect();
        let pending: Vec<FindingId> = findings
            .iter()
            .filter(|f| f.disposition == Disposition::PendingFalsifier)
            .map(|f| f.finding.id.clone())
            .collect();

        let mut payload = ArtifactPayload::Review(report.clone());
        let aid = self
            .store_artifact(session, &mut payload, Some(project_root.clone()))
            .await;
        if let ArtifactPayload::Review(r) = payload {
            report = r;
        }
        let rid = ReviewId(id::new_id(id::prefix::REVIEW));
        let record = ReviewRecord {
            v: PROTO_VERSION,
            id: rid.clone(),
            report,
            report_g1: g1.report.clone(),
            findings,
            returned_once: already_returned,
            complete: pending.is_empty(),
        };
        self.store_review(StoredReview {
            record,
            project_root,
            repo_dir: repo_dir.clone(),
            reviewer: author_of(session),
            pipeline_id: pipeline.as_ref().map(|p| p.id.clone()),
            falsifier_session: None,
        });
        if let Some(p) = &pipeline {
            self.update_pipeline(&p.id, |p| {
                if let Some(s) = p.stage_mut(StageKind::TestReview) {
                    s.artifacts.push(aid.clone());
                }
            })?;
        }

        let next = if !pending.is_empty() {
            self.request_falsification(&rid, &pending).await;
            format!(
                "已通过证据校验；{} 条 blocker/major 正在交给另一模型证伪复核",
                pending.len()
            )
        } else {
            match self.finish_review(&rid).await {
                Ok(()) => "Review 已裁决完成".to_string(),
                Err(e) => format!("Review 已裁决完成；后续阶段启动失败：{}", e.message),
            }
        };
        log::info!(
            "review {rid} (round {round}) submitted by {}",
            session.session_id
        );
        to_json(SubmitOutcome {
            artifact_id: aid,
            accepted: true,
            g1,
            g2: g2_map.into_iter().map(|(k, v)| (k.0, v)).collect(),
            review_id: Some(rid),
            pending_falsifier: pending,
            next: Some(next),
        })
    }

    fn store_review(&self, r: StoredReview) {
        let path = paths::project_dir(Path::new(&r.project_root))
            .join("reviews")
            .join(format!("{}.json", r.record.id));
        match serde_json::to_vec_pretty(&r.record) {
            Ok(bytes) => {
                if let Err(e) = crate::store::atomic_write(&path, &bytes) {
                    log::warn!("write {}: {e}", path.display());
                }
            }
            Err(e) => log::error!("serialize review: {e}"),
        }
        let rid = r.record.id.clone();
        lock(&self.inner.state).reviews.insert(rid.clone(), r);
        self.mark_dirty();
        self.publish(|seq| DaemonEvent::Review {
            seq,
            review_id: rid,
        });
    }

    fn persist_review(&self, rid: &ReviewId) {
        let r = lock(&self.inner.state).reviews.get(rid).cloned();
        if let Some(r) = r {
            self.store_review(r);
        }
    }

    /// Pick (or spawn) a session on a different model and send the request.
    async fn request_falsification(&self, rid: &ReviewId, ids: &[FindingId]) {
        let Some(rev) = lock(&self.inner.state).reviews.get(rid).cloned() else {
            return;
        };
        let (title, body) = tasks::render_falsify_request(&rev.record, ids, &rev.repo_dir);
        let item = InboxItem {
            id: id::new_id("Q"),
            kind: InboxKind::FalsifyRequest,
            from: Some(rev.reviewer.clone()),
            created_ms: id::now_ms(),
            title,
            body_md: body,
            stage: None,
            artifact: None,
            handoff_id: None,
            review_id: Some(rid.clone()),
        };
        let candidate = {
            let st = lock(&self.inner.state);
            let author_of_builder = rev.pipeline_id.as_ref().and_then(|pid| {
                st.pipelines
                    .get(pid)
                    .and_then(|p| p.stage(StageKind::Implement))
                    .and_then(|s| s.session_id.clone())
            });
            let mut c: Vec<&SessionRecord> = st
                .sessions
                .values()
                .filter(|s| !s.is_subagent && s.state == SessionState::Idle && s.pane.is_some())
                .filter(|s| s.git_root.as_deref() == Some(rev.project_root.as_str()))
                .filter(|s| !same_model(&author_of(s), &rev.reviewer))
                .collect();
            // Prefer a neutral session over the builder.
            c.sort_by_key(|s| {
                (
                    Some(&s.session_id) == author_of_builder.as_ref(),
                    std::cmp::Reverse(s.last_event_ms),
                )
            });
            c.first().map(|s| s.session_id.clone())
        };
        let line = format!(
            "starfane: 你收到一个证伪请求（Review {}），请调用 starfane 的 read_inbox 工具获取",
            util::short_id(rid.as_str())
        );
        match candidate {
            Some(sid) => {
                lock(&self.inner.state).push_inbox(&sid, item);
                if let Some(r) = lock(&self.inner.state).reviews.get_mut(rid) {
                    r.falsifier_session = Some(sid.clone());
                }
                self.mark_dirty();
                let core = self.clone();
                tokio::spawn(async move {
                    let r = core
                        .deliver_line(
                            &sid,
                            &line,
                            &[DeliveryLevel::L1CodexQueue, DeliveryLevel::L2Inject],
                        )
                        .await;
                    log::info!("falsify request to {sid}: {:?}", r.status);
                });
            }
            None => {
                let agent = [AgentKind::Claude, AgentKind::Codex, AgentKind::Agy]
                    .into_iter()
                    .find(|a| *a != rev.reviewer.agent)
                    .unwrap_or(AgentKind::Claude);
                let plan = SpawnPlan {
                    purpose: BindPurpose::Falsifier {
                        review_id: rid.clone(),
                    },
                    agent,
                    model: None,
                    effort: None,
                    cwd: rev.repo_dir.clone(),
                    name: format!("falsify-{}", util::short_id(rid.as_str())),
                    prompt: line,
                    project_root: rev.project_root.clone(),
                };
                match self.spawn_session(plan).await {
                    Ok((token, _pane)) => {
                        {
                            let mut st = lock(&self.inner.state);
                            let key = st
                                .binds
                                .get(&token)
                                .and_then(|b| b.session_id.clone())
                                .unwrap_or_else(|| Core::inbox_key_for_bind(&token));
                            st.push_inbox(&key, item);
                        }
                        self.mark_dirty();
                    }
                    Err(e) => {
                        log::warn!("no falsifier available for {rid}: {}", e.message);
                        // Without G3 the findings go to a human instead of being dropped.
                        {
                            let mut st = lock(&self.inner.state);
                            if let Some(r) = st.reviews.get_mut(rid) {
                                for f in r.record.findings.iter_mut() {
                                    if f.disposition == Disposition::PendingFalsifier {
                                        f.disposition = Disposition::Delivered;
                                        f.falsifier = Some(FalsifierSubmission {
                                            review_id: rid.clone(),
                                            finding_id: f.finding.id.clone(),
                                            verdict: FalsifierVerdict::NeedsHuman,
                                            rationale: format!("无可用的证伪会话：{}", e.message),
                                            counter_evidence: Vec::new(),
                                            falsifier: AuthorRef::default(),
                                        });
                                    }
                                }
                                r.record.complete = true;
                            }
                        }
                        self.persist_review(rid);
                        if let Err(e) = self.finish_review(rid).await {
                            log::warn!("finish review {rid}: {}", e.message);
                        }
                    }
                }
            }
        }
    }

    pub(crate) async fn submit_falsifier(
        &self,
        sid: &SessionId,
        mut sub: FalsifierSubmission,
    ) -> Result<serde_json::Value, ApiError> {
        let session = lock(&self.inner.state)
            .sessions
            .get(sid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("unknown session {sid}")))?;
        let issues = starfane_evidence::validate_g1_falsifier(&sub);
        if !issues.is_empty() {
            return Err(ApiError::bad_request(format!(
                "falsifier verdict rejected by G1: {}",
                issues
                    .iter()
                    .map(|i| format!("{}: {}", i.path, i.reason))
                    .collect::<Vec<_>>()
                    .join("; ")
            )));
        }
        let rev = lock(&self.inner.state)
            .reviews
            .get(&sub.review_id)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("review {}", sub.review_id)))?;
        let author = author_of(&session);
        if same_model(&author, &rev.reviewer) {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                "the falsifier must run on a different model than the reviewer",
            ));
        }
        let finding = rev
            .record
            .findings
            .iter()
            .find(|f| f.finding.id == sub.finding_id)
            .ok_or_else(|| ApiError::not_found(format!("finding {}", sub.finding_id)))?;
        if finding.disposition != Disposition::PendingFalsifier {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!(
                    "finding {} is not awaiting falsification ({:?})",
                    sub.finding_id, finding.disposition
                ),
            ));
        }
        sub.falsifier = author;
        let mut g2 = None;
        let mut effective = sub.verdict;
        if sub.verdict == FalsifierVerdict::Disproved {
            let (dir, head, quotes) = (
                PathBuf::from(&rev.repo_dir),
                rev.record.report.head.clone(),
                sub.counter_evidence.clone(),
            );
            let outcome = tokio::task::spawn_blocking(move || {
                let src = starfane_evidence::GitSource::new(dir);
                starfane_evidence::verify_g2_quotes(&src, &head, &quotes, &[], None, &g2_config())
            })
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?;
            if !outcome.accepted() {
                effective = FalsifierVerdict::NeedsHuman;
                sub.rationale = format!("[原判 disproved，反证未通过 G2 校验] {}", sub.rationale);
            }
            g2 = Some(outcome);
        }
        sub.verdict = effective;
        let complete = {
            let mut st = lock(&self.inner.state);
            let r = st
                .reviews
                .get_mut(&sub.review_id)
                .ok_or_else(|| ApiError::not_found("review vanished"))?;
            if let Some(f) = r
                .record
                .findings
                .iter_mut()
                .find(|f| f.finding.id == sub.finding_id)
            {
                f.disposition = disposition_for(effective);
                f.falsifier = Some(sub.clone());
            }
            r.record.complete = !r
                .record
                .findings
                .iter()
                .any(|f| f.disposition == Disposition::PendingFalsifier);
            r.record.complete
        };
        self.persist_review(&sub.review_id);
        if complete {
            if let Err(e) = self.finish_review(&sub.review_id).await {
                log::warn!("after falsification of {}: {}", sub.review_id, e.message);
            }
        }
        to_json(FalsifierOutcome {
            accepted: true,
            g2,
            effective,
        })
    }

    pub(crate) async fn human_verdict(
        &self,
        rid: &ReviewId,
        fid: &FindingId,
        verdict: FalsifierVerdict,
        note: Option<String>,
        by: String,
    ) -> Result<serde_json::Value, ApiError> {
        let (was_complete, now_complete, finding) = {
            let mut st = lock(&self.inner.state);
            let r = st
                .reviews
                .get_mut(rid)
                .ok_or_else(|| ApiError::not_found(format!("review {rid}")))?;
            let was = r.record.complete;
            let f = r
                .record
                .findings
                .iter_mut()
                .find(|f| &f.finding.id == fid)
                .ok_or_else(|| ApiError::not_found(format!("finding {fid}")))?;
            f.disposition = disposition_for(verdict);
            f.falsifier = Some(FalsifierSubmission {
                review_id: rid.clone(),
                finding_id: fid.clone(),
                verdict,
                rationale: format!("人工裁决（{by}）：{}", note.unwrap_or_default()),
                counter_evidence: Vec::new(),
                falsifier: AuthorRef::default(),
            });
            let finding = f.clone();
            r.record.complete = !r
                .record
                .findings
                .iter()
                .any(|f| f.disposition == Disposition::PendingFalsifier);
            (was, r.record.complete, finding)
        };
        self.persist_review(rid);
        if !was_complete && now_complete {
            if let Err(e) = self.finish_review(rid).await {
                log::warn!("after human verdict on {rid}: {}", e.message);
            }
        }
        to_json(finding)
    }

    pub(crate) fn review_feedback(
        &self,
        rid: &ReviewId,
        fid: &FindingId,
        label: FeedbackLabel,
        note: Option<String>,
        by: String,
    ) -> Result<serde_json::Value, ApiError> {
        let record = FeedbackRecord {
            label,
            note,
            by,
            ts_ms: id::now_ms(),
        };
        let root = {
            let mut st = lock(&self.inner.state);
            let r = st
                .reviews
                .get_mut(rid)
                .ok_or_else(|| ApiError::not_found(format!("review {rid}")))?;
            let f = r
                .record
                .findings
                .iter_mut()
                .find(|f| &f.finding.id == fid)
                .ok_or_else(|| ApiError::not_found(format!("finding {fid}")))?;
            f.feedback = Some(record.clone());
            r.project_root.clone()
        };
        self.persist_review(rid);
        let line = serde_json::json!({"review_id": rid, "finding_id": fid, "feedback": record});
        let path = paths::project_dir(Path::new(&root)).join("feedback.jsonl");
        if let Err(e) = crate::store::append_json_line(&path, &line) {
            log::warn!("write {}: {e}", path.display());
        }
        Ok(serde_json::json!({}))
    }

    pub(crate) fn precision(
        &self,
        agent: Option<AgentKind>,
        model: Option<&str>,
    ) -> Vec<PrecisionStat> {
        type Key = (AgentKind, Option<String>, ReviewDepth, Category);
        let mut acc: BTreeMap<Key, PrecisionStat> = BTreeMap::new();
        let st = lock(&self.inner.state);
        for r in st.reviews.values() {
            let reviewer = &r.reviewer;
            if agent.is_some_and(|a| a != reviewer.agent) {
                continue;
            }
            if model.is_some_and(|m| reviewer.model.as_deref() != Some(m)) {
                continue;
            }
            for f in &r.record.findings {
                let key = (
                    reviewer.agent,
                    reviewer.model.clone(),
                    r.record.report.depth,
                    f.finding.category,
                );
                let s = acc.entry(key).or_insert_with(|| PrecisionStat {
                    agent: reviewer.agent,
                    model: reviewer.model.clone(),
                    depth: r.record.report.depth,
                    category: f.finding.category,
                    delivered: 0,
                    valid: 0,
                    false_positive: 0,
                    wontfix: 0,
                    fabricated: 0,
                    disproved: 0,
                    precision: None,
                });
                match f.disposition {
                    Disposition::Delivered => s.delivered += 1,
                    Disposition::Fabricated => s.fabricated += 1,
                    Disposition::Dropped => s.disproved += 1,
                    _ => {}
                }
                match f.feedback.as_ref().map(|fb| fb.label) {
                    Some(FeedbackLabel::Valid) => s.valid += 1,
                    Some(FeedbackLabel::FalsePositive) => s.false_positive += 1,
                    Some(FeedbackLabel::Wontfix) => s.wontfix += 1,
                    None => {}
                }
            }
        }
        acc.into_values()
            .map(|mut s| {
                s.precision = PrecisionStat::compute_precision(s.valid, s.false_positive);
                s
            })
            .collect()
    }

    /// All findings adjudicated: loop back to the builder or move on.
    async fn finish_review(&self, rid: &ReviewId) -> Result<(), ApiError> {
        let Some(rev) = lock(&self.inner.state).reviews.get(rid).cloned() else {
            return Ok(());
        };
        let actionable: Vec<&AdjudicatedFinding> = rev
            .record
            .findings
            .iter()
            .filter(|f| {
                f.disposition == Disposition::Delivered && effective_severity(f).is_actionable()
            })
            .collect();
        let count = |d: Disposition| {
            rev.record
                .findings
                .iter()
                .filter(|f| f.disposition == d)
                .count()
        };
        let summary = format!(
            "**交付**：{} 条（需修复 {} 条）· **折叠**：{} · **证伪剔除**：{} · **臆造拦截**：{} · **G1 退回**：{}",
            count(Disposition::Delivered),
            actionable.len(),
            count(Disposition::Folded),
            count(Disposition::Dropped),
            count(Disposition::Fabricated),
            count(Disposition::ReturnedG1)
        );
        let Some(pid) = rev.pipeline_id.clone() else {
            return Ok(());
        };
        let p = lock(&self.inner.state)
            .pipelines
            .get(&pid)
            .cloned()
            .ok_or_else(|| ApiError::not_found(format!("pipeline {pid}")))?;
        let round = p.stage(StageKind::Implement).map(|s| s.round).unwrap_or(0);
        let max = p.options.max_review_rounds;
        let mut body = summary;
        for f in actionable.iter().take(5) {
            body.push_str(&format!(
                "\n- [{:?}] {} (`{}:{}`)",
                effective_severity(f),
                util::sanitize_line(&f.finding.claim, 120),
                f.finding.location.file,
                f.finding.location.line_start
            ));
        }
        self.notify(Notice {
            key: format!("review-{}", util::short_id(rid.as_str())),
            kind: NoticeKind::ReviewResult,
            title: format!("🧪 Review 结论：{}", util::sanitize_line(&p.goal, 40)),
            body_md: body,
            pipeline_id: Some(pid.clone()),
            session_id: None,
            review_id: Some(rid.clone()),
            actions: Vec::new(),
            binding_form: Vec::new(),
            reply_session: None,
        });

        if actionable.is_empty() {
            return self.advance(&pid, StageKind::TestReview).await;
        }
        if round >= max {
            self.update_pipeline(&pid, |p| {
                p.status = PipelineStatus::AwaitingHuman;
                if let Some(s) = p.stage_mut(StageKind::TestReview) {
                    s.status = StageStatus::AwaitingHuman;
                }
            })?;
            return Ok(());
        }
        // Back to the builder with the confirmed findings.
        self.update_pipeline(&pid, |p| {
            if let Some(s) = p.stage_mut(StageKind::TestReview) {
                s.status = StageStatus::Pending;
                s.ended_ms = Some(id::now_ms());
            }
            if let Some(s) = p.stage_mut(StageKind::Implement) {
                s.status = StageStatus::Pending;
                s.round += 1;
            }
            p.status = PipelineStatus::Running;
        })?;
        // Prefer the same builder session.
        let builder = p
            .stage(StageKind::Implement)
            .and_then(|s| s.session_id.clone());
        if let Some(sid) = builder {
            self.update_pipeline(&pid, |p| {
                if let Some(s) = p.stage_mut(StageKind::Implement) {
                    s.binding.session = SessionTarget::Session(sid.clone());
                }
            })?;
        }
        self.start_stage(&pid, StageKind::Implement).await
    }
}

fn author_of(s: &SessionRecord) -> AuthorRef {
    AuthorRef {
        agent: s.agent,
        session_id: s.session_id.clone(),
        model: s.model.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispositions() {
        assert_eq!(
            disposition_for(FalsifierVerdict::Confirmed),
            Disposition::Delivered
        );
        assert_eq!(
            disposition_for(FalsifierVerdict::NeedsHuman),
            Disposition::Delivered
        );
        assert_eq!(
            disposition_for(FalsifierVerdict::Disproved),
            Disposition::Dropped
        );
        assert_eq!(
            disposition_for(FalsifierVerdict::Downgraded {
                to: Severity::Minor
            }),
            Disposition::Folded
        );
        assert_eq!(
            disposition_for(FalsifierVerdict::Downgraded {
                to: Severity::Major
            }),
            Disposition::Delivered
        );
    }

    #[test]
    fn model_comparison() {
        let a = AuthorRef {
            agent: AgentKind::Codex,
            session_id: SessionId::new("a"),
            model: Some("gpt-5".into()),
        };
        let mut b = a.clone();
        assert!(same_model(&a, &b));
        b.model = Some("o3".into());
        assert!(!same_model(&a, &b));
        b.agent = AgentKind::Claude;
        b.model = None;
        assert!(!same_model(&a, &b));
        let mut c = a.clone();
        c.model = None;
        assert!(same_model(&a, &c));
    }
}
