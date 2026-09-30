use super::*;
use codex_http_client::OutboundProxyPolicy;
use codex_protocol::models::FunctionCallOutputPayload;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn message(role: &str, text: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::Message {
        id: None,
        role: role.to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    })
}

fn call(call_id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCall {
        id: None,
        name: "exec_command".to_string(),
        namespace: None,
        arguments: "{\"cmd\":\"test\"}".to_string(),
        encrypted_function_args: None,
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn output(call_id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text("x".repeat(5_000)),
        internal_chat_message_metadata_passthrough: None,
    })
}

fn history() -> Vec<ResponseItemEnvelope> {
    let mut items = vec![message("user", "Find the bug"), call("old"), output("old")];
    items.extend((0..6).map(|index| message("assistant", &format!("Recent {index}"))));
    items
}

fn answer(call: f64, result: f64) -> JevResponse {
    JevResponse {
        answers: HashMap::from([
            ("call_t0".to_string(), JevAnswer { noul: call }),
            ("result_t0".to_string(), JevAnswer { noul: result }),
        ]),
    }
}

#[test]
fn jev_drops_a_call_and_its_result_together() {
    let items = history();
    let candidates = collect_candidates(&items);
    assert_eq!(candidates.len(), 1);
    let retained = apply_answers(&items, &candidates, &answer(0.1, 0.1), 0, None)
        .unwrap()
        .items;
    assert_eq!(retained.len(), items.len() - 2);
    assert_eq!(retained[0], items[0]);
    assert_eq!(retained[1..], items[3..]);
}

#[test]
fn jev_shortens_only_the_result_when_the_call_matters() {
    let items = history();
    let candidates = collect_candidates(&items);
    let retained = apply_answers(&items, &candidates, &answer(0.9, 0.1), 0, None)
        .unwrap()
        .items;
    assert_eq!(retained.len(), items.len());
    assert_eq!(retained[1], items[1]);
    let ResponseItem::FunctionCallOutput { output, .. } = &retained[2].item else {
        panic!("expected tool output");
    };
    let text = output.text_content().unwrap();
    assert!(text.len() < 500);
    assert!(text.contains("run the tool again"));
}

#[test]
fn adaptive_pruning_stops_when_the_byte_target_is_met() {
    let mut items = vec![message("user", "Find the bug")];
    for id in ["low", "medium", "protected"] {
        items.extend([call(id), output(id)]);
    }
    items.extend((0..6).map(|index| message("assistant", &format!("Recent {index}"))));
    let candidates = collect_candidates(&items);
    let response = JevResponse {
        answers: HashMap::from([
            ("call_t0".to_string(), JevAnswer { noul: 0.9 }),
            ("result_t0".to_string(), JevAnswer { noul: 0.55 }),
            ("call_t1".to_string(), JevAnswer { noul: 0.9 }),
            ("result_t1".to_string(), JevAnswer { noul: 0.65 }),
            ("call_t2".to_string(), JevAnswer { noul: 0.9 }),
            ("result_t2".to_string(), JevAnswer { noul: 0.85 }),
        ]),
    };

    let applied = apply_answers(&items, &candidates, &response, 8_000, None).unwrap();
    assert_eq!(applied.adaptive_result_count, 2);
    assert_eq!(applied.items[1], items[1]);
    assert_eq!(applied.items[3], items[3]);
    assert_eq!(applied.items[5], items[5]);
    assert_eq!(applied.items[6], items[6]);
    assert_eq!(&applied.items[7..], &items[7..]);
    assert!(
        serialized_item_bytes(&items).unwrap() - serialized_item_bytes(&applied.items).unwrap()
            >= 8_000
    );
}

#[test]
fn adaptive_pruning_cannot_cut_high_scoring_results_to_force_the_target() {
    let items = history();
    let candidates = collect_candidates(&items);
    let applied = apply_answers(&items, &candidates, &answer(0.9, 0.85), 4_000, None).unwrap();
    assert_eq!(applied.adaptive_result_count, 0);
    assert_eq!(applied.items, items);
}

#[test]
fn token_budget_can_trigger_adaptive_pruning_after_byte_target_is_met() {
    let items = history();
    let candidates = collect_candidates(&items);
    let original_tokens = items
        .iter()
        .map(|item| estimate_item_token_count(&item.item))
        .sum::<i64>();
    let applied = apply_answers(
        &items,
        &candidates,
        &answer(0.9, 0.6),
        0,
        Some(original_tokens),
    )
    .unwrap();
    assert_eq!(applied.adaptive_result_count, 1);
    assert_eq!(applied.items[1], items[1]);
    assert_eq!(&applied.items[3..], &items[3..]);
    assert!(
        applied
            .items
            .iter()
            .map(|item| estimate_item_token_count(&item.item))
            .sum::<i64>()
            < original_tokens
    );
}

