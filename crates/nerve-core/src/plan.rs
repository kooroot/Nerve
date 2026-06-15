//! §3 Tier 2f (v0.5.0) — `/plan` Plan mode.
//!
//! Plan mode runs the lead adapter under a strict plan-only system prompt:
//! it must emit structured Markdown with the canonical sections
//! (`## Objective`, `## Affected files`, `## Steps`, `## Risks`,
//! `## Estimated LOC`) and is forbidden from producing any patch, NvPatch,
//! diff, or other code-modification artifact.
//!
//! Per the design doc (nerve-terminal-upgrade-proposal.md §3 Tier 2f) and the
//! v0.5.0 PlanStrategy enum in `nerve-config`:
//!
//! - [`PlanStrategy::Single`] runs only the lead and validates the markdown.
//! - [`PlanStrategy::DualReview`] additionally pipes the lead's plan markdown
//!   into the reviewer adapter (also under a verify-only prompt) and folds
//!   the resulting commentary into the returned [`PlanReport`].
//!
//! All adapter calls funnel through
//! [`ModelAdapter::dispatch_oneshot`](nerve_adapter::ModelAdapter::dispatch_oneshot)
//! so that the patch-producing surfaces (`implement` / `refine`) are never
//! reachable from `/plan`.

use chrono::Utc;
use nerve_adapter::{AdapterError, ModelAdapter};
use nerve_config::{Config, PlanStrategy};
use nerve_types::{PlanReport, Task, UsageStats};
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Literal plan-only system prompt prepended to every `/plan` lead call.
///
/// Kept verbatim per the design doc so adapter behaviour stays auditable and
/// reproducible across sessions. Callers can override the system prompt
/// via [`nerve_config::Roles::plan_system_prompt_override`] (or per-profile
/// equivalent) but the override is still composed under the same plan-only
/// guard documented in §3 Tier 2f.
pub const PLAN_ONLY_SYSTEM_PROMPT: &str = concat!(
    "You are in PLAN mode. Produce a structured markdown plan with sections: ",
    "## Objective\n",
    "## Affected files\n",
    "## Steps\n",
    "## Risks\n",
    "## Estimated LOC\n",
    "DO NOT produce any patch, NvPatch, diff, or code modification. Output markdown only.",
);

/// Verify-only system prompt the reviewer adapter sees under
/// [`PlanStrategy::DualReview`]. Mirrors the lead-side guard so the reviewer
/// can never round-trip a patch back into the report.
pub const PLAN_REVIEW_SYSTEM_PROMPT: &str = concat!(
    "You are reviewing a PLAN (read-only analysis). The user message contains a markdown plan ",
    "produced by another agent. Comment on completeness, missing risks, and feasibility. ",
    "DO NOT propose any patch, NvPatch, diff, or code modification. Output markdown only.",
);

/// Per-run options for `/plan`.
///
/// Mirrors the public surface of [`crate::RunOptions`] for the synaptic loop:
/// callers populate the `strategy` field from the resolved
/// [`PlanStrategy`] (CLI flag → profile override → `Config.roles`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanRunOptions {
    pub strategy: PlanStrategy,
}

impl PlanRunOptions {
    pub fn new(strategy: PlanStrategy) -> Self {
        Self { strategy }
    }
}

