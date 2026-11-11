//! Reconcile client instructions when restored history already established startup context.

use super::Session;
use super::TurnContext;
use crate::context::ContextualUserFragment;
use crate::context::DeveloperInstructions;
use crate::context_manager::updates::build_rendered_message;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

impl Session {
    pub(super) async fn record_client_developer_instructions_update(&self, turn: &TurnContext) {
        // Subagents and Guardians own their instruction inheritance. A missing reference
        // context is handled by the normal first-turn startup block instead.
        if turn.session_source.is_non_root_agent() || self.reference_context_item().await.is_none()
        {
            return;
        }
        let update = {
            let state = self.state.lock().await;
            fork_developer_instructions_update(
                state.history.raw_items(),
                turn.developer_instructions.as_deref(),
            )
        };
        if let Some(update) = update {
            self.record_conversation_items(turn, turn.model_info(), &[update])
                .await;
        }
    }
}

pub(super) fn fork_developer_instructions_update<'a>(
    history: impl DoubleEndedIterator<Item = &'a ResponseItem>,
    instructions: Option<&str>,
) -> Option<ResponseItem> {
    let instructions = instructions.filter(|instructions| !instructions.is_empty())?;
    let kind = DeveloperInstructions::new(instructions).content_kind();
    let previous = history.rev().find_map(|item| {
        let ResponseItem::Message {
            role,
            content,
            internal_chat_message_metadata_passthrough,
            ..
        } = item
        else {
            return None;
        };
        if role != "developer" {
            return None;
        }
        let kinds = internal_chat_message_metadata_passthrough
            .as_ref()?
            .content_item_kinds
            .as_ref()?;
        content
            .iter()
            .zip(kinds)
            .rev()
            .find_map(|(content, item_kind)| {
                if item_kind != &kind {
                    return None;
                }
                match content {
                    ContentItem::InputText { text } => Some(text.as_str()),
                    _ => None,
                }
            })
    });
    if previous == Some(instructions) {
        return None;
    }
    build_rendered_message(vec![
        DeveloperInstructions::new(instructions).render_fragment(),
    ])
}

#[cfg(test)]
#[path = "fork_instructions_tests.rs"]
mod tests;
