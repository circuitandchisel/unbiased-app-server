// Copyright 2026 Circuit & Chisel. Licensed under the Apache License, Version 2.0.

use crate::compact::CompactedHistoryMetadata;
use crate::compact::InitialContextInjection;
use crate::compact::build_compaction_initial_context;
use crate::compact::insert_initial_context_before_last_real_user_or_summary;
use crate::context_manager::estimate_item_token_count;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_history::ResponseItemEnvelope;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use futures::future::try_join_all;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const MAX_TEXT_ITEMS: usize = 200;
const MAX_CALLS: usize = 80;
const MAX_STATE_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BYTES: usize = 30 * 1024;
const MIN_SAVED_BYTES: usize = 4_096;
const MAX_RETAINED_BYTES: usize = 6 * 1024 * 1024;
const MAX_JEV_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_ADAPTIVE_RESULT_SCORE: f64 = 0.75;

struct Candidate {
    call_index: usize,
    result_index: usize,
    name: String,
    input: String,
    result_chars: usize,
    result_preview: String,
}

struct StateLine {
    item_index: usize,
    text: String,
    removable: bool,
    candidate_index: Option<usize>,
}

#[derive(Clone, Copy)]
enum HistoryEdit {
    Keep,
    Shorten,
    DropPair,
}

struct AppliedAnswers {
    items: Vec<ResponseItemEnvelope>,
    adaptive_result_count: usize,
}

struct AdaptiveOption {
    candidate_index: usize,
    result_score: f64,
    bytes_saved: usize,
    tokens_saved: i64,
}

pub(crate) struct JevPreview {
    pub(crate) history: Vec<ResponseItemEnvelope>,
    pub(crate) original_bytes: usize,
    pub(crate) retained_bytes: usize,
    pub(crate) candidate_count: usize,
    pub(crate) adaptive_result_count: usize,
}

#[derive(Deserialize)]
struct JevAnswer {
    noul: f64,
}

#[derive(Deserialize)]
struct JevResponse {
    answers: HashMap<String, JevAnswer>,
}

pub(crate) async fn try_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    initial_context_injection: &InitialContextInjection,
) -> bool {
    if turn_context.config.model_provider_id != "unbiased" {
        return false;
    }
    let Ok(url) = std::env::var("UNBIASED_JEV_URL") else {
        tracing::info!(reason = "url_not_configured", "Jev compaction skipped");
        return false;
    };
    let Ok(key) = std::env::var("UNBIASED_API_KEY") else {
        tracing::warn!("Jev compaction skipped: Unbiased API key is unavailable");
        return false;
    };
    match compact_with_jev(sess, turn_context, initial_context_injection, &url, &key).await {
        Ok(true) => true,
        Ok(false) => false,
        Err(reason) => {
            tracing::warn!(reason, "Jev compaction fell back to the built-in summary");
            false
        }
    }
}

async fn compact_with_jev(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    initial_context_injection: &InitialContextInjection,
    url: &str,
    key: &str,
) -> Result<bool, &'static str> {
    let snapshot = sess.clone_history().await;
    let Some(preview) = plan_compact(turn_context, snapshot.annotated_items(), url, key).await?
    else {
        return Ok(false);
    };
    let mut retained = preview.history;
    let (initial_context, world_state_baseline) =
        build_compaction_initial_context(sess.as_ref(), initial_context_injection).await;
    retained = insert_initial_context_before_last_real_user_or_summary(retained, initial_context);
    let retained_with_context_bytes = serialized_item_bytes(&retained)?;
    if retained_with_context_bytes > MAX_RETAINED_BYTES {
        tracing::info!(
            reason = "history_with_context_too_large",
            candidate_count = preview.candidate_count,
            retained_with_context_bytes,
            adaptive_result_count = preview.adaptive_result_count,
            "Jev compaction skipped"
        );
        return Ok(false);
    }
    let item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &item).await;
    let (window_number, window_ids) = sess.advance_auto_compact_window().await;
    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage { step_context, .. } => {
            Some(step_context.to_turn_context_item())
        }
    };
    sess.replace_compacted_history(
        retained,
        reference_context_item,
        world_state_baseline,
        CompactedHistoryMetadata {
            message: String::new(),
            window_number,
            window_ids,
            compaction_response_id: None,
            compaction_model_hash: turn_context.model_info().comp_hash.clone(),
            reviewer_compaction_hash: None,
        },
    )
    .await;
    sess.recompute_token_usage(turn_context).await;
    sess.emit_turn_item_completed(turn_context, item).await;
    tracing::info!(
        candidate_count = preview.candidate_count,
        original_bytes = preview.original_bytes,
        retained_bytes = preview.retained_bytes,
        saved_bytes = preview
            .original_bytes
            .saturating_sub(preview.retained_bytes),
        adaptive_result_count = preview.adaptive_result_count,
        "Jev compaction installed"
    );
    Ok(true)
}

