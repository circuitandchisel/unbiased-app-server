// Copyright 2026 Circuit & Chisel. Licensed under the Apache License, Version 2.0.

use crate::common::ResponsesApiRequest;
use crate::error::ApiError;
use codex_client::EncodedJsonBody;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use std::path::Path;

const OMITTED_IMAGE: &str = "[Earlier image omitted from this request due to its size; no reopenable local source is available.]";
const OMITTED_AUDIO: &str = "[Earlier audio omitted from this request due to its size.]";

fn local_image_reference(path: &str) -> Option<String> {
    if !Path::new(path).is_absolute() || !Path::new(path).is_file() {
        return None;
    }
    let quoted_path = serde_json::to_string(path).ok()?;
    Some(format!(
        "[Earlier image omitted to fit this request. Reopen the original with view_image({{\"path\":{quoted_path}}}) if needed.]"
    ))
}

fn source_path_for_tool_image(input: &[ResponseItem], output_index: usize) -> Option<String> {
    let ResponseItem::FunctionCallOutput {
        call_id: Some(call_id),
        name: Some(name),
        ..
    } = &input[output_index]
    else {
        return None;
    };
    if name != "view_image" {
        return None;
    }
    input[..output_index].iter().rev().find_map(|item| {
        let ResponseItem::FunctionCall {
            name,
            call_id: prior_call_id,
            arguments,
            ..
        } = item
        else {
            return None;
        };
        if name != "view_image" || prior_call_id != call_id {
            return None;
        }
        let args: serde_json::Value = serde_json::from_str(arguments).ok()?;
        args.get("path")?.as_str().map(str::to_owned)
    })
}

fn source_path_for_user_image(parts: &[ContentItem], part_index: usize) -> Option<String> {
    let ContentItem::InputText { text } = parts.get(part_index.checked_sub(1)?)? else {
        return None;
    };
    let (_, suffix) = text.rsplit_once(" path=\"")?;
    suffix.strip_suffix("\">").map(str::to_owned)
}

fn image_reference_for_tool_output(input: &[ResponseItem], index: usize) -> String {
    source_path_for_tool_image(input, index)
        .and_then(|path| local_image_reference(&path))
        .unwrap_or_else(|| OMITTED_IMAGE.to_string())
}

pub(super) fn encode_request(
    request: &mut ResponsesApiRequest,
    max_body_bytes: usize,
) -> Result<EncodedJsonBody, ApiError> {
    let original_bytes = serde_json::to_vec(request)
        .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?
        .len();
    let (final_bytes, omitted) = trim_response_request(request, max_body_bytes)
        .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))?;

    if omitted > 0 {
        tracing::warn!(
            original_bytes,
            final_bytes,
            omitted_media = omitted,
            "omitted older inline media from outgoing response request"
        );
    }

    if final_bytes > max_body_bytes {
        return Err(ApiError::InvalidRequest {
            message: format!(
                "response request is {} bytes, above the {max_body_bytes}-byte limit; no older inline media remains to omit. Reduce the latest attachment or large tool output and retry",
                final_bytes
            ),
        });
    }

    encode(request)
}

/// Bounds the request copy, leaving the persisted conversation unchanged.
pub fn trim_response_request(
    request: &mut ResponsesApiRequest,
    max_body_bytes: usize,
) -> Result<(usize, usize), serde_json::Error> {
    let mut bytes = serde_json::to_vec(&*request)?.len();
    let mut omitted = 0;
    while bytes > max_body_bytes && omit_redundant_or_oldest_inline_media(request).is_some() {
        bytes = serde_json::to_vec(&*request)?.len();
        omitted += 1;
    }
    Ok((bytes, omitted))
}

pub(super) fn omit_redundant_or_oldest_inline_media(
    request: &mut ResponsesApiRequest,
) -> Option<usize> {
    omit_duplicate_tool_image(request).or_else(|| omit_oldest_inline_media(request))
}

