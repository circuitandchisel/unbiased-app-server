use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_calls_request_a_corrected_call_without_resending_invalid_arguments() -> Result<()>
{
    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| config.update_plan_enabled = true);
    let test = builder.build_with_auto_env(&server).await?;
    let args = r#"{"plan":[{"step":"small task","status":"completed"}]}"#;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("r1"),
                ev_function_call("bad", "update_plan", r#"{"plan":"#),
                ev_function_call("bad2", "update_plan", r#"{"plan":"#),
                ev_function_call("bad3", "update_plan", r#"{"plan":"#),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_response_created("r2"),
                ev_function_call("good", "update_plan", args),
                ev_completed("r2"),
            ]),
            sse(vec![
                ev_response_created("r3"),
                ev_assistant_message("m3", "done"),
                ev_completed("r3"),
            ]),
        ],
    )
    .await;
    test.submit_turn("Make a small plan.").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let second = requests[1].body_json();
    let input = second["input"].as_array().unwrap();
    assert!(!input.iter().any(|item| {
        ["bad", "bad2", "bad3"]
            .iter()
            .any(|id| item["call_id"] == *id)
    }));
    let serialized = serde_json::to_string(input)?;
    assert!(serialized.contains("recheck the tool's declared schema"));
    assert!(serialized.contains("one complete JSON object"));
    let third = requests[2].body_json();
    assert!(
        third["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "good")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_calls_stop_after_two_corrections_and_allow_the_next_user_turn() -> Result<()> {
    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build_with_auto_env(&server).await?;
    let mut replies = Vec::new();
    for i in 0..3 {
        replies.push(sse(vec![
            ev_response_created(&format!("r{i}")),
            ev_function_call(&format!("bad{i}"), "exec_command", r#"{"cmd":"#),
            ev_completed(&format!("r{i}")),
        ]));
    }
    replies.push(sse(vec![
        ev_response_created("fresh"),
        ev_assistant_message("m", "Recovered"),
        ev_completed("fresh"),
    ]));
    let mock = mount_sse_sequence(&server, replies).await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Do the task".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let EventMsg::Error(error) =
        wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::Error(_))).await
    else {
        unreachable!();
    };
    assert!(
        error
            .message
            .contains("Stopped after two correction attempts")
    );
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    assert_eq!(mock.requests().len(), 3);
    test.submit_turn("Hello").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        !requests[3].body_json()["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == json!("function_call")
                || item["type"] == json!("function_call_output"))
    );
    Ok(())
}
