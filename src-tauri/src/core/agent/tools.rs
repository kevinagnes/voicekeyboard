//! Built-in tools.
//!
//! The catalog is intentionally small and consolidated: one `read`, one
//! `write`, one `edit`, no sandbox variants. Every handler returns a
//! model-readable failure instead of a raw exception.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::gemini::GeminiAgentClient;
use super::store::Db;
use super::types::{
    new_id, now_ms, ContentPart, PlanItem, ToolFailure, ToolPack, ToolResult, ToolSpec,
};

pub type ToolHandler =
    Arc<dyn Fn(&ToolContext, Value) -> Result<ToolResult, ToolFailure> + Send + Sync>;

/// Sentinel stored in `before_blob` when the previous content exceeded the
/// snapshot cap. Undo must not delete the file in that case.
pub const SNAPSHOT_TOO_LARGE: &str = "\u{0}TOO_LARGE";

#[derive(Clone)]
pub struct ToolDefinition {
    pub spec: ToolSpec,
    pub handler: ToolHandler,
}

/// Host services the runtime cannot implement portably.
pub trait DesktopHost: Send + Sync {
    fn read_clipboard(&self) -> Result<String, String>;
    fn write_clipboard(&self, text: &str) -> Result<(), String>;
    fn paste_text(&self, text: &str) -> Result<(), String>;
    fn open_url(&self, url: &str) -> Result<(), String>;
}

pub struct ToolContext {
    pub session_id: String,
    pub run_id: String,
    pub tool_run_id: String,
    pub workspace: PathBuf,
    pub db: Arc<Db>,
    pub gemini: GeminiAgentClient,
    pub desktop: Arc<dyn DesktopHost>,
    pub cancel: Arc<AtomicBool>,
    pub depth: u8,
}

impl Clone for ToolContext {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            tool_run_id: self.tool_run_id.clone(),
            workspace: self.workspace.clone(),
            db: self.db.clone(),
            gemini: self.gemini.clone(),
            desktop: self.desktop.clone(),
            cancel: self.cancel.clone(),
            depth: self.depth,
        }
    }
}

impl ToolContext {
    fn check_cancel(&self) -> Result<(), ToolFailure> {
        if self.cancel.load(Ordering::SeqCst) {
            Err(ToolFailure::new("cancelled"))
        } else {
            Ok(())
        }
    }

