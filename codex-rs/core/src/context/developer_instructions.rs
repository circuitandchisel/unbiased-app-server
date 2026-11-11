use super::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;
use codex_utils_string::approx_bytes_for_tokens;
use std::io;

const MAX_DEVELOPER_INSTRUCTIONS_TOKENS: usize = 10_000;

/// Reject oversized client policy before starting a session rather than truncating constraints.
pub(crate) fn validate_developer_instructions(instructions: Option<&str>) -> io::Result<()> {
    let max_bytes = approx_bytes_for_tokens(MAX_DEVELOPER_INSTRUCTIONS_TOKENS);
    if instructions.is_some_and(|instructions| instructions.len() > max_bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "`developer_instructions` exceeds the model-context limit of {MAX_DEVELOPER_INSTRUCTIONS_TOKENS} estimated tokens ({max_bytes} UTF-8 bytes); shorten the instructions without dropping required constraints"
            ),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeveloperInstructions {
    instructions: String,
}

impl DeveloperInstructions {
    pub(crate) fn new(instructions: impl Into<String>) -> Self {
        Self {
            instructions: instructions.into(),
        }
    }
}

impl ContextualUserFragment for DeveloperInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("generic.developer_instructions".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.instructions.clone()
    }
}

#[cfg(test)]
#[path = "developer_instructions_tests.rs"]
mod tests;
