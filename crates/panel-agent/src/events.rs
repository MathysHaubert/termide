//! Runtime events: how each [`AgentEvent`] lands in the transcript, the
//! session log and the live activity indicators.

use std::time::{Duration, Instant};

use termide_agent_core::{AgentEvent, Message, StopReason, StreamEvent, Timing, ToolUpdate};
use termide_core::PanelEvent;
use termide_ui::ChoiceForm;

use crate::{
    changed_file, millis, now_hms, Activity, AgentPanel, Item, NoticeKind, Pending, Phase,
};

impl AgentPanel {
    /// Note streamed output: enter the generating phase on the first token,
    /// then count characters for the live token estimate and speed.
    pub(crate) fn note_generation(&mut self, chars: usize) {
        let activity = self
            .activity
            .get_or_insert_with(|| Activity::new(Phase::Generating));
        activity.first_token.get_or_insert_with(Instant::now);
        if activity.phase != Phase::Generating {
            activity.enter(Phase::Generating);
        }
        activity.gen_chars += chars;
    }

    /// Switch the current activity to `phase` (starting one if idle).
    pub(crate) fn set_phase(&mut self, phase: Phase) {
        match &mut self.activity {
            Some(activity) => activity.enter(phase),
            None => self.activity = Some(Activity::new(phase)),
        }
    }