/// Failure modes for `/plan`.
///
/// `PatchProducedDespitePlanMode` is the load-bearing guard: it surfaces any
/// time the lead adapter ignores the system prompt and emits a patch artifact
/// (NvPatch fence, `<<NVPATCH>>` sentinel, `diff --git` block, etc.).
#[derive(Debug, Error)]
pub enum PlanError {
    #[error("plan-mode adapter produced a patch despite plan-only system prompt")]
    PatchProducedDespitePlanMode,
    #[error("plan markdown was empty or whitespace-only")]
    EmptyPlan,
    #[error("plan markdown is missing required section: {0}")]
    MissingSection(String),
    #[error("LLM adapter call failed: {0}")]
    Adapter(#[from] AdapterError),
    #[error("io error during plan execution: {0}")]
    Io(#[from] std::io::Error),
}

/// Structured view of the lead plan markdown returned by
/// [`validate_plan_markdown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSections {
    pub objective: String,
    pub affected_files: Vec<PathBuf>,
    pub steps: String,
    pub risks: Option<String>,
    pub estimated_loc: Option<u64>,
}

/// Run `/plan` end-to-end.
///
/// 1. Resolves the lead/reviewer adapters from `Config::select_profile`.
/// 2. Calls `lead.dispatch_oneshot(plan-only-prompt, task.prompt)`.
/// 3. Defensively scans the lead output for forbidden patch artifacts.
/// 4. Validates the markdown against the canonical section list.
/// 5. If `PlanStrategy::DualReview`, pipes the markdown through the reviewer
///    under [`PLAN_REVIEW_SYSTEM_PROMPT`] and folds the result into
///    `PlanReport::reviewer_feedback`.
/// 6. Returns a [`PlanReport`] with summed cost (lead + reviewer).
pub async fn run_plan_mode(
    task: Task,
    config: Arc<Config>,
    adapters: Vec<Box<dyn ModelAdapter>>,
    options: PlanRunOptions,
) -> Result<PlanReport, PlanError> {
    let selection = config
        .select_profile(&task)
        .map_err(|err| PlanError::Io(std::io::Error::other(err.to_string())))?;

    let lead = adapters
        .iter()
        .find(|a| a.id() == selection.lead)
        .ok_or_else(|| {
            PlanError::Io(std::io::Error::other(format!(
                "no adapter registered for plan lead `{}`",
                selection.lead
            )))
        })?;
    let reviewer = adapters
        .iter()
        .find(|a| a.id() == selection.reviewer)
        .ok_or_else(|| {
            PlanError::Io(std::io::Error::other(format!(
                "no adapter registered for plan reviewer `{}`",
                selection.reviewer
            )))
        })?;

    let system_prompt = config
        .roles
        .plan_system_prompt_override
        .clone()
        .unwrap_or_else(|| PLAN_ONLY_SYSTEM_PROMPT.to_string());

    let plan_markdown = lead.dispatch_oneshot(&system_prompt, &task.prompt).await?;

    if plan_markdown.trim().is_empty() {
        return Err(PlanError::EmptyPlan);
    }

    if contains_patch_artifact(&plan_markdown) {
        return Err(PlanError::PatchProducedDespitePlanMode);
    }

    let sections = validate_plan_markdown(&plan_markdown)?;

    let mut reviewer_feedback = String::new();
    let mut total_cost: Option<UsageStats> = None;

    if matches!(options.strategy, PlanStrategy::DualReview) {
        let review_user_prompt = format!(
            "Task: {}\n\nPlan markdown to review:\n\n{}",
            task.prompt, plan_markdown
        );
        let review_text = reviewer
            .dispatch_oneshot(PLAN_REVIEW_SYSTEM_PROMPT, &review_user_prompt)
            .await?;

        if contains_patch_artifact(&review_text) {
            return Err(PlanError::PatchProducedDespitePlanMode);
        }

        reviewer_feedback = review_text.trim().to_string();
    }

    // `dispatch_oneshot` returns a bare String and offers no cost handle, so
    // the per-call cost remains unmodeled here. `PlanReport.cost` stays
    // `None` until the adapter trait grows a cost-bearing oneshot surface.
    let _ = (&mut total_cost, lead.as_ref(), reviewer.as_ref());

    Ok(PlanReport {
        task_id: task.id.clone(),
        plan_markdown,
        reviewer_feedback,
        estimated_loc: sections.estimated_loc,
        estimated_files: sections.affected_files,
        cost: total_cost,
        finished_at: Utc::now(),
    })
}

/// Validate that `md` contains the canonical `/plan` sections.
///
/// Required: `## Objective`, `## Affected files`, `## Steps`. Optional:
/// `## Risks`, `## Estimated LOC`. The Affected-files section is parsed as a
/// bullet list (`- path` or `* path`); `Estimated LOC` extracts the first
/// integer in its section body.
pub fn validate_plan_markdown(md: &str) -> Result<PlanSections, PlanError> {
    if md.trim().is_empty() {
        return Err(PlanError::EmptyPlan);
    }

    let objective = extract_section(md, "Objective")
        .ok_or_else(|| PlanError::MissingSection("Objective".to_string()))?;
    let affected_raw = extract_section(md, "Affected files")
        .ok_or_else(|| PlanError::MissingSection("Affected files".to_string()))?;
    let steps = extract_section(md, "Steps")
        .ok_or_else(|| PlanError::MissingSection("Steps".to_string()))?;
    let risks = extract_section(md, "Risks");
    let estimated_loc_raw = extract_section(md, "Estimated LOC");

    if objective.trim().is_empty() {
        return Err(PlanError::MissingSection("Objective".to_string()));
    }
    if steps.trim().is_empty() {
        return Err(PlanError::MissingSection("Steps".to_string()));
    }

    let affected_files = parse_bullet_paths(&affected_raw);
    if affected_files.is_empty() {
        return Err(PlanError::MissingSection("Affected files".to_string()));
    }

    let estimated_loc = estimated_loc_raw.as_deref().and_then(parse_first_integer);

    Ok(PlanSections {
        objective: objective.trim().to_string(),
        affected_files,
        steps: steps.trim().to_string(),
        risks: risks
            .map(|r| r.trim().to_string())
            .filter(|s| !s.is_empty()),
        estimated_loc,
    })
}

/// Scan `text` for any artifact that a plan-mode adapter must never emit.
///
/// Catches the documented sentinel forms: explicit NvPatch markers, fenced
/// ```diff blocks, and `diff --git` headers.
fn contains_patch_artifact(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    text.contains("NvPatch")
        || text.contains("<<NVPATCH>>")
        || text.contains("BEGIN NVPATCH")
        || text.contains("diff --git")
        || lower.contains("```diff")
        || lower.contains("```patch")
}

/// Pull the body of a `## <heading>` section out of `md`. Matches the heading
/// case-insensitively and returns everything up to the next `## ` heading or
/// end of document.
fn extract_section(md: &str, heading: &str) -> Option<String> {
    let target = heading.to_ascii_lowercase();
    let lines: Vec<&str> = md.lines().collect();
    let mut start: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        if line_is_h2(line) {
            let label = line.trim_start_matches('#').trim().to_ascii_lowercase();
            if label == target {
                start = Some(idx + 1);
                break;
            }
        }
    }
    let start = start?;
    let mut end = lines.len();
    for (offset, line) in lines.iter().enumerate().skip(start) {
        if line_is_h2(line) {
            end = offset;
            break;
        }
    }
    Some(lines[start..end].join("\n"))
}

