//! Task / request bodies delivered to agents through the inbox (rendered
//! Markdown; usable verbatim as instructions).

use super::util;
use baton_proto::*;

pub(crate) struct StageCtx<'a> {
    pub pipeline: &'a PipelineSpec,
    pub stage: StageKind,
    pub cwd: &'a str,
    pub plan: Option<&'a PlanDoc>,
    pub handoff: Option<&'a HandoffPacket>,
    pub triage: Option<&'a TriageReport>,
    pub base: Option<&'a str>,
    pub review_findings: Option<&'a [AdjudicatedFinding]>,
}

pub(crate) fn role_name(stage: StageKind) -> &'static str {
    match stage {
        StageKind::Plan => "Planner",
        StageKind::Implement => "Builder",
        StageKind::TestReview => "Reviewer",
        StageKind::Report => "Reporter",
        StageKind::Triage => "快筛执行者",
        StageKind::Deep => "深挖执行者",
    }
}

pub(crate) fn pointer(pipeline: &PipelineId, stage: StageKind) -> String {
    format!(
        "baton: 你是 {} 的 {}（{}）。请先调用 baton 的 read_inbox 工具获取任务与上下文。",
        util::short_id(pipeline.as_str()),
        role_name(stage),
        stage.as_str()
    )
}

fn header(c: &StageCtx<'_>) -> String {
    format!(
        "# baton 任务：{} / {}\n\n- Pipeline：`{}`（{}）\n- 项目：`{}`\n- 工作目录：`{}`\n- 目标：{}\n\n",
        role_name(c.stage),
        c.stage.as_str(),
        c.pipeline.id,
        c.pipeline.playbook.as_str(),
        c.pipeline.project.root,
        c.cwd,
        c.pipeline.goal
    )
}

const MCP_NOTE: &str = "\n> 所有结果都通过 baton 的 MCP 工具提交（不要只在终端里回复）。\n";

pub(crate) fn render_stage_task(c: &StageCtx<'_>) -> (String, String) {
    let mut body = header(c);
    let pid = c.pipeline.id.as_str();
    match c.stage {
        StageKind::Plan => {
            body.push_str(
                "## 你要做的\n1. 阅读仓库，设计方案并拆分任务。\n2. 写清**验收标准**和**测试计划**。\n\
                 3. 调用 `report_milestone`，参数 `kind=\"plan\"`，并在 `plan` 中填写 goal、design_md、tasks、acceptance、test_plan、risks。\n\
                 4. 提交后等待人工批准；如收到修改意见，修正后重新提交。\n",
            );
        }
        StageKind::Implement => {
            if let Some(p) = c.plan {
                body.push_str(&format!("## 已批准的计划\n{}\n\n### 任务\n", p.design_md));
                for t in &p.tasks {
                    body.push_str(&format!("- [{}] {}\n", t.id, t.title));
                }
                body.push_str("\n### 验收标准\n");
                for a in &p.acceptance {
                    body.push_str(&format!("- {a}\n"));
                }
                body.push_str("\n### 测试计划\n");
                for t in &p.test_plan {
                    body.push_str(&format!("- {t}\n"));
                }
            }
            if let Some(findings) = c.review_findings {
                body.push_str("\n## 需要修复的 Review 发现（已通过证据校验与证伪复核）\n");
                body.push_str(&render_findings(findings));
            }
            body.push_str(&format!(
                "\n## 你要做的\n1. 在工作目录 `{}` 中实现（这是独立的 git worktree 分支）。\n\
                 2. 在会话中运行测试。\n3. **把改动 git commit 到当前分支**（Reviewer 会按提交校验证据）。\n\
                 4. 调用 `create_handoff`：`to={{\"stage\":{{\"pipeline_id\":\"{pid}\",\"stage\":\"test_review\"}}}}`、`intent=\"review\"`、`mode=\"read_only\"`，\
                 填写 summary、body_md，并且**必须**带上 `self_test`（cmd + outcome）。\n",
                c.cwd
            ));
        }
        StageKind::TestReview => {
            let depth = c
                .pipeline
                .stage(StageKind::TestReview)
                .and_then(|s| s.binding.depth)
                .unwrap_or(c.pipeline.options.review_depth);
            if let Some(h) = c.handoff {
                body.push_str(&format!(
                    "## Builder 交接\n**摘要**：{}\n\n{}\n\n",
                    h.summary, h.body_md
                ));
                if let Some(t) = &h.self_test {
                    body.push_str(&format!("自测：`{}` → {:?}\n\n", t.cmd, t.outcome));
                }
                if let Some(r) = &h.repo {
                    body.push_str(&format!(
                        "- 分支：`{}`\n- base：`{}`\n- head：`{}`\n",
                        r.branch.as_deref().unwrap_or("?"),
                        c.base.or(r.base.as_deref()).unwrap_or("?"),
                        r.head.as_deref().unwrap_or("?")
                    ));
                    if !r.dirty.is_empty() {
                        body.push_str(&format!("- ⚠ 未提交文件：{}\n", r.dirty.join(", ")));
                    }
                }
            }
            body.push_str(&format!(
                "\n## 你要做的（review.v1，深度 {}）\n1. 按测试计划运行测试。\n\
                 2. 审查 base..head 的改动。**每条发现都必须**：逐字引用 head 提交中的代码（`evidence`）、给出 `trigger_path`（入口→问题点，每一跳带 file:line）、\
                 列出看过的 `checked_guards` 及为什么挡不住、`observable_impact`、`verification_method`。\n\
                 3. 没有真实问题时，提交 `findings: []` 并在 `checked_areas` 写明看过的范围——这是合法且受鼓励的结果。\n\
                 4. 数量上限 {} 条；风格问题请标为 nit。\n5. 调用 `submit_review`（`review.head` 填 head 提交）。\n\
                 > 证据会被机器逐字校验，臆造的引用会被自动拒绝并计入误报统计。\n",
                depth.as_str(),
                depth.max_findings()
            ));
        }
        StageKind::Report => {
            body.push_str(
                "## 你要做的\n1. 汇总本次流程：做了什么、关键决策、测试与 Review 结果、遗留问题。\n\
                 2. 调用 `report_milestone`，参数 `kind=\"final_report\"`，`report={summary_md, changes, decisions, follow_ups}`。\n",
            );
        }
        StageKind::Triage => {
            body.push_str(&format!(
                "## 现象\n{}\n\n## 你要做的（Phase A 快筛，软时限 {} 分钟）\n1. 尽快定位 Top 1–2 个**假设**（不是结论），每个都附逐字证据（代码 `type=code` 或日志 `type=log`）和置信度。\n\
                 2. 给出临时止血建议（mitigation）和下一步探针（probes）。\n3. 调用 `submit_triage`。\n",
                c.pipeline.goal,
                c.pipeline.options.triage_timebox_secs / 60
            ));
        }
        StageKind::Deep => {
            body.push_str(&format!("## 现象\n{}\n\n", c.pipeline.goal));
            if let Some(t) = c.triage {
                body.push_str("## 快筛假设（未经证实）\n");
                for h in &t.hypotheses {
                    body.push_str(&format!(
                        "- ({:.0}%) {} — 验证方法：{}\n",
                        h.confidence * 100.0,
                        h.claim,
                        h.how_to_confirm
                    ));
                }
                if let Some(m) = &t.mitigation {
                    body.push_str(&format!("\n止血建议：{m}\n"));
                }
            }
            body.push_str(&format!(
                "\n## 你要做的（Phase B 深挖）\n1. 先**复现**：写失败测试或复现命令（`repro_before` 必须为 fail，或说明无法复现的原因）。\n\
                 2. 给出根因链 `trigger_path` 和逐字证据；列出已排除的假设。\n3. 在 `{}` 修复并 commit，跑回归（`verification_after` 必须为 pass）。\n\
                 4. 调用 `report_milestone`，参数 `kind=\"rca\"`。\n",
                c.cwd
            ));
        }
    }
    body.push_str(MCP_NOTE);
    let title = format!(
        "{} {}：{}",
        util::short_id(pid),
        role_name(c.stage),
        util::sanitize_line(&c.pipeline.goal, 40)
    );
    (title, body)
}

