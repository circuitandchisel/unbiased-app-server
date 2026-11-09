use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn call(id: &str, arguments: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(
        serde_json::from_value(json!({
            "type": "function_call", "call_id": id, "name": "exec_command", "arguments": arguments,
        }))
        .unwrap(),
    )
}

fn output(id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(
        serde_json::from_value(json!({
            "type": "function_call_output", "call_id": id, "output": "parse failure",
        }))
        .unwrap(),
    )
}

#[test]
fn malformed_calls_quarantine_only_the_bad_pair_and_preserve_diagnostics() {
    let original = vec![
        call("bad", r#"{"cmd":"unfinished"#),
        output("bad"),
        call("good", r#"{"cmd":"pwd"}"#),
        output("good"),
    ];
    let mut outgoing = original.clone();
    quarantine_malformed_calls(&mut outgoing);
    assert_eq!(
        outgoing,
        vec![
            ResponseItemEnvelope::new(ContextualUserFragment::into(ToolCallFormatError)),
            original[2].clone(),
            original[3].clone()
        ]
    );
    assert_eq!(malformed_call_id(&original[0].item), Some("bad"));
    assert!(crate::context::is_guardian_context_message(
        &outgoing[0].item
    ));
}

#[test]
fn malformed_calls_validate_objects_and_leave_valid_and_custom_calls_unchanged() {
    for args in [
        "",
        "null",
        "[]",
        "true",
        r#"{"cmd":"x"} trailing"#,
        r#"{"cmd": "#,
        r#"{"cmd":"\uD800"}"#,
    ] {
        assert_eq!(malformed_call_id(&call("bad", args).item), Some("bad"));
    }
    for args in [
        "{}",
        "  {\"cmd\":\"echo \\\"hi\\\"\"} \n",
        r#"{"x":{"y":[1,null]}}"#,
    ] {
        let original = vec![call("good", args), output("good")];
        let mut outgoing = original.clone();
        quarantine_malformed_calls(&mut outgoing);
        assert_eq!(outgoing, original);
    }
    let custom: ResponseItem = serde_json::from_value(json!({
        "type":"custom_tool_call", "call_id":"patch", "name":"apply_patch", "input":"not JSON",
    }))
    .unwrap();
    assert_eq!(malformed_call_id(&custom), None);
}

#[test]
fn malformed_calls_reproduce_runaway_truncation_without_echoing_the_payload() {
    let args = format!(
        "{{\"cmd\": \"{}cat /",
        "cat /tmp/private.log; ".repeat(2167)
    );
    let mut outgoing = vec![call("bad", &args), output("bad")];
    quarantine_malformed_calls(&mut outgoing);
    assert_eq!(
        outgoing,
        vec![ResponseItemEnvelope::new(ContextualUserFragment::into(
            ToolCallFormatError
        ))]
    );
    let text = serde_json::to_string(
        &outgoing
            .iter()
            .map(|envelope| &envelope.item)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(!text.contains("private.log"));
    assert!(text.len() < 1000);
}

#[test]
fn malformed_calls_budget_counts_responses_not_parallel_calls_and_resets_per_turn() {
    let budget = RecoveryBudget::<()>::default();
    let first = Arc::new(());
    let second = Arc::new(());
    let third = Arc::new(());
    assert_eq!(
        (
            budget.request_correction(&first),
            budget.request_correction(&first),
            budget.request_correction(&second),
            budget.request_correction(&third)
        ),
        (true, true, true, false)
    );
    assert!(RecoveryBudget::<()>::default().request_correction(&third));
}

#[test]
fn malformed_calls_do_not_interpret_encrypted_argument_payloads_as_json() {
    let encrypted: ResponseItem = serde_json::from_value(json!({
        "type":"function_call", "call_id":"encrypted", "name":"send_message_to_agent",
        "arguments":"opaque", "encrypted_function_args":["opaque-ciphertext"]
    }))
    .unwrap();
    assert_eq!(malformed_call_id(&encrypted), None);
}
