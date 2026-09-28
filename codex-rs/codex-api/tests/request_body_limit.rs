// Copyright 2026 Circuit & Chisel. Licensed under the Apache License, Version 2.0.
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use codex_api::ApiError;
use codex_api::AuthProvider;
use codex_api::Provider;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesClient;
use codex_api::ResponsesOptions;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::RequestBody;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use http::HeaderMap;
use http::StatusCode;
use serde_json::Value;

#[derive(Clone, Default)]
struct NoAuth;

impl AuthProvider for NoAuth {
    fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
}

#[derive(Clone, Default)]
struct CaptureTransport {
    requests: Arc<Mutex<Vec<Request>>>,
    reject_first: bool,
    reject_all: bool,
}

impl CaptureTransport {
    fn bodies(&self) -> Vec<Value> {
        self.requests
            .lock()
            .expect("requests mutex")
            .iter()
            .map(|request| {
                let Some(RequestBody::EncodedJson(body)) = request.body.as_ref() else {
                    panic!("expected encoded JSON body");
                };
                serde_json::from_slice(body.as_bytes()).expect("valid JSON")
            })
            .collect()
    }
}

impl HttpTransport for CaptureTransport {
    async fn execute(&self, _request: Request) -> Result<Response, TransportError> {
        Err(TransportError::Build("unexpected execute".to_string()))
    }

    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        let mut requests = self.requests.lock().expect("requests mutex");
        requests.push(request);
        if self.reject_all || self.reject_first && requests.len() == 1 {
            return Err(TransportError::Http {
                status: StatusCode::PAYLOAD_TOO_LARGE,
                url: None,
                headers: None,
                body: None,
                retry_after: None,
            });
        }
        Ok(StreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            bytes: Box::pin(futures::stream::iter(
                Vec::<Result<Bytes, TransportError>>::new(),
            )),
        })
    }
}

fn provider() -> Provider {
    Provider {
        name: "pareto".to_string(),
        base_url: "https://example.com/v1".to_string(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: codex_api::RetryConfig {
            max_attempts: 1,
            base_delay: Duration::from_millis(1),
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_millis(10),
    }
}

fn user_message(content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn request_with_history() -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "test".to_string(),
        instructions: "Keep the current question".to_string(),
        input: vec![
            user_message(vec![
                ContentItem::InputText {
                    text: "Earlier note".to_string(),
                },
                ContentItem::InputImage {
                    image: ImageReference::Inline {
                        image_url: format!("data:image/png;base64,{}", "A".repeat(2000)),
                    },
                    detail: None,
                },
            ]),
            user_message(vec![ContentItem::InputText {
                text: "Current question".to_string(),
            }]),
        ],
        tools: None,
        tool_choice: "auto".to_string(),
        parallel_tool_calls: false,
        reasoning: None,
        store: false,
        stream: true,
        stream_options: None,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        access_programs: None,
    }
}

#[tokio::test]
async fn trims_older_inline_image_from_wire_but_preserves_current_message() -> Result<()> {
    let transport = CaptureTransport::default();
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth))
        .with_max_body_bytes(1000);
    let request = request_with_history();
    let original = serde_json::to_value(&request)?;

    client
        .stream_request(request, ResponsesOptions::default())
        .await?;

    let bodies = transport.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["input"][1], original["input"][1]);
    assert_eq!(
        bodies[0]["input"][0]["content"][0],
        original["input"][0]["content"][0]
    );
    assert!(
        bodies[0]["input"][0]["content"][1]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Earlier image omitted"))
    );
    Ok(())
}

#[tokio::test]
async fn latest_image_is_not_trimmed_or_sent_when_too_large() -> Result<()> {
    let transport = CaptureTransport::default();
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth))
        .with_max_body_bytes(1000);
    let mut request = request_with_history();
    request.input = vec![request.input.remove(0)];

    let error = client
        .stream_request(request, ResponsesOptions::default())
        .await
        .err()
        .expect("oversized current message should fail");

    assert!(
        matches!(error, ApiError::InvalidRequest { message } if message.contains("no older inline media"))
    );
    assert!(transport.bodies().is_empty());
    Ok(())
}

#[tokio::test]
async fn does_not_replace_small_media_with_a_larger_placeholder() -> Result<()> {
    let transport = CaptureTransport::default();
    let mut request = request_with_history();
    let ResponseItem::Message { content, .. } = &mut request.input[0] else {
        panic!("expected earlier user message");
    };
    content[1] = ContentItem::InputImage {
        image: ImageReference::Inline {
            image_url: "data:image/png;base64,A".to_string(),
        },
        detail: None,
    };
    let max_body_bytes = serde_json::to_vec(&request)?.len() - 1;
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth))
        .with_max_body_bytes(max_body_bytes);

    let error = client
        .stream_request(request, ResponsesOptions::default())
        .await
        .err()
        .expect("request should remain over limit");

    assert!(matches!(error, ApiError::InvalidRequest { .. }));
    assert!(transport.bodies().is_empty());
    Ok(())
}

#[tokio::test]
async fn trims_older_inline_audio() -> Result<()> {
    let transport = CaptureTransport::default();
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth))
        .with_max_body_bytes(1000);
    let mut request = request_with_history();
    let ResponseItem::Message { content, .. } = &mut request.input[0] else {
        panic!("expected earlier user message");
    };
    content[1] = ContentItem::InputAudio {
        audio_url: format!("data:audio/wav;base64,{}", "A".repeat(2000)),
    };

    client
        .stream_request(request, ResponsesOptions::default())
        .await?;

    let bodies = transport.bodies();
    assert!(
        bodies[0]["input"][0]["content"][1]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Earlier audio omitted"))
    );
    Ok(())
}

#[tokio::test]
async fn retries_413_once_after_omitting_older_media() -> Result<()> {
    let transport = CaptureTransport {
        reject_first: true,
        ..Default::default()
    };
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth))
        .with_max_body_bytes(4000);

    client
        .stream_request(request_with_history(), ResponsesOptions::default())
        .await?;

    let bodies = transport.bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["input"][1], bodies[1]["input"][1]);
    assert!(
        bodies[0]["input"][0]["content"][1]["image_url"]
            .as_str()
            .is_some()
    );
    assert!(
        bodies[1]["input"][0]["content"][1]["text"]
            .as_str()
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn stops_after_one_413_retry() -> Result<()> {
    let transport = CaptureTransport {
        reject_all: true,
        ..Default::default()
    };
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth))
        .with_max_body_bytes(4000);

    let error = client
        .stream_request(request_with_history(), ResponsesOptions::default())
        .await
        .err()
        .expect("second 413 should fail");

    assert!(matches!(
        error,
        ApiError::Transport(TransportError::Http {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            ..
        })
    ));
    assert_eq!(transport.bodies().len(), 2);
    Ok(())
}

#[tokio::test]
async fn does_not_retry_413_without_older_media() -> Result<()> {
    let transport = CaptureTransport {
        reject_all: true,
        ..Default::default()
    };
    let client = ResponsesClient::new(transport.clone(), provider(), Arc::new(NoAuth))
        .with_max_body_bytes(4000);
    let mut request = request_with_history();
    request.input = vec![request.input.remove(1)];

    let error = client
        .stream_request(request, ResponsesOptions::default())
        .await
        .err()
        .expect("413 without older media should fail");

    assert!(matches!(
        error,
        ApiError::Transport(TransportError::Http {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            ..
        })
    ));
    assert_eq!(transport.bodies().len(), 1);
    Ok(())
}