    fn path(&self, raw: &str) -> Result<PathBuf, ToolFailure> {
        resolve_path(&self.workspace, raw)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn catalog(
    workspace: &Path,
    db: Arc<Db>,
    gemini: GeminiAgentClient,
    desktop: Arc<dyn DesktopHost>,
    session_id: &str,
    run_id: &str,
    depth: u8,
    deep_work: bool,
    cancel: Arc<AtomicBool>,
) -> Vec<ToolDefinition> {
    let base = ToolContext {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        tool_run_id: String::new(),
        workspace: workspace.to_path_buf(),
        db,
        gemini,
        desktop,
        cancel,
        depth,
    };
    let mut tools = Vec::new();
    tools.extend(core_tools(&base));
    tools.extend(file_tools(&base));
    tools.extend(web_tools(&base));
    tools.extend(desktop_tools(&base));
    tools.extend(memory_tools(&base));
    if deep_work && depth == 0 {
        // Delegation is one level; the child catalog never includes it.
        tools.push(task_tool());
    }
    tools
}

fn spec(name: &str, description: &str, schema: Value, pack: ToolPack, action: &str) -> ToolSpec {
    ToolSpec::new(name, description, schema, pack, action)
}

fn core_tools(base: &ToolContext) -> Vec<ToolDefinition> {
    let mut out = Vec::new();

    let db = base.db.clone();
    let sid = base.session_id.clone();
    out.push(ToolDefinition {
        spec: spec(
            "get_time",
            "Current local date and time.",
            json!({"type":"object","properties":{}}),
            ToolPack::Core,
            "tool:get_time",
        )
        .read_only(),
        handler: Arc::new(move |_ctx, _| {
            let ms = now_ms();
            Ok(ToolResult {
                text: format!("{} (epoch ms {ms})", crate::core::agent::prompt::today()),
                data: Some(json!({"epochMs": ms})),
            })
        }),
    });

    let db2 = db.clone();
    let sid2 = sid.clone();
    out.push(ToolDefinition {
        spec: spec(
            "update_plan",
            "Replace the durable work plan. Keep it current as work progresses. \
             Items with status pending or in_progress keep the turn alive.",
            json!({
                "type":"object",
                "properties":{
                    "items":{
                        "type":"array",
                        "items":{
                            "type":"object",
                            "properties":{
                                "id":{"type":"string"},
                                "title":{"type":"string"},
                                "status":{"type":"string","enum":["pending","in_progress","completed","blocked"]},
                                "note":{"type":"string"}
                            },
                            "required":["title","status"]
                        }
                    }
                },
                "required":["items"]
            }),
            ToolPack::Core,
            "tool:update_plan",
        ),
        handler: Arc::new(move |_ctx, input| {
            let items = input
                .get("items")
                .and_then(|v| v.as_array())
                .ok_or_else(|| ToolFailure::new("items must be an array"))?;
            let mut plan = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let title = item
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .trim();
                if title.is_empty() {
                    return Err(ToolFailure::new(format!("items[{index}].title is required")));
                }
                let status = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending");
                if !matches!(status, "pending" | "in_progress" | "completed" | "blocked") {
                    return Err(ToolFailure::new(format!(
                        "items[{index}].status must be pending|in_progress|completed|blocked"
                    )));
                }
                plan.push(PlanItem {
                    id: item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("p{}", index + 1)),
                    title: title.to_string(),
                    status: status.to_string(),
                    note: item
                        .get("note")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            db2.set_plan(&sid2, &plan);
            let done = plan.iter().filter(|p| p.status == "completed").count();
            Ok(ToolResult {
                text: format!("Plan updated: {}/{} items completed.", done, plan.len()),
                data: Some(json!(plan)),
            })
        }),
    });

    let db3 = db.clone();
    let sid3 = sid.clone();
    out.push(ToolDefinition {
        spec: spec(
            "list_artifacts",
            "List files produced in this session with size, path and hash.",
            json!({"type":"object","properties":{}}),
            ToolPack::Core,
            "tool:list_artifacts",
        )
        .read_only(),
        handler: Arc::new(move |_ctx, _| {
            let artifacts = db3.artifacts(&sid3);
            if artifacts.is_empty() {
                return Ok(ToolResult::text("No artifacts recorded in this session."));
            }
            let mut text = String::new();
            for artifact in &artifacts {
                text.push_str(&format!(
                    "- {} ({} bytes, sha256 {})\n",
                    artifact.path, artifact.bytes, artifact.sha256
                ));
            }
            Ok(ToolResult {
                text,
                data: Some(json!(artifacts)),
            })
        }),
    });

    let db4 = db.clone();
    let sid4 = sid.clone();
    out.push(ToolDefinition {
        spec: spec(
            "verify_artifacts",
            "Read artifacts back from disk and report missing, empty or incomplete files. \
             Use before declaring a task done.",
            json!({
                "type":"object",
                "properties":{
                    "paths":{"type":"array","items":{"type":"string"}},
                    "expected_sections":{"type":"array","items":{"type":"string"}}
                }
            }),
            ToolPack::Core,
            "tool:verify_artifacts",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            let requested: Vec<String> = input
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_else(|| db4.artifacts(&sid4).into_iter().map(|a| a.path).collect());
            if requested.is_empty() {
                return Err(ToolFailure::new(
                    "No artifacts to verify. Write a file first or pass paths.",
                ));
            }
            let mut report = String::new();
            let mut failures = Vec::new();
            let mut combined = String::new();
            for raw in &requested {
                let path = ctx.path(raw)?;
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        if content.trim().is_empty() {
                            failures.push(format!("{raw}: file is empty"));
                        } else {
                            report.push_str(&format!("- {raw}: OK ({} bytes)\n", content.len()));
                            combined.push_str(&content);
                        }
                    }
                    Err(e) => failures.push(format!("{raw}: {e}")),
                }
            }
            if let Some(sections) = input.get("expected_sections").and_then(|v| v.as_array()) {
                for section in sections.iter().filter_map(|v| v.as_str()) {
                    if !combined.contains(section) {
                        failures.push(format!("missing section: {section}"));
                    }
                }
            }
            if !failures.is_empty() {
                return Err(ToolFailure::new(format!(
                    "Verification failed:\n- {}\n\nFix the files and verify again.",
                    failures.join("\n- ")
                )));
            }
            Ok(ToolResult::text(format!("Verification passed:\n{report}")))
        }),
    });

    out
}

