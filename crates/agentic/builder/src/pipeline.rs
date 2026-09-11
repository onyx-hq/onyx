//! Builder pipeline facade.
//!
//! Mirrors the analytics `start_pipeline()` pattern: builds the solver and
//! orchestrator, spawns the pipeline task, and returns a generic
//! [`PipelineHandle`] that the runtime can drive.

use std::path::PathBuf;
use std::sync::Arc;

use chrono_tz::Tz;

use agentic_core::events::{CoreEvent, Event, EventStream};
use agentic_core::human_input::SuspendedRunData;
use agentic_core::orchestrator::{Orchestrator, OrchestratorError};
use agentic_llm::LlmClient;
use agentic_runtime::handle::{PipelineHandle, PipelineOutcome};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::app_runner::BuilderAppRunner;
use crate::database::BuilderDatabaseProvider;
use crate::events::BuilderEvent;
use crate::prompts::KnowledgeCard;
use crate::schema_provider::BuilderSchemaProvider;
use crate::secrets::BuilderSecretsProvider;
use crate::semantic::BuilderSemanticCompiler;
use crate::solver::{BuilderSolver, build_builder_handlers};
use crate::test_runner::BuilderTestRunner;
use crate::types::BuilderIntent;
use crate::validator::BuilderProjectValidator;

/// Parameters for starting a builder pipeline.
pub struct BuilderPipelineParams {
    pub client: LlmClient,
    pub project_root: PathBuf,
    pub question: String,
    pub history: Vec<crate::types::ConversationTurn>,
    pub db_provider: Option<Arc<dyn BuilderDatabaseProvider>>,
    pub project_validator: Option<Arc<dyn BuilderProjectValidator>>,
    pub schema_provider: Option<Arc<dyn BuilderSchemaProvider>>,
    pub semantic_compiler: Option<Arc<dyn BuilderSemanticCompiler>>,
    pub test_runner: Option<Arc<dyn BuilderTestRunner>>,
    /// Optional runner for executing `*.app.yml` files end-to-end. Backs the
    /// `run_app` tool used to verify apps work at runtime.
    pub app_runner: Option<Arc<dyn BuilderAppRunner>>,
    /// Override the default [`DeferredInputProvider`] for human-in-the-loop
    /// tools (`file_change`, `ask_user`). When set to
    /// [`AutoAcceptInputProvider`], changes are applied without suspension.
    pub human_input: Option<agentic_core::human_input::HumanInputHandle>,
    /// Provides a [`SecretsManager`] for dbt tools that need to resolve
    /// warehouse credentials (`run_dbt_models`, `test_dbt_models`).
    pub secrets_provider: Option<Arc<dyn BuilderSecretsProvider>>,
    /// Reference cards to pre-populate into the cached system prefix.
    /// Empty (the default) keeps the prefix at index + rules only and
    /// relies on the `lookup_reference` tool for depth.  Onboarding
    /// phases pass cards relevant to the artifact being authored.
    pub knowledge_cards: Vec<KnowledgeCard>,
    /// Skip the Interpreting LLM call after Solving completes.  Set
    /// `true` for onboarding (the UI collapses the per-phase trace and
    /// surfaces CTAs); leave `false` for the chat builder which shows
    /// the synthesized summary to the user.
    pub skip_interpreting: bool,
    /// When `Some`, restrict the tool list exposed to the LLM during
    /// solving to the named tools.  `None` exposes the full tool set.
    /// Onboarding sets a per-phase allowlist so dbt / search_text /
    /// run_tests etc. aren't surfaced to phases that don't need them.
    pub tool_allowlist: Option<Vec<String>>,
    /// Timezone for the "Today is …" date hint, sourced from the project
    /// `config.yml` `timezone:` via `ProjectContext::timezone`. `None` → UTC.
    pub timezone: Option<Tz>,
}