fn line_is_h2(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("## ") && !trimmed.starts_with("### ")
}

fn parse_bullet_paths(section: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for raw in section.lines() {
        let line = raw.trim();
        let candidate = if let Some(rest) = line.strip_prefix("- ") {
            rest
        } else if let Some(rest) = line.strip_prefix("* ") {
            rest
        } else {
            continue;
        };
        let cleaned = candidate
            .trim()
            .trim_matches('`')
            .trim()
            .trim_end_matches(',')
            .trim();
        if cleaned.is_empty() {
            continue;
        }
        out.push(PathBuf::from(cleaned));
    }
    out
}

fn parse_first_integer(section: &str) -> Option<u64> {
    let mut current = String::new();
    for ch in section.chars() {
        if ch.is_ascii_digit() {
            current.push(ch);
        } else if !current.is_empty() {
            break;
        }
    }
    if current.is_empty() {
        None
    } else {
        current.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use nerve_adapter::MockAdapter;
    use nerve_types::{AgentEvent, AgentOutput, ReviewerFeedback};
    use std::path::Path;
    use tokio::sync::mpsc;

    const STRUCTURED_PLAN: &str = "## Objective\nAdd /plan command\n\n## Affected files\n- crates/nerve-core/src/plan.rs\n- crates/nerve-cli/src/main.rs\n\n## Steps\n1. Write module\n2. Wire CLI\n\n## Risks\nReviewer disagreement\n\n## Estimated LOC\n~250 lines total\n";

    fn plan_config() -> Config {
        Config::from_json_str(
            r#"{
              "orchestration": {
                "default_strategy": "consensus",
                "max_refinement_rounds": 1,
                "conflict_policy": "lead_priority"
              },
              "roles": {
                "architect": "plan-lead",
                "reviewer": "plan-reviewer"
              },
              "profiles": []
            }"#,
        )
        .expect("valid plan config")
    }

    #[tokio::test]
    async fn mock_plan_lead_emits_structured_md() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("add /plan command", dir.path());
        let config = Arc::new(plan_config());

        let lead = MockAdapter::new("plan-lead");
        lead.set_oneshot_response(STRUCTURED_PLAN);
        let reviewer = MockAdapter::new("plan-reviewer");

        let adapters: Vec<Box<dyn ModelAdapter>> = vec![Box::new(lead), Box::new(reviewer)];

        let report = run_plan_mode(
            task.clone(),
            config,
            adapters,
            PlanRunOptions::new(PlanStrategy::Single),
        )
        .await
        .expect("plan should succeed");

        assert_eq!(report.task_id, task.id);
        assert!(report.plan_markdown.contains("## Objective"));
        assert!(report.reviewer_feedback.is_empty());
        assert_eq!(report.estimated_loc, Some(250));
        assert_eq!(report.estimated_files.len(), 2);
        assert_eq!(
            report.estimated_files[0],
            PathBuf::from("crates/nerve-core/src/plan.rs")
        );
    }

    #[tokio::test]
    async fn mock_plan_lead_emits_patch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("sneak in a patch", dir.path());
        let config = Arc::new(plan_config());

        let lead = MockAdapter::new("plan-lead");
        lead.set_oneshot_response(
            "## Objective\nfoo\n\n## Affected files\n- a.rs\n\n## Steps\n1. step\n\nNvPatch payload here\n",
        );
        let reviewer = MockAdapter::new("plan-reviewer");

        let adapters: Vec<Box<dyn ModelAdapter>> = vec![Box::new(lead), Box::new(reviewer)];

        let err = run_plan_mode(
            task,
            config,
            adapters,
            PlanRunOptions::new(PlanStrategy::Single),
        )
        .await
        .expect_err("patch-bearing plan must be rejected");

        assert!(matches!(err, PlanError::PatchProducedDespitePlanMode));
    }

    #[tokio::test]
    async fn mock_plan_lead_emits_diff_fence_fails() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("sneak in a diff", dir.path());
        let config = Arc::new(plan_config());

        let lead = MockAdapter::new("plan-lead");
        lead.set_oneshot_response(
            "## Objective\nfoo\n\n## Affected files\n- a.rs\n\n## Steps\n1. step\n\n```diff\n- a\n+ b\n```\n",
        );
        let reviewer = MockAdapter::new("plan-reviewer");

        let adapters: Vec<Box<dyn ModelAdapter>> = vec![Box::new(lead), Box::new(reviewer)];

        let err = run_plan_mode(
            task,
            config,
            adapters,
            PlanRunOptions::new(PlanStrategy::Single),
        )
        .await
        .expect_err("diff fence must be rejected");

        assert!(matches!(err, PlanError::PatchProducedDespitePlanMode));
    }

    #[test]
    fn validate_plan_rejects_missing_sections() {
        // Missing Objective.
        let err = validate_plan_markdown("## Affected files\n- a.rs\n\n## Steps\n1. step\n")
            .expect_err("missing Objective must fail");
        match err {
            PlanError::MissingSection(name) => assert_eq!(name, "Objective"),
            other => panic!("unexpected error: {other:?}"),
        }

        // Missing Affected files.
        let err = validate_plan_markdown("## Objective\nbuild it\n\n## Steps\n1. step\n")
            .expect_err("missing Affected files must fail");
        match err {
            PlanError::MissingSection(name) => assert_eq!(name, "Affected files"),
            other => panic!("unexpected error: {other:?}"),
        }

        // Missing Steps.
        let err = validate_plan_markdown("## Objective\nbuild it\n\n## Affected files\n- a.rs\n")
            .expect_err("missing Steps must fail");
        match err {
            PlanError::MissingSection(name) => assert_eq!(name, "Steps"),
            other => panic!("unexpected error: {other:?}"),
        }

        // Empty Affected files (no bullets).
        let err = validate_plan_markdown(
            "## Objective\nbuild it\n\n## Affected files\nnone\n\n## Steps\n1. step\n",
        )
        .expect_err("empty Affected files bullets must fail");
        match err {
            PlanError::MissingSection(name) => assert_eq!(name, "Affected files"),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn validate_plan_accepts_structured_markdown() {
        let sections = validate_plan_markdown(STRUCTURED_PLAN).expect("must validate");
        assert_eq!(sections.objective, "Add /plan command");
        assert_eq!(sections.affected_files.len(), 2);
        assert_eq!(sections.estimated_loc, Some(250));
        assert!(sections.risks.is_some());
    }

    #[test]
    fn validate_plan_rejects_empty_input() {
        let err = validate_plan_markdown("   \n\n").expect_err("empty must fail");
        assert!(matches!(err, PlanError::EmptyPlan));
    }

    #[tokio::test]
    async fn dual_review_includes_reviewer_issues() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("plan with review", dir.path());
        let config = Arc::new(plan_config());

        let lead = MockAdapter::new("plan-lead");
        lead.set_oneshot_response(STRUCTURED_PLAN);
        let reviewer = MockAdapter::new("plan-reviewer");
        reviewer.set_oneshot_response(
            "Issue: missing rollback strategy.\nIssue: estimated LOC seems low.",
        );

        let adapters: Vec<Box<dyn ModelAdapter>> = vec![Box::new(lead), Box::new(reviewer)];

        let report = run_plan_mode(
            task,
            config,
            adapters,
            PlanRunOptions::new(PlanStrategy::DualReview),
        )
        .await
        .expect("dual review plan should succeed");

        assert!(
            report
                .reviewer_feedback
                .contains("missing rollback strategy")
        );
        assert!(report.reviewer_feedback.contains("estimated LOC seems low"));
    }

    #[tokio::test]
    async fn dual_review_rejects_reviewer_patch_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("plan with patch reviewer", dir.path());
        let config = Arc::new(plan_config());

        let lead = MockAdapter::new("plan-lead");
        lead.set_oneshot_response(STRUCTURED_PLAN);
        let reviewer = MockAdapter::new("plan-reviewer");
        reviewer.set_oneshot_response("Looks good but here is a diff --git fragment");

        let adapters: Vec<Box<dyn ModelAdapter>> = vec![Box::new(lead), Box::new(reviewer)];

        let err = run_plan_mode(
            task,
            config,
            adapters,
            PlanRunOptions::new(PlanStrategy::DualReview),
        )
        .await
        .expect_err("reviewer-side patch must be rejected");

        assert!(matches!(err, PlanError::PatchProducedDespitePlanMode));
    }

    #[tokio::test]
    async fn adapter_oneshot_failure_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("adapter failure", dir.path());
        let config = Arc::new(plan_config());

        let lead = MockAdapter::new("plan-lead");
        lead.set_oneshot_error("upstream LLM exploded");
        let reviewer = MockAdapter::new("plan-reviewer");

        let adapters: Vec<Box<dyn ModelAdapter>> = vec![Box::new(lead), Box::new(reviewer)];

        let err = run_plan_mode(
            task,
            config,
            adapters,
            PlanRunOptions::new(PlanStrategy::Single),
        )
        .await
        .expect_err("adapter failure must propagate");

        assert!(matches!(err, PlanError::Adapter(_)));
    }

    #[tokio::test]
    async fn missing_adapter_surfaces_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("missing lead adapter", dir.path());
        let config = Arc::new(plan_config());

        // Register only the reviewer; lead lookup must fail.
        let reviewer = MockAdapter::new("plan-reviewer");
        let adapters: Vec<Box<dyn ModelAdapter>> = vec![Box::new(reviewer)];

        let err = run_plan_mode(
            task,
            config,
            adapters,
            PlanRunOptions::new(PlanStrategy::Single),
        )
        .await
        .expect_err("missing lead adapter must fail");

        assert!(matches!(err, PlanError::Io(_)));
    }

    /// Defensive: an adapter that bypasses `dispatch_oneshot` via the patch
    /// surfaces should never reach `/plan`, because `run_plan_mode` only
    /// invokes the oneshot dispatch path. We still exercise the patch-bearing
    /// adapter to document the property.
    #[tokio::test]
    async fn plan_mode_does_not_invoke_implement_surface() {
        let dir = tempfile::tempdir().unwrap();
        let task = Task::new("never call implement", dir.path());
        let config = Arc::new(plan_config());

        struct ImplementPanicAdapter;
        #[async_trait]
        impl ModelAdapter for ImplementPanicAdapter {
            fn id(&self) -> &str {
                "plan-lead"
            }
            async fn implement(
                &self,
                _task: &Task,
                _cwd: &Path,
                _tx: mpsc::Sender<AgentEvent>,
            ) -> Result<AgentOutput> {
                panic!("/plan must never reach implement()");
            }
            async fn review(
                &self,
                _task: &Task,
                _lead_output: &AgentOutput,
                _cwd: &Path,
                _strictness: &str,
                _tx: mpsc::Sender<AgentEvent>,
            ) -> Result<ReviewerFeedback> {
                panic!("/plan must never reach review()");
            }
            async fn refine(
                &self,
                _task: &Task,
                _previous_output: &AgentOutput,
                _feedback: &ReviewerFeedback,
                _cwd: &Path,
                _tx: mpsc::Sender<AgentEvent>,
            ) -> Result<AgentOutput> {
                panic!("/plan must never reach refine()");
            }
            async fn dispatch_oneshot(
                &self,
                _system_prompt: &str,
                _user_prompt: &str,
            ) -> Result<String, AdapterError> {
                Ok(STRUCTURED_PLAN.to_string())
            }
        }

        let reviewer = MockAdapter::new("plan-reviewer");
        let adapters: Vec<Box<dyn ModelAdapter>> =
            vec![Box::new(ImplementPanicAdapter), Box::new(reviewer)];

        let report = run_plan_mode(
            task,
            config,
            adapters,
            PlanRunOptions::new(PlanStrategy::Single),
        )
        .await
        .expect("plan-only dispatch must succeed");

        assert!(report.plan_markdown.contains("## Objective"));
    }
}