fn file_tools(base: &ToolContext) -> Vec<ToolDefinition> {
    let mut out = Vec::new();

    let workspace = base.workspace.clone();
    out.push(ToolDefinition {
        spec: spec(
            "read",
            "Read a UTF-8 text file from the workspace. Use offset/limit for large files.",
            json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "offset":{"type":"integer","minimum":0},
                    "limit":{"type":"integer","minimum":1}
                },
                "required":["path"]
            }),
            ToolPack::Workspace,
            "file.read",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            ctx.check_cancel()?;
            let raw = required_str(&input, "path")?;
            let path = ctx.path(raw)?;
            let content = std::fs::read_to_string(&path).map_err(|e| {
                ToolFailure::new(format!(
                    "Could not read `{raw}`: {e}. The path is relative to the workspace."
                ))
            })?;
            let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(2_000) as usize;
            let lines: Vec<&str> = content.lines().collect();
            let start = offset.min(lines.len());
            let end = (start + limit).min(lines.len());
            let mut text = String::new();
            for (index, line) in lines[start..end].iter().enumerate() {
                text.push_str(&format!("{:>5}: {}\n", start + index + 1, line));
            }
            if end < lines.len() {
                text.push_str(&format!(
                    "\n[showing lines {}-{} of {}; call read with offset {} to continue]\n",
                    start + 1,
                    end,
                    lines.len(),
                    end
                ));
            }
            let _ = workspace;
            Ok(ToolResult::text(text))
        }),
    });

    let db = base.db.clone();
    let sid = base.session_id.clone();
    let run = base.run_id.clone();
    let workspace = base.workspace.clone();
    out.push(ToolDefinition {
        spec: spec(
            "write",
            "Create or overwrite a UTF-8 text file in the workspace. Parent folders are created. \
             Prefer this over printing long content in the reply.",
            json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "content":{"type":"string"}
                },
                "required":["path","content"]
            }),
            ToolPack::Workspace,
            "file.write",
        ),
        handler: Arc::new(move |ctx, input| {
            ctx.check_cancel()?;
            let raw = required_str(&input, "path")?;
            let content = required_str(&input, "content")?;
            let path = ctx.path(raw)?;
            let before = std::fs::read_to_string(&path).ok();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ToolFailure::new(format!("Could not create `{}`: {e}", parent.display()))
                })?;
            }
            std::fs::write(&path, content)
                .map_err(|e| ToolFailure::new(format!("Could not write `{raw}`: {e}")))?;
            snapshot(
                &db,
                &sid,
                &run,
                &ctx.tool_run_id,
                raw,
                before.as_deref(),
                Some(content),
            );
            register_artifact(&db, &sid, raw, content);
            let _ = &workspace;
            Ok(ToolResult {
                text: format!("Wrote {} bytes to `{raw}`.", content.len()),
                data: Some(json!({"path": raw, "bytes": content.len()})),
            })
        }),
    });

    let db = base.db.clone();
    let sid = base.session_id.clone();
    let run = base.run_id.clone();
    out.push(ToolDefinition {
        spec: spec(
            "edit",
            "Replace an exact string in a workspace file. Read the file first. \
             Set replace_all to change every occurrence.",
            json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "old_string":{"type":"string"},
                    "new_string":{"type":"string"},
                    "replace_all":{"type":"boolean"}
                },
                "required":["path","old_string","new_string"]
            }),
            ToolPack::Workspace,
            "file.write",
        ),
        handler: Arc::new(move |ctx, input| {
            ctx.check_cancel()?;
            let raw = required_str(&input, "path")?;
            let old = required_str(&input, "old_string")?;
            let new = required_str(&input, "new_string")?;
            if old.is_empty() {
                return Err(ToolFailure::new("old_string must not be empty"));
            }
            let path = ctx.path(raw)?;
            let before = std::fs::read_to_string(&path).map_err(|e| {
                ToolFailure::new(format!("Could not read `{raw}` before editing: {e}"))
            })?;
            let count = before.matches(old).count();
            if count == 0 {
                return Err(ToolFailure::new(format!(
                    "old_string was not found in `{raw}`. Read the file with `read` and \
                     copy the exact text (including indentation)."
                )));
            }
            let replace_all = input
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let after = if replace_all {
                before.replace(old, new)
            } else {
                before.replacen(old, new, 1)
            };
            std::fs::write(&path, &after)
                .map_err(|e| ToolFailure::new(format!("Could not write `{raw}`: {e}")))?;
            snapshot(
                &db,
                &sid,
                &run,
                &ctx.tool_run_id,
                raw,
                Some(&before),
                Some(&after),
            );
            register_artifact(&db, &sid, raw, &after);
            Ok(ToolResult {
                text: format!(
                    "Edited `{raw}` ({} replacement{}).",
                    if replace_all { count } else { 1 },
                    if replace_all && count != 1 { "s" } else { "" }
                ),
                data: Some(
                    json!({"path": raw, "replacements": if replace_all { count } else { 1 }}),
                ),
            })
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "glob",
            "Find files by glob pattern (supports **, *, ?). Results are workspace-relative.",
            json!({
                "type":"object",
                "properties":{
                    "pattern":{"type":"string"},
                    "limit":{"type":"integer","minimum":1}
                },
                "required":["pattern"]
            }),
            ToolPack::Workspace,
            "file.read",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            let pattern = required_str(&input, "pattern")?;
            let limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(200)
                .min(1_000) as usize;
            let mut files = Vec::new();
            walk(&ctx.workspace, &mut files, 0);
            let matcher = Glob::new(pattern);
            let mut matches: Vec<String> = files
                .into_iter()
                .filter_map(|path| {
                    let rel = path
                        .strip_prefix(&ctx.workspace)
                        .ok()?
                        .to_string_lossy()
                        .replace('\\', "/");
                    if matcher.matches(&rel) {
                        Some(rel)
                    } else {
                        None
                    }
                })
                .collect();
            matches.sort();
            matches.truncate(limit);
            if matches.is_empty() {
                return Ok(ToolResult::text(format!("No files match `{pattern}`.")));
            }
            Ok(ToolResult::text(matches.join("\n")))
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "grep",
            "Search text files for a literal string (case-insensitive). Returns file:line matches.",
            json!({
                "type":"object",
                "properties":{
                    "query":{"type":"string"},
                    "glob":{"type":"string"},
                    "limit":{"type":"integer","minimum":1}
                },
                "required":["query"]
            }),
            ToolPack::Workspace,
            "file.read",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            let query = required_str(&input, "query")?;
            let needle = query.to_lowercase();
            let filter = input.get("glob").and_then(|v| v.as_str()).map(Glob::new);
            let limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(80)
                .min(500) as usize;
            let mut files = Vec::new();
            walk(&ctx.workspace, &mut files, 0);
            let mut hits = Vec::new();
            for path in files {
                let rel = path
                    .strip_prefix(&ctx.workspace)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if let Some(filter) = &filter {
                    if !filter.matches(&rel) {
                        continue;
                    }
                }
                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (index, line) in content.lines().enumerate() {
                    if line.to_lowercase().contains(&needle) {
                        hits.push(format!("{rel}:{}: {}", index + 1, line.trim()));
                        if hits.len() >= limit {
                            break;
                        }
                    }
                }
                if hits.len() >= limit {
                    break;
                }
            }
            if hits.is_empty() {
                return Ok(ToolResult::text(format!("No matches for `{query}`.")));
            }
            Ok(ToolResult::text(hits.join("\n")))
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "list_files",
            "List the workspace root or a subfolder.",
            json!({
                "type":"object",
                "properties":{"path":{"type":"string"}}
            }),
            ToolPack::Workspace,
            "file.read",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            let raw = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let dir = ctx.path(raw)?;
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&dir)
                .map_err(|e| ToolFailure::new(format!("Could not list `{raw}`: {e}")))?
            {
                let Ok(entry) = entry else { continue };
                let kind = if entry.path().is_dir() {
                    "dir "
                } else {
                    "file"
                };
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                entries.push(format!(
                    "{kind} {:>10}  {}",
                    size,
                    entry.file_name().to_string_lossy()
                ));
            }
            entries.sort();
            if entries.is_empty() {
                Ok(ToolResult::text("(empty folder)"))
            } else {
                Ok(ToolResult::text(entries.join("\n")))
            }
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "file_info",
            "Size, modification time and type for a workspace path.",
            json!({
                "type":"object",
                "properties":{"path":{"type":"string"}},
                "required":["path"]
            }),
            ToolPack::Workspace,
            "file.read",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            let raw = required_str(&input, "path")?;
            let path = ctx.path(raw)?;
            let meta = std::fs::metadata(&path)
                .map_err(|e| ToolFailure::new(format!("Could not stat `{raw}`: {e}")))?;
            Ok(ToolResult {
                text: format!(
                    "{raw}: {} bytes, {}, modified {}",
                    meta.len(),
                    if meta.is_dir() { "directory" } else { "file" },
                    meta.modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis().to_string())
                        .unwrap_or_else(|| "unknown".into())
                ),
                data: None,
            })
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "create_directory",
            "Create a folder (and parents) inside the workspace.",
            json!({
                "type":"object",
                "properties":{"path":{"type":"string"}},
                "required":["path"]
            }),
            ToolPack::Workspace,
            "file.write",
        ),
        handler: Arc::new(move |ctx, input| {
            let raw = required_str(&input, "path")?;
            let path = ctx.path(raw)?;
            std::fs::create_dir_all(&path)
                .map_err(|e| ToolFailure::new(format!("Could not create `{raw}`: {e}")))?;
            Ok(ToolResult::text(format!("Created folder `{raw}`.")))
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "delete_file",
            "Delete a file inside the workspace. Requires a fresh permission grant.",
            json!({
                "type":"object",
                "properties":{"path":{"type":"string"}},
                "required":["path"]
            }),
            ToolPack::Workspace,
            "file.delete",
        ),
        handler: Arc::new(move |ctx, input| {
            let raw = required_str(&input, "path")?;
            let path = ctx.path(raw)?;
            if path.is_dir() {
                return Err(ToolFailure::new(
                    "delete_file only removes files; folders are left alone.",
                ));
            }
            let before = std::fs::read_to_string(&path).ok();
            std::fs::remove_file(&path)
                .map_err(|e| ToolFailure::new(format!("Could not delete `{raw}`: {e}")))?;
            let db = ctx.db.clone();
            snapshot(
                &db,
                &ctx.session_id,
                &ctx.run_id,
                &ctx.tool_run_id,
                raw,
                before.as_deref(),
                None,
            );
            Ok(ToolResult::text(format!("Deleted `{raw}`.")))
        }),
    });

    out
}

