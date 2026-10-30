//! Out-of-band permission rules.
//!
//! The model never writes its own confirmation text. The runtime evaluates
//! `{action, resource}`, renders the prompt from the real arguments, and only
//! then asks the user.

use serde_json::Value;

use super::store::PermissionRow;
use super::types::{Effect, PermissionRule};

/// Built-in defaults: reads allow, writes ask, destructive actions ask.
pub fn default_effect(action: &str) -> Effect {
    match action {
        "file.read" | "web.search" | "web.fetch" | "memory.read" | "clipboard.read" => {
            Effect::Allow
        }
        "file.write" | "file.delete" | "clipboard.write" | "desktop.paste" | "desktop.open" => {
            Effect::Ask
        }
        "memory.write" => Effect::Allow,
        "memory.forget" | "skill.write" | "agent.task" => Effect::Ask,
        "tool:get_time" | "tool:update_plan" | "tool:list_artifacts" | "tool:verify_artifacts" => {
            Effect::Allow
        }
        _ => Effect::Ask,
    }
}

pub fn evaluate(action: &str, resource: &str, rules: &[PermissionRule]) -> Effect {
    let mut effect = default_effect(action);
    for rule in rules {
        if wildcard(&rule.action, action) && wildcard(&rule.resource, resource) {
            effect = rule.effect;
        }
    }
    effect
}

pub fn from_rows(rows: &[PermissionRow]) -> Vec<PermissionRule> {
    rows.iter()
        .map(|row| PermissionRule {
            action: row.action.clone(),
            resource: row.resource.clone(),
            effect: Effect::parse(&row.effect),
            scope: row.scope.clone(),
        })
        .collect()
}

/// `*` matches any run of characters, `?` matches one.
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub fn resource_for(action: &str, tool: &str, input: &Value) -> String {
    match action {
        "file.read" | "file.write" | "file.delete" => input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("workspace")
            .to_string(),
        "web.fetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("web")
            .to_string(),
        "web.search" => "web".into(),
        "clipboard.read" | "clipboard.write" => "clipboard".into(),
        "desktop.paste" => "focused-field".into(),
        "desktop.open" => input
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("browser")
            .to_string(),
        "memory.write" => "memory".into(),
        "memory.forget" => "memory".into(),
        "skill.write" => input
            .get("name")
            .and_then(|v| v.as_str())
            .map(|n| format!("skill:{n}"))
            .unwrap_or_else(|| "skill".into()),
        "agent.task" => "subagent".into(),
        _ => tool.to_string(),
    }
}

pub fn prompt(tool: &str, action: &str, resource: &str, input: &Value) -> (String, String) {
    let pretty = serde_json::to_string_pretty(input).unwrap_or_else(|_| "{}".into());
    let (title, detail) = match action {
        "file.write" => (
            "Write a file".to_string(),
            format!("The assistant wants to write `{resource}`.\n\nArguments:\n{pretty}"),
        ),
        "file.delete" => (
            "Delete a file".to_string(),
            format!("The assistant wants to delete `{resource}`.\n\nThis cannot be undone."),
        ),
        "desktop.paste" => (
            "Paste into the focused app".to_string(),
            format!("The assistant wants to paste text into the focused field.\n\n{pretty}"),
        ),
        "desktop.open" => (
            "Open a link".to_string(),
            format!("The assistant wants to open `{resource}` in your browser."),
        ),
        "clipboard.read" => (
            "Read the clipboard".to_string(),
            "The assistant wants to read the current clipboard text.".to_string(),
        ),
        "memory.forget" => (
            "Delete a memory".to_string(),
            format!("The assistant wants to delete a stored memory.\n\n{pretty}"),
        ),
        "skill.write" => (
            "Save a skill".to_string(),
            format!("The assistant wants to save a skill file for `{resource}`."),
        ),
        "agent.task" => (
            "Delegate to a subagent".to_string(),
            format!("The assistant wants to start a child agent.\n\n{pretty}"),
        ),
        _ => (
            format!("Allow `{tool}`?"),
            format!("Action `{action}` on `{resource}`.\n\nArguments:\n{pretty}"),
        ),
    };
    (title, detail)
}

/// Resource patterns worth saving for "Always allow"; a specific path is
/// session-scoped and a folder grant is explicitly not offered yet.
pub fn always_resource(action: &str, resource: &str) -> Option<String> {
    match action {
        "file.write" | "file.delete" | "file.read" => {
            let path = std::path::Path::new(resource);
            path.parent()
                .map(|parent| format!("{}/*", parent.to_string_lossy().replace('\\', "/")))
                .filter(|p| p != "/*")
        }
        "web.fetch" => url::Url::parse(resource)
            .ok()
            .and_then(|u| u.host_str().map(|h| format!("{h}/*"))),
        "desktop.open" => Some("browser".into()),
        "clipboard.read" | "clipboard.write" | "desktop.paste" | "memory.write"
        | "memory.forget" => Some(resource.to_string()),
        "agent.task" | "web.search" => Some("*".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matches_paths_and_commands() {
        assert!(wildcard("workspace/*", "workspace/notes/a.md"));
        assert!(wildcard("*.md", "notes/a.md"));
        assert!(wildcard("file.???", "file.txt"));
        assert!(!wildcard("workspace/*", "user/notes/a.md"));
    }

    #[test]
    fn last_matching_rule_wins() {
        let rules = vec![
            PermissionRule {
                action: "file.write".into(),
                resource: "*".into(),
                effect: Effect::Allow,
                scope: "global".into(),
            },
            PermissionRule {
                action: "file.write".into(),
                resource: "secrets/*".into(),
                effect: Effect::Deny,
                scope: "global".into(),
            },
        ];
        assert_eq!(evaluate("file.write", "notes/a", &rules), Effect::Allow);
        assert_eq!(evaluate("file.write", "secrets/key", &rules), Effect::Deny);
    }

    #[test]
    fn defaults_allow_reads_and_ask_writes() {
        assert_eq!(evaluate("file.read", "a.txt", &[]), Effect::Allow);
        assert_eq!(evaluate("file.write", "a.txt", &[]), Effect::Ask);
        assert_eq!(evaluate("file.delete", "a.txt", &[]), Effect::Ask);
    }

    #[test]
    fn resources_are_derived_from_arguments() {
        let input = serde_json::json!({"path": "notes/a.md"});
        assert_eq!(resource_for("file.write", "write", &input), "notes/a.md");
        let url = serde_json::json!({"url": "https://example.com/x"});
        assert_eq!(
            resource_for("web.fetch", "web_fetch", &url),
            "https://example.com/x"
        );
    }
}
