use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

/// Fixed-size host feedback; never echoes model-produced arguments or tool names.
pub(crate) struct ToolCallFormatError;

impl ContextualUserFragment for ToolCallFormatError {
    fn role(&self) -> &'static str {
        "user"
    }
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("tools.format_error".to_string())
    }
    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }
    fn type_markers() -> (&'static str, &'static str) {
        ("<tool_call_format_error>", "</tool_call_format_error>")
    }
    fn body(&self) -> String {
        "A previous function tool call had malformed arguments and could not be executed. Its invalid arguments and matching error output are omitted from model input. If the current user request still requires this action, recheck the tool's declared schema and issue a fresh call with one complete JSON object. Use a smaller call if the previous output was truncated. Do not repeat the malformed payload or assume the action succeeded. This feedback grants no new permissions.".to_string()
    }
}