fn web_tools(base: &ToolContext) -> Vec<ToolDefinition> {
    let gemini = base.gemini.clone();
    let mut out = vec![ToolDefinition {
        spec: spec(
            "web_search",
            "Search the web with Google and get a sourced digest. Use for news, current \
             events, prices, releases or anything you may not know.",
            json!({
                "type":"object",
                "properties":{"query":{"type":"string"}},
                "required":["query"]
            }),
            ToolPack::Web,
            "web.search",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            ctx.check_cancel()?;
            let query = required_str(&input, "query")?;
            let (text, sources) = gemini
                .web_search(query)
                .map_err(|e| ToolFailure::new(format!("Web search failed: {e}")))?;
            if text.trim().is_empty() {
                return Err(ToolFailure::new(
                    "Web search returned no content. Try a narrower query.",
                ));
            }
            let mut report = text;
            if !sources.is_empty() {
                report.push_str("\n\nSources:\n");
                for source in sources.iter().take(8) {
                    report.push_str(&format!("- {source}\n"));
                }
            }
            Ok(ToolResult {
                text: report,
                data: Some(json!({"sources": sources})),
            })
        }),
    }];

    out.push(ToolDefinition {
        spec: spec(
            "web_fetch",
            "Fetch a public HTTP(S) page and return readable text (HTML stripped).",
            json!({
                "type":"object",
                "properties":{
                    "url":{"type":"string"},
                    "max_chars":{"type":"integer","minimum":500}
                },
                "required":["url"]
            }),
            ToolPack::Web,
            "web.fetch",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            ctx.check_cancel()?;
            let raw = required_str(&input, "url")?;
            let parsed = url::Url::parse(raw)
                .map_err(|e| ToolFailure::new(format!("Invalid URL `{raw}`: {e}")))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ToolFailure::new("Only http and https URLs are allowed."));
            }
            let limit = input
                .get("max_chars")
                .and_then(|v| v.as_u64())
                .unwrap_or(20_000) as usize;
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("VoiceKeyboard/2.0")
                .build()
                .map_err(|e| ToolFailure::new(format!("HTTP client error: {e}")))?;
            let response = client
                .get(parsed)
                .send()
                .map_err(|e| ToolFailure::new(format!("Fetch failed: {e}")))?;
            if !response.status().is_success() {
                return Err(ToolFailure::new(format!(
                    "Fetch failed with HTTP {}.",
                    response.status()
                )));
            }
            let body = response
                .text()
                .map_err(|e| ToolFailure::new(format!("Could not read response: {e}")))?;
            let text = strip_html(&body);
            let mut out = text.chars().take(limit).collect::<String>();
            if text.chars().count() > limit {
                out.push_str(&format!("\n[truncated at {limit} characters]"));
            }
            Ok(ToolResult::text(out))
        }),
    });

    out
}