    /// Apply one runtime event to the transcript and the session log.
    pub(crate) fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::AgentStart => {
                self.busy = true;
                self.paused = false;
                // A resumed run keeps its start, so its clock counts from the
                // request; anything else (a new prompt over a pause) starts
                // afresh.
                self.end_pause();
                if !std::mem::take(&mut self.resuming) || self.run_start.is_none() {
                    self.run_start = Some(Instant::now());
                    self.run_failed = false;
                }
                self.run_paused = false;
            }
            AgentEvent::Paused => {
                // The run's closing line records the pause; the state strip
                // shows it until `/continue`.
                self.paused = true;
                self.run_paused = true;
                self.pause_requested = false;
            }
            AgentEvent::AgentEnd => {
                self.busy = false;
                self.activity = None;
                self.attention = true;
                if self.run_paused {
                    // The run waits at a pause: its line ticks the pause's
                    // length (no time of day), and the run's own clock stays
                    // for `/continue`.
                    self.pause_start = Some(Instant::now());
                    self.transcript.end_run(0, "", !self.run_failed, true);
                } else if let Some(start) = self.run_start.take() {
                    self.transcript.end_run(
                        millis(start.elapsed()),
                        &now_hms(),
                        !self.run_failed,
                        false,
                    );
                }
                self.pause_requested = false;
                self.stop_requested = false;
                self.set_queued(self.runtime.queue_lens());
                if let Some(store) = &self.checkpoints {
                    store.lock().unwrap().end_run();
                }
                if self.prompt_stale {
                    self.sync_system_prompt();
                }
                self.offer_plan();
                // A loop schedules its next iteration once the run ends, unless
                // it was paused (then it waits for `/continue`) or a card is up.
                if !self.paused && self.pending.is_none() {
                    if let Some(task) = self.loop_task.as_mut() {
                        task.next_at =
                            Some(Instant::now() + task.interval.unwrap_or(Duration::ZERO));
                    }
                }
                // A goal judges the finished work turn next, unless it was
                // paused or a card is up; a turn that errored stops the goal
                // rather than looping on the failure.
                if self.goal_task.is_some() && self.goal_errored {
                    self.goal_task = None;
                    self.notice(
                        termide_i18n::t().agent_notice_goal_stopped_failed(),
                        NoticeKind::Warn,
                    );
                } else if !self.paused && self.pending.is_none() {
                    if let Some(task) = self.goal_task.as_mut() {
                        task.judge_at = Some(Instant::now());
                    }
                }
            }
            AgentEvent::TurnStart | AgentEvent::TurnEnd => {}
            AgentEvent::MessageStart => {
                // The reasoning and answer blocks are created lazily on their
                // first delta, so the reasoning lands above the answer and a
                // prefill with neither shows only the spinner.
                self.activity = Some(Activity::new(Phase::Prefill));
            }
            AgentEvent::MessageUpdate(StreamEvent::TextDelta(delta)) => {
                self.note_generation(delta.chars().count());
                self.transcript.stream_answer(&delta);
            }
            AgentEvent::MessageUpdate(StreamEvent::ThinkingDelta(delta)) => {
                self.note_generation(delta.chars().count());
                self.transcript.stream_thinking(&delta);
            }
            AgentEvent::MessageUpdate(StreamEvent::Retry {
                attempt,
                max_attempts,
                delay_ms,
                error,
            }) => self.notice(
                termide_i18n::t().agent_notice_retry_fmt(
                    attempt as usize,
                    max_attempts as usize,
                    delay_ms,
                    &error.to_string(),
                ),
                NoticeKind::Warn,
            ),
            AgentEvent::MessageUpdate(_) => {}
            AgentEvent::MessageEnd(message) => {
                // How long the message took, logged with it so a reopened
                // session shows the same figures.
                let timing = match &message {
                    Message::User(user) => {
                        self.transcript.push(Item::User {
                            text: user.plain_text(),
                            at: now_hms(),
                            command: user.command.clone(),
                        });
                        None
                    }
                    Message::Assistant(assistant) => {
                        if assistant.usage.total() > 0 {
                            self.context_tokens = assistant.usage.total();
                        }
                        // What the cache served is counted apart from what is
                        // billed in full.
                        self.session_input += assistant.usage.uncached();
                        self.session_cached += assistant.usage.cache_read;
                        self.session_output += assistant.usage.output;
                        // A call that failed before its first token (no network,
                        // say) went through no prefill or generation: it has
                        // no cost to show, only its time and failure. Nor has
                        // an external agent's message: its first text arrives
                        // with the message's start and it reports no tokens, so
                        // prefill, generation and speed would all be made up.
                        let cost = self
                            .activity
                            .as_ref()
                            .filter(|_| !self.external)
                            .filter(|a| a.first_token.is_some() || assistant.usage.total() > 0)
                            .map(|a| a.cost(assistant.usage.input, assistant.usage.output));
                        let at = now_hms();
                        let error = assistant.error_message.clone();
                        // A goal work turn that errored must not be judged and
                        // retried on the failure; note it for `AgentEnd`.
                        if error.is_some() && self.goal_task.is_some() {
                            self.goal_errored = true;
                        }
                        if error.is_some()
                            || matches!(
                                assistant.stop_reason,
                                StopReason::Error | StopReason::Aborted
                            )
                        {
                            self.run_failed = true;
                        }
                        // The answer always carries the wall-clock time; a
                        // reasoning block, if any, carries the prefill/generation
                        // indicators (else the answer does). A tool-only turn
                        // (reasoning, no answer text) leaves no answer block.
                        let had_thinking = self.transcript.finish_thinking(&at, cost);
                        let answer_cost = if had_thinking { None } else { cost };
                        self.transcript.finish_assistant(
                            assistant.plain_text(),
                            error,
                            answer_cost,
                            at,
                            had_thinking,
                        );
                        cost.map(|cost| Timing::Turn {
                            prefill_ms: cost.prefill_ms,
                            gen_ms: cost.gen_ms,
                        })
                    }
                    // The call's end came first and timed it.
                    Message::ToolResult(result) => self
                        .transcript
                        .tool_duration(&result.tool_call_id)
                        .map(|duration_ms| Timing::Tool {
                            duration_ms,
                            waited_ms: self.transcript.tool_wait(&result.tool_call_id),
                        }),
                };
                if let Some(session) = &mut self.session {
                    if let Err(error) = session.append_timed_message(&message, timing) {
                        log::warn!("agent session write failed: {error}");
                    }
                }
            }
            AgentEvent::ToolExecutionStart { call } => {
                self.set_phase(Phase::Tool);
                self.tool_starts.insert(call.id.clone(), Instant::now());
                self.transcript.push(Item::Tool {
                    call,
                    result: None,
                    live: None,
                    at: String::new(),
                    duration_ms: None,
                    waited_ms: None,
                    waiting: false,
                });
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                update: ToolUpdate::Output(output),
            } => {
                self.transcript.with_tool(&tool_call_id, |item| {
                    if let Item::Tool { live, .. } = item {
                        *live = Some(output);
                    }
                });
            }
            AgentEvent::ToolExecutionEnd { result } => {
                if let Some(path) = changed_file(&result) {
                    self.pending_events
                        .push(PanelEvent::FileChangedOnDisk(path));
                }
                // Tally how much the output cleaning saved (bash reports the
                // raw and cleaned byte counts in its details).
                if let Some(details) = result.details.as_ref() {
                    if let (Some(raw), Some(clean)) = (
                        details.get("raw_bytes").and_then(|v| v.as_u64()),
                        details.get("cleaned_bytes").and_then(|v| v.as_u64()),
                    ) {
                        self.clean_raw_bytes += raw;
                        self.clean_out_bytes += clean;
                    }
                }
                let id = result.tool_call_id.clone();
                let finished = now_hms();
                // A wait on a permission answer is the call's pause, not its
                // run time.
                self.end_permission_wait();
                // A question's call ends only once it is answered or its run
                // stopped; a card still up then has no one waiting for it.
                if matches!(self.pending, Some(Pending::Question { .. })) {
                    self.pending = None;
                }
                let waited = self.transcript.tool_wait(&id).unwrap_or(0);
                let elapsed = self
                    .tool_starts
                    .remove(&id)
                    .map(|start| millis(start.elapsed()).saturating_sub(waited));
                self.transcript.with_tool(&id, |item| {
                    if let Item::Tool {
                        result: slot,
                        live,
                        at,
                        duration_ms,
                        ..
                    } = item
                    {
                        *slot = Some(result);
                        *live = None;
                        *at = finished;
                        *duration_ms = elapsed;
                    }
                });
            }
            AgentEvent::QueueUpdate {
                steering,
                follow_up,
            } => self.set_queued((steering, follow_up)),
            AgentEvent::CompactionStart { .. } => {
                self.set_phase(Phase::Compact);
                self.notice(
                    termide_i18n::t().agent_notice_compacting(),
                    NoticeKind::Info,
                )
            }
            AgentEvent::Compacted {
                summary,
                kept,
                tokens_before,
            } => {
                self.notice(
                    termide_i18n::t().agent_notice_compacted_fmt(tokens_before, kept),
                    NoticeKind::Info,
                );
                if let Some(session) = &mut self.session {
                    if let Err(error) = session.append_compaction(&summary, tokens_before, kept) {
                        log::warn!("agent session write failed: {error}");
                    }
                }
                // The conversation is re-read anyway: what is refused can leave
                // the context almost for free once the run is between turns.
                if self.toolset_off != self.context_off {
                    self.context_stale = true;
                }
            }
            AgentEvent::CompactionFailed { error } => self.notice(
                termide_i18n::t().agent_notice_compaction_failed_fmt(&error.to_string()),
                NoticeKind::Warn,
            ),
            AgentEvent::GoalJudged { done, reason } => self.on_goal_verdict(done, &reason),
            AgentEvent::GoalJudgeFailed { error } => {
                self.goal_task = None;
                self.notice(
                    termide_i18n::t().agent_notice_goal_check_failed_fmt(&error.to_string()),
                    NoticeKind::Warn,
                );
            }
            AgentEvent::Handoff { brief } => match brief {
                Ok(text) => {
                    // Offer the brief, with what to do with it; the text is kept
                    // on the card until the choice is made.
                    let t = termide_i18n::t();
                    let form = ChoiceForm::new(
                        t.agent_handoff_ready_title(),
                        vec![
                            t.agent_handoff_save().to_string(),
                            t.agent_handoff_new_session().to_string(),
                        ],
                    )
                    .with_detail(text.clone())
                    .with_cancel(t.agent_handoff_dismiss());
                    self.pending = Some(Pending::Handoff { form, brief: text });
                    self.attention = true;
                }
                Err(error) => self.notice(
                    termide_i18n::t().agent_notice_handoff_failed_fmt(&error.to_string()),
                    NoticeKind::Warn,
                ),
            },
        }
    }
}
