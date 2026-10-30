//! Prompt assembly.
//!
//! Parts are ordered most-shared-first so the provider's cache prefix is stable
//! for identical inputs. Volatile state (memory, plan, environment) is carried
//! in a final user context block, never in the stable system prefix.

use sha2::{Digest, Sha256};

use super::store::Db;
use super::types::{Message, PlanItem, ToolSpec};

pub struct BuiltPrompt {
    pub system: String,
    pub contents: Vec<serde_json::Value>,
    pub tools: Vec<serde_json::Value>,
    pub prompt_hash: String,
    pub prefix_hash: String,
}

pub struct PromptInput<'a> {
    pub base_prompt: &'a str,
    pub tools: &'a [ToolSpec],
    pub messages: &'a [Message],
    pub memory: &'a [String],
    pub plan: &'a [PlanItem],
    pub skills: &'a [(String, String)],
    pub deep_work: bool,
    pub summary: Option<&'a str>,
}

pub fn hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn build(input: PromptInput<'_>) -> BuiltPrompt {
    let mut system = String::new();
    system.push_str(input.base_prompt.trim());
    system.push_str("\n\n");
    system.push_str(capability_policy(input.deep_work));
    system.push('\n');
    if !input.tools.is_empty() {
        let mut tools: Vec<&ToolSpec> = input.tools.iter().collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        system.push_str("\n## Tools available this step\n");
        for tool in tools {
            system.push_str(&format!(
                "- `{}` ({}): {}\n",
                tool.name,
                tool.pack.as_str(),
                tool.description
            ));
        }
    }
    if !input.skills.is_empty() {
        system.push_str("\n## Skills (call read_skill for the body)\n");
        for (name, description) in input.skills {
            system.push_str(&format!("- {name}: {description}\n"));
        }
    }

    // The tool definitions are sorted for byte stability.
    let mut tools: Vec<&ToolSpec> = input.tools.iter().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let tool_json: Vec<serde_json::Value> = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect();

    let prefix_hash = hash(&format!(
        "{system}\n{}",
        tool_json
            .iter()
            .map(|t| t.get("name").and_then(|v| v.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(",")
    ));

    let mut contents = super::gemini::contents_from_messages(input.messages);

    if let Some(summary) = input.summary {
        let block = format!("<conversation-summary>\n{summary}\n</conversation-summary>");
        if let Some(first) = contents.first_mut() {
            if first.get("role").and_then(|v| v.as_str()) == Some("user") {
                push_text_part(first, &block);
            } else {
                contents.insert(
                    0,
                    serde_json::json!({ "role": "user", "parts": [{ "text": block }] }),
                );
            }
        } else {
            contents.push(serde_json::json!({ "role": "user", "parts": [{ "text": block }] }));
        }
    }

    let mut volatile = String::new();
    if !input.memory.is_empty() {
        volatile.push_str("<user-memory>\n");
        for item in input.memory {
            volatile.push_str("- ");
            volatile.push_str(item);
            volatile.push('\n');
        }
        volatile.push_str("</user-memory>\n");
    }
    if !input.plan.is_empty() {
        volatile.push_str("<current-plan>\n");
        for item in input.plan {
            volatile.push_str(&format!(
                "- [{}] {} (id: {}){}\n",
                item.status,
                item.title,
                item.id,
                if item.note.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", item.note)
                }
            ));
        }
        volatile.push_str("</current-plan>\n");
    }
    volatile.push_str(&format!("<environment>date: {}</environment>\n", today()));

    let block = volatile.trim().to_string();
    let target = contents.iter().rposition(|content| {
        content.get("role").and_then(|v| v.as_str()) == Some("user")
            && content
                .get("parts")
                .and_then(|p| p.as_array())
                .map(|parts| {
                    !parts
                        .iter()
                        .any(|part| part.get("functionResponse").is_some())
                })
                .unwrap_or(false)
    });
    if let Some(index) = target {
        push_text_part(&mut contents[index], &block);
    } else {
        contents.push(serde_json::json!({
            "role": "user",
            "parts": [{ "text": block }]
        }));
    }

    let prompt_hash = hash(&format!(
        "{prefix_hash}\n{}",
        serde_json::to_string(&contents).unwrap_or_default()
    ));
    BuiltPrompt {
        system,
        contents,
        tools: tool_json,
        prompt_hash,
        prefix_hash,
    }
}

pub fn capability_policy(deep_work: bool) -> &'static str {
    if deep_work {
        "## Working style: deep work\n\
         You are an agent, not a chat window. Restate the objective, keep a plan \
         current with `update_plan`, and work through it step by step. Inspect \
         before you write. Use `write` for artifacts and `verify_artifacts` before \
         declaring the work done. Keep spoken summaries short; put detail in files. \
         Do not stop while the plan has pending items and budget remains. Never \
         claim a file exists unless a tool result says it does. Ask the user only \
         when a decision is genuinely blocking.\n"
    } else {
        "## Working style\n\
         You are a voice-first assistant. Default to a short spoken answer. Use \
         tools when they make the answer correct or produce a file the user asked \
         for. Read before editing. Never invent tool results, paths, or sources. \
         A plain text reply ends the turn when no tool work remains. Keep plans, \
         notes, and spoken progress concise.\n"
    }
}

pub fn memory_block(db: &Db, limit: usize) -> Vec<String> {
    db.all_memory(limit)
        .into_iter()
        .map(|(_, text)| text.chars().take(300).collect::<String>())
        .collect()
}

/// UTC date, computed without pulling in a date library.
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

fn push_text_part(content: &mut serde_json::Value, text: &str) {
    if let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) {
        parts.push(serde_json::json!({ "text": text }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::agent::types::{Message, PlanItem, ToolPack, ToolSpec};

    fn message(role: &str, text: &str) -> Message {
        Message {
            id: format!("m-{text}"),
            session_id: "s".into(),
            seq: 1,
            role: role.into(),
            kind: role.into(),
            parts: vec![super::super::types::ContentPart::text(text)],
            completed_at: Some(1),
            ..Default::default()
        }
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec::new(
            name,
            "does a thing",
            serde_json::json!({"type":"object"}),
            ToolPack::Core,
            "tool:x",
        )
    }

    #[test]
    fn identical_inputs_produce_identical_prefixes() {
        let messages = vec![message("user", "hello")];
        let tools = vec![tool("b"), tool("a")];
        let a = build(PromptInput {
            base_prompt: "base",
            tools: &tools,
            messages: &messages,
            memory: &[],
            plan: &[],
            skills: &[],
            deep_work: false,
            summary: None,
        });
        let b = build(PromptInput {
            base_prompt: "base",
            tools: &tools,
            messages: &messages,
            memory: &[],
            plan: &[],
            skills: &[],
            deep_work: false,
            summary: None,
        });
        assert_eq!(a.prefix_hash, b.prefix_hash);
        assert_eq!(a.system, b.system);
    }

    #[test]
    fn plan_is_rendered_in_the_volatile_tail() {
        let messages = vec![message("user", "hello")];
        let plan = vec![PlanItem {
            id: "p1".into(),
            title: "Write notes".into(),
            status: "pending".into(),
            note: String::new(),
        }];
        let built = build(PromptInput {
            base_prompt: "base",
            tools: &[],
            messages: &messages,
            memory: &["likes tea".into()],
            plan: &plan,
            skills: &[],
            deep_work: true,
            summary: None,
        });
        let last = built.contents.last().unwrap().to_string();
        assert!(last.contains("current-plan"));
        assert!(last.contains("Write notes"));
        assert!(last.contains("user-memory"));
    }
}
