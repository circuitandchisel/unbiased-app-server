use super::*;

const BUDGET: usize = 7 * 1024 * 1024;

fn request(input: Vec<ResponseItem>) -> ResponsesApiRequest {
    ResponsesApiRequest {
        model: "test".to_string(),
        instructions: String::new(),
        input,
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

fn user(content: Vec<ContentItem>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content,
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn image(url: String) -> ContentItem {
    ContentItem::InputImage {
        image: ImageReference::Inline { image_url: url },
        detail: None,
    }
}

fn view_image_output(url: String) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some("view-image".to_string()),
        name: Some("view_image".to_string()),
        namespace: None,
        output: codex_protocol::models::FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline { image_url: url },
                    detail: None,
                },
            ]),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    }
}

fn view_image_call(path: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "view_image".to_string(),
        namespace: None,
        arguments: serde_json::json!({ "path": path }).to_string(),
        encrypted_function_args: None,
        call_id: "view-image".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn existing_local_file() -> String {
    format!(
        "{}/src/endpoint/request_body_limit_tests.rs",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn first_user_content(request: &ResponsesApiRequest, index: usize) -> &ContentItem {
    let ResponseItem::Message { content, .. } = &request.input[0] else {
        panic!("expected user message")
    };
    &content[index]
}

fn tool_output_text(request: &ResponsesApiRequest, index: usize) -> &str {
    let ResponseItem::FunctionCallOutput { output, .. } = &request.input[index] else {
        panic!("expected tool output")
    };
    let FunctionCallOutputBody::ContentItems(items) = &output.body else {
        panic!("expected content items")
    };
    let FunctionCallOutputContentItem::InputText { text } = &items[0] else {
        panic!("expected text replacement")
    };
    text
}

fn tool_image_is_retained(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::FunctionCallOutput { output, .. }
        if matches!(&output.body, FunctionCallOutputBody::ContentItems(items)
            if matches!(&items[0], FunctionCallOutputContentItem::InputImage { .. })))
}

#[test]
fn re_viewed_attachment_is_removed_from_request_copy() {
    let url = format!("data:image/png;base64,{}", "A".repeat(5_235_000));
    let draft_one = format!("data:image/png;base64,{}", "B".repeat(360_000));
    let draft_two = format!("data:image/png;base64,{}", "C".repeat(370_000));
    let mut request = request(vec![
        user(vec![image(url.clone())]),
        view_image_output(draft_one),
        view_image_output(draft_two),
        view_image_output(url),
    ]);
    let (bytes, omitted) = trim_response_request(&mut request, BUDGET).unwrap();

    assert!(bytes <= BUDGET);
    assert_eq!(omitted, 1);
    assert!(
        matches!(&request.input[0], ResponseItem::Message { content, .. }
        if matches!(&content[0], ContentItem::InputImage { .. }))
    );
    assert!(tool_image_is_retained(&request.input[1]));
    assert!(tool_image_is_retained(&request.input[2]));
    assert!(
        matches!(&request.input[3], ResponseItem::FunctionCallOutput { output, .. }
        if matches!(&output.body, FunctionCallOutputBody::ContentItems(items)
            if matches!(&items[0], FunctionCallOutputContentItem::InputText { .. })))
    );
}

#[test]
fn follow_up_after_re_view_does_not_inherit_oversized_request() {
    let url = format!("data:image/png;base64,{}", "A".repeat(5_235_000));
    let mut request = request(vec![
        user(vec![image(url.clone())]),
        view_image_output(url),
        user(vec![ContentItem::InputText {
            text: "Make one final polished image".to_string(),
        }]),
    ]);
    let (bytes, omitted) = trim_response_request(&mut request, BUDGET).unwrap();

    assert!(bytes <= BUDGET);
    assert_eq!(omitted, 1);
    assert!(
        matches!(&request.input[0], ResponseItem::Message { content, .. }
        if matches!(&content[0], ContentItem::InputImage { .. }))
    );
    assert!(
        matches!(&request.input[1], ResponseItem::FunctionCallOutput { output, .. }
        if matches!(&output.body, FunctionCallOutputBody::ContentItems(items)
            if matches!(&items[0], FunctionCallOutputContentItem::InputText { .. })))
    );
}

#[test]
fn current_user_images_are_never_silently_discarded() {
    let url = format!("data:image/png;base64,{}", "A".repeat(5_235_000));
    let mut request = request(vec![user(vec![image(url.clone()), image(url)])]);
    let (bytes, omitted) = trim_response_request(&mut request, BUDGET).unwrap();

    assert!(bytes > BUDGET);
    assert_eq!(omitted, 0);
}

#[test]
fn trimmed_view_image_output_keeps_reopenable_source_path() {
    let path = existing_local_file();
    let mut request = request(vec![
        view_image_call(&path),
        view_image_output(format!("data:image/png;base64,{}", "A".repeat(1000))),
        user(vec![ContentItem::InputText {
            text: "continue".to_string(),
        }]),
    ]);
    assert!(omit_oldest_inline_media(&mut request).is_some());
    let text = tool_output_text(&request, 1);
    assert!(text.contains(&path));
    assert!(text.contains("view_image"));
}

#[test]
fn trimmed_user_image_keeps_path_from_adjacent_tag() {
    let path = existing_local_file();
    let mut request = request(vec![
        user(vec![
            ContentItem::InputText {
                text: format!("<image name=\"[Image #1]\" path=\"{path}\">"),
            },
            image(format!("data:image/png;base64,{}", "A".repeat(1000))),
            ContentItem::InputText {
                text: "</image>".to_string(),
            },
        ]),
        user(vec![ContentItem::InputText {
            text: "continue".to_string(),
        }]),
    ]);
    assert!(omit_oldest_inline_media(&mut request).is_some());
    let ContentItem::InputText { text } = first_user_content(&request, 1) else {
        panic!("expected text replacement")
    };
    assert!(text.contains(&path));
}

#[test]
fn missing_source_is_not_advertised_as_reopenable() {
    let path = "/this/image/does/not/exist.png";
    let mut request = request(vec![
        view_image_call(path),
        view_image_output(format!("data:image/png;base64,{}", "A".repeat(1000))),
        user(vec![ContentItem::InputText {
            text: "continue".to_string(),
        }]),
    ]);
    assert!(omit_oldest_inline_media(&mut request).is_some());
    assert_eq!(tool_output_text(&request, 1), OMITTED_IMAGE);
}
