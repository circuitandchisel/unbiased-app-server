// Copyright 2026 Circuit & Chisel. Licensed under the Apache License, Version 2.0.

use crate::common::ResponsesApiRequest;
use crate::error::ApiError;
use codex_client::EncodedJsonBody;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;

const OMITTED_IMAGE: &str = "[Earlier image omitted from this request due to its size.]";
const OMITTED_AUDIO: &str = "[Earlier audio omitted from this request due to its size.]";

pub(super) fn encode_request(
    request: &mut ResponsesApiRequest,
    max_body_bytes: usize,
) -> Result<EncodedJsonBody, ApiError> {
    let mut body = encode(request)?;
    let original_bytes = body.as_bytes().len();
    let mut estimated_bytes = original_bytes;
    let mut omitted = 0;

    while estimated_bytes > max_body_bytes {
        let Some(bytes_saved) = omit_oldest_inline_media(request) else {
            break;
        };
        estimated_bytes -= bytes_saved;
        omitted += 1;
    }
    if omitted > 0 {
        body = encode(request)?;
    }

    if omitted > 0 {
        tracing::warn!(
            original_bytes,
            final_bytes = body.as_bytes().len(),
            omitted_media = omitted,
            "omitted older inline media from outgoing response request"
        );
    }

    if body.as_bytes().len() > max_body_bytes {
        return Err(ApiError::InvalidRequest {
            message: format!(
                "response request is {} bytes, above the {max_body_bytes}-byte limit; no older inline media remains to omit. Reduce the latest attachment or large tool output and retry",
                body.as_bytes().len()
            ),
        });
    }

    Ok(body)
}

pub(super) fn omit_oldest_inline_media(request: &mut ResponsesApiRequest) -> Option<usize> {
    let latest_user_index = request
        .input
        .iter()
        .rposition(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))?;

    for item in request.input.iter_mut().take(latest_user_index) {
        let ResponseItem::Message { role, content, .. } = item else {
            continue;
        };
        if role != "user" {
            continue;
        }
        for part in content {
            let replacement = match part {
                ContentItem::InputImage {
                    image: ImageReference::Inline { .. },
                    ..
                } => Some(OMITTED_IMAGE),
                ContentItem::InputAudio { .. } => Some(OMITTED_AUDIO),
                ContentItem::InputText { .. }
                | ContentItem::InputImage {
                    image: ImageReference::File { .. },
                    ..
                }
                | ContentItem::OutputText { .. } => None,
            };
            if let Some(text) = replacement {
                let replacement = ContentItem::InputText {
                    text: text.to_string(),
                };
                let original_bytes = serde_json::to_vec(part).ok()?.len();
                let replacement_bytes = serde_json::to_vec(&replacement).ok()?.len();
                if original_bytes > replacement_bytes {
                    *part = replacement;
                    return Some(original_bytes - replacement_bytes);
                }
            }
        }
    }

    None
}

fn encode(request: &ResponsesApiRequest) -> Result<EncodedJsonBody, ApiError> {
    EncodedJsonBody::encode(request)
        .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))
}
