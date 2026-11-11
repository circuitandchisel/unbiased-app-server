use super::*;
use crate::context::ModelSwitchInstructions;
use pretty_assertions::assert_eq;

#[test]
fn unchanged_instructions_in_a_mixed_startup_bundle_are_not_duplicated() {
    let history = [build_rendered_message(vec![
        ModelSwitchInstructions::new("model context").render_fragment(),
        DeveloperInstructions::new("client policy").render_fragment(),
    ])
    .expect("startup message")];
    assert_eq!(
        fork_developer_instructions_update(history.iter(), Some("client policy")),
        None
    );
}

#[test]
fn a_fork_compares_against_the_latest_client_policy() {
    let parent = DeveloperInstructions::new("parent policy");
    let child = DeveloperInstructions::new("side policy");
    let history = [
        build_rendered_message(vec![parent.render_fragment()]).expect("parent message"),
        build_rendered_message(vec![child.render_fragment()]).expect("child message"),
    ];
    assert_eq!(
        fork_developer_instructions_update(history.iter(), Some("side policy")),
        None
    );
    assert_eq!(
        fork_developer_instructions_update(history.iter(), Some("parent policy")),
        build_rendered_message(vec![parent.render_fragment()])
    );
}

#[test]
fn a_fork_without_an_instruction_override_does_not_add_context() {
    let history = [build_rendered_message(vec![
        DeveloperInstructions::new("parent policy").render_fragment(),
    ])
    .expect("parent message")];
    assert_eq!(
        fork_developer_instructions_update(history.iter(), None),
        None
    );
    assert_eq!(
        fork_developer_instructions_update(history.iter(), Some("")),
        None
    );
}
