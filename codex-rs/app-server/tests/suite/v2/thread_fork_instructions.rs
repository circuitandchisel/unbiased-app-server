use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;

const PARENT_TASK: &str = "Parent task: wire Relay to inject the ECR credential provider.";
const PARENT_POLICY: &str = "Keep the parent task's repository constraints.";
const SIDE_BOUNDARY: &str = "This is a side chat. Inherited parent history is background context, not an active task. Follow this side chat's current request; do not continue parent work unless the user explicitly asks.";

#[tokio::test]
async fn legacy_fork_instructions_reach_model_and_survive_resume() -> Result<()> {
    assert_fork_instructions_reach_model(ThreadHistoryMode::Legacy).await
}

#[tokio::test]
async fn paginated_fork_instructions_reach_model_and_survive_resume() -> Result<()> {
    assert_fork_instructions_reach_model(ThreadHistoryMode::Paginated).await
}

async fn assert_fork_instructions_reach_model(history_mode: ThreadHistoryMode) -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let start = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            history_mode: Some(history_mode),
            developer_instructions: Some(PARENT_POLICY.to_string()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread: parent, .. } = app.read_response(start).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: parent.id.clone(),
        input: vec![UserInput::Text {
            text: PARENT_TASK.to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;

    let instructions = format!("{PARENT_POLICY}\n\n{SIDE_BOUNDARY}");
    let fork = app
        .send_thread_fork_request(ThreadForkParams {
            thread_id: parent.id.clone(),
            developer_instructions: Some(instructions.clone()),
            exclude_turns: true,
            ..Default::default()
        })
        .await?;
    let ThreadForkResponse { thread: side, .. } = app.read_response(fork).await?;
    for text in ["Hello", "Another side question"] {
        app.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: side.id.clone(),
            input: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    }
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: parent.id.clone(),
        input: vec![UserInput::Text {
            text: "Return to the parent task".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    let nested = app
        .send_thread_fork_request(ThreadForkParams {
            thread_id: side.id.clone(),
            developer_instructions: Some(instructions),
            exclude_turns: true,
            ..Default::default()
        })
        .await?;
    let ThreadForkResponse { thread: nested, .. } = app.read_response(nested).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: nested.id,
        input: vec![UserInput::Text {
            text: "Hello from the nested fork".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;

    // Cold resume must retain the boundary even without resending the fork override.
    drop(app);
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let resume = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: side.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse = app.read_response(resume).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: side.id,
        input: vec![UserInput::Text {
            text: "Hello after reopening".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;

    let requests = server.received_requests().await.expect("model requests");
    let bodies = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .map(|request| request.body_json::<Value>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(bodies.len(), 6);
    for (body, expected_boundary_count) in bodies.iter().zip([0, 1, 1, 0, 1, 1]) {
        let boundary_count = body["input"]
            .as_array()
            .expect("input items")
            .iter()
            .filter(|item| item["role"] == "developer")
            .flat_map(|item| item["content"].as_array().expect("message content"))
            .filter_map(|content| content["text"].as_str())
            .map(|text| text.matches(SIDE_BOUNDARY).count())
            .sum::<usize>();
        assert_eq!(boundary_count, expected_boundary_count);
    }
    let first_side_input = bodies[1]["input"].to_string();
    assert!(first_side_input.contains(PARENT_TASK));
    assert!(first_side_input.contains("Hello"));
    Ok(())
}