fn has_inline_image(item: &ResponseItem, url: &str) -> bool {
    match item {
        ResponseItem::Message { content, .. } => content.iter().any(|part| {
            matches!(part, ContentItem::InputImage {
                image: ImageReference::Inline { image_url }, ..
            } if image_url == url)
        }),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => match &output.body {
            FunctionCallOutputBody::ContentItems(parts) => parts.iter().any(|part| {
                matches!(part, FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline { image_url }, ..
                } if image_url == url)
            }),
            FunctionCallOutputBody::Text(_) => false,
        },
        _ => false,
    }
}

fn omit_duplicate_tool_image(request: &mut ResponsesApiRequest) -> Option<usize> {
    for index in 0..request.input.len() {
        let duplicate_part = match &request.input[index] {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => match &output.body {
                FunctionCallOutputBody::ContentItems(parts) => {
                    parts.iter().enumerate().find_map(|(part_index, part)| {
                        let FunctionCallOutputContentItem::InputImage {
                            image: ImageReference::Inline { image_url },
                            ..
                        } = part
                        else {
                            return None;
                        };
                        request.input[..index]
                            .iter()
                            .any(|earlier| has_inline_image(earlier, image_url))
                            .then_some(part_index)
                    })
                }
                FunctionCallOutputBody::Text(_) => None,
            },
            _ => None,
        };
        let Some(part_index) = duplicate_part else {
            continue;
        };
        let reference = image_reference_for_tool_output(&request.input, index);
        let (ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. }) = &mut request.input[index]
        else {
            unreachable!();
        };
        let FunctionCallOutputBody::ContentItems(parts) = &mut output.body else {
            unreachable!();
        };
        let part = &mut parts[part_index];
        let replacement = FunctionCallOutputContentItem::InputText {
            text: if reference == OMITTED_IMAGE {
                "[Repeated image omitted from this request; the original appeared earlier.]"
                    .to_string()
            } else {
                reference
            },
        };
        let original_bytes = serde_json::to_vec(part).ok()?.len();
        let replacement_bytes = serde_json::to_vec(&replacement).ok()?.len();
        if original_bytes > replacement_bytes {
            *part = replacement;
            return Some(original_bytes - replacement_bytes);
        }
    }
    None
}

fn omit_oldest_inline_media(request: &mut ResponsesApiRequest) -> Option<usize> {
    let latest_user_index = request
        .input
        .iter()
        .rposition(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))
        .unwrap_or(request.input.len());

    for index in 0..request.input.len() {
        let tool_reference = image_reference_for_tool_output(&request.input, index);
        let item = &mut request.input[index];
        let parts = match item {
            ResponseItem::Message { role, content, .. }
                if role == "user" && index < latest_user_index =>
            {
                Some(content)
            }
            _ => None,
        };
        if let Some(parts) = parts {
            for part_index in 0..parts.len() {
                let user_reference = source_path_for_user_image(parts, part_index)
                    .and_then(|path| local_image_reference(&path))
                    .unwrap_or_else(|| OMITTED_IMAGE.to_string());
                let part = &mut parts[part_index];
                let replacement = match part {
                    ContentItem::InputImage {
                        image: ImageReference::Inline { .. },
                        ..
                    } => Some(user_reference.as_str()),
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
        let output = match item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => output,
            _ => continue,
        };
        let FunctionCallOutputBody::ContentItems(parts) = &mut output.body else {
            continue;
        };
        for part in parts {
            let replacement = match part {
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline { .. },
                    ..
                } => Some(tool_reference.as_str()),
                FunctionCallOutputContentItem::InputAudio { .. } => Some(OMITTED_AUDIO),
                FunctionCallOutputContentItem::InputText { .. }
                | FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::File { .. },
                    ..
                }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            };
            if let Some(text) = replacement {
                let replacement = FunctionCallOutputContentItem::InputText {
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

#[cfg(test)]
#[path = "request_body_limit_tests.rs"]
mod tests;

fn encode(request: &ResponsesApiRequest) -> Result<EncodedJsonBody, ApiError> {
    EncodedJsonBody::encode(request)
        .map_err(|e| ApiError::Stream(format!("failed to encode responses request: {e}")))
}
