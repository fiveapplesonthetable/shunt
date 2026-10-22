//! High-level Input event dispatching.

use crate::event::Event;
use crate::input::Input;

use super::Recorder;

impl Recorder {
    pub(crate) fn dispatch_input(&mut self, input: Input, ts: u64, out: &mut Vec<Event>) {
        match input {
            Input::RunStart {
                provider,
                session_id,
                agent,
                task,
                pid,
                cwd,
                model,
                parent_session,
                git_tree,
                role,
                ..
            } => {
                self.handle_run_start(
                    provider,
                    session_id,
                    agent,
                    task,
                    pid,
                    cwd,
                    model,
                    parent_session,
                    git_tree,
                    role,
                    ts,
                    out,
                );
            }
            Input::RunEnd {
                session,
                exit_code,
                reason,
                ..
            } => self.handle_run_end(session, exit_code, reason, ts, out),
            Input::Prompt {
                session,
                text_len,
                digest,
                ..
            } => self.handle_prompt(session, text_len, digest, ts, out),
            Input::TurnStart { session, model, .. } => {
                self.handle_turn_start(session, model, ts, out)
            }
            Input::TurnEnd {
                session,
                input_tokens,
                output_tokens,
                cached_tokens,
                text_bytes,
                cost_micros,
                ..
            } => self.handle_turn_end(
                session,
                input_tokens,
                output_tokens,
                cached_tokens,
                text_bytes,
                cost_micros,
                ts,
                out,
            ),
            Input::ToolBegin {
                session,
                tool_use_id,
                tool_name,
                input: tool_in,
                ..
            } => self.handle_tool_begin(session, tool_use_id, tool_name, tool_in, ts, out),
            Input::ToolEnd {
                session,
                tool_use_id,
                error,
                result_bytes,
                exit_code,
                ..
            } => self.handle_tool_end(
                session,
                tool_use_id,
                error,
                result_bytes,
                exit_code,
                ts,
                out,
            ),
            Input::Wait {
                session,
                target,
                on,
                ..
            } => self.handle_wait(session, target, on, ts, out),
            Input::WaitEnd {
                session, timed_out, ..
            } => self.handle_wait_end(session, timed_out, ts, out),
            Input::Message {
                from,
                to,
                id,
                bytes,
                dir,
                ..
            } => self.handle_message(from, to, id, bytes, dir, ts, out),
            Input::Compacted {
                session,
                tokens_before,
                tokens_after,
                bytes_compacted,
                ..
            } => self.handle_compacted(
                session,
                tokens_before,
                tokens_after,
                bytes_compacted,
                ts,
                out,
            ),
            Input::ProviderError {
                provider,
                error_code,
                message,
                rate_limit,
                retry_after_ms,
                ..
            } => self.handle_provider_error(
                provider,
                error_code,
                message,
                rate_limit,
                retry_after_ms,
                ts,
                out,
            ),
            Input::Task {
                slug,
                brief_digest,
                brief_bytes,
                paths,
                agent,
                base_commit,
                token_budget,
                cost_budget_micros,
                ..
            } => self.handle_task(
                slug,
                brief_digest,
                brief_bytes,
                paths,
                agent,
                base_commit,
                token_budget,
                cost_budget_micros,
                ts,
                out,
            ),
            Input::TaskState {
                slug,
                state,
                because,
                ..
            } => self.handle_task_state(slug, state, because, ts, out),
            Input::Finding {
                task,
                severity,
                path,
                line,
                title,
                ..
            } => self.handle_finding(task, severity, path, line, title, ts, out),
            Input::Custom {
                track,
                op,
                name,
                id,
                args,
                value,
                flow_out,
                flow_in,
                ..
            } => self.handle_custom(track, op, name, id, args, value, flow_out, flow_in, ts, out),
            Input::CounterSample {
                track_key,
                value,
                unit,
                ..
            } => self.handle_counter_sample(track_key, value, unit, ts, out),
        }
    }
}