/// Build the solver, create the orchestrator, and start the builder pipeline.
///
/// Returns a [`PipelineHandle`] immediately. The pipeline runs in a spawned
/// task. The caller reads events and outcomes from the handle's channels.
pub fn start_pipeline(params: BuilderPipelineParams) -> PipelineHandle<BuilderEvent> {
    let run_span = tracing::info_span!(
        parent: None,
        "builder.run",
        oxy.name = "builder.run",
        oxy.span_type = "builder",
        question = %params.question,
    );
    // A root for the product console (`ParentSpanId = ''`), but not an
    // orphan in HyperDX: `follows_from` becomes an OpenTelemetry span link to
    // whatever started the run — the HTTP request, or the `agentic_task` a
    // worker claimed — so the trace view can step from one to the other.
    run_span.follows_from(tracing::Span::current());
    let (event_tx, event_rx) = mpsc::channel::<Event<BuilderEvent>>(256);
    let event_stream: EventStream<BuilderEvent> = event_tx;

    let cancel_event_tx = event_stream.clone();

    let project_root = params.project_root;
    let mut solver = BuilderSolver::new(params.client, project_root.clone())
        .with_events(event_stream.clone())
        .with_knowledge_cards(params.knowledge_cards)
        .with_timezone(params.timezone)
        .with_skip_interpreting(params.skip_interpreting);
    if let Some(allowlist) = params.tool_allowlist {
        solver = solver.with_tool_allowlist(allowlist);
    }
    if let Some(provider) = params.db_provider {
        solver = solver.with_db_provider(provider);
    }
    if let Some(validator) = params.project_validator {
        solver = solver.with_project_validator(validator);
    }
    if let Some(provider) = params.schema_provider {
        solver = solver.with_schema_provider(provider);
    }
    if let Some(compiler) = params.semantic_compiler {
        solver = solver.with_semantic_compiler(compiler);
    }
    if let Some(runner) = params.test_runner {
        solver = solver.with_test_runner(runner);
    }
    if let Some(runner) = params.app_runner {
        solver = solver.with_app_runner(runner);
    }
    if let Some(provider) = params.human_input {
        solver = solver.with_human_input(provider);
    }
    if let Some(provider) = params.secrets_provider {
        solver = solver.with_secrets_provider(provider);
    }

    let (outcome_tx, outcome_rx) = mpsc::channel::<PipelineOutcome>(4);
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();

    let mut orchestrator = Orchestrator::new(solver)
        .with_handlers(build_builder_handlers())
        .with_events(event_stream);

    let initial_intent = BuilderIntent {
        question: params.question,
        history: params.history,
    };

    let join = tokio::spawn(
        async move {
            let result = tokio::select! {
                r = orchestrator.run(initial_intent) => Some(r),
                _ = cancel_child.cancelled() => None,
            };

            let outcome = match result {
                Some(Ok(answer)) => PipelineOutcome::Done {
                    answer: answer.text,
                    metadata: None,
                },
                Some(Err(OrchestratorError::Suspended {
                    reason,
                    resume_data,
                    trace_id,
                    ..
                })) => PipelineOutcome::Suspended {
                    reason,
                    resume_data,
                    trace_id,
                },
                Some(Err(OrchestratorError::Fatal(e))) => {
                    PipelineOutcome::Failed(format!("fatal: {e:?}"))
                }
                Some(Err(OrchestratorError::MaxIterationsExceeded)) => {
                    PipelineOutcome::Failed("max iterations exceeded".into())
                }
                Some(Err(OrchestratorError::ResumeNotSupported)) => {
                    PipelineOutcome::Failed("resume not supported".into())
                }
                // Skip emitting an Error event on cancel — the
                // `PipelineOutcome::Cancelled` already drives state, and a DB
                // event write during tear-down could race on tracing span
                // refcounts.
                None => PipelineOutcome::Cancelled,
            };

            drop(orchestrator);
            drop(cancel_event_tx);
            let _ = outcome_tx.send(outcome).await;
        }
        .instrument(run_span),
    );

    PipelineHandle {
        events: event_rx,
        outcomes: outcome_rx,
        cancel,
        join,
    }
}

// ── resume_pipeline ─────────────────────────────────────────────────────────

