use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use crate::curator::CuratorOracle;
use crate::librarian::MemoryLibrarian;
use crate::types::*;

// ── JSON-RPC 2.0 types ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)] // validated by serde deserialization, not read
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

// ── MCP types ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ToolInfo {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

#[derive(Debug, Serialize)]
struct CallToolResult {
    content: Vec<ToolContent>,
    #[serde(rename = "isError", skip_serializing_if = "std::ops::Not::not")]
    is_error: bool,
}

#[derive(Debug, Serialize)]
struct ToolContent {
    #[serde(rename = "type")]
    content_type: String,
    text: String,
}

// ── Server ─────────────────────────────────────────────────────────

/// Degraded MCP server used when storage is unreachable. Responds to
/// handshake + tools/list (empty) + tools/call (returns structured error).
/// Prevents the client from hanging on the initial read when the backing
/// store is down (Phase 4.8 item 4.8.17).
pub async fn run_degraded_stdio_server(reason: String) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    eprintln!("[muninn] WARNING: running in degraded mode — {reason}");

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let resp = match req.method.as_str() {
            "initialize" => Some(JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": {
                        "name": "muninn",
                        "version": "0.1.0-degraded",
                    },
                    "instructions": format!(
                        "Muninn is running in DEGRADED mode: {reason}. No tools are available until storage is reachable."
                    ),
                })),
                error: None,
            }),
            "notifications/initialized" => None,
            "tools/list" => Some(JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: Some(serde_json::json!({ "tools": [] })),
                error: None,
            }),
            "tools/call" => Some(JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: Some(serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::json!({
                            "error": true,
                            "code": "STORAGE_UNAVAILABLE",
                            "message": reason,
                        }).to_string(),
                    }],
                    "isError": true,
                })),
                error: None,
            }),
            _ => None,
        };

        if let Some(resp) = resp {
            let mut out = serde_json::to_string(&resp).unwrap();
            out.push('\n');
            let _ = stdout.write_all(out.as_bytes()).await;
            let _ = stdout.flush().await;
        }
    }
    Ok(())
}

pub async fn run_stdio_server(
    librarian: Arc<MemoryLibrarian>,
    curator: Arc<CuratorOracle>,
) -> anyhow::Result<()> {
    // Bound debug-log growth without a scheduler: best-effort 7-day prune
    // at every server start.
    {
        let lib = librarian.clone();
        tokio::spawn(async move {
            let _ = lib.prune_debug_log(7).await;
        });
    }

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    let mut last_session_id: Option<Uuid> = None;

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let response = handle_request(&request, &librarian, &curator, &mut last_session_id).await;

        if let Some(resp) = response {
            let mut out = serde_json::to_string(&resp).unwrap();
            out.push('\n');
            let _ = stdout.write_all(out.as_bytes()).await;
            let _ = stdout.flush().await;
        }
    }

    Ok(())
}

async fn handle_request(
    req: &JsonRpcRequest,
    librarian: &Arc<MemoryLibrarian>,
    curator: &Arc<CuratorOracle>,
    last_session_id: &mut Option<Uuid>,
) -> Option<JsonRpcResponse> {
    match req.method.as_str() {
        "initialize" => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id.clone(),
            result: Some(serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": { "listChanged": true }
                },
                "serverInfo": {
                    "name": "muninn",
                    "version": "0.1.0"
                }
            })),
            error: None,
        }),

        "notifications/initialized" => None,

        "tools/list" => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id.clone(),
            result: Some(serde_json::json!({ "tools": tool_definitions() })),
            error: None,
        }),

        "tools/call" => {
            let tool_name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or(Value::Object(Default::default()));

            let result =
                handle_tool_call(tool_name, &args, librarian, curator, last_session_id).await;

            Some(JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: req.id.clone(),
                result: Some(serde_json::to_value(result).unwrap()),
                error: None,
            })
        }

        _ => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: req.id.clone(),
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("method not found: {}", req.method),
            }),
        }),
    }
}

/// Breaker key used by every MCP tool dispatch so storage outages trip
/// once and short-circuit subsequent calls instead of leaking errors per
/// tool. Intentionally a single global key — storage is one backend.
const MCP_BREAKER_KEY: &str = "mcp-storage";

async fn handle_tool_call(
    tool_name: &str,
    args: &Value,
    librarian: &Arc<MemoryLibrarian>,
    curator: &Arc<CuratorOracle>,
    last_session_id: &mut Option<Uuid>,
) -> CallToolResult {
    // A9 — if the breaker has tripped from recent consecutive storage
    // failures, short-circuit without hitting the DB. Claude sees a clear
    // degraded-mode response and can retry after the trip expires (60 s).
    if crate::breaker::is_tripped(MCP_BREAKER_KEY) {
        return CallToolResult {
            content: vec![ToolContent {
                content_type: "text".into(),
                text: serde_json::json!({
                    "error": true,
                    "code": "MUNINN_DEGRADED",
                    "message": "Muninn is in degraded mode after consecutive storage failures. Retry after the breaker cools down (60s).",
                })
                .to_string(),
            }],
            is_error: true,
        };
    }

    let result = match tool_name {
        "muninn_save" => tool_save(args, librarian).await,
        "muninn_verify" => tool_verify(args, librarian).await,
        "muninn_capture_passive" => tool_capture_passive(args, librarian).await,
        "muninn_search" => tool_search(args, librarian).await,
        "muninn_context" => tool_context(args, librarian).await,
        "muninn_get" => tool_get(args, librarian).await,
        "muninn_timeline" => tool_timeline(args, librarian).await,
        "muninn_session_start" => tool_session_start(args, librarian, last_session_id).await,
        "muninn_session_end" => tool_session_end(args, librarian, last_session_id).await,
        "muninn_related" => tool_related(args, librarian).await,
        "muninn_stats" => tool_stats(args, librarian).await,
        "muninn_merge_projects" => tool_merge_projects(args, librarian).await,
        "muninn_debug" => tool_debug(args, librarian).await,
        "curator_ask" => tool_curator_ask(args, curator).await,
        "curator_topics" => tool_curator_topics(args, curator).await,
        "curator_decisions" => tool_curator_decisions(args, curator).await,
        "curator_techdebt" => tool_curator_techdebt(args, curator).await,
        "curator_onboard" => tool_curator_onboard(args, curator).await,
        "huginn_crawl" => tool_huginn_crawl(args, librarian).await,
        "huginn_status" => tool_huginn_status(args, librarian).await,
        "huginn_architecture" => tool_huginn_architecture(args, librarian).await,
        "huginn_techdebt" => tool_huginn_techdebt(args, librarian).await,
        "huginn_recrawl_file" => tool_huginn_recrawl_file(args, librarian).await,
        "huginn_search_graph" => tool_huginn_search_graph(args).await,
        "huginn_symbol" => tool_huginn_symbol(args).await,
        "huginn_trace" => tool_huginn_trace(args).await,
        "huginn_skills" => tool_huginn_skills().await,
        "huginn_benchmark" => tool_huginn_benchmark(args, curator).await,
        _ => Err(format!("unknown tool: {tool_name}")),
    };

    match result {
        Ok(text) => {
            // A9 — success resets the breaker counter.
            crate::breaker::record_success(MCP_BREAKER_KEY);
            CallToolResult {
                content: vec![ToolContent {
                    content_type: "text".into(),
                    text,
                }],
                is_error: false,
            }
        }
        Err(msg) => {
            // A9 — record failure for breaker tripping. Heuristic: treat
            // messages containing "database", "storage", "pool", or "connection"
            // as storage-layer errors; validation errors (unknown tool, bad
            // args) don't count.
            if looks_like_storage_err(&msg) {
                crate::breaker::record_failure(MCP_BREAKER_KEY);
            }
            CallToolResult {
                content: vec![ToolContent {
                    content_type: "text".into(),
                    text: serde_json::json!({
                        "error": true,
                        "message": msg,
                    })
                    .to_string(),
                }],
                is_error: true,
            }
        }
    }
}

