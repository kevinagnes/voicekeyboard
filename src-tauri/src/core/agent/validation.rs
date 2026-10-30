//! Conservative JSON-Schema validation and repair.
//!
//! Only unambiguous mistakes are repaired (stringified object root, numeric
//! strings, boolean strings). Everything else becomes a model-readable error
//! naming the path, the JSON sent, and the next action.

use serde_json::Value;

pub fn repair(schema: &Value, input: Value) -> Value {
    let mut value = input;
    if schema.get("type").and_then(|t| t.as_str()) == Some("object") && value.is_string() {
        if let Some(text) = value.as_str() {
            if let Ok(parsed) = serde_json::from_str::<Value>(text.trim()) {
                if parsed.is_object() {
                    value = parsed;
                }
            }
        }
    }
    repair_value(schema, &mut value, 0);
    value
}

fn repair_value(schema: &Value, value: &mut Value, depth: u32) {
    if depth > 8 {
        return;
    }
    let kind = schema.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "integer" => {
            if let Some(text) = value.as_str() {
                if let Ok(number) = text.trim().parse::<i64>() {
                    *value = Value::from(number);
                } else if let Ok(number) = text.trim().parse::<f64>() {
                    if number.fract() == 0.0 {
                        *value = Value::from(number as i64);
                    }
                }
            }
        }
        "number" => {
            if let Some(text) = value.as_str() {
                if let Ok(number) = text.trim().parse::<f64>() {
                    if let Some(number) = serde_json::Number::from_f64(number) {
                        *value = Value::Number(number);
                    }
                }
            }
        }
        "boolean" => {
            if let Some(text) = value.as_str() {
                match text.trim().to_lowercase().as_str() {
                    "true" | "yes" | "1" => *value = Value::Bool(true),
                    "false" | "no" | "0" => *value = Value::Bool(false),
                    _ => {}
                }
            }
        }
        "object" => {
            if let Value::Object(object) = value {
                if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
                    for (key, child) in properties {
                        if let Some(field) = object.get_mut(key) {
                            repair_value(child, field, depth + 1);
                        }
                    }
                }
            }
        }
        "array" => {
            if let Value::Array(items) = value {
                if let Some(item_schema) = schema.get("items") {
                    for item in items {
                        repair_value(item_schema, item, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn validate(schema: &Value, input: &Value) -> Vec<String> {
    let mut issues = Vec::new();
    validate_value(schema, input, "arguments", &mut issues);
    issues.truncate(5);
    issues
}

fn validate_value(schema: &Value, input: &Value, path: &str, issues: &mut Vec<String>) {
    if issues.len() >= 5 {
        return;
    }
    if let Some(kind) = schema.get("type").and_then(|t| t.as_str()) {
        let ok = match kind {
            "object" => input.is_object(),
            "array" => input.is_array(),
            "string" => input.is_string(),
            "boolean" => input.is_boolean(),
            "number" => input.is_number(),
            "integer" => input.as_i64().is_some() || input.as_u64().is_some(),
            _ => true,
        };
        if !ok {
            issues.push(format!("{path}: expected {kind}, got {}", json_type(input)));
            return;
        }
    }
    if let Some(values) = schema.get("enum").and_then(|e| e.as_array()) {
        if !values.contains(input) {
            issues.push(format!(
                "{path}: must be one of {}",
                values
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    match input {
        Value::Object(object) => {
            let required = schema
                .get("required")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();
            for key in required.iter().filter_map(|v| v.as_str()) {
                if !object.contains_key(key) {
                    issues.push(format!("{path}.{key}: required"));
                }
            }
            if let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) {
                for (key, child) in properties {
                    if let Some(value) = object.get(key) {
                        if !value.is_null() {
                            validate_value(child, value, &format!("{path}.{key}"), issues);
                        }
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    if issues.len() >= 5 {
                        break;
                    }
                    validate_value(item_schema, item, &format!("{path}[{index}]"), issues);
                }
            }
        }
        _ => {}
    }
}

pub fn error_text(tool: &str, issues: &[String], input: &Value) -> String {
    let mut text = format!("Invalid arguments for tool \"{tool}\":\n");
    for issue in issues {
        text.push_str(&format!("- {issue}\n"));
    }
    text.push_str("\nArguments provided:\n");
    text.push_str(&serde_json::to_string_pretty(input).unwrap_or_else(|_| "{}".into()));
    text.push_str("\n\nUpdate the arguments and call the tool again.");
    text
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path"]
        })
    }

    #[test]
    fn repairs_stringified_objects_and_scalars() {
        let input = serde_json::json!("{\"path\":\"a\",\"limit\":\"12\",\"replace_all\":\"true\"}");
        let repaired = repair(&schema(), input);
        assert_eq!(repaired["limit"], 12);
        assert_eq!(repaired["replace_all"], true);
        assert!(validate(&schema(), &repaired).is_empty());
    }

    #[test]
    fn reports_missing_and_wrong_types() {
        let input = serde_json::json!({"limit": "many"});
        let repaired = repair(&schema(), input);
        let issues = validate(&schema(), &repaired);
        assert!(issues.iter().any(|i| i.contains("path")));
        assert!(issues.iter().any(|i| i.contains("limit")));
    }

    #[test]
    fn error_text_names_the_tool_and_arguments() {
        let text = error_text(
            "read",
            &["arguments.path: required".into()],
            &serde_json::json!({}),
        );
        assert!(text.contains("read"));
        assert!(text.contains("Arguments provided"));
        assert!(text.contains("call the tool again"));
    }
}
