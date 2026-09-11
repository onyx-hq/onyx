//! "Brief" pipeline — one-shot LLM call for connector-less analytics agents.
//!
//! When an agent's YAML declares no `databases:`, no `context:` glob, no
//! per-state overrides, and provides explicit `instructions:`, the
//! Clarifying → Specifying → Solving → Executing → Interpreting FSM is
//! useless overhead. Such agents are pure text-shaping prompts —
//! "format these numbers into one sentence" — that receive structured
//! data in the prompt itself and need only a single LLM round-trip.
//!
//! [`start_brief_pipeline`] returns the same [`PipelineHandle`] shape as
//! [`crate::pipeline::start_pipeline`] so the automation-step executor
//! doesn't branch on which path produced the answer: events channel,
//! outcomes channel, cancel token, join handle. It just emits one
//! `PipelineOutcome::Done` (or `Failed`/`Cancelled`) and closes.
//!
//! Detection lives in [`is_brief_agent`] and is intentionally
//! conservative — anything that asks for any DB, any context file, or
//! any FSM stage override runs the full pipeline. Loosen later if a
//! valid brief shape ends up routed through analytics by mistake.

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use agentic_core::events::Event;
use agentic_runtime::handle::{PipelineHandle, PipelineOutcome};

use crate::config::{AgentConfig, ConfigError, LlmClientSpec, build_llm_client, resolve_vendor};
use crate::events::AnalyticsEvent;
use crate::llm::LlmClient;
use crate::pipeline::PipelineParams;

/// `true` when an agent opts into the one-shot LLM brief path.
///
/// Conservative — only fires on the unambiguous narrative-wrapping
/// shape: empty `databases`, empty `context`, empty `states`, and a
/// non-empty `instructions`. Anything that declares connector access,
/// context globs, or per-stage overrides keeps the full FSM path.
pub fn is_brief_agent(config: &AgentConfig) -> bool {
    let has_instructions = config
        .instructions
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    config.databases.is_empty()
        && config.context.is_empty()
        && config.states.is_empty()
        && has_instructions
}

/// Run a connector-less analytics agent as a single LLM call.
///
/// Skips solver build, connector resolution, semantic catalog load, and
/// FSM orchestration. The spawned task does exactly one
/// [`LlmClient::complete`] and emits a terminal outcome.
///
/// Returns the same [`PipelineHandle<AnalyticsEvent>`] as the FSM
/// starter so callers don't branch on which path was used. The events
/// channel never receives any [`AnalyticsEvent`] — domain events are
/// FSM-specific — and is closed immediately after spawn.
pub async fn start_brief_pipeline(
    params: PipelineParams,
) -> Result<PipelineHandle<AnalyticsEvent>, ConfigError> {
    let client = brief_llm_client(&params)?;

    // System prompt comes from `instructions:`; user prompt is the
    // automation step's `prompt:` field (already templated by the caller).
    let system = params.config.instructions.clone().unwrap_or_default();
    let user = params.question.clone();

    // Tiny buffers — the brief path emits no domain events and exactly
    // one terminal outcome.
    let (event_tx, event_rx) = mpsc::channel::<Event<AnalyticsEvent>>(1);
    let (outcome_tx, outcome_rx) = mpsc::channel::<PipelineOutcome>(2);
    let cancel = CancellationToken::new();
    let cancel_child = cancel.clone();

    // Parent: None so the brief run gets its own root span (mirrors
    // `start_pipeline`). The "brief" suffix lets observability filters
    // distinguish FSM vs one-shot agent invocations.
    let run_span = tracing::info_span!(
        parent: None,
        "analytics.run.brief",
        oxy.name = "analytics.run.brief",
        oxy.span_type = "analytics",
        oxy.agent.ref = %params.agent_id,
        agent.prompt = %params.question,
        question = %params.question,
    );
    // A root for the product console (`ParentSpanId = ''`), but not an
    // orphan in HyperDX: `follows_from` becomes an OpenTelemetry span link to
    // whatever started the run — the HTTP request, or the `agentic_task` a
    // worker claimed — so the trace view can step from one to the other.
    run_span.follows_from(tracing::Span::current());

    let join = tokio::spawn(
        async move {
            let outcome = tokio::select! {
                r = client.complete(&system, &user) => match r {
                    Ok(text) => PipelineOutcome::Done {
                        answer: text,
                        metadata: None,
                    },
                    Err(e) => PipelineOutcome::Failed(format!("brief LLM call: {e}")),
                },
                _ = cancel_child.cancelled() => PipelineOutcome::Cancelled,
            };
            // Drop the event sender so the receiver closes — no domain
            // events are emitted on the brief path.
            drop(event_tx);
            let _ = outcome_tx.send(outcome).await;
        }
        .instrument(run_span),
    );

    Ok(PipelineHandle {
        events: event_rx,
        outcomes: outcome_rx,
        cancel,
        join,
    })
}