fn desktop_tools(base: &ToolContext) -> Vec<ToolDefinition> {
    let mut out = Vec::new();

    let desktop = base.desktop.clone();
    out.push(ToolDefinition {
        spec: spec(
            "read_clipboard",
            "Read the current clipboard text.",
            json!({"type":"object","properties":{}}),
            ToolPack::Desktop,
            "clipboard.read",
        )
        .read_only(),
        handler: Arc::new(move |ctx, _| {
            let text = desktop
                .read_clipboard()
                .map_err(|e| ToolFailure::new(format!("Clipboard read failed: {e}")))?;
            let mut text = text;
            if text.chars().count() > 8_000 {
                text = text.chars().take(8_000).collect::<String>() + "…";
            }
            let _ = ctx;
            Ok(ToolResult::text(text))
        }),
    });

    let desktop = base.desktop.clone();
    out.push(ToolDefinition {
        spec: spec(
            "write_clipboard",
            "Replace the clipboard with text.",
            json!({
                "type":"object",
                "properties":{"text":{"type":"string"}},
                "required":["text"]
            }),
            ToolPack::Desktop,
            "clipboard.write",
        ),
        handler: Arc::new(move |_ctx, input| {
            let text = required_str(&input, "text")?;
            desktop
                .write_clipboard(text)
                .map_err(|e| ToolFailure::new(format!("Clipboard write failed: {e}")))?;
            Ok(ToolResult::text("Clipboard updated."))
        }),
    });

    let desktop = base.desktop.clone();
    out.push(ToolDefinition {
        spec: spec(
            "paste_text",
            "Paste text into the focused application (clipboard + paste keystroke). \
             This is a real side effect; confirm before using it.",
            json!({
                "type":"object",
                "properties":{"text":{"type":"string"}},
                "required":["text"]
            }),
            ToolPack::Desktop,
            "desktop.paste",
        ),
        handler: Arc::new(move |_ctx, input| {
            let text = required_str(&input, "text")?;
            desktop
                .paste_text(text)
                .map_err(|e| ToolFailure::new(format!("Paste failed: {e}")))?;
            Ok(ToolResult::text("Pasted into the focused field."))
        }),
    });

    let desktop = base.desktop.clone();
    out.push(ToolDefinition {
        spec: spec(
            "open_url",
            "Open a URL in the default browser.",
            json!({
                "type":"object",
                "properties":{"url":{"type":"string"}},
                "required":["url"]
            }),
            ToolPack::Desktop,
            "desktop.open",
        ),
        handler: Arc::new(move |_ctx, input| {
            let raw = required_str(&input, "url")?;
            let parsed = url::Url::parse(raw)
                .map_err(|e| ToolFailure::new(format!("Invalid URL `{raw}`: {e}")))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ToolFailure::new("Only http and https URLs are allowed."));
            }
            desktop
                .open_url(raw)
                .map_err(|e| ToolFailure::new(format!("Could not open URL: {e}")))?;
            Ok(ToolResult::text(format!("Opened {raw}.")))
        }),
    });

    out
}

