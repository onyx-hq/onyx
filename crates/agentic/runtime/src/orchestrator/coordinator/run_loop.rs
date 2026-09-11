//! Main select-loop and suspend-timeout enforcement.

use agentic_core::transport::WorkerMessage;

use crate::crud;
use crate::lifecycle::state::RunStatus;

use super::{ChildResult, Coordinator, LoopAction, TaskStatus};

impl Coordinator {
    /// Run the coordinator's main loop.
    ///
    /// Processes messages from the transport until all tasks are terminal
    /// or the transport is closed.
    ///
    /// The whole loop is one span, and that is deliberate: `coordinator` is
    /// the single loudest error target in prod (729 `handle_failed` lines in
    /// six hours,
    /// `internal-docs/2026-09-09-hyperdx-observability-findings.md` §3/§9) and
    /// every one of them was emitted outside any span, so it arrived in
    /// HyperDX with no trace id and nothing to group it by. Wrapping `run`
    /// gives each of those lines the run it belongs to and a duration for the
    /// run itself — the coordinator's lifetime *is* the agentic run, so this
    /// is the trace an operator wants when asked "what happened to that run".
    #[tracing::instrument(
        target = "coordinator",
        name = "agentic_run",
        skip_all,
        fields(
            run_id = tracing::field::Empty,
            attempt = self.attempt,
            task_count = self.tasks.len(),
        )
    )]
    pub async fn run(&mut self) {
        // The coordinator is constructed with its root task already seeded, so
        // the run id is known here; an empty map means a test driving the loop
        // with nothing in it, where `Empty` is the honest value.
        if let Some(run_id) = self.tasks.values().next().map(|t| t.run_id.clone()) {
            tracing::Span::current().record("run_id", run_id.as_str());
        }
        tracing::debug!(target: "coordinator", task_count = self.tasks.len(), "run loop started");
        loop {
            let has_active = self.tasks.values().any(|t| {
                matches!(
                    t.status,
                    TaskStatus::Running
                        | TaskStatus::SuspendedHuman
                        | TaskStatus::WaitingOnChildren { .. }
                )
            });
            if !has_active && !self.tasks.is_empty() {
                tracing::debug!(target: "coordinator", "all tasks terminal, shutting down");
                break;
            }

            // Check for timed-out suspended tasks before waiting for messages.
            if self.check_suspend_timeouts().await {
                // Some tasks were timed out — re-check active status at top of loop.
                continue;
            }

            // Poll all suspended tasks' answer channels (non-blocking) before
            // entering the blocking transport recv. This ensures we don't
            // starve answers for tasks beyond the first suspended one.
            if let Some((task_id, answer)) = self.try_recv_any_answer() {
                self.handle_human_answer(&task_id, &answer).await;
                continue;
            }

            // Now block on transport, with a concurrent poll on the first
            // suspended task's answer channel (if any) so we stay responsive.
            let action = self.recv_next_action().await;

            match action {
                LoopAction::WorkerEvent {
                    task_id,
                    event_type,
                    payload,
                } => {
                    self.handle_event(&task_id, &event_type, payload).await;
                }
                LoopAction::TaskDeferred {
                    task_id,
                    delay_secs,
                    reason,
                } => {
                    // The task was handed back unrun. On the durable transport
                    // the send side already returned it to the queue with a
                    // delayed `available_at`; there is nothing to record and no
                    // outcome to accumulate — the coordinator's view is that
                    // this task simply has not run yet.
                    tracing::info!(
                        target: "coordinator",
                        task_id = %task_id, delay_secs, %reason,
                        "task deferred; returned to the queue unrun"
                    );
                }
                LoopAction::WorkerOutcome { task_id, outcome } => {
                    self.handle_outcome(&task_id, outcome).await;
                }
                LoopAction::HumanAnswer { task_id, answer } => {
                    self.handle_human_answer(&task_id, &answer).await;
                }
                LoopAction::TransportClosed => {
                    tracing::info!(target: "coordinator", "transport closed");
                    break;
                }
                LoopAction::SuspendTimeout => {
                    // A suspend timeout expired — re-check at top of loop.
                    continue;
                }
            }
        }

        // Drain remaining events from the transport so late-arriving events
        // (e.g. the `done` CoreEvent) get persisted before we exit.
        loop {
            match tokio::time::timeout(self.drain_timeout, self.transport.recv()).await {
                Ok(Some(WorkerMessage::Event {
                    task_id,
                    event_type,
                    payload,
                })) => {
                    self.handle_event(&task_id, &event_type, payload).await;
                }
                Ok(Some(WorkerMessage::Outcome { .. })) => {
                    // Ignore late outcomes — we're shutting down.
                }
                _ => break, // Timeout or channel closed.
            }
        }

        // Notify SSE subscribers one final time so they can read
        // any events that were just flushed.
        for node in self.tasks.values() {
            self.state.notify(&node.run_id);
        }
    }

    // ── Main loop helpers ───────────────────────────────────────────────

    /// Non-blocking poll of ALL suspended tasks' answer channels.
    /// Returns the first answer found, if any.
    ///
    /// Both `SuspendedHuman` and `WaitingOnChildren` accept human input:
    /// - `SuspendedHuman`: the task explicitly asked for input and resumes with it.
    /// - `WaitingOnChildren`: a user-initiated override. `handle_human_answer`
    ///   cancels the in-flight children and redirects the parent. Without this
    ///   case, users could not interrupt a delegating task.
    pub(super) fn try_recv_any_answer(&mut self) -> Option<(String, String)> {
        let suspended: Vec<(String, String)> = self
            .tasks
            .iter()
            .filter(|(_, n)| {
                matches!(
                    n.status,
                    TaskStatus::SuspendedHuman | TaskStatus::WaitingOnChildren { .. }
                )
            })
            .map(|(id, n)| (id.clone(), n.run_id.clone()))
            .collect();

        for (task_id, run_id) in &suspended {
            if let Some(rx) = self.answer_rxs.get_mut(run_id)
                && let Ok(answer) = rx.try_recv()
            {
                return Some((task_id.clone(), answer));
            }
        }
        None
    }

    /// Block on the transport, optionally racing with one suspended task's
    /// answer channel for responsiveness.
    pub(super) async fn recv_next_action(&mut self) -> LoopAction {
        // Compute the soonest delegation timeout deadline so we can wake up for it.
        // Only delegating tasks time out — awaiting_input tasks never time out.
        let soonest_deadline = self
            .tasks
            .values()
            .filter(|n| matches!(n.status, TaskStatus::WaitingOnChildren { .. }))
            .filter_map(|n| n.suspended_at.map(|at| at + self.suspend_timeout))
            .min();

        // Find the first suspended task that has an answer channel.
        let suspended = self
            .tasks
            .iter()
            .find(|(_, n)| {
                matches!(
                    n.status,
                    TaskStatus::SuspendedHuman | TaskStatus::WaitingOnChildren { .. }
                )
            })
            .map(|(id, n)| (id.clone(), n.run_id.clone()));

        let Some((task_id, run_id)) = suspended else {
            // No suspended tasks — just wait on transport.
            return match self.transport.recv().await {
                Some(WorkerMessage::Event {
                    task_id,
                    event_type,
                    payload,
                }) => LoopAction::WorkerEvent {
                    task_id,
                    event_type,
                    payload,
                },
                Some(WorkerMessage::Outcome { task_id, outcome }) => {
                    LoopAction::WorkerOutcome { task_id, outcome }
                }
                Some(WorkerMessage::Defer {
                    task_id,
                    delay_secs,
                    reason,
                    ..
                }) => LoopAction::TaskDeferred {
                    task_id,
                    delay_secs,
                    reason,
                },
                None => LoopAction::TransportClosed,
            };
        };

        // Compute a sleep future that expires at the soonest suspend deadline.
        // If no deadline, use a very far future (effectively no timeout).
        let timeout_sleep = async {
            match soonest_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(timeout_sleep);

        // Take the answer channel out to avoid &mut self borrow in select!.
        let Some(mut answer_rx) = self.answer_rxs.remove(&run_id) else {
            // No answer channel for this run — wait on transport or timeout.
            return tokio::select! {
                biased;
                _ = &mut timeout_sleep => {
                    LoopAction::SuspendTimeout
                }
                msg = self.transport.recv() => {
                    match msg {
                        Some(WorkerMessage::Event { task_id, event_type, payload }) =>
                            LoopAction::WorkerEvent { task_id, event_type, payload },
                        Some(WorkerMessage::Outcome { task_id, outcome }) =>
                            LoopAction::WorkerOutcome { task_id, outcome },
                        Some(WorkerMessage::Defer { task_id, delay_secs, reason, .. }) =>
                            LoopAction::TaskDeferred { task_id, delay_secs, reason },
                        None => LoopAction::TransportClosed,
                    }
                }
            };
        };

        let action = tokio::select! {
            biased;
            answer = answer_rx.recv() => {
                match answer {
                    Some(a) => LoopAction::HumanAnswer { task_id, answer: a },
                    // Answer channel closed (client disconnected) — NOT a
                    // transport close. Just continue the loop; the task
                    // stays suspended and will eventually time out.
                    None => {
                        tracing::warn!(
                            target: "coordinator",
                            run_id = %run_id,
                            "answer channel closed (client disconnected), task remains suspended"
                        );
                        // Don't put the closed channel back.
                        return tokio::select! {
                            biased;
                            _ = &mut timeout_sleep => LoopAction::SuspendTimeout,
                            msg = self.transport.recv() => {
                                match msg {
                                    Some(WorkerMessage::Event { task_id, event_type, payload }) =>
                                        LoopAction::WorkerEvent { task_id, event_type, payload },
                                    Some(WorkerMessage::Outcome { task_id, outcome }) =>
                                        LoopAction::WorkerOutcome { task_id, outcome },
                                    Some(WorkerMessage::Defer { task_id, delay_secs, reason, .. }) =>
                                        LoopAction::TaskDeferred { task_id, delay_secs, reason },
                                    None => LoopAction::TransportClosed,
                                }
                            }
                        };
                    }
                }
            }
            _ = &mut timeout_sleep => {
                // Timeout fired — put answer channel back and return to main loop.
                self.answer_rxs.insert(run_id, answer_rx);
                return LoopAction::SuspendTimeout;
            }
            msg = self.transport.recv() => {
                // Put the answer channel back before processing.
                self.answer_rxs.insert(run_id, answer_rx);
                return match msg {
                    Some(WorkerMessage::Event { task_id, event_type, payload }) =>
                        LoopAction::WorkerEvent { task_id, event_type, payload },
                    Some(WorkerMessage::Outcome { task_id, outcome }) =>
                        LoopAction::WorkerOutcome { task_id, outcome },
                    Some(WorkerMessage::Defer { task_id, delay_secs, reason, .. }) =>
                        LoopAction::TaskDeferred { task_id, delay_secs, reason },
                    None => LoopAction::TransportClosed,
                };
            }
        };

        // Answer was received — put the channel back for future answers.
        self.answer_rxs.insert(run_id, answer_rx);
        action
    }

    /// Check delegating tasks for timeout. Returns `true` if any were timed out.
    ///
    /// Only `WaitingOnChildren` (delegating) tasks time out — `SuspendedHuman`
    /// (awaiting_input) tasks never time out because they consume no resources
    /// and users may answer hours/days later.
    pub(super) async fn check_suspend_timeouts(&mut self) -> bool {
        let now = tokio::time::Instant::now();
        let timed_out: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, n)| {
                matches!(n.status, TaskStatus::WaitingOnChildren { .. })
                    && n.suspended_at
                        .is_some_and(|at| now.duration_since(at) > self.suspend_timeout)
            })
            .map(|(id, _)| id.clone())
            .collect();

        if timed_out.is_empty() {
            return false;
        }

        for task_id in timed_out {
            let status_desc = self
                .tasks
                .get(&task_id)
                .map(|n| match &n.status {
                    TaskStatus::WaitingOnChildren { child_task_ids, .. } => {
                        format!("delegation:{}", child_task_ids.join(","))
                    }
                    _ => "unknown".to_string(),
                })
                .unwrap_or_default();

            tracing::error!(
                target: "coordinator",
                task_id = %task_id,
                status = %status_desc,
                timeout_secs = self.suspend_timeout.as_secs(),
                "delegation timed out"
            );

            // Mark as timed_out — distinct from failed (retriable, not a logic error).
            let node = self.tasks.get_mut(&task_id);
            if let Some(node) = node {
                node.status = TaskStatus::Failed;
                node.suspended_at = None;
                let run_id = node.run_id.clone();
                let parent_id = node.parent_task_id.clone();
                let msg = format!(
                    "Timed out after {}s waiting for {status_desc}",
                    self.suspend_timeout.as_secs()
                );

                if let Some(parent_id) = parent_id {
                    // Atomically mark timed-out child + record outcome.
                    crud::complete_child_failed_txn(
                        &self.db,
                        &run_id,
                        &task_id,
                        &parent_id,
                        "timed_out",
                        &msg,
                    )
                    .await
                    .ok();
                    self.emit_delegation_completed(&parent_id, &task_id, false, None, Some(&msg))
                        .await;
                    self.record_child_result(&parent_id, &task_id, ChildResult::Failed(msg))
                        .await;
                } else {
                    // Root task timed out — single write.
                    crud::transition_run(&self.db, &run_id, "timed_out", None, None, Some(&msg))
                        .await
                        .ok();
                    self.state
                        .statuses
                        .insert(run_id.clone(), RunStatus::Failed(msg));
                    self.state.notify(&run_id);
                }
            }
        }
        true
    }
}