fn looks_like_storage_err(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("database")
        || lower.contains("storage")
        || lower.contains("pool")
        || lower.contains("connection")
        || lower.contains("postgres")
        || lower.contains("sqlite")
        || lower.contains("timeout")
}

// ── Tool handlers ──────────────────────────────────────────────────

/// Generate a topic key from entry type and title for upsert dedup.
fn suggest_topic_key(entry_type: &str, title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .collect();
    format!("{entry_type}:{slug}")
}

async fn tool_verify(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let id_str = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required field: id".to_string())?;
    let id: Uuid = id_str
        .parse()
        .map_err(|_| format!("invalid UUID: {id_str}"))?;

    let entry = lib.mark_verified(id).await.map_err(|e| e.to_string())?;

    let response = serde_json::json!({
        "status": "verified",
        "entry": {
            "id": entry.id.to_string(),
            "title": entry.title,
            "verified": entry.verified,
            "verifiedAt": entry.verified_at.map(|t| t.to_rfc3339()),
            "verifiedBy": entry.verified_by,
        },
    });
    Ok(serde_json::to_string(&response).unwrap_or_default())
}

async fn tool_capture_passive(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required field: text".to_string())?;
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let entry_type: Option<EntryType> = args
        .get("type")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let extra_tags: Vec<String> = args
        .get("tags")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let results = lib
        .capture_passive(text, project_id.as_deref(), entry_type, &extra_tags)
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<Value> = results
        .iter()
        .map(|r| {
            let status = match r.action {
                SaveAction::Created => "saved",
                SaveAction::Updated => "updated",
                SaveAction::Rejected => "rejected",
                SaveAction::Duplicate => "duplicate",
            };
            serde_json::json!({
                "id": r.id.to_string(),
                "status": status,
            })
        })
        .collect();

    let response = serde_json::json!({
        "status": if results.is_empty() { "empty" } else { "ok" },
        "savedCount": results.len(),
        "items": items,
    });

    Ok(serde_json::to_string(&response).unwrap_or_default())
}

async fn tool_save(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let entry_type: EntryType = args
        .get("type")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or(EntryType::Note);
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let topic_key = args
        .get("topicKey")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // Phase 5.7 — explicit override of the resolved git-config author.
    // Useful when an agent saves on behalf of a named user.
    let author = args
        .get("author")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Derive the fallback key from the REDACTED title, or a secret pasted
    // into the title survives inside topic_key after propose scrubs the
    // title itself.
    let effective_topic_key = topic_key.unwrap_or_else(|| {
        let (clean_title, _) = crate::redact::redact_secrets(&title);
        suggest_topic_key(entry_type.as_str(), &clean_title)
    });

    let confidence = resolve_confidence(args.get("confidence"));

    let result = lib
        .propose(MemoryEntryInput {
            title,
            content,
            entry_type,
            tags,
            project_id,
            topic_key: Some(effective_topic_key.clone()),
            confidence,
            author,
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;

    let status = match result.action {
        SaveAction::Updated => "updated",
        SaveAction::Rejected => "rejected",
        SaveAction::Created => "saved",
        SaveAction::Duplicate => "duplicate",
    };

    let mut response = serde_json::json!({
        "status": status,
        "entry": {
            "id": result.id.to_string(),
            "title": args.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            "type": args.get("type").and_then(|v| v.as_str()).unwrap_or("note"),
            "topicKey": effective_topic_key,
            "confidence": confidence.unwrap_or(DEFAULT_CONFIDENCE),
        }
    });

    if let Some(ref old) = result.superseded {
        response["superseded"] = serde_json::json!({
            "id": old.id.to_string(),
            "title": old.title,
            "createdAt": old.created_at.to_rfc3339(),
        });
    }

    Ok(response.to_string())
}

async fn tool_search(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
    let compact = args
        .get("compact")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let project_id = args.get("projectId").and_then(|v| v.as_str());
    let entry_type: Option<EntryType> = args
        .get("type")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // Phase 5.7 — case-insensitive substring filter on author. Applied
    // after fused search returns; the limit honors the unfiltered ranking.
    let author_filter: Option<String> = args
        .get("author")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());

    let raw = lib
        .search(query, limit, None, project_id, entry_type, None)
        .await
        .map_err(|e| e.to_string())?;
    let results: Vec<_> = match author_filter {
        Some(needle) => raw
            .into_iter()
            .filter(|e| {
                e.author
                    .as_deref()
                    .map(|a| a.to_lowercase().contains(&needle))
                    .unwrap_or(false)
            })
            .collect(),
        None => raw,
    };

    // Estimate full-content cost for every result so callers can decide
    // whether to `muninn_get` the full body or stay with the compact snippet.
    let full_tokens_per_result: Vec<usize> = results
        .iter()
        .map(|e| crate::tokens::estimate(&e.title) + crate::tokens::estimate(&e.content))
        .collect();

    let items: Vec<Value> = results
        .iter()
        .zip(full_tokens_per_result.iter())
        .map(|(e, full_tokens)| {
            if compact {
                let snippet: String = e.content.chars().take(150).collect();
                let snippet_tokens =
                    crate::tokens::estimate(&e.title) + crate::tokens::estimate(&snippet);
                serde_json::json!({
                    "id": e.id.to_string(),
                    "title": e.title,
                    "type": e.entry_type.as_str(),
                    "tags": e.tags,
                    "createdAt": e.created_at.to_rfc3339(),
                    "confidence": e.confidence,
                    "author": e.author,
                    "verifiedBy": e.verified_by,
                    "snippet": snippet,
                    "tokensEstimate": snippet_tokens,
                    "fullTokensEstimate": *full_tokens,
                })
            } else {
                serde_json::json!({
                    "id": e.id.to_string(),
                    "title": e.title,
                    "type": e.entry_type.as_str(),
                    "content": e.content,
                    "tags": e.tags,
                    "createdAt": e.created_at.to_rfc3339(),
                    "confidence": e.confidence,
                    "accessCount": e.access_count,
                    "author": e.author,
                    "verifiedBy": e.verified_by,
                    "tokensEstimate": *full_tokens,
                })
            }
        })
        .collect();

    let total_tokens: usize = items
        .iter()
        .filter_map(|v| {
            v.get("tokensEstimate")
                .and_then(|t| t.as_u64())
                .map(|u| u as usize)
        })
        .sum();

    Ok(serde_json::json!({
        "count": items.len(),
        "compact": compact,
        "totalTokensEstimate": total_tokens,
        "results": items,
    })
    .to_string())
}

async fn tool_context(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let project_id = args.get("projectId").and_then(|v| v.as_str());
    let session_count = args
        .get("sessionCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as usize;

    let ctx = lib
        .get_context(None, project_id, session_count)
        .await
        .map_err(|e| e.to_string())?;

    let recent_entries: Vec<Value> = ctx
        .recent_entries
        .iter()
        .take(32)
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "title": e.title,
                "type": e.entry_type.as_str(),
                "content": e.content,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "formatted": ctx.formatted,
        "stats": {
            "totalEntries": ctx.stats.total_entries,
            "totalSessions": ctx.stats.total_sessions,
        },
        "recentSessions": ctx.recent_sessions.len(),
        "recentEntries": recent_entries.len(),
        "recentMemories": recent_entries,
    })
    .to_string())
}