pub(crate) async fn preview_compact(
    turn_context: &Arc<TurnContext>,
    items: &[ResponseItemEnvelope],
) -> Result<Option<JevPreview>, &'static str> {
    if turn_context.config.model_provider_id != "unbiased" {
        return Err("Jev is unavailable for this provider");
    }
    let url = std::env::var("UNBIASED_JEV_URL").map_err(|_| "Jev URL is unavailable")?;
    let key = std::env::var("UNBIASED_API_KEY").map_err(|_| "Unbiased API key is unavailable")?;
    plan_compact(turn_context, items, &url, &key).await
}

async fn plan_compact(
    turn_context: &Arc<TurnContext>,
    items: &[ResponseItemEnvelope],
    url: &str,
    key: &str,
) -> Result<Option<JevPreview>, &'static str> {
    let candidates = collect_candidates(items);
    if candidates.is_empty() {
        tracing::info!(
            reason = "no_eligible_tool_results",
            item_count = items.len(),
            "Jev compaction skipped"
        );
        return Ok(None);
    }

    let state = build_state(items, &candidates)?;
    let batches = build_question_batches(&state, &candidates)?;
    let factory = turn_context.config.http_client_factory();
    let answer = score_batches(&factory, url, key, &state, &batches).await?;
    let original_bytes = serialized_item_bytes(items)?;
    let required_saved_bytes = MIN_SAVED_BYTES
        .max(original_bytes.div_ceil(4))
        .max(original_bytes.saturating_sub(MAX_RETAINED_BYTES));
    let token_budget = turn_context
        .model_info()
        .auto_compact_token_limit()
        .map(|limit| limit.saturating_mul(4) / 5);
    let applied = apply_answers(
        items,
        &candidates,
        &answer,
        required_saved_bytes,
        token_budget,
    )?;
    let retained = applied.items;
    let retained_bytes = serialized_item_bytes(&retained)?;
    let saved_bytes = original_bytes.saturating_sub(retained_bytes);
    if retained_bytes > MAX_RETAINED_BYTES {
        tracing::info!(
            reason = "retained_history_too_large",
            candidate_count = candidates.len(),
            original_bytes,
            retained_bytes,
            adaptive_result_count = applied.adaptive_result_count,
            "Jev compaction skipped"
        );
        return Ok(None);
    }
    if saved_bytes < MIN_SAVED_BYTES {
        tracing::info!(
            reason = "insufficient_bytes_saved",
            candidate_count = candidates.len(),
            original_bytes,
            retained_bytes,
            saved_bytes,
            adaptive_result_count = applied.adaptive_result_count,
            "Jev compaction skipped"
        );
        return Ok(None);
    }
    if retained_bytes.saturating_mul(4) > original_bytes.saturating_mul(3) {
        tracing::info!(
            reason = "insufficient_fraction_saved",
            candidate_count = candidates.len(),
            original_bytes,
            retained_bytes,
            saved_bytes,
            adaptive_result_count = applied.adaptive_result_count,
            "Jev compaction skipped"
        );
        return Ok(None);
    }
    if let Some(token_budget) = token_budget {
        let retained_tokens = retained
            .iter()
            .map(|item| estimate_item_token_count(&item.item))
            .sum::<i64>();
        if retained_tokens >= token_budget {
            tracing::info!(
                reason = "retained_history_over_token_budget",
                candidate_count = candidates.len(),
                retained_tokens,
                token_budget,
                adaptive_result_count = applied.adaptive_result_count,
                "Jev compaction skipped"
            );
            return Ok(None);
        }
    }
    Ok(Some(JevPreview {
        history: retained,
        original_bytes,
        retained_bytes,
        candidate_count: candidates.len(),
        adaptive_result_count: applied.adaptive_result_count,
    }))
}

