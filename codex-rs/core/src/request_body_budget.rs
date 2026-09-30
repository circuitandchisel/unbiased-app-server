use crate::session::TurnInput;
use codex_history::ResponseItemEnvelope;
use codex_protocol::user_input::UserInput;

pub(crate) const UNBIASED_REQUEST_BUDGET_BYTES: usize = 7 * 1024 * 1024;
const REQUEST_OVERHEAD_BYTES: usize = 256 * 1024;

pub(crate) fn estimate_turn_bytes(
    history: &[ResponseItemEnvelope],
    pending: &[TurnInput],
) -> Result<(usize, usize), serde_json::Error> {
    let history_bytes = history
        .iter()
        .try_fold(REQUEST_OVERHEAD_BYTES, |total, item| {
            serde_json::to_vec(&item.item).map(|bytes| total.saturating_add(bytes.len() + 1))
        })?;
    let pending_bytes = pending.iter().try_fold(0usize, |total, item| {
        let serialized = serde_json::to_vec(item)?.len();
        let media = match item {
            TurnInput::UserInput { content, .. } => content.iter().fold(0usize, |sum, input| {
                let bytes = match input {
                    UserInput::LocalImage { path, .. } | UserInput::LocalAudio { path } => {
                        std::fs::metadata(path).map_or(0, |file| file.len() as usize)
                    }
                    _ => 0,
                };
                let encoded = bytes.saturating_add(2) / 3 * 4;
                sum.saturating_add(encoded)
            }),
            _ => 0,
        };
        Ok::<usize, serde_json::Error>(total.saturating_add(serialized).saturating_add(media))
    })?;
    Ok((history_bytes.saturating_add(pending_bytes), pending_bytes))
}

#[cfg(test)]
#[path = "request_body_budget_tests.rs"]
mod tests;