/// Resolve an [`LlmClient`] for the brief path.
///
/// Resolution order (matches the FSM path's solver build):
///   1. If `params.config.llm` has explicit `api_key` + `model`, use them.
///   2. If `params.project_model` is `Some`, use that — the host already
///      resolved vendor / api_key / base_url / azure_* from `config.yml`
///      based on the agent's `llm.ref:`.
///   3. Otherwise error: the caller hasn't given the brief path enough
///      to talk to a model.
fn brief_llm_client(params: &PipelineParams) -> Result<LlmClient, ConfigError> {
    let cfg = &params.config.llm;

    // Vendor resolves independently of which branch supplies the key/model, and
    // mirrors the FSM path: an explicitly-written `llm.vendor` wins over the one
    // inherited from `llm.ref:`. Branch (2) previously always took the ref's
    // vendor, so a `ref` + `vendor:` override silently lost its override here.
    let vendor = resolve_vendor(
        cfg.vendor.as_ref(),
        params.project_model.as_ref().map(|m| &m.vendor),
    );

    // Headers only ever come from the project model — the agent YAML has no
    // `headers:` of its own — so both branches read them from there.
    let headers = params
        .project_model
        .as_ref()
        .and_then(|m| m.headers.as_ref());

    // (1) Explicit per-agent override on the YAML.
    if let (Some(api_key), Some(model)) = (cfg.api_key.as_deref(), cfg.model.as_deref()) {
        return Ok(build_llm_client(LlmClientSpec {
            vendor,
            api_key,
            model,
            base_url: cfg.base_url.as_deref(),
            azure_deployment_id: None,
            azure_api_version: None,
            headers,
        }));
    }

    // (2) Project-resolved model from `config.yml`.
    if let Some(info) = params.project_model.as_ref() {
        return Ok(build_llm_client(LlmClientSpec {
            vendor,
            api_key: info.api_key.as_deref().unwrap_or_default(),
            model: &info.model,
            base_url: info.base_url.as_deref(),
            azure_deployment_id: info.azure_deployment_id.as_deref(),
            azure_api_version: info.azure_api_version.as_deref(),
            headers,
        }));
    }

    // (3) Nothing to build with.
    Err(ConfigError::MissingLlmConfig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AgentConfig;

    fn parse(yaml: &str) -> AgentConfig {
        AgentConfig::from_yaml(yaml).expect("yaml parses")
    }

    #[test]
    fn narrative_wrapper_is_brief() {
        // The motivating shape: no databases, no context, no states,
        // only `instructions` + `llm.ref`.
        let cfg = parse(
            r#"
llm:
  ref: claude-sonnet-4-6
instructions: |
  Format these numbers into one sentence.
"#,
        );
        assert!(is_brief_agent(&cfg));
    }

    #[test]
    fn agent_with_databases_is_not_brief() {
        let cfg = parse(
            r#"
llm:
  ref: claude-sonnet-4-6
databases:
  - gallant
instructions: |
  Format these numbers.
"#,
        );
        assert!(!is_brief_agent(&cfg));
    }

    #[test]
    fn agent_with_context_glob_is_not_brief() {
        let cfg = parse(
            r#"
llm:
  ref: claude-sonnet-4-6
context:
  - "*.view.yml"
instructions: |
  Format these numbers.
"#,
        );
        assert!(!is_brief_agent(&cfg));
    }

    #[test]
    fn agent_without_instructions_is_not_brief() {
        // No `instructions:` — even if everything else is empty, we
        // can't run a brief LLM call without a system prompt.
        let cfg = parse(
            r#"
llm:
  ref: claude-sonnet-4-6
"#,
        );
        assert!(!is_brief_agent(&cfg));
    }

    #[test]
    fn empty_instructions_string_is_not_brief() {
        // `instructions: ""` parses as Some("") — guard against that
        // explicitly so a trivially-blank prompt doesn't silently
        // route to the brief path with no system message.
        let cfg = parse(
            r#"
llm:
  ref: claude-sonnet-4-6
instructions: "   "
"#,
        );
        assert!(!is_brief_agent(&cfg));
    }

    #[test]
    fn agent_with_state_override_is_not_brief() {
        let cfg = parse(
            r#"
llm:
  ref: claude-sonnet-4-6
instructions: |
  Format these numbers.
states:
  Clarifying:
    model: claude-sonnet-4-6
"#,
        );
        assert!(!is_brief_agent(&cfg));
    }
}
