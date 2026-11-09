//! Reject malformed plaintext function calls before dispatch and quarantine their
//! wire representation without rewriting the recorded diagnostic history.

use crate::context::ContextualUserFragment;
use crate::context::ToolCallFormatError;
use crate::session::step_context::StepContext;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::ResponseItem;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::Weak;

const MAX_CORRECTION_REQUESTS: usize = 2;

pub(crate) type FormatRecoveryBudget = RecoveryBudget<StepContext>;

pub(crate) struct RecoveryBudget<T>(Mutex<(usize, Option<Weak<T>>)>);

impl<T> Default for RecoveryBudget<T> {
    fn default() -> Self {
        Self(Mutex::new((0, None)))
    }
}

impl<T> RecoveryBudget<T> {
    pub(crate) fn request_correction(&self, step: &Arc<T>) -> bool {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let step = Arc::downgrade(step);
        // Several bad calls in one response consume only one correction attempt.
        // A Weak keeps the identity stable without retaining a turn/step cycle.
        if !state
            .1
            .as_ref()
            .is_some_and(|previous| previous.ptr_eq(&step))
        {
            state.0 = state.0.saturating_add(1);
            state.1 = Some(step);
        }
        state.0 <= MAX_CORRECTION_REQUESTS
    }
}

pub(crate) fn malformed_call_id(item: &ResponseItem) -> Option<&str> {
    let ResponseItem::FunctionCall {
        arguments,
        call_id,
        encrypted_function_args,
        ..
    } = item
    else {
        return None;
    };
    // Encrypted agent-message arguments are validated by their owning runtime.
    if encrypted_function_args
        .as_ref()
        .is_some_and(|args| !args.is_empty())
    {
        return None;
    }
    // Parse a complete object, including string escapes and numeric values, before dispatch.
    if arguments.trim_start().starts_with('{')
        && serde_json::from_str::<serde_json::Value>(arguments).is_ok()
    {
        return None;
    }
    Some(call_id)
}

pub(crate) fn quarantine_malformed_calls(items: &mut Vec<ResponseItemEnvelope>) {
    let rejected: HashSet<String> = items
        .iter()
        .filter_map(|envelope| malformed_call_id(&envelope.item).map(str::to_owned))
        .collect();
    if rejected.is_empty() {
        return;
    }
    items.retain_mut(|envelope| {
        if matches!(&envelope.item, ResponseItem::FunctionCallOutput { call_id: Some(id), .. }
            if rejected.contains(id))
        {
            return false;
        }
        if malformed_call_id(&envelope.item).is_some() {
            // Only the outgoing snapshot changes. The rollout retains the original
            // call, including its id and arguments, for diagnostics and replay.
            *envelope =
                ResponseItemEnvelope::new(ContextualUserFragment::into(ToolCallFormatError));
        }
        true
    });
}

#[cfg(test)]
#[path = "malformed_calls_tests.rs"]
mod tests;