async fn tool_get(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let id_str = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let id: Uuid = id_str
        .parse()
        .map_err(|_| format!("invalid UUID: {id_str}"))?;

    let entry = lib.get(id).await.map_err(|e| e.to_string())?;
    let tokens = crate::tokens::estimate(&entry.title) + crate::tokens::estimate(&entry.content);

    Ok(serde_json::json!({
        "id": entry.id.to_string(),
        "title": entry.title,
        "type": entry.entry_type.as_str(),
        "content": entry.content,
        "tags": entry.tags,
        "createdAt": entry.created_at.to_rfc3339(),
        "updatedAt": entry.updated_at.to_rfc3339(),
        "accessCount": entry.access_count,
        "layer": entry.layer.value(),
        "confidence": entry.confidence,
        "author": entry.author,
        "verifiedBy": entry.verified_by,
        "tokensEstimate": tokens,
    })
    .to_string())
}

async fn tool_timeline(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let id_str = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let id: Uuid = id_str
        .parse()
        .map_err(|_| format!("invalid UUID: {id_str}"))?;
    let window = args.get("window").and_then(|v| v.as_i64()).unwrap_or(24);

    let entries = lib
        .get_timeline(id, window)
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "title": e.title,
                "type": e.entry_type.as_str(),
                "createdAt": e.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "count": items.len(),
        "entries": items,
    })
    .to_string())
}

async fn tool_session_start(
    args: &Value,
    lib: &MemoryLibrarian,
    last_session_id: &mut Option<Uuid>,
) -> Result<String, String> {
    let session = lib
        .propose_session(SessionInput {
            goal: args
                .get("goal")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            project_id: args
                .get("projectId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            tool: args
                .get("tool")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
        .await
        .map_err(|e| e.to_string())?;

    *last_session_id = Some(session.id);

    Ok(serde_json::json!({
        "sessionId": session.id.to_string(),
        "status": "active",
        "startedAt": session.started_at.to_rfc3339(),
    })
    .to_string())
}

async fn tool_session_end(
    args: &Value,
    lib: &MemoryLibrarian,
    last_session_id: &mut Option<Uuid>,
) -> Result<String, String> {
    let session_id = args
        .get("sessionId")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Uuid>().ok())
        .or(*last_session_id)
        .ok_or("no session ID provided and no active session")?;

    let summary_text = args
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("No summary");
    let goal = args
        .get("goal")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let instructions: Vec<String> = args
        .get("instructions")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let accomplished: Vec<String> = args
        .get("accomplished")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let discoveries: Vec<String> = args
        .get("discoveries")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let files_modified: Vec<String> = args
        .get("filesModified")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let project_id = args.get("projectId").and_then(|v| v.as_str());
    let checkpoint = args
        .get("checkpoint")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let summary = SessionSummary {
        summary: summary_text.to_string(),
        goal,
        instructions,
        accomplished,
        discoveries,
        files_modified,
    };

    let session = if checkpoint {
        lib.checkpoint_session(session_id, summary, project_id)
            .await
            .map_err(|e| e.to_string())?
    } else {
        lib.end_session(session_id, summary, project_id)
            .await
            .map_err(|e| e.to_string())?
    };

    if !checkpoint {
        *last_session_id = None;
    }

    let status = if checkpoint { "active" } else { "completed" };

    Ok(serde_json::json!({
        "sessionId": session.id.to_string(),
        "status": status,
        "checkpoint": checkpoint,
        "endedAt": session.ended_at.map(|t| t.to_rfc3339()),
    })
    .to_string())
}

async fn tool_merge_projects(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let source = args
        .get("sourceProjectId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let target = args
        .get("targetProjectId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let dry_run = args.get("dryRun").and_then(|v| v.as_bool()).unwrap_or(true);

    if source.is_empty() || target.is_empty() {
        return Err("sourceProjectId and targetProjectId are required".into());
    }
    if source == target {
        return Ok(serde_json::json!({
            "dryRun": dry_run,
            "source": source,
            "target": target,
            "entries": 0,
            "sessions": 0,
            "message": "Source and target are identical — no-op.",
        })
        .to_string());
    }

    if dry_run {
        let counts = lib
            .preview_merge(&source)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({
            "dryRun": true,
            "source": source,
            "target": target,
            "entries": counts.entries,
            "sessions": counts.sessions,
            "message": format!(
                "Would merge {} entries and {} sessions. Call with dryRun: false to execute.",
                counts.entries, counts.sessions
            ),
        })
        .to_string());
    }

    let counts = lib
        .merge_projects(&source, &target)
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "dryRun": false,
        "source": source,
        "target": target,
        "entriesMerged": counts.entries,
        "sessionsMerged": counts.sessions,
        "message": format!(
            "Merged {} entries and {} sessions from '{}' into '{}'.",
            counts.entries, counts.sessions, source, target
        ),
    })
    .to_string())
}

async fn tool_related(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let id_str = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let id: Uuid = id_str
        .parse()
        .map_err(|_| format!("invalid UUID: {id_str}"))?;
    let direction = args.get("direction").and_then(|v| v.as_str());

    let edges = lib
        .get_edges(id, direction)
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<Value> = edges
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "fromId": e.from_id.to_string(),
                "toId": e.to_id.to_string(),
                "type": serde_json::to_value(e.edge_type).unwrap(),
                "strength": e.strength,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "count": items.len(),
        "edges": items,
    })
    .to_string())
}

