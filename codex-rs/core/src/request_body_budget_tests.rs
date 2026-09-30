use super::*;
use crate::session::UserInputMetadata;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn message(text: String) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn pending(text: String) -> TurnInput {
    TurnInput::UserInput {
        content: vec![UserInput::Text {
            text,
            text_elements: Vec::new(),
        }],
        client_id: None,
        metadata: UserInputMetadata::default(),
    }
}

#[test]
fn earlier_history_and_new_input_are_counted_together() {
    let old = message("x".repeat(6 * 1024 * 1024));
    let new = pending("y".repeat(2 * 1024 * 1024));
    let (combined, incoming) = estimate_turn_bytes(&[old], std::slice::from_ref(&new)).unwrap();
    assert!(combined > UNBIASED_REQUEST_BUDGET_BYTES);
    assert!(incoming < UNBIASED_REQUEST_BUDGET_BYTES);

    let (after_compaction, _) =
        estimate_turn_bytes(&[message("summary".to_string())], &[new]).unwrap();
    assert!(after_compaction < UNBIASED_REQUEST_BUDGET_BYTES);
    assert_eq!(UNBIASED_REQUEST_BUDGET_BYTES, 7 * 1024 * 1024);
}