fn memory_tools(base: &ToolContext) -> Vec<ToolDefinition> {
    let mut out = Vec::new();

    let db = base.db.clone();
    out.push(ToolDefinition {
        spec: spec(
            "remember",
            "Store a durable fact or preference about the user.",
            json!({
                "type":"object",
                "properties":{
                    "text":{"type":"string"},
                    "source":{"type":"string"}
                },
                "required":["text"]
            }),
            ToolPack::Memory,
            "memory.write",
        ),
        handler: Arc::new(move |_ctx, input| {
            let text = required_str(&input, "text")?;
            if text.trim().is_empty() {
                return Err(ToolFailure::new("text must not be empty"));
            }
            let source = input.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let id = db.remember(text, source);
            Ok(ToolResult {
                text: "Remembered.".into(),
                data: Some(json!({"id": id})),
            })
        }),
    });

    let db = base.db.clone();
    out.push(ToolDefinition {
        spec: spec(
            "recall",
            "Search durable memory for facts relevant to a query.",
            json!({
                "type":"object",
                "properties":{
                    "query":{"type":"string"},
                    "limit":{"type":"integer","minimum":1}
                },
                "required":["query"]
            }),
            ToolPack::Memory,
            "memory.read",
        )
        .read_only(),
        handler: Arc::new(move |_ctx, input| {
            let query = required_str(&input, "query")?;
            let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            let items = db.recall_items(query, limit);
            if items.is_empty() {
                Ok(ToolResult::text("No matching memories."))
            } else {
                Ok(ToolResult::text(
                    items
                        .iter()
                        .map(|(id, text)| format!("- {id}: {text}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
            }
        }),
    });

    let db = base.db.clone();
    out.push(ToolDefinition {
        spec: spec(
            "forget",
            "Delete a memory by id.",
            json!({
                "type":"object",
                "properties":{"id":{"type":"string"}},
                "required":["id"]
            }),
            ToolPack::Memory,
            "memory.forget",
        ),
        handler: Arc::new(move |_ctx, input| {
            let id = required_str(&input, "id")?;
            if db.forget(id) {
                Ok(ToolResult::text(format!("Forgot {id}.")))
            } else {
                Err(ToolFailure::new(format!("No memory with id `{id}`.")))
            }
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "list_skills",
            "List saved skills (playbooks) with a one-line description.",
            json!({"type":"object","properties":{}}),
            ToolPack::Memory,
            "skill.read",
        )
        .read_only(),
        handler: Arc::new(move |ctx, _| {
            let dir = ctx.workspace.join("skills");
            let mut items = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("md") {
                        continue;
                    }
                    let name = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let description = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|text| {
                            text.lines()
                                .map(str::trim)
                                .find(|line| !line.is_empty())
                                .map(|line| line.trim_start_matches('#').trim().to_string())
                        })
                        .unwrap_or_default();
                    items.push(format!("- {name}: {description}"));
                }
            }
            items.sort();
            if items.is_empty() {
                Ok(ToolResult::text("No skills saved yet."))
            } else {
                Ok(ToolResult::text(items.join("\n")))
            }
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "read_skill",
            "Read a saved skill by name.",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"]
            }),
            ToolPack::Memory,
            "skill.read",
        )
        .read_only(),
        handler: Arc::new(move |ctx, input| {
            let name = required_str(&input, "name")?;
            let path = skill_path(&ctx.workspace, name)?;
            let text = std::fs::read_to_string(&path)
                .map_err(|e| ToolFailure::new(format!("Could not read skill `{name}`: {e}")))?;
            Ok(ToolResult::text(text))
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "write_skill",
            "Save or update a skill/playbook as markdown. Use a short kebab-case name.",
            json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string"},
                    "content":{"type":"string"}
                },
                "required":["name","content"]
            }),
            ToolPack::Memory,
            "skill.write",
        ),
        handler: Arc::new(move |ctx, input| {
            let name = required_str(&input, "name")?;
            let content = required_str(&input, "content")?;
            let path = skill_path(&ctx.workspace, name)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    ToolFailure::new(format!("Could not create skills folder: {e}"))
                })?;
            }
            std::fs::write(&path, content)
                .map_err(|e| ToolFailure::new(format!("Could not save skill `{name}`: {e}")))?;
            Ok(ToolResult::text(format!("Saved skill `{name}`.")))
        }),
    });

    out.push(ToolDefinition {
        spec: spec(
            "delete_skill",
            "Delete a saved skill by name.",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"]
            }),
            ToolPack::Memory,
            "skill.write",
        ),
        handler: Arc::new(move |ctx, input| {
            let name = required_str(&input, "name")?;
            let path = skill_path(&ctx.workspace, name)?;
            std::fs::remove_file(&path)
                .map_err(|e| ToolFailure::new(format!("Could not delete skill `{name}`: {e}")))?;
            Ok(ToolResult::text(format!("Deleted skill `{name}`.")))
        }),
    });

    out
}

/// Skill names are simple slugs; reject anything that could escape `skills/`.
fn skill_path(workspace: &Path, name: &str) -> Result<PathBuf, ToolFailure> {
    let clean = name.trim().trim_end_matches(".md");
    if clean.is_empty()
        || !clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ToolFailure::new(
            "Skill name must be a slug: letters, digits, '-' or '_'.",
        ));
    }
    Ok(workspace.join("skills").join(format!("{clean}.md")))
}