async fn score_batches(
    factory: &HttpClientFactory,
    url: &str,
    key: &str,
    state: &str,
    batches: &[Value],
) -> Result<JevResponse, &'static str> {
    let answers = try_join_all(batches.iter().map(|questions| async move {
        let request = json!({ "state": state, "questions": questions });
        request_scores(factory, url, key, &request).await
    }))
    .await?;
    Ok(JevResponse {
        answers: answers
            .into_iter()
            .flat_map(|batch| batch.answers)
            .collect(),
    })
}

async fn request_scores(
    factory: &HttpClientFactory,
    url: &str,
    key: &str,
    request: &Value,
) -> Result<JevResponse, &'static str> {
    let url = Url::parse(url).map_err(|_| "invalid Jev URL")?;
    let host = url.host_str().ok_or("Jev URL has no host")?;
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return Err("Jev URL cannot contain credentials or a fragment");
    }
    let allowed = url.scheme() == "https" && host == "api.unbiased.ai";
    let local = url.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "::1");
    if !allowed && !local {
        return Err("Jev URL must use Unbiased HTTPS or a local test server");
    }

    let request_bytes = serde_json::to_vec(request).map_err(|_| "could not encode Jev request")?;
    if request_bytes.len() > MAX_REQUEST_BYTES {
        return Err("Jev request exceeded its size budget");
    }

    let client = factory
        .build_client_without_request_logging(url.as_str(), ClientRouteClass::Api)
        .map_err(|_| "could not create Jev HTTP client")?;
    let mut response = client
        .post(url.as_str())
        .bearer_auth(key)
        .json(request)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|_| "Jev request failed")?
        .error_for_status()
        .map_err(|_| "Jev returned an unsuccessful status")?;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "could not read Jev response")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_JEV_RESPONSE_BYTES {
            return Err("Jev response exceeded its size budget");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| "Jev returned invalid JSON")
}

fn collect_candidates(items: &[ResponseItemEnvelope]) -> Vec<Candidate> {
    let recent = items.len().saturating_sub(6);
    let mut outputs = HashMap::<&str, Option<usize>>::new();
    let mut call_counts = HashMap::<&str, usize>::new();
    for (index, envelope) in items.iter().enumerate() {
        match &envelope.item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. } => {
                *call_counts.entry(call_id).or_default() += 1;
            }
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => {
                outputs
                    .entry(call_id)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(index));
            }
            _ => {}
        }
    }
    let mut candidates = items
        .iter()
        .enumerate()
        .skip(1)
        .take(recent.saturating_sub(1))
        .filter_map(|(call_index, envelope)| {
            let (name, input, call_id) = match &envelope.item {
                ResponseItem::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    encrypted_function_args: None,
                    ..
                } => (name, arguments, call_id),
                ResponseItem::CustomToolCall {
                    name,
                    input,
                    call_id,
                    ..
                } => (name, input, call_id),
                _ => return None,
            };
            if call_counts.get(call_id.as_str()) != Some(&1) {
                return None;
            }
            let result_index = (*outputs.get(call_id.as_str())?)?;
            if result_index <= call_index || result_index >= recent {
                return None;
            }
            let result_preview = match &items[result_index].item {
                ResponseItem::FunctionCallOutput { output, .. }
                | ResponseItem::CustomToolCallOutput { output, .. } => match &output.body {
                    FunctionCallOutputBody::Text(text) => prefix(text, 500),
                    FunctionCallOutputBody::ContentItems(_) => return None,
                },
                _ => return None,
            };
            let result_chars = serde_json::to_string(&items[result_index].item).ok()?.len();
            (result_chars > 300).then(|| Candidate {
                call_index,
                result_index,
                name: prefix(name, 80),
                input: prefix(input, 1_000),
                result_chars,
                result_preview,
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.result_chars));
    candidates.truncate(MAX_CALLS);
    candidates.sort_by_key(|candidate| candidate.call_index);
    candidates
}