async fn tool_stats(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    let project_id = args.get("projectId").and_then(|v| v.as_str());
    let namespace = args.get("namespace").and_then(|v| v.as_str());

    // Explicit scope (projectId acts as the namespace, matching the write
    // invariant) → single-namespace stats. No scope → global aggregate.
    if let Some(scope) = namespace.or(project_id) {
        let stats = lib
            .get_stats(Some(scope))
            .await
            .map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({
            "scope": scope,
            "totalEntries": stats.total_entries,
            "totalSessions": stats.total_sessions,
            "entriesByType": stats.entries_by_type,
            "entriesByLayer": stats.entries_by_layer,
            "namespaces": stats.namespaces,
        })
        .to_string());
    }

    let stats = lib.get_stats_global().await.map_err(|e| e.to_string())?;
    Ok(serde_json::json!({
        "scope": "all",
        "totalEntries": stats.total_entries,
        "totalSessions": stats.total_sessions,
        "entriesByType": stats.entries_by_type,
        "entriesByLayer": stats.entries_by_layer,
        "byNamespace": stats.by_namespace,
    })
    .to_string())
}

async fn tool_debug(args: &Value, lib: &MemoryLibrarian) -> Result<String, String> {
    if args.get("prune").and_then(|v| v.as_bool()).unwrap_or(false) {
        let pruned = lib.prune_debug_log(7).await.map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({ "pruned": pruned }).to_string());
    }

    let event = match args.get("event").and_then(|v| v.as_str()) {
        Some(s) => Some(
            serde_json::from_value::<DebugEvent>(serde_json::Value::String(s.to_string()))
                .map_err(|_| format!("unknown event type: {s}"))?,
        ),
        None => None,
    };
    let entry_id = args
        .get("entryId")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());
    let since = args
        .get("since")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

    let entries = lib
        .query_debug_log(DebugLogQuery {
            event,
            entry_id,
            since,
            limit,
        })
        .await
        .map_err(|e| e.to_string())?;

    let logging_active = crate::debug::enabled();
    Ok(serde_json::json!({
        "loggingEnabled": logging_active,
        "note": if logging_active { serde_json::Value::Null } else {
            serde_json::json!("logging disabled — set RUNAR_DEBUG=true (and restart the MCP server) to record events")
        },
        "count": entries.len(),
        "entries": entries,
    })
    .to_string())
}

// ── Curator tool handlers ──────────────────────────────────────────

async fn tool_curator_ask(args: &Value, curator: &CuratorOracle) -> Result<String, String> {
    let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
    let project_id = args.get("projectId").and_then(|v| v.as_str());

    let answer = curator
        .ask(question, project_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string(&answer).unwrap_or_default())
}

async fn tool_curator_topics(args: &Value, curator: &CuratorOracle) -> Result<String, String> {
    let project_id = args.get("projectId").and_then(|v| v.as_str());
    let topics = curator
        .get_topics(project_id)
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "count": topics.len(),
        "topics": topics,
    })
    .to_string())
}

async fn tool_curator_decisions(args: &Value, curator: &CuratorOracle) -> Result<String, String> {
    let project_id = args.get("projectId").and_then(|v| v.as_str());
    let decisions = curator
        .get_decisions(project_id)
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<Value> = decisions
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "title": e.title,
                "content": e.content,
                "createdAt": e.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "count": items.len(),
        "decisions": items,
    })
    .to_string())
}

async fn tool_curator_techdebt(args: &Value, curator: &CuratorOracle) -> Result<String, String> {
    let project_id = args.get("projectId").and_then(|v| v.as_str());
    let debts = curator
        .get_tech_debt(project_id)
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<Value> = debts
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "title": e.title,
                "content": e.content,
                "createdAt": e.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "count": items.len(),
        "techDebt": items,
    })
    .to_string())
}

async fn tool_curator_onboard(args: &Value, curator: &CuratorOracle) -> Result<String, String> {
    let project_id = args.get("projectId").and_then(|v| v.as_str());
    let report = curator
        .onboard(project_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string(&report).unwrap_or_default())
}

// ── Huginn tool handlers ───────────────────────────────────────────

async fn tool_huginn_crawl(
    args: &Value,
    librarian: &Arc<MemoryLibrarian>,
) -> Result<String, String> {
    use crate::huginn::{CrawlMode, CrawlOrchestrator};

    let project_path = args
        .get("projectPath")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectPath is required".to_string())?;
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .map(CrawlMode::parse)
        .unwrap_or(CrawlMode::Auto);
    let focus = args.get("focus").and_then(|v| v.as_str()).map(String::from);
    let deep = args.get("deep").and_then(|v| v.as_bool()).unwrap_or(true);

    let root = std::path::Path::new(project_path)
        .canonicalize()
        .map_err(|e| format!("invalid path: {e}"))?;
    let orchestrator = CrawlOrchestrator::new(librarian, project_id, mode, focus).with_deep(deep);
    let result = orchestrator.run(&root).await.map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "projectId": result.project_id,
        "mode": format!("{:?}", result.mode),
        "totalFiles": result.total_files,
        "analyzedDeep": result.analyzed_deep,
        "analyzedMedium": result.analyzed_medium,
        "analyzedLight": result.analyzed_light,
        "skipped": result.skipped,
        "patternsFound": result.patterns_found,
        "techDebtMarkers": result.techdebt_markers,
        "entriesSaved": result.entries_saved,
        "entriesDeprecated": result.entries_deprecated,
        "wholeProjectEntriesRefreshed": result.whole_project_entries_refreshed,
        "codeGraph": result.codegraph.as_ref().map(|cg| serde_json::json!({
            "symbols": cg.symbols,
            "edges": cg.edges,
            "filesParsed": cg.files_indexed + cg.files_partial,
            "filesPartial": cg.files_partial,
            "filesReused": cg.files_reused,
            "filesNotParseable": cg.files_skipped,
            "filesErrored": cg.files_errored,
            "unresolvedCalls": cg.unresolved_calls,
        })),
    })
    .to_string())
}