fn task_tool() -> ToolDefinition {
    ToolDefinition {
        spec: spec(
            "task",
            "Delegate a self-contained, read-only research or drafting task to a child agent. \
             The child has read/web tools and cannot delegate again. Returns its final text.",
            json!({
                "type":"object",
                "properties":{
                    "prompt":{"type":"string"},
                    "pack":{"type":"string","enum":["read_only","workspace"]}
                },
                "required":["prompt"]
            }),
            ToolPack::Delegation,
            "agent.task",
        ),
        handler: Arc::new(|_ctx, _input| {
            Err(ToolFailure::new(
                "Delegation is wired by the runner; this placeholder should never run.",
            ))
        }),
    }
}

pub fn is_task_tool(name: &str) -> bool {
    name == "task"
}

/// Names + one-line descriptions only; the body is fetched with `read_skill`.
pub fn skill_catalog(workspace: &Path) -> Vec<(String, String)> {
    let dir = workspace.join("skills");
    let mut items = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let description = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| {
                    text.lines()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                        .map(|line| line.trim_start_matches('#').trim().to_string())
                })
                .unwrap_or_default();
            items.push((name, description));
        }
    }
    items.sort();
    items.truncate(30);
    items
}

// ---------------------------------------------------------------- helpers

fn required_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolFailure> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolFailure::new(format!("`{key}` is required and must be a string")))
}

pub fn resolve_path(workspace: &Path, raw: &str) -> Result<PathBuf, ToolFailure> {
    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let clean = |path: &Path| {
        let text = path.to_string_lossy().replace('\\', "/");
        text.strip_prefix("//?/").unwrap_or(&text).to_lowercase()
    };
    let root_str = clean(&root).trim_end_matches('/').to_string();
    let path_str = clean(&normalized);
    let inside = |text: &str| text == root_str || text.starts_with(&format!("{root_str}/"));
    if !inside(&path_str) {
        return Err(ToolFailure::new(format!(
            "Path `{raw}` is outside the workspace. Use a path relative to the workspace."
        )));
    }
    // Follow symlinks for existing paths so a link cannot escape the root.
    let existing = if normalized.exists() {
        normalized.canonicalize().ok()
    } else {
        normalized
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
    };
    if let Some(real) = existing {
        if !inside(&clean(&real)) {
            return Err(ToolFailure::new(format!(
                "Path `{raw}` resolves outside the workspace."
            )));
        }
    }
    Ok(normalized)
}

fn sha256(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn snapshot(
    db: &Arc<Db>,
    session_id: &str,
    run_id: &str,
    tool_run_id: &str,
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) {
    const MAX_SNAPSHOT: usize = 1_048_576;
    let pair = |text: &str| {
        if text.len() <= MAX_SNAPSHOT {
            Some((sha256(text), text.to_string()))
        } else {
            Some((sha256(text), SNAPSHOT_TOO_LARGE.to_string()))
        }
    };
    let before_pair = before.and_then(pair);
    let after_pair = after.and_then(pair);
    db.add_snapshot(
        session_id,
        run_id,
        tool_run_id,
        path,
        before_pair
            .as_ref()
            .map(|(hash, text)| (hash.as_str(), text.as_str())),
        after_pair
            .as_ref()
            .map(|(hash, text)| (hash.as_str(), text.as_str())),
    );
}

fn register_artifact(db: &Arc<Db>, session_id: &str, path: &str, content: &str) {
    let title = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    let mime = match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        _ => "text/plain",
    };
    db.register_artifact(
        Some(session_id),
        None,
        path,
        &title,
        mime,
        content.len() as i64,
        &sha256(content),
    );
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 20 || out.len() > 20_000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, ".git" | "node_modules" | "target" | "__pycache__") {
                continue;
            }
            walk(&path, out, depth + 1);
        } else {
            out.push(path);
        }
    }
}

struct Glob {
    pattern: String,
}

impl Glob {
    fn new(pattern: &str) -> Self {
        Self {
            pattern: pattern.replace('\\', "/"),
        }
    }

    fn matches(&self, path: &str) -> bool {
        let value = path.replace('\\', "/");
        glob_match(&self.pattern, &value)
    }
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let mut memo = HashSet::new();
    glob_inner(&p, &t, 0, 0, &mut memo)
}

