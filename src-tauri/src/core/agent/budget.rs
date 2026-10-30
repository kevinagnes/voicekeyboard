//! Token budget and compaction helpers.

use super::store::Db;
use super::types::Message;

/// Conservative default when the provider does not expose a model limit.
pub const DEFAULT_CONTEXT_BUDGET: i64 = 120_000;
const RESERVE_FRACTION: f64 = 0.10;

pub fn estimate_messages(messages: &[Message]) -> i64 {
    let mut total = 0i64;
    for message in messages {
        for part in &message.parts {
            total += match part {
                super::types::ContentPart::Text { text } => (text.chars().count() as i64 / 4) + 4,
                super::types::ContentPart::Image { .. } => 1_000,
                super::types::ContentPart::ToolCall { name, input, .. } => {
                    (name.len() as i64 + input.to_string().len() as i64) / 4 + 8
                }
                super::types::ContentPart::ToolResponse { name, output, .. } => {
                    (name.len() as i64 + output.chars().count() as i64) / 4 + 8
                }
            };
        }
    }
    total
}

/// Measured anchor + conservative estimate of everything after it.
pub fn size(db: &Db, session_id: &str) -> i64 {
    let messages = db.messages(session_id);
    let mut measured = 0i64;
    let mut anchor = None;
    for (index, message) in messages.iter().enumerate() {
        if message.role == "assistant"
            && (message.tokens_in > 0 || message.tokens_out > 0)
            && message.completed_at.is_some()
        {
            measured = message.tokens_in
                + message.tokens_out
                + message.tokens_cache_read
                + message.tokens_cache_write;
            anchor = Some(index);
        }
    }
    match anchor {
        Some(index) => {
            measured + ((estimate_messages(&messages[index + 1..]) as f64) * 1.15) as i64
        }
        None => estimate_messages(&messages),
    }
}

pub fn ceiling(budget: i64) -> i64 {
    let window = if budget > 0 {
        budget
    } else {
        DEFAULT_CONTEXT_BUDGET
    };
    let reserve = ((window as f64) * RESERVE_FRACTION) as i64;
    (window - reserve.max(4_000)).max(8_000)
}

pub fn needs_compaction(db: &Db, session_id: &str, budget: i64) -> bool {
    let latest_is_checkpoint = db
        .messages(session_id)
        .last()
        .map(|m| m.kind == "compaction")
        .unwrap_or(false);
    !latest_is_checkpoint && size(db, session_id) >= ceiling(budget)
}

/// Index of the newest user request. Everything before it may be summarised;
/// the newest exchange and its tool results stay verbatim.
pub fn split_point(messages: &[Message]) -> Option<usize> {
    messages
        .iter()
        .rposition(|m| m.role == "user" && (m.kind == "user" || m.kind == "synthetic"))
}

pub fn summary_prompt(previous: Option<&str>, messages: &[Message], tokens: i64) -> String {
    let mut transcript = String::new();
    if let Some(previous) = previous {
        transcript.push_str("Earlier checkpoint:\n");
        transcript.push_str(previous);
        transcript.push_str("\n\nNewer messages:\n");
    }
    for message in messages {
        let who = match message.role.as_str() {
            "assistant" => "assistant",
            "tool" => "tool",
            _ => "user",
        };
        for part in &message.parts {
            match part {
                super::types::ContentPart::Text { text } => {
                    if !text.trim().is_empty() {
                        transcript.push_str(&format!("[{who}] {}\n", text.trim()));
                    }
                }
                super::types::ContentPart::Image { path, .. } => {
                    transcript.push_str(&format!("[image] {path}\n"));
                }
                super::types::ContentPart::ToolCall { name, input, .. } => {
                    transcript.push_str(&format!(
                        "[tool call] {name} {}\n",
                        truncate(&input.to_string(), 400)
                    ));
                }
                super::types::ContentPart::ToolResponse {
                    name,
                    output,
                    is_error,
                    ..
                } => {
                    transcript.push_str(&format!(
                        "[tool {}] {name}: {}\n",
                        if *is_error { "error" } else { "result" },
                        truncate(output, 400)
                    ));
                }
            }
        }
    }
    format!(
        "You are compacting an assistant's working memory. Produce a checkpoint that \
         lets the assistant continue without the original messages. Preserve:\n\
         - Objective (the user's request, verbatim if short)\n\
         - Decisions made and why\n\
         - Work state (what is done, what is in progress)\n\
         - Next move (the immediate next action)\n\
         - Relevant files/artifacts (exact paths)\n\
         - Important context (constraints, preferences, failures and their causes)\n\
         Do not invent. Do not include tool output verbatim. Keep it under {tokens} tokens.\n\n\
         Transcript:\n{transcript}"
    )
}

pub fn output_cap(room: i64, requested: u32) -> u32 {
    let room = room.max(1_024) as u32;
    requested.min(room).min(256_000)
}

pub fn truncate(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent::types::{ContentPart, Message};

    fn message(role: &str, kind: &str, text: &str, seq: i64) -> Message {
        Message {
            id: format!("m{seq}"),
            session_id: "s".into(),
            seq,
            role: role.into(),
            kind: kind.into(),
            parts: vec![ContentPart::text(text)],
            completed_at: Some(1),
            ..Default::default()
        }
    }

    #[test]
    fn estimates_chars_as_tokens() {
        let messages = vec![message("user", "user", &"a".repeat(400), 1)];
        assert!(estimate_messages(&messages) >= 100);
    }

    #[test]
    fn split_keeps_the_newest_user_turn() {
        let messages = vec![
            message("user", "user", "old", 1),
            message("assistant", "assistant", "reply", 2),
            message("user", "user", "new", 3),
        ];
        assert_eq!(split_point(&messages), Some(2));
    }

    #[test]
    fn ceiling_leaves_a_reserve() {
        assert!(ceiling(100_000) < 100_000);
        assert!(ceiling(0) > 8_000);
    }

    #[test]
    fn output_cap_never_exceeds_room() {
        assert_eq!(output_cap(500, 2_000), 1_024);
        assert_eq!(output_cap(50_000, 2_000), 2_000);
    }
}