async fn tool_huginn_status(
    args: &Value,
    librarian: &Arc<MemoryLibrarian>,
) -> Result<String, String> {
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;

    let entries = librarian
        .list(ListFilters {
            project_id: Some(project_id.to_string()),
            namespace: Some(project_id.to_string()),
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;

    let mut by_type: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut latest_arch: Option<&MemoryEntry> = None;
    for e in &entries {
        *by_type
            .entry(e.entry_type.as_str().to_string())
            .or_default() += 1;
        if e.entry_type == EntryType::Architecture
            && (e.title.starts_with("Architecture summary")
                || e.title.starts_with("Architecture pattern"))
            && latest_arch.is_none_or(|a| e.created_at > a.created_at)
        {
            latest_arch = Some(e);
        }
    }

    // Absence of a code graph is normal — it is opt-in via `--deep` — so it is
    // reported as a null rather than an error.
    let code_graph = open_graph(project_id)
        .ok()
        .and_then(|s| s.coverage(project_id).ok())
        .map(|c| crate::codegraph::format::coverage_json(&c));

    Ok(serde_json::json!({
        "projectId": project_id,
        "totalEntries": entries.len(),
        "byType": by_type,
        "lastCrawl": latest_arch.map(|e| e.created_at.to_rfc3339()),
        "codeGraph": code_graph,
    })
    .to_string())
}

/// Open the code graph read-only. A read must never create or migrate the file.
///
/// The project is checked too: once any project has a graph the file exists, so
/// a file-only check answers "nothing found" for a project that was simply
/// never crawled deep.
fn open_graph(project: &str) -> Result<crate::codegraph::store::CodeGraphStore, String> {
    use crate::codegraph::store::CodeGraphStore;
    let path = CodeGraphStore::default_path();
    if !path.exists() {
        return Err(
            "no code graph yet — run huginn_crawl with \"deep\": true for this project".to_string(),
        );
    }
    let store = CodeGraphStore::open_readonly(&path).map_err(|e| e.to_string())?;
    let known = store.projects().map_err(|e| e.to_string())?;
    if !known.iter().any(|p| p == project) {
        return Err(format!(
            "project {project} has no code graph — run huginn_crawl with \"deep\": true for it. \
             Projects that do: {}",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ));
    }
    Ok(store)
}

/// Labels the graph actually stores. An unrecognised one would silently match
/// nothing and read as "this symbol does not exist".
const SYMBOL_LABELS: &[&str] = &[
    "Function",
    "Method",
    "Class",
    "Interface",
    "Trait",
    "Struct",
    "Enum",
    "Type",
    "Const",
    "Module",
];

fn wants_json(args: &Value) -> bool {
    args.get("format").and_then(|v| v.as_str()) == Some("json")
}

fn arg_usize(args: &Value, key: &str, default: usize, max: usize) -> usize {
    args.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| (n as usize).clamp(1, max))
        .unwrap_or(default)
}

async fn tool_huginn_search_graph(args: &Value) -> Result<String, String> {
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "query is required".to_string())?;
    let label = args.get("label").and_then(|v| v.as_str());
    if let Some(l) = label {
        if !SYMBOL_LABELS.contains(&l) {
            return Err(format!(
                "unknown label {l}; expected one of {}",
                SYMBOL_LABELS.join(", ")
            ));
        }
    }
    let limit = arg_usize(args, "limit", 12, 100);

    let store = open_graph(project_id)?;
    let rows = store
        .search(project_id, query, label, limit)
        .map_err(|e| e.to_string())?;

    if wants_json(args) {
        return Ok(crate::codegraph::format::symbols_json(&rows).to_string());
    }
    Ok(crate::codegraph::format::symbols(&rows))
}

async fn tool_huginn_symbol(args: &Value) -> Result<String, String> {
    use crate::codegraph::store::Direction;

    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;
    let name = args
        .get("symbol")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "symbol is required".to_string())?;

    let store = open_graph(project_id)?;
    let found = store
        .symbol(project_id, name, 8)
        .map_err(|e| e.to_string())?;
    let Some(row) = found.first() else {
        return Ok(format!("no definition named {name}"));
    };
    // Several definitions share the name: name them rather than picking one,
    // and state how many there are rather than how many fitted the limit.
    if found.total > 1 {
        if wants_json(args) {
            return Ok(serde_json::json!({
                "ambiguous": true,
                "total": found.total,
                "shown": found.rows.len(),
                "candidates": crate::codegraph::format::symbols_json(&found.rows),
            })
            .to_string());
        }
        return Ok(format!(
            "{name} is ambiguous — {} definitions:\n{}",
            found.total,
            crate::codegraph::format::matches(&found)
        ));
    }

    let callers = store
        .neighbors(row.id, Direction::Callers)
        .map_err(|e| e.to_string())?;
    let callees = store
        .neighbors(row.id, Direction::Callees)
        .map_err(|e| e.to_string())?;

    if wants_json(args) {
        return Ok(serde_json::json!({
            "symbol": crate::codegraph::format::symbol_json(row),
            "callers": crate::codegraph::format::neighbors_json(&callers),
            "callees": crate::codegraph::format::neighbors_json(&callees),
        })
        .to_string());
    }
    Ok(crate::codegraph::format::symbol_detail(
        row, &callers, &callees,
    ))
}

async fn tool_huginn_trace(args: &Value) -> Result<String, String> {
    use crate::codegraph::store::Direction;

    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;
    let from = args
        .get("from")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "from is required".to_string())?;
    let dir_arg = args
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("callers");
    let direction = Direction::parse(dir_arg)
        .ok_or_else(|| format!("direction must be callers or callees, got {dir_arg}"))?;
    let depth = arg_usize(args, "depth", 2, 5);
    let limit = arg_usize(args, "limit", 50, 200);

    let store = open_graph(project_id)?;
    let found = store
        .symbol(project_id, from, 8)
        .map_err(|e| e.to_string())?;
    let Some(row) = found.first() else {
        return Ok(format!("no definition named {from}"));
    };
    if found.total > 1 {
        return Ok(format!(
            "{from} is ambiguous — {} definitions, qualify it:\n{}",
            found.total,
            crate::codegraph::format::matches(&found)
        ));
    }

    let reached = store
        .trace(row.id, direction, depth, limit)
        .map_err(|e| e.to_string())?;

    if wants_json(args) {
        return Ok(serde_json::json!({
            "from": crate::codegraph::format::symbol_json(row),
            "direction": dir_arg,
            "depth": depth,
            "truncated": reached.truncated,
            "reached": crate::codegraph::format::neighbors_json(&reached.nodes),
        })
        .to_string());
    }
    Ok(format!(
        "{} {dir_arg} of {} within {depth} hop(s)\n{}",
        reached.nodes.len(),
        row.qualified_name,
        crate::codegraph::format::reached(dir_arg, &reached)
    ))
}