fn glob_inner(
    p: &[char],
    t: &[char],
    mut pi: usize,
    mut ti: usize,
    memo: &mut HashSet<(usize, usize)>,
) -> bool {
    if !memo.insert((pi, ti)) {
        return false;
    }
    while pi < p.len() {
        match p[pi] {
            '*' => {
                if pi + 1 < p.len() && p[pi + 1] == '*' {
                    // `**/` also matches zero directories.
                    let mut next = pi + 2;
                    if next < p.len() && p[next] == '/' {
                        next += 1;
                    }
                    return glob_inner(p, t, next, ti, memo)
                        || (ti < t.len() && glob_inner(p, t, pi, ti + 1, memo));
                }
                return glob_inner(p, t, pi + 1, ti, memo)
                    || (ti < t.len() && glob_inner(p, t, pi, ti + 1, memo));
            }
            '?' => {
                if ti >= t.len() {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            c => {
                if ti >= t.len() || t[ti] != c {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    ti == t.len()
}

fn strip_html(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut in_script = false;
    let mut tag = String::new();
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                let name = tag
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                if name == "script" || name == "style" {
                    in_script = !tag.trim_start().starts_with('/');
                }
                tag.clear();
                in_tag = false;
            } else {
                tag.push(ch);
            }
            continue;
        }
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if !in_script {
            out.push(ch);
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    decoded
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a content part for a tool response.
pub fn response_part(call_id: &str, name: &str, output: &str, is_error: bool) -> ContentPart {
    ContentPart::ToolResponse {
        call_id: call_id.to_string(),
        name: name.to_string(),
        output: output.to_string(),
        is_error,
    }
}

/// Read a spilled tool output, used by the inspector and undo.
pub fn read_spill(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

pub fn new_call_id() -> String {
    new_id("call")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeDesktop;
    impl DesktopHost for FakeDesktop {
        fn read_clipboard(&self) -> Result<String, String> {
            Ok(String::new())
        }
        fn write_clipboard(&self, _text: &str) -> Result<(), String> {
            Ok(())
        }
        fn paste_text(&self, _text: &str) -> Result<(), String> {
            Ok(())
        }
        fn open_url(&self, _url: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn workspace() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vk-tools-{}", new_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn catalog_for(ws: &Path) -> (Vec<ToolDefinition>, Arc<Db>) {
        let db_dir = ws.join(".db");
        std::fs::create_dir_all(&db_dir).unwrap();
        let db = Db::open(db_dir.join("agent.db")).unwrap();
        let session = db.create_session("test", None, "gemini");
        let tools = catalog(
            ws,
            db.clone(),
            super::super::gemini::GeminiAgentClient::new("", "gemini", ""),
            Arc::new(FakeDesktop),
            &session,
            "run_test",
            0,
            false,
            Arc::new(AtomicBool::new(false)),
        );
        (tools, db)
    }

    fn call(tools: &[ToolDefinition], name: &str, input: Value) -> Result<ToolResult, ToolFailure> {
        let tool = tools.iter().find(|t| t.spec.name == name).unwrap();
        let ws = std::env::temp_dir();
        let ctx = ToolContext {
            session_id: "s".into(),
            run_id: "r".into(),
            tool_run_id: "t".into(),
            workspace: ws,
            db: Db::open(std::env::temp_dir().join("vk-tools-unused.db")).unwrap(),
            gemini: super::super::gemini::GeminiAgentClient::new("", "gemini", ""),
            desktop: Arc::new(FakeDesktop),
            cancel: Arc::new(AtomicBool::new(false)),
            depth: 0,
        };
        (tool.handler)(&ctx, input)
    }

    #[test]
    fn path_guard_rejects_escapes() {
        let ws = workspace();
        assert!(resolve_path(&ws, "notes/a.md").is_ok());
        assert!(resolve_path(&ws, "../escape.md").is_err());
        assert!(resolve_path(&ws, "C:\\Windows\\System32").is_err());
    }

    #[test]
    fn write_read_edit_and_verify_round_trip() {
        let ws = workspace();
        let (tools, _db) = catalog_for(&ws);
        call(
            &tools,
            "write",
            serde_json::json!({"path":"notes/a.md","content":"# Title\n\nBody"}),
        )
        .unwrap();
        let read = call(&tools, "read", serde_json::json!({"path":"notes/a.md"})).unwrap();
        assert!(read.text.contains("# Title"));
        call(
            &tools,
            "edit",
            serde_json::json!({
                "path":"notes/a.md","old_string":"Body","new_string":"Better body"
            }),
        )
        .unwrap();
        let verify = call(
            &tools,
            "verify_artifacts",
            serde_json::json!({
                "paths":["notes/a.md"],"expected_sections":["Better body"]
            }),
        )
        .unwrap();
        assert!(verify.text.contains("passed"));
    }

    #[test]
    fn edit_reports_missing_old_string() {
        let ws = workspace();
        let (tools, _db) = catalog_for(&ws);
        call(
            &tools,
            "write",
            serde_json::json!({"path":"a.txt","content":"hello"}),
        )
        .unwrap();
        let error = call(
            &tools,
            "edit",
            serde_json::json!({
                "path":"a.txt","old_string":"nope","new_string":"x"
            }),
        )
        .unwrap_err();
        assert!(error.message.contains("not found"));
    }

    #[test]
    fn update_plan_persists_items() {
        let ws = workspace();
        let (tools, db) = catalog_for(&ws);
        let session = db.sessions(1)[0].id.clone();
        let tool = tools.iter().find(|t| t.spec.name == "update_plan").unwrap();
        let ctx = ToolContext {
            session_id: session.clone(),
            run_id: "r".into(),
            tool_run_id: "t".into(),
            workspace: ws.clone(),
            db: db.clone(),
            gemini: super::super::gemini::GeminiAgentClient::new("", "gemini", ""),
            desktop: Arc::new(FakeDesktop),
            cancel: Arc::new(AtomicBool::new(false)),
            depth: 0,
        };
        (tool.handler)(
            &ctx,
            serde_json::json!({"items":[
                {"title":"First","status":"completed"}, {"title":"Second","status":"pending"}
            ]}),
        )
        .unwrap();
        assert_eq!(db.plan(&session).len(), 2);
    }
}
