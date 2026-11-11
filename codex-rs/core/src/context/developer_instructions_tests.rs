use super::*;
use pretty_assertions::assert_eq;

#[test]
fn client_instruction_limit_accepts_the_boundary_and_rejects_overflow() {
    let limit = approx_bytes_for_tokens(MAX_DEVELOPER_INSTRUCTIONS_TOKENS);
    let instructions = "a".repeat(limit);
    assert!(validate_developer_instructions(Some(&instructions)).is_ok());
    let oversized = format!("{instructions}x");
    let error = validate_developer_instructions(Some(&oversized)).expect_err("oversized policy");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(!error.to_string().contains(&oversized));
}

#[test]
fn client_instruction_limit_counts_utf8_bytes_not_characters() {
    let instructions =
        "界".repeat(approx_bytes_for_tokens(MAX_DEVELOPER_INSTRUCTIONS_TOKENS) / 3 + 1);
    assert_eq!(
        validate_developer_instructions(Some(&instructions))
            .expect_err("oversized multibyte policy")
            .kind(),
        io::ErrorKind::InvalidInput
    );
}