async fn tool_huginn_architecture(
    args: &Value,
    librarian: &Arc<MemoryLibrarian>,
) -> Result<String, String> {
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;

    let entries = librarian
        .list(ListFilters {
            entry_type: Some(EntryType::Architecture),
            project_id: Some(project_id.to_string()),
            namespace: Some(project_id.to_string()),
            limit: Some(50),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;

    let summary = entries
        .iter()
        .filter(|e| {
            e.title.starts_with("Architecture summary")
                || e.title.starts_with("Architecture pattern")
        })
        .max_by_key(|e| e.created_at);

    match summary {
        Some(e) => Ok(serde_json::json!({
            "projectId": project_id,
            "title": e.title,
            "content": e.content,
            "createdAt": e.created_at.to_rfc3339(),
        })
        .to_string()),
        None => Ok(serde_json::json!({
            "projectId": project_id,
            "message": "no architecture summary yet — run huginn_crawl first",
        })
        .to_string()),
    }
}

async fn tool_huginn_techdebt(
    args: &Value,
    librarian: &Arc<MemoryLibrarian>,
) -> Result<String, String> {
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;
    let type_filter = args
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("all")
        .to_lowercase();

    let entries = librarian
        .list(ListFilters {
            entry_type: Some(EntryType::TechDebt),
            project_id: Some(project_id.to_string()),
            namespace: Some(project_id.to_string()),
            limit: Some(500),
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;

    let filtered: Vec<&MemoryEntry> = entries
        .iter()
        .filter(|e| type_filter == "all" || e.tags.iter().any(|t| t.to_lowercase() == type_filter))
        .collect();

    let items: Vec<Value> = filtered
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id.to_string(),
                "title": e.title,
                "content": e.content,
                "tags": e.tags,
                "createdAt": e.created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "projectId": project_id,
        "filter": type_filter,
        "count": items.len(),
        "items": items,
    })
    .to_string())
}

/// Project root as recorded by the last crawl, if there is one.
async fn crawl_root_for(
    librarian: &Arc<MemoryLibrarian>,
    project_id: &str,
) -> Option<std::path::PathBuf> {
    let key = crate::huginn::crawl::crawl_state_key_for(project_id);
    let entry = librarian
        .get_by_topic_key(Some(project_id), &key)
        .await
        .ok()??;
    let state = crate::huginn::git::deserialize_state(&entry.content)?;
    let root = std::path::PathBuf::from(state.project_root);
    root.exists().then_some(root)
}

async fn tool_huginn_recrawl_file(
    args: &Value,
    librarian: &Arc<MemoryLibrarian>,
) -> Result<String, String> {
    use crate::huginn::analysis::{file_analyzer, memory_entry_generator};
    use crate::huginn::analyzer::AnalysisDepth;
    use crate::huginn::graph::DependencyGraph;
    use crate::huginn::scanner::FileEntry;

    let file_path = args
        .get("filePath")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "filePath is required".to_string())?;
    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;

    let abs = std::path::Path::new(file_path)
        .canonicalize()
        .map_err(|e| format!("invalid path: {e}"))?;
    let metadata = std::fs::metadata(&abs).map_err(|e| e.to_string())?;
    let extension = abs
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();
    let line_count = std::fs::read_to_string(&abs)
        .map(|s| s.lines().count())
        .unwrap_or(0);

    // Entry topic keys are built from the project-relative path, so this has
    // to resolve to the same root the crawl used or the write lands as a
    // duplicate instead of superseding. Prefer the root the crawl recorded.
    let root = crawl_root_for(librarian, project_id)
        .await
        .or_else(|| crate::huginn::crawl::infer_project_root(&abs));
    let relative_path = root
        .as_ref()
        .and_then(|r| abs.strip_prefix(r).ok())
        .map(|p| p.to_string_lossy().to_string())
        .ok_or_else(|| {
            format!(
                "cannot resolve {} relative to project {project_id}; run huginn_crawl for this project first",
                abs.display()
            )
        })?;

    let entry = FileEntry {
        path: abs.clone(),
        relative_path,
        size: metadata.len(),
        line_count,
        last_modified: metadata.modified().ok(),
        extension,
    };

    // Single-file recrawl uses an empty graph; relationships will be missing
    // but content/responsibilities/patterns/techdebt still extract.
    let graph = DependencyGraph::default();
    let analysis = file_analyzer::analyze_file(&entry, AnalysisDepth::Deep, &graph);

    let mut saved = 0usize;
    let mut duplicates = 0usize;
    let mut superseded = 0usize;
    for input in memory_entry_generator::file_entries(&analysis, project_id) {
        if let Ok(result) = librarian.propose(input).await {
            match result.action {
                SaveAction::Duplicate => duplicates += 1,
                SaveAction::Updated => {
                    saved += 1;
                    superseded += 1;
                }
                _ => saved += 1,
            }
        }
    }

    Ok(serde_json::json!({
        "filePath": file_path,
        "projectId": project_id,
        "depth": format!("{:?}", analysis.depth),
        "entriesSaved": saved,
        "duplicates": duplicates,
        "superseded": superseded,
    })
    .to_string())
}