pub(crate) fn render_findings(findings: &[AdjudicatedFinding]) -> String {
    let mut out = String::new();
    for f in findings {
        let fi = &f.finding;
        out.push_str(&format!(
            "\n### {} [{:?}/{:?}] {}\n- 位置：`{}:{}-{}`\n",
            fi.id,
            fi.severity,
            fi.category,
            fi.claim,
            fi.location.file,
            fi.location.line_start,
            fi.location.line_end
        ));
        for e in &fi.evidence {
            out.push_str(&format!(
                "- 证据 `{}:{}-{}`：\n```\n{}\n```\n",
                e.file, e.line_start, e.line_end, e.quote
            ));
        }
        if !fi.trigger_path.is_empty() {
            let path: Vec<String> = fi
                .trigger_path
                .iter()
                .map(|r| format!("{}:{}", r.file, r.line))
                .collect();
            out.push_str(&format!("- 触发路径：{}\n", path.join(" → ")));
        }
        if let Some(i) = &fi.observable_impact {
            out.push_str(&format!("- 影响：{i}\n"));
        }
        if let Some(fix) = &fi.suggested_fix {
            out.push_str(&format!("- 建议修复：{fix}\n"));
        }
        if let Some(fs) = &f.falsifier {
            out.push_str(&format!("- 复核：{:?} — {}\n", fs.verdict, fs.rationale));
        }
    }
    out
}

pub(crate) fn render_falsify_request(
    review: &ReviewRecord,
    ids: &[FindingId],
    repo_dir: &str,
) -> (String, String) {
    let findings: Vec<AdjudicatedFinding> = review
        .findings
        .iter()
        .filter(|f| ids.contains(&f.finding.id))
        .cloned()
        .collect();
    let body = format!(
        "# baton 证伪请求：Review `{}`\n\n你是**证伪者（Falsifier）**：唯一任务是尝试**推翻**下列发现。请在 `{}` 阅读 head 提交 `{}` 的真实代码、调用方与测试。\n\n\
         对每条发现调用一次 `submit_falsifier_verdict`（`review_id=\"{}\"`）：\n\
         - `confirmed`：确认问题真实且可触发；\n- `downgraded` + `to`：真实但严重度被高估；\n\
         - `disproved`：**必须**附逐字反证 `counter_evidence`（file:line + quote），否则会被降级为 needs_human；\n\
         - `needs_human`：无法证实也无法推翻。\n\n## 待复核发现\n{}",
        review.id,
        repo_dir,
        review.report.head,
        review.id,
        render_findings(&findings)
    );
    (
        format!("证伪请求 {}", util::short_id(review.id.as_str())),
        body,
    )
}