fn build_state(
    items: &[ResponseItemEnvelope],
    candidates: &[Candidate],
) -> Result<String, &'static str> {
    let recent = items.len().saturating_sub(6);
    let text_start = items.len().saturating_sub(MAX_TEXT_ITEMS);
    let candidate_positions = candidates
        .iter()
        .enumerate()
        .map(|(position, candidate)| (candidate.call_index, position))
        .collect::<HashMap<_, _>>();
    let mut goals = items
        .iter()
        .rev()
        .filter_map(|envelope| match &envelope.item {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                content.iter().find_map(|part| match part {
                    ContentItem::InputText { text } => Some(prefix(text, 300)),
                    _ => None,
                })
            }
            _ => None,
        })
        .take(3)
        .collect::<Vec<_>>();
    goals.reverse();
    let header = format!(
        "Decide which older tool calls and full results are needed to continue. Results can be re-run.\nGoal: {}\nHistory (oldest first):",
        json!(goals.join(" | "))
    );
    let mut lines = items
        .iter()
        .enumerate()
        .filter_map(|(index, envelope)| {
            if let Some(&position) = candidate_positions.get(&index) {
                let call = &candidates[position];
                return Some(StateLine {
                    item_index: index,
                    text: format!(
                        "{index} TOOL t{position} {} input={} result_preview={}",
                        call.name,
                        json!(prefix(&call.input, 300)),
                        json!(prefix(&call.result_preview, 300))
                    ),
                    removable: false,
                    candidate_index: Some(position),
                });
            }
            if index != 0 && index < text_start {
                return None;
            }
            match &envelope.item {
                ResponseItem::Message { role, content, .. }
                    if role == "user" || role == "assistant" =>
                {
                    let text = content
                        .iter()
                        .filter_map(|part| match part {
                            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    (!text.trim().is_empty()).then(|| StateLine {
                        item_index: index,
                        text: format!("{index} {role}: {}", json!(prefix(&text, 300))),
                        removable: index != 0 && index < recent,
                        candidate_index: None,
                    })
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    let render = |lines: &[StateLine]| {
        format!(
            "{header}\n{}",
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    let mut state = render(&lines);
    if state.len() > MAX_STATE_BYTES {
        for line in &mut lines {
            if let Some(position) = line.candidate_index {
                let call = &candidates[position];
                line.text = format!(
                    "{} TOOL t{position} {} input={} result={} chars",
                    line.item_index,
                    call.name,
                    json!(prefix(&call.input, 60)),
                    call.result_chars
                );
            }
        }
        state = render(&lines);
    }
    while state.len() > MAX_STATE_BYTES {
        let Some(index) = lines.iter().position(|line| line.removable) else {
            return Err("history could not fit in Jev state budget");
        };
        lines.remove(index);
        state = render(&lines);
    }
    Ok(state)
}

fn add_questions(questions: &mut Map<String, Value>, index: usize, candidate: &Candidate) {
    questions.insert(format!("call_t{index}"), json!({
        "type": "noul", "instructions": format!("Tool call t{index} ({}) should stay in the history: knowing it was made and its input still matters for the next step", candidate.name)
    }));
    questions.insert(format!("result_t{index}"), json!({
        "type": "noul", "instructions": format!("The full result of tool call t{index} ({}, {} chars) should stay verbatim: it is still needed and re-running the tool would not do", candidate.name, candidate.result_chars)
    }));
}

fn build_question_batches(
    state: &str,
    candidates: &[Candidate],
) -> Result<Vec<Value>, &'static str> {
    let mut batches = Vec::new();
    let mut current = Map::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let mut next = current.clone();
        add_questions(&mut next, index, candidate);
        let size = serde_json::to_vec(&json!({ "state": state, "questions": next }))
            .map_err(|_| "could not encode Jev request")?
            .len();
        if size > MAX_REQUEST_BYTES && !current.is_empty() {
            batches.push(Value::Object(current));
            current = Map::new();
            add_questions(&mut current, index, candidate);
        } else {
            current = next;
        }
        let size = serde_json::to_vec(&json!({ "state": state, "questions": current }))
            .map_err(|_| "could not encode Jev request")?
            .len();
        if size > MAX_REQUEST_BYTES {
            return Err("Jev state leaves no room for a question pair");
        }
    }
    if !current.is_empty() {
        batches.push(Value::Object(current));
    }
    Ok(batches)
}

fn apply_answers(
    items: &[ResponseItemEnvelope],
    candidates: &[Candidate],
    response: &JevResponse,
    required_saved_bytes: usize,
    token_budget: Option<i64>,
) -> Result<AppliedAnswers, &'static str> {
    let mut edits = vec![HistoryEdit::Keep; candidates.len()];
    let mut adaptive = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let score = |kind: &str| -> Result<f64, &'static str> {
            let name = format!("{kind}_t{index}");
            let value = response
                .answers
                .get(&name)
                .ok_or("Jev answer is missing")?
                .noul;
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                return Err("Jev score is invalid");
            }
            Ok(value)
        };
        let call = score("call")?;
        let result = score("result")?;
        if result < 0.5 {
            edits[index] = if call < 0.5 {
                HistoryEdit::DropPair
            } else {
                HistoryEdit::Shorten
            };
        } else if result < MAX_ADAPTIVE_RESULT_SCORE {
            let shortened = shortened_result(&items[candidate.result_index])?;
            let original = &items[candidate.result_index].item;
            let original_bytes = serde_json::to_vec(original)
                .map_err(|_| "could not measure compaction size")?
                .len();
            let shortened_bytes = serde_json::to_vec(&shortened.item)
                .map_err(|_| "could not measure compaction size")?
                .len();
            let bytes_saved = original_bytes.saturating_sub(shortened_bytes);
            if bytes_saved > 0 {
                adaptive.push(AdaptiveOption {
                    candidate_index: index,
                    result_score: result,
                    bytes_saved,
                    tokens_saved: estimate_item_token_count(original)
                        - estimate_item_token_count(&shortened.item),
                });
            }
        }
    }

    let mut retained = apply_edits(items, candidates, &edits)?;
    let original_bytes = serialized_item_bytes(items)?;
    let mut saved_bytes = original_bytes.saturating_sub(serialized_item_bytes(&retained)?);
    let mut retained_tokens = token_budget.map(|_| {
        retained
            .iter()
            .map(|item| estimate_item_token_count(&item.item))
            .sum::<i64>()
    });
    adaptive.sort_by(|a, b| {
        a.result_score
            .total_cmp(&b.result_score)
            .then_with(|| b.bytes_saved.cmp(&a.bytes_saved))
    });
    let mut adaptive_result_count = 0;
    for option in adaptive {
        if saved_bytes >= required_saved_bytes
            && retained_tokens
                .zip(token_budget)
                .is_none_or(|(retained, budget)| retained < budget)
        {
            break;
        }
        edits[option.candidate_index] = HistoryEdit::Shorten;
        saved_bytes = saved_bytes.saturating_add(option.bytes_saved);
        if let Some(tokens) = &mut retained_tokens {
            *tokens -= option.tokens_saved;
        }
        adaptive_result_count += 1;
    }
    if adaptive_result_count > 0 {
        retained = apply_edits(items, candidates, &edits)?;
    }
    Ok(AppliedAnswers {
        items: retained,
        adaptive_result_count,
    })
}

fn apply_edits(
    items: &[ResponseItemEnvelope],
    candidates: &[Candidate],
    edits: &[HistoryEdit],
) -> Result<Vec<ResponseItemEnvelope>, &'static str> {
    let mut drop = vec![false; items.len()];
    let mut shorten = vec![false; items.len()];
    for (candidate, edit) in candidates.iter().zip(edits) {
        match edit {
            HistoryEdit::Keep => {}
            HistoryEdit::Shorten => shorten[candidate.result_index] = true,
            HistoryEdit::DropPair => {
                drop[candidate.call_index] = true;
                drop[candidate.result_index] = true;
            }
        }
    }
    items
        .iter()
        .enumerate()
        .filter_map(|(index, envelope)| {
            if drop[index] {
                None
            } else if shorten[index] {
                Some(shortened_result(envelope))
            } else {
                Some(Ok(envelope.clone()))
            }
        })
        .collect()
}

fn shortened_result(envelope: &ResponseItemEnvelope) -> Result<ResponseItemEnvelope, &'static str> {
    let mut shortened = envelope.clone();
    let output = match &mut shortened.item {
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => output,
        _ => return Err("Jev candidate is not a tool result"),
    };
    let FunctionCallOutputBody::Text(text) = &output.body else {
        return Err("Jev candidate is not a text result");
    };
    output.body = FunctionCallOutputBody::Text(format!(
        "{}\n[Earlier tool result truncated by compaction; run the tool again if needed]",
        prefix(text, 300)
    ));
    Ok(shortened)
}

fn serialized_item_bytes(items: &[ResponseItemEnvelope]) -> Result<usize, &'static str> {
    items.iter().try_fold(0usize, |total, item| {
        serde_json::to_vec(&item.item)
            .map(|encoded| total.saturating_add(encoded.len()))
            .map_err(|_| "could not measure compaction size")
    })
}

fn prefix(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
#[path = "compact_jev_tests.rs"]
mod tests;