async fn tool_huginn_benchmark(
    args: &Value,
    curator: &Arc<CuratorOracle>,
) -> Result<String, String> {
    use crate::huginn::benchmark;

    let project_id = args
        .get("projectId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "projectId is required".to_string())?;
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("quick");
    let quick = mode != "full";

    let result = benchmark::run(curator, project_id, quick)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::to_string(&result).map_err(|e| e.to_string())
}

async fn tool_huginn_skills() -> Result<String, String> {
    Ok(serde_json::json!({
        "skills": [
            { "name": "muninn_save", "when": "After completing a fix, decision, or learning a pattern" },
            { "name": "muninn_search", "when": "Before starting work — check for existing knowledge" },
            { "name": "muninn_context", "when": "After context compaction or session reset" },
            { "name": "huginn_crawl", "when": "First-time setup of a project, or after large refactors" },
            { "name": "huginn_status", "when": "Check whether a project has been crawled" },
            { "name": "huginn_architecture", "when": "Get the latest architecture summary" },
            { "name": "huginn_techdebt", "when": "Inventory TODOs/FIXMEs in a project" },
            { "name": "huginn_search_graph", "when": "Find where a function, type or method is defined" },
            { "name": "huginn_symbol", "when": "Read one definition's signature, complexity, callers and callees" },
            { "name": "huginn_trace", "when": "Find the blast radius of a change, or what a definition depends on" },
            { "name": "huginn_recrawl_file", "when": "Refresh a single file after editing it" },
            { "name": "huginn_benchmark", "when": "Measure whether stored memory answers real questions" },
            { "name": "curator_ask", "when": "Ask a synthesized question about the codebase" },
            { "name": "curator_onboard", "when": "Producing a first-time onboarding report for a project" },
        ]
    }).to_string())
}

// ── Tool definitions ───────────────────────────────────────────────

fn tool_definitions() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "muninn_verify".into(),
            description: "Owner-endorse a memory entry. Sets `verified=true` and grants a ranking bonus in fused search so the entry outranks un-verified peers at similar relevance. Use when reviewing an agent-generated memory that is accurate and worth surfacing first. Independent from `confidence`.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Entry UUID to mark verified" }
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_capture_passive".into(),
            description: "Parse a markdown blob for a `## Key Learnings:` (or `## Learnings`, `## Takeaways`) section and save each bulleted / numbered item as its own memory entry. One call fans out into many entries. Dedup is stable — re-saving the same text upserts instead of duplicating. Use at end-of-turn when you have multiple discrete learnings to persist.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "Markdown text containing a `## Key Learnings:` section" },
                    "projectId": { "type": "string", "description": "Project this memory belongs to (optional)" },
                    "type": {
                        "type": "string",
                        "enum": ["decision", "pattern", "bug", "rule", "business-rule", "architecture", "tech-debt", "session", "context", "preference", "note"],
                        "description": "Entry type applied to every parsed item (default: note)"
                    },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Extra tags appended to the default `auto-capture, key-learning` set" }
                },
                "required": ["text"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_save".into(),
            description: "Save a memory entry. Use after completing significant work — bug fixes, architectural decisions, patterns discovered, business rules learned.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short descriptive title (5-200 chars)" },
                    "content": { "type": "string", "description": "Full content of the memory (10-10,000 chars)" },
                    "type": {
                        "type": "string",
                        "enum": ["decision", "pattern", "bug", "rule", "business-rule", "architecture", "tech-debt", "session", "context", "preference", "note", "auto-change", "user-prompt"],
                        "description": "Category of this memory"
                    },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Optional tags for filtering (max 10)" },
                    "projectId": { "type": "string", "description": "Optional project this memory belongs to" },
                    "topicKey": { "type": "string", "description": "Optional stable key for upsert. Format: \"scope:key\" e.g. \"auth:approach\"" },
                    "confidence": {
                        "description": "Source confidence multiplier applied to search ranking. Preset string (verified=1.0, observed=0.9, inferred=0.7, speculative=0.4) or a numeric 0..1 value. Default: observed.",
                        "oneOf": [
                            { "type": "string", "enum": ["verified", "observed", "inferred", "speculative"] },
                            { "type": "number", "minimum": 0, "maximum": 1 }
                        ]
                    },
                    "author": { "type": "string", "description": "Override the auto-resolved author (from `git config user.name`). Use when saving on behalf of a named user." },
                },
                "required": ["title", "content", "type"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_search".into(),
            description: "Search memory using fused retrieval (semantic + keyword + decay + recency). Returns compact results by default (id+title+snippet). Every result carries `tokensEstimate` (cost of the snippet form) and `fullTokensEstimate` (cost if you fetch it via muninn_get). Prefer search→get for large batches: compact rows cost ~20-50 tokens each, full entries often 200-2000. Set compact=false only when you know you want every byte.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural language search query" },
                    "limit": { "type": "number", "description": "Maximum results (default: 10, max: 50)" },
                    "compact": { "type": "boolean", "description": "Return compact results with snippet instead of full content (default: true). Use muninn_get for full content." },
                    "type": { "type": "string", "description": "Filter by memory type (optional)" },
                    "projectId": { "type": "string", "description": "Filter by project (optional)" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Filter by tags (optional)" },
                    "author": { "type": "string", "description": "Case-insensitive substring filter on author (optional)" },
                },
                "required": ["query"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_context".into(),
            description: "Get recent context from previous sessions. Call this after any context compaction or session reset.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Get context for specific project (optional)" },
                    "sessionCount": { "type": "number", "description": "Number of recent sessions to include (default: 3)" },
                },
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_get".into(),
            description: "Get the full content of a specific memory entry by ID. Response includes `tokensEstimate`. Typically 200-2000 tokens per entry — only fetch when the snippet from muninn_search was insufficient.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Memory entry ID" },
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_timeline".into(),
            description: "Get entries created around the same time as a specific entry.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Reference entry ID" },
                    "window": { "type": "number", "description": "Hours before/after to include (default: 24)" },
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_session_start".into(),
            description: "Register the start of a work session.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "What you plan to work on" },
                    "projectId": { "type": "string", "description": "Which project (optional)" },
                    "tool": { "type": "string", "description": "AI tool being used (claude-code, cursor, etc.)" },
                },
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_session_end".into(),
            description: "Save an end-of-session summary. Call before ending a session. Structured fields (goal, instructions, accomplished, discoveries, filesModified) are recommended over free-form summary alone — they retrieve better and double as defaults for the next session's context injection.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "Session ID from muninn_session_start" },
                    "summary": { "type": "string", "description": "One-paragraph prose summary of the session" },
                    "goal": { "type": "string", "description": "What the user was trying to accomplish (one line)" },
                    "instructions": { "type": "array", "items": { "type": "string" }, "description": "Explicit constraints / directives the user gave, in order" },
                    "accomplished": { "type": "array", "items": { "type": "string" }, "description": "Concrete tasks finished (e.g. \"added retry to auth middleware\")" },
                    "discoveries": { "type": "array", "items": { "type": "string" }, "description": "Gotchas, surprises, newly-learned constraints" },
                    "filesModified": { "type": "array", "items": { "type": "string" }, "description": "Paths of files changed" },
                    "projectId": { "type": "string", "description": "Project identifier" },
                    "checkpoint": {
                        "type": "boolean",
                        "description": "Save summary without ending session. Use for mid-session progress saves. Default: false"
                    },
                },
                "required": ["summary"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_related".into(),
            description: "Get edges (relationships) for a memory entry.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Memory entry ID" },
                    "direction": { "type": "string", "enum": ["from", "to", "both"], "description": "Edge direction (default: both)" },
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_stats".into(),
            description: "Get memory system statistics. Without arguments: totals across every namespace plus a per-namespace breakdown. Pass projectId (or namespace) to scope to one project.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Scope stats to this project (its namespace)" },
                    "namespace": { "type": "string", "description": "Explicit namespace override (rarely needed)" },
                },
            }),
        },
        ToolInfo {
            name: "muninn_merge_projects".into(),
            description: "Merge all memory entries and sessions from one project namespace into another. Use dryRun to preview counts before committing.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "sourceProjectId": { "type": "string", "description": "Project ID to merge FROM (will be emptied)" },
                    "targetProjectId": { "type": "string", "description": "Project ID to merge INTO" },
                    "dryRun": { "type": "boolean", "description": "Preview affected counts without making changes (default: true)" },
                },
                "required": ["sourceProjectId", "targetProjectId"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "muninn_debug".into(),
            description: "Query the debug log for internal operation traces. Requires RUNAR_DEBUG=true.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "event": { "type": "string", "enum": ["search_scoring", "decay_compute", "auto_link", "layer_graduation", "dedup_decision", "touch_promotion", "hook_timing", "injection"], "description": "Filter by event type" },
                    "limit": { "type": "number", "description": "Max entries (default: 20)" },
                    "since": { "type": "string", "description": "ISO timestamp" },
                    "entryId": { "type": "string", "description": "Filter by entry ID" },
                    "prune": { "type": "boolean", "description": "Delete entries older than 7 days" },
                },
                "additionalProperties": false,
            }),
        },
        // ── Curator tools ──────────────────────────────────────────
        ToolInfo {
            name: "curator_ask".into(),
            description: "Ask a question about the codebase. Uses memory to assemble context and synthesize an answer.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "Natural language question about the codebase" },
                    "projectId": { "type": "string", "description": "Project to scope the question to (optional)" },
                },
                "required": ["question"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "curator_topics".into(),
            description: "Discover what knowledge topics exist in memory, grouped by type.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Filter by project (optional)" },
                },
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "curator_decisions".into(),
            description: "List architectural and technical decisions stored in memory.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Filter by project (optional)" },
                },
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "curator_techdebt".into(),
            description: "List known technical debt items from memory.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Filter by project (optional)" },
                },
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "curator_onboard".into(),
            description: "Generate a multi-section onboarding report (architecture, decisions, patterns, rules, tech debt) for a project.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Project to onboard into (optional)" },
                },
                "additionalProperties": false,
            }),
        },
        // ── Huginn tools ───────────────────────────────────────────
        ToolInfo {
            name: "huginn_crawl".into(),
            description: "Crawl a project: scan files, build dependency graph, score importance, analyze depth-appropriate, extract patterns + tech-debt + architecture summary, save all to memory.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectPath": { "type": "string", "description": "Absolute path to project root" },
                    "projectId": { "type": "string", "description": "Project identifier (used as namespace)" },
                    "mode": { "type": "string", "enum": ["full", "incremental", "auto"], "description": "Crawl mode (default: auto)" },
                    "focus": { "type": "string", "description": "Optional subpath to focus crawl on" },
                    "deep": { "type": "boolean", "description": "Build the symbol-level code graph: definitions, call edges and per-function complexity (default: true; unchanged files are reused, so a re-crawl is cheap)" },
                },
                "required": ["projectPath", "projectId"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_status".into(),
            description: "Show crawl status for a project: total entries, breakdown by type, last crawl time.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Project identifier" },
                },
                "required": ["projectId"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_architecture".into(),
            description: "Return the latest architecture summary for a project.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Project identifier" },
                },
                "required": ["projectId"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_techdebt".into(),
            description: "List tech-debt entries (TODO, FIXME, HACK, XXX) found by the crawler.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Project identifier" },
                    "type": { "type": "string", "enum": ["todo", "fixme", "hack", "xxx", "all"], "description": "Filter by marker type (default: all)" },
                },
                "required": ["projectId"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_recrawl_file".into(),
            description: "Re-analyze a single file and update its memory entry. Useful after editing a file.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "Absolute path to the file" },
                    "projectId": { "type": "string", "description": "Project identifier" },
                },
                "required": ["filePath", "projectId"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_benchmark".into(),
            description: "Run a memory-quality benchmark by asking the Curator a fixed bank of questions and grading answers (deterministic scoring — no LLM call). quick = 10 questions, full = 30.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Project identifier" },
                    "mode": { "type": "string", "enum": ["quick", "full"], "description": "Question set size (default: quick)" },
                },
                "required": ["projectId"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_search_graph".into(),
            description: "Find definitions by name in the code graph. Returns qualified name, kind, file:line, fan-in and cyclomatic complexity, ranked by relevance. Needs a crawl run with deep=true. Absence of a result is not proof the symbol does not exist — check huginn_status for what the graph could not parse.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Project identifier" },
                    "query": { "type": "string", "description": "Name or word to search for; camelCase and snake_case parts both match" },
                    "label": { "type": "string", "enum": ["Function", "Method", "Class", "Interface", "Trait", "Struct", "Enum", "Type", "Const", "Module"], "description": "Restrict to one kind of definition" },
                    "limit": { "type": "integer", "description": "Max results (default 12)" },
                    "format": { "type": "string", "enum": ["text", "json"], "description": "text (default, compact) or json" },
                },
                "required": ["projectId", "query"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_symbol".into(),
            description: "One definition in full: signature, location, complexity metrics, and its immediate callers and callees. Each edge names how it was resolved (import_map, same_module, unique_name, suffix) — that is how confident the match is.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Project identifier" },
                    "symbol": { "type": "string", "description": "Qualified name (path:Type.method), bare name, or Type.method" },
                    "format": { "type": "string", "enum": ["text", "json"] },
                },
                "required": ["projectId", "symbol"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_trace".into(),
            description: "Walk the call graph outward from a definition to find what would be affected by changing it (direction=callers) or what it depends on (direction=callees). Breadth-first, nearest first.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "projectId": { "type": "string", "description": "Project identifier" },
                    "from": { "type": "string", "description": "Definition to start from; same forms as huginn_symbol" },
                    "direction": { "type": "string", "enum": ["callers", "callees"], "description": "callers = blast radius (default), callees = dependencies" },
                    "depth": { "type": "integer", "description": "Hops to follow, 1-5 (default 2)" },
                    "limit": { "type": "integer", "description": "Max definitions returned (default 50)" },
                    "format": { "type": "string", "enum": ["text", "json"] },
                },
                "required": ["projectId", "from"],
                "additionalProperties": false,
            }),
        },
        ToolInfo {
            name: "huginn_skills".into(),
            description: "Static list of skills/best-practices for using Muninn + Huginn from Claude Code.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_err_heuristic_matches_real_errors() {
        assert!(looks_like_storage_err("pool error: timeout"));
        assert!(looks_like_storage_err("Database error: connection refused"));
        assert!(looks_like_storage_err("SQLite disk full"));
        assert!(looks_like_storage_err("Postgres query failed"));
        assert!(!looks_like_storage_err("missing required field: title"));
        assert!(!looks_like_storage_err("unknown tool: muninn_foo"));
        assert!(!looks_like_storage_err("invalid UUID: not-a-uuid"));
    }

    #[test]
    fn suggest_topic_key_basic() {
        assert_eq!(
            suggest_topic_key("bug", "JWT refresh token expiry too short"),
            "bug:jwt-refresh-token-expiry"
        );
    }

    #[test]
    fn suggest_topic_key_four_words() {
        assert_eq!(
            suggest_topic_key("decision", "Use PostgreSQL for production"),
            "decision:use-postgresql-for-production"
        );
    }

    #[test]
    fn suggest_topic_key_strips_special_chars() {
        // Parens, colons, exclamation marks stripped; words still counted by whitespace
        assert_eq!(
            suggest_topic_key("pattern", "Auth (v2) middleware: validates tokens!"),
            "pattern:auth-v2-middleware-validates"
        );
    }

    #[test]
    fn suggest_topic_key_short_title() {
        assert_eq!(suggest_topic_key("note", "fix"), "note:fix");
    }

    #[test]
    fn suggest_topic_key_empty_title() {
        assert_eq!(suggest_topic_key("note", ""), "note:");
    }
}