#[test]
fn invalid_or_missing_answers_cannot_change_history() {
    let items = history();
    let candidates = collect_candidates(&items);
    assert!(apply_answers(&items, &candidates, &answer(f64::NAN, 0.0), 0, None).is_err());
    assert!(
        apply_answers(
            &items,
            &candidates,
            &JevResponse {
                answers: HashMap::new()
            },
            0,
            None,
        )
        .is_err()
    );
}

#[test]
fn recent_calls_are_not_scored() {
    let mut items = history();
    items.extend([call("recent"), output("recent")]);
    assert_eq!(collect_candidates(&items).len(), 1);
}

#[test]
fn duplicate_results_are_not_scored() {
    let mut items = history();
    items.insert(3, output("old"));
    assert!(collect_candidates(&items).is_empty());
}

#[test]
fn duplicate_calls_are_not_scored() {
    let mut items = history();
    items.insert(2, call("old"));
    assert!(collect_candidates(&items).is_empty());
}

#[tokio::test]
async fn jev_request_uses_unbiased_key_and_expected_wire_format() {
    let server = MockServer::start().await;
    let request = json!({
        "state": "USER: find issue\nTOOL: build failed",
        "questions": { "call_t0": { "type": "noul", "instructions": "keep?" } }
    });
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer test-key"))
        .and(body_json(request.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "answers": { "call_t0": { "noul": 0.75, "confidence": 0.95 } },
            "usage": { "input_tokens": 20 }, "latency_ms": 8
        })))
        .expect(1)
        .mount(&server)
        .await;
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let response = request_scores(
        &factory,
        &format!("{}/v1/systemone", server.uri()),
        "test-key",
        &request,
    )
    .await
    .unwrap();
    assert_eq!(response.answers["call_t0"].noul, 0.75);
}

#[test]
fn state_and_batches_stay_below_endpoint_budget() {
    let mut items = vec![message("user", "Investigate all failures")];
    for index in 0..80 {
        let id = format!("old-{index}");
        items.push(call(&id));
        items.push(output(&id));
    }
    items.extend((0..6).map(|index| message("assistant", &format!("Recent {index}"))));
    let candidates = collect_candidates(&items);
    assert_eq!(candidates.len(), 80);
    let state = build_state(&items, &candidates).unwrap();
    assert!(state.len() <= MAX_STATE_BYTES);
    assert!(state.contains("t0"));
    assert!(state.contains("t79"));
    let batches = build_question_batches(&state, &candidates).unwrap();
    assert!(batches.len() > 1);
    let answer_count = batches
        .iter()
        .map(|questions| questions.as_object().unwrap().len())
        .sum::<usize>();
    assert_eq!(answer_count, candidates.len() * 2);
    for questions in batches {
        let request = json!({ "state": state, "questions": questions });
        assert!(serde_json::to_vec(&request).unwrap().len() <= MAX_REQUEST_BYTES);
        assert_eq!(request["state"].as_str().unwrap(), state);
    }
}

#[tokio::test]
async fn batched_scores_rewrite_only_selected_old_text_results() {
    let mut items = vec![message("user", "Investigate all failures")];
    for index in 0..80 {
        let id = format!("old-{index}");
        items.push(call(&id));
        items.push(output(&id));
    }
    items.extend((0..6).map(|index| message("assistant", &format!("Recent {index}"))));
    let candidates = collect_candidates(&items);
    let state = build_state(&items, &candidates).unwrap();
    let batches = build_question_batches(&state, &candidates).unwrap();
    assert!(batches.len() > 1);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(|request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let questions = body["questions"].as_object().unwrap();
            let answers = questions
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        json!({ "noul": if name.starts_with("call_") { 0.9 } else { 0.1 } }),
                    )
                })
                .collect::<Map<String, Value>>();
            ResponseTemplate::new(200).set_body_json(json!({ "answers": answers }))
        })
        .expect(batches.len() as u64)
        .mount(&server)
        .await;

    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let scores = score_batches(
        &factory,
        &format!("{}/v1/systemone", server.uri()),
        "test-key",
        &state,
        &batches,
    )
    .await
    .unwrap();
    assert_eq!(scores.answers.len(), 160);
    let retained = apply_answers(&items, &candidates, &scores, 0, None)
        .unwrap()
        .items;
    assert_eq!(retained.len(), items.len());
    assert_eq!(&retained[retained.len() - 6..], &items[items.len() - 6..]);
    let ResponseItem::FunctionCallOutput { output, .. } = &retained[2].item else {
        panic!("expected old tool result");
    };
    assert!(output.text_content().unwrap().len() < 500);
}

#[tokio::test]
async fn jev_rejects_untrusted_destinations_before_sending_key() {
    let factory = HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault);
    let result = request_scores(&factory, "https://example.com/jev", "secret", &json!({})).await;
    assert!(result.is_err());
}