/// Rebuild the solver + orchestrator and resume a previously suspended builder run.
pub fn resume_pipeline(
    params: BuilderPipelineParams,
    resume_data: SuspendedRunData,
    answer: String,
) -> PipelineHandle<BuilderEvent> {
    let run_span = tracing::info_span!(
        parent: None,
        "builder.run",
        oxy.name = "builder.run",
        oxy.span_type = "builder",
        question = %params.question,
        resumed = true,
    );
    // A root for the product console (`ParentSpanId = ''`), but not an
    // orphan in HyperDX: `follows_from` becomes an OpenTelemetry span link to
    // whatever started the run — the HTTP request, or the `agentic_task` a
    // worker claimed — so the trace view can step from one to the other.
    run_span.follows_from(tracing::Span::current());
    let (event_tx, event_rx) = mpsc::channel::<Event<BuilderEvent>>(256);
    let event_stream: EventStream<BuilderEvent> = event_tx;

    let cancel_event_tx = event_stream.clone();
    let resume_event_tx = event_stream.clone();

    let project_root = params.project_root;
    let mut solver = BuilderSolver::new(params.client, project_root.clone())
        .with_events(event_stream.clone())
        .with_knowledge_cards(params.knowledge_cards)
        .with_timezone(params.timezone)
        .with_skip_interpreting(params.skip_interpreting);
    if let Some(allowlist) = params.tool_allowlist {
        solver = solver.with_tool_allowlist(allowlist);
    }
    if let Some(provider) = params.db_provider {
        solver = solver.with_db_provider(provider);
    }
    if let Some(validator) = params.project_validator {
        solver = solver.with_project_validator(validator);
    }
    if let Some(provider) = params.schema_provider {
        solver = solver.with_schema_provider(provider);
    }
    if let Some(compiler) = params.semantic_compiler {
        solver = solver.with_semantic_compiler(compiler);
    }
    if let Some(runner) = params.test_runner {
        solver = solver.with_test_runner(runner);
    }
    if let Some(runner) = params.app_runner {
        solver = solver.with_app_runner(runner);
    }
    if let Some(provider) = params.human_input {
        solver = solver.with_human_input(provider);
    }
    if let Some(provider) = params.secrets_provider {
        solver = solver.with_secrets_provider(provider);
    }

    let (outcome_tx, outcome_rx) = mpsc::channel::<PipelineOutcome>(4);
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();

    let mut orchestrator = Orchestrator::new(solver)
        .with_handlers(build_builder_handlers())
        .with_events(event_stream);

    let join = tokio::spawn(
        async move {
            // Emit synthetic ToolResult for file_change so the LLM sees the
            // user's accept/reject decision when the orchestrator resumes.
            if let Some((tool_name, output)) =
                resumed_builder_tool_result(&resume_data.question, &answer, &project_root)
            {
                let _ = resume_event_tx
                    .send(Event::Core(CoreEvent::ToolResult {
                        name: tool_name,
                        output,
                        duration_ms: 0,
                        is_error: false,
                        sub_spec_index: None,
                    }))
                    .await;
            }

            let result = tokio::select! {
                r = orchestrator.resume(resume_data, answer) => Some(r),
                _ = cancel_child.cancelled() => None,
            };

            let outcome = match result {
                Some(Ok(answer)) => PipelineOutcome::Done {
                    answer: answer.text,
                    metadata: None,
                },
                Some(Err(OrchestratorError::Suspended {
                    reason,
                    resume_data,
                    trace_id,
                    ..
                })) => PipelineOutcome::Suspended {
                    reason,
                    resume_data,
                    trace_id,
                },
                Some(Err(OrchestratorError::Fatal(e))) => {
                    PipelineOutcome::Failed(format!("fatal: {e:?}"))
                }
                Some(Err(OrchestratorError::MaxIterationsExceeded)) => {
                    PipelineOutcome::Failed("max iterations exceeded".into())
                }
                Some(Err(OrchestratorError::ResumeNotSupported)) => {
                    PipelineOutcome::Failed("resume not supported".into())
                }
                // See `start_pipeline`: no extra Error event on cancel.
                None => PipelineOutcome::Cancelled,
            };

            drop(orchestrator);
            drop(cancel_event_tx);
            drop(resume_event_tx);
            let _ = outcome_tx.send(outcome).await;
        }
        .instrument(run_span),
    );

    PipelineHandle {
        events: event_rx,
        outcomes: outcome_rx,
        cancel,
        join,
    }
}

// ── Resume helpers ──────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct FileChangeSuspension {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    file_path: String,
}

/// Returns `true` when `file_path` is relative and cannot escape `base_dir`
/// via path-traversal components. No I/O is performed.
fn is_within_project(base_dir: &std::path::Path, file_path: &str) -> bool {
    let p = std::path::Path::new(file_path);
    if p.is_absolute() {
        return false;
    }
    let mut normalized = std::path::PathBuf::new();
    for component in base_dir.join(p).components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            c => normalized.push(c),
        }
    }
    normalized.starts_with(base_dir)
}

/// Generate a synthetic tool result for a `file_change` resumption so
/// the LLM sees the user's accept/reject decision.
fn resumed_builder_tool_result(
    question: &str,
    answer: &str,
    base_dir: &std::path::Path,
) -> Option<(String, String)> {
    let suspension: FileChangeSuspension = serde_json::from_str(question).ok()?;
    if suspension.kind != "file_change" {
        return None;
    }

    let file_path = suspension.file_path.trim();
    if !file_path.is_empty() && !is_within_project(base_dir, file_path) {
        tracing::warn!(
            file_path,
            "rejected file_change with path outside project root"
        );
        return None;
    }

    let answer_lower = answer.to_lowercase();
    let output = if answer_lower.contains("accept") {
        if file_path.is_empty() {
            "The user accepted the proposed change. The file has been updated.".to_string()
        } else {
            format!(
                "The user accepted the proposed change to '{file_path}'. The file has been updated."
            )
        }
    } else {
        "The user rejected the proposed change. Please reconsider or propose an alternative approach."
            .to_string()
    };

    Some(("file_change".to_string(), output))
}
