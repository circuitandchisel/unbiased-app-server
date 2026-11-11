use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadHistoryMode;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tempfile::TempDir;

const BOUNDARY: &str =
    "Side chat: inherited parent tasks are background only. Follow the current side request.";

enum ResumeKind {
    Cold,
    UnsubscribedIdle,
}

#[tokio::test]
async fn legacy_resume_repairs_a_pre_upgrade_side_chat() -> Result<()> {
    assert_resume_repairs_missing_instructions(ThreadHistoryMode::Legacy, ResumeKind::Cold).await
}

#[tokio::test]
async fn paginated_resume_repairs_a_pre_upgrade_side_chat() -> Result<()> {
    assert_resume_repairs_missing_instructions(ThreadHistoryMode::Paginated, ResumeKind::Cold).await
}

#[tokio::test]
async fn legacy_idle_resume_applies_instructions_after_unsubscribe() -> Result<()> {
    assert_resume_repairs_missing_instructions(
        ThreadHistoryMode::Legacy,
        ResumeKind::UnsubscribedIdle,
    )
    .await
}

#[tokio::test]
async fn paginated_idle_resume_applies_instructions_after_unsubscribe() -> Result<()> {
    assert_resume_repairs_missing_instructions(
        ThreadHistoryMode::Paginated,
        ResumeKind::UnsubscribedIdle,
    )
    .await
}

async fn assert_resume_repairs_missing_instructions(
    history_mode: ThreadHistoryMode,
    resume_kind: ResumeKind,
) -> Result<()> {
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
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread: parent, .. } = app.read_response(start).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: parent.id.clone(),
        input: vec![UserInput::Text {
            text: "Unfinished parent task marker".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    // This models an old side chat whose rollout never received its boundary.
    let fork = app
        .send_thread_fork_request(ThreadForkParams {
            thread_id: parent.id,
            exclude_turns: true,
            ..Default::default()
        })
        .await?;
    let ThreadForkResponse { thread: side, .. } = app.read_response(fork).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: side.id.clone(),
        input: vec![UserInput::Text {
            text: "Hello before upgrading".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    match resume_kind {
        ResumeKind::Cold => {
            drop(app);
            app = TestAppServer::builder()
                .with_codex_home(home.path())
                .without_managed_config()
                .build_initialized()
                .await?;
        }
        ResumeKind::UnsubscribedIdle => {
            let unsubscribe = app
                .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
                    thread_id: side.id.clone(),
                })
                .await?;
            let _: ThreadUnsubscribeResponse = app.read_response(unsubscribe).await?;
        }
    }
    let resume = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: side.id.clone(),
            developer_instructions: Some(BOUNDARY.to_string()),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse = app.read_response(resume).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: side.id.clone(),
        input: vec![UserInput::Text {
            text: "Hello after upgrading".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;

    // The repaired boundary must be durable, rather than reinserted on every reopen.
    drop(app);
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let resume = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: side.id.clone(),
            developer_instructions: Some(BOUNDARY.to_string()),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse = app.read_response(resume).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: side.id,
        input: vec![UserInput::Text {
            text: "Hello after another reopen".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    let requests = server.received_requests().await.expect("model requests");
    let bodies = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .map(wiremock::Request::body_json::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(bodies.len(), 4);
    for (body, expected) in bodies.iter().zip([0, 0, 1, 1]) {
        let count = body["input"]
            .as_array()
            .expect("input items")
            .iter()
            .filter(|item| item["role"] == "developer")
            .flat_map(|item| item["content"].as_array().expect("message content"))
            .filter_map(|content| content["text"].as_str())
            .map(|text| text.matches(BOUNDARY).count())
            .sum::<usize>();
        assert_eq!(count, expected);
    }
    let repaired_input = bodies[2]["input"].to_string();
    assert!(repaired_input.contains("Unfinished parent task marker"));
    assert!(repaired_input.contains("Hello before upgrading"));
    Ok(())
}

#[tokio::test]
async fn oversized_client_instructions_are_rejected_before_inference() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let oversized = format!("PRIVATE_POLICY_MARKER{}", "word ".repeat(20_000));
    let rejected_start = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            developer_instructions: Some(oversized.clone()),
            ..Default::default()
        })
        .await?;
    let error = app
        .read_stream_until_error_message(RequestId::Integer(rejected_start))
        .await?;
    assert!(error.error.message.contains("10000 estimated tokens"));
    assert!(!error.error.message.contains("PRIVATE_POLICY_MARKER"));
    let start = app
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread: parent, .. } = app.read_response(start).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: parent.id.clone(),
        input: vec![UserInput::Text {
            text: "Parent history survives rejected overrides".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    let fork = app
        .send_thread_fork_request(ThreadForkParams {
            thread_id: parent.id.clone(),
            developer_instructions: Some(oversized.clone()),
            ..Default::default()
        })
        .await?;
    let error = app
        .read_stream_until_error_message(RequestId::Integer(fork))
        .await?;
    assert!(error.error.message.contains("10000 estimated tokens"));
    assert!(!error.error.message.contains("PRIVATE_POLICY_MARKER"));
    let unsubscribe = app
        .send_thread_unsubscribe_request(ThreadUnsubscribeParams {
            thread_id: parent.id.clone(),
        })
        .await?;
    let _: ThreadUnsubscribeResponse = app.read_response(unsubscribe).await?;
    let resume = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: parent.id.clone(),
            developer_instructions: Some(oversized),
            ..Default::default()
        })
        .await?;
    let error = app
        .read_stream_until_error_message(RequestId::Integer(resume))
        .await?;
    assert!(error.error.message.contains("10000 estimated tokens"));
    assert!(!error.error.message.contains("PRIVATE_POLICY_MARKER"));
    let resume = app
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: parent.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse = app.read_response(resume).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: parent.id,
        input: vec![UserInput::Text {
            text: "Continue the intact parent".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    let requests = server.received_requests().await.expect("model requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/responses"))
            .count(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn client_instruction_byte_limit_is_preserved_without_truncation() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri()).write(home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let tail = "REQUIRED_TAIL_CONSTRAINT";
    let policy = format!("{}{tail}", "a".repeat(40_000 - tail.len()));
    let start = app
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            developer_instructions: Some(policy.clone()),
            ..Default::default()
        })
        .await?;
    let ThreadStartResponse { thread, .. } = app.read_response(start).await?;
    app.start_turn_and_wait_for_completion(TurnStartParams {
        thread_id: thread.id,
        input: vec![UserInput::Text {
            text: "Hello at the limit".to_string(),
            text_elements: Vec::new(),
        }],
        ..Default::default()
    })
    .await?;
    let requests = server.received_requests().await.expect("model requests");
    let body = requests
        .iter()
        .find(|request| request.url.path().ends_with("/responses"))
        .expect("response request")
        .body_json::<Value>()?;
    let actual = body["input"]
        .as_array()
        .expect("input items")
        .iter()
        .filter(|item| item["role"] == "developer")
        .flat_map(|item| item["content"].as_array().expect("message content"))
        .filter_map(|content| content["text"].as_str())
        .find(|text| text.ends_with(tail))
        .expect("complete policy");
    assert_eq!(actual, policy);
    Ok(())
}
