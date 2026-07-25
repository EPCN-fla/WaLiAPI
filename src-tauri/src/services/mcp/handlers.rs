use axum::{
    extract::{State, Query},
    http::{StatusCode, header},
    response::{Json, IntoResponse, Response},
    body::Body,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use crate::server::router::SharedState;
use crate::db::repository::Repository;
use crate::services::knowledge::{repository::KbRepository, embedder, retriever, rag};

// ── Session management for SSE transport ──────────────────────────
// Each SSE client gets a unique session_id. The POST handler uses
// the session_id to push JSON-RPC responses back through the SSE stream.

type SessionSender = mpsc::UnboundedSender<String>;

fn sse_sessions() -> &'static Arc<RwLock<HashMap<String, SessionSender>>> {
    static SESSIONS: std::sync::OnceLock<Arc<RwLock<HashMap<String, SessionSender>>>> = std::sync::OnceLock::new();
    SESSIONS.get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
}

// ── MCP JSON-RPC types ────────────────────────────────────────────

/// MCP JSON-RPC 2.0 request
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpRequest {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// MCP JSON-RPC 2.0 response
#[derive(Debug, Serialize)]
pub struct McpResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpError>,
}

#[derive(Debug, Serialize)]
pub struct McpError {
    code: i32,
    message: String,
}

impl McpResponse {
    pub fn success(id: Option<serde_json::Value>, result: serde_json::Value) -> Self {
        Self { jsonrpc: "2.0".to_string(), id, result: Some(result), error: None }
    }

    pub fn error(id: Option<serde_json::Value>, code: i32, message: String) -> Self {
        Self { jsonrpc: "2.0".to_string(), id, result: None, error: Some(McpError { code, message }) }
    }

    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// ── MCP tool definitions ──────────────────────────────────────────

fn get_tools() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "search_knowledge_base",
            "description": "Search the local knowledge base for relevant content. Returns matching text chunks with similarity scores.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query to find relevant content in the knowledge base"
                    },
                    "kb_id": {
                        "type": "string",
                        "description": "Knowledge base ID. If not provided, searches all active knowledge bases."
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Number of top results to return (default: 5)",
                        "default": 5
                    }
                },
                "required": ["query"]
            }
        }),
        serde_json::json!({
            "name": "list_knowledge_bases",
            "description": "List all available knowledge bases.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "read_document",
            "description": "Read the full content of a specific document in a knowledge base.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb_id": { "type": "string", "description": "Knowledge base ID" },
                    "doc_id": { "type": "string", "description": "Document ID" }
                },
                "required": ["kb_id", "doc_id"]
            }
        }),
        serde_json::json!({
            "name": "ask_knowledge_base",
            "description": "Ask a question to the knowledge base and get an AI-generated answer based on retrieved context.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question to ask" },
                    "kb_id": { "type": "string", "description": "Knowledge base ID. If not provided, uses all active KBs." },
                    "top_k": { "type": "integer", "description": "Number of chunks to retrieve (default: 5)", "default": 5 }
                },
                "required": ["question"]
            }
        }),
        serde_json::json!({
            "name": "get_knowledge_base_stats",
            "description": "Get statistics about a specific knowledge base.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kb_id": { "type": "string", "description": "Knowledge base ID" }
                },
                "required": ["kb_id"]
            }
        }),
    ]
}

// ── Core JSON-RPC dispatch ────────────────────────────────────────

/// Main MCP JSON-RPC handler — async dispatch
async fn dispatch_jsonrpc_async(
    shared: &SharedState,
    req: &McpRequest,
) -> McpResponse {
    match req.method.as_str() {
        "initialize" => {
            McpResponse::success(req.id.clone(), serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "WaLiAPI Knowledge Base",
                    "version": "0.1.0"
                }
            }))
        }
        "notifications/initialized" => {
            McpResponse::success(req.id.clone(), serde_json::json!({}))
        }
        "tools/list" => {
            McpResponse::success(req.id.clone(), serde_json::json!({
                "tools": get_tools()
            }))
        }
        "tools/call" => {
            let tool_name = req.params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("");

            let args = req.params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));

            match handle_tool_call(shared, tool_name, &args).await {
                Ok(result) => McpResponse::success(req.id.clone(), result),
                Err(e) => McpResponse::error(req.id.clone(), -32603, e),
            }
        }
        "ping" => {
            McpResponse::success(req.id.clone(), serde_json::json!({}))
        }
        _ => {
            McpResponse::error(req.id.clone(), -32601, format!("Unknown method: {}", req.method))
        }
    }
}

// ── SSE endpoint: GET /mcp/sse ────────────────────────────────────
// Standard MCP SSE transport:
// 1. Client opens SSE connection
// 2. Server sends `endpoint` event with POST URL (includes session_id)
// 3. Client POSTs JSON-RPC requests to that URL
// 4. Server pushes responses back through the SSE stream

pub async fn handle_mcp_sse(
    State(_shared): State<SharedState>,
) -> Response {
    // Generate unique session ID
    let session_id = uuid::Uuid::new_v4().to_string();

    // Create channel for this session
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    // Register session
    sse_sessions().write().await.insert(session_id.clone(), tx);

    // Build SSE stream
    let session_id_clone = session_id.clone();
    let stream = async_stream::stream! {
        // 1. Send endpoint event — tells client where to POST JSON-RPC
        let endpoint_url = format!("/mcp?session_id={}", session_id_clone);
        let endpoint_event = format!(
            "event: endpoint\ndata: {}\n\n",
            endpoint_url
        );
        yield Ok::<_, std::io::Error>(endpoint_event.into_bytes());

        // 2. Keep-alive loop + forward JSON-RPC responses
        let mut keepalive_interval = tokio::time::interval(std::time::Duration::from_secs(15));
        keepalive_interval.tick().await; // first tick is immediate

        loop {
            tokio::select! {
                // Forward JSON-RPC responses to client
                Some(msg) = rx.recv() => {
                    let sse_data = format!("data: {}\n\n", msg);
                    yield Ok::<_, std::io::Error>(sse_data.into_bytes());
                }
                // Keepalive
                _ = keepalive_interval.tick() => {
                    yield Ok::<_, std::io::Error>(b": keepalive\n\n".to_vec());
                }
            }
        }
    };

    // Clean up session when client disconnects (stream dropped)
    let session_id_cleanup = session_id.clone();
    let cleanup_sessions = sse_sessions().clone();
    tokio::spawn(async move {
        // Wait a bit then check if the sender is still registered
        // The stream drop will cause rx to be dropped, but tx remains in the map.
        // We use a periodic cleanup: if sending fails, the session is dead.
        // For simplicity, clean up after a long timeout.
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        cleanup_sessions.write().await.remove(&session_id_cleanup);
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

// ── POST endpoint: POST /mcp?session_id=xxx ───────────────────────
// Receives JSON-RPC requests and pushes responses through the SSE stream

#[derive(Debug, Deserialize)]
pub struct McpQueryParams {
    #[serde(default)]
    pub session_id: Option<String>,
}

pub async fn handle_mcp(
    State(shared): State<SharedState>,
    Query(params): Query<McpQueryParams>,
    body: axum::body::Bytes,
) -> Response {
    let body_str = String::from_utf8_lossy(&body);

    // Parse JSON-RPC request
    let req: McpRequest = match serde_json::from_str(&body_str) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("Invalid JSON: {}", e)).into_response();
        }
    };

    // Check if this is a notification (no id → no response)
    let is_notification = req.id.is_none();

    let response = dispatch_jsonrpc_async(&shared, &req).await;

    // If session_id is provided, push response through SSE stream
    if let Some(session_id) = &params.session_id {
        let sessions = sse_sessions().read().await;
        if let Some(tx) = sessions.get(session_id) {
            let _ = tx.send(response.to_json_string());
        }
    }

    // For SSE transport: return 202 Accepted (response goes through SSE)
    // For direct POST (no session_id): return JSON response directly
    if params.session_id.is_some() {
        if is_notification {
            return StatusCode::ACCEPTED.into_response();
        }
        // Response is sent via SSE, but also return 202
        return StatusCode::ACCEPTED.into_response();
    }

    // No session_id — return JSON directly (backwards compatible)
    Json(response).into_response()
}

// ── Tool call handlers ────────────────────────────────────────────

async fn handle_tool_call(
    shared: &SharedState,
    tool_name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let pool = &shared.state.db.pool;

    match tool_name {
        "search_knowledge_base" => {
            let query = args.get("query").and_then(|q| q.as_str()).ok_or("Missing query")?;
            let kb_id = args.get("kb_id").and_then(|k| k.as_str()).unwrap_or("");
            let top_k = args.get("top_k").and_then(|t| t.as_u64()).unwrap_or(5) as usize;

            let emb_model = if !kb_id.is_empty() {
                let kb_repo = KbRepository::new(pool.clone());
                kb_repo.get_kb(kb_id).await.ok()
                    .and_then(|kb| kb.embedding_model)
                    .unwrap_or_else(|| "text-embedding-3-small".to_string())
            } else {
                "text-embedding-3-small".to_string()
            };

            let repo = Repository::new(pool.clone());
            let embeddings = embedder::embed(&[query.to_string()], &emb_model, &repo).await?;
            if embeddings.is_empty() {
                return Err("Failed to embed query".to_string());
            }

            let results = if kb_id.is_empty() {
                retriever::search_all(pool, &embeddings[0], top_k, true).await?
            } else {
                retriever::search(pool, kb_id, &embeddings[0], top_k).await?
            };

            let content: Vec<serde_json::Value> = results.iter().map(|r| {
                serde_json::json!({
                    "type": "text",
                    "text": format!("[{}] (score: {:.2})\n{}", r.filename, r.score, r.content)
                })
            }).collect();

            Ok(serde_json::json!({
                "content": content,
                "isError": false
            }))
        }

        "list_knowledge_bases" => {
            let kb_repo = KbRepository::new(pool.clone());
            let kbs = kb_repo.get_all_kbs().await.map_err(|e| e.to_string())?;

            // Only expose KBs with mcp_enabled = 1
            let exposed: Vec<_> = kbs.iter().filter(|kb| kb.mcp_enabled == 1).collect();

            let content: Vec<serde_json::Value> = exposed.iter().map(|kb| {
                serde_json::json!({
                    "type": "text",
                    "text": format!("ID: {}\nName: {}\nDocuments: {}\nChunks: {}\nDescription: {}",
                        kb.id, kb.name, kb.doc_count, kb.chunk_count,
                        kb.description.as_deref().unwrap_or("N/A"))
                })
            }).collect();

            Ok(serde_json::json!({
                "content": content,
                "isError": false
            }))
        }

        "read_document" => {
            let _kb_id = args.get("kb_id").and_then(|k| k.as_str()).ok_or("Missing kb_id")?;
            let doc_id = args.get("doc_id").and_then(|d| d.as_str()).ok_or("Missing doc_id")?;

            let kb_repo = KbRepository::new(pool.clone());
            let doc = kb_repo.get_document(doc_id).await.map_err(|e| e.to_string())?;

            let content = if let Some(path) = &doc.file_path {
                std::fs::read_to_string(path).unwrap_or_else(|_| "Failed to read file".to_string())
            } else {
                "No file path available".to_string()
            };

            Ok(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!("File: {}\n\n{}", doc.filename, content)
                }],
                "isError": false
            }))
        }

        "ask_knowledge_base" => {
            let question = args.get("question").and_then(|q| q.as_str()).ok_or("Missing question")?;
            let kb_id = args.get("kb_id").and_then(|k| k.as_str()).unwrap_or("");
            let top_k = args.get("top_k").and_then(|t| t.as_u64()).unwrap_or(5) as usize;
            let chat_model = args.get("model").and_then(|m| m.as_str()).unwrap_or("gpt-4o");

            let emb_model = if !kb_id.is_empty() {
                let kb_repo = KbRepository::new(pool.clone());
                kb_repo.get_kb(kb_id).await.ok()
                    .and_then(|kb| kb.embedding_model)
                    .unwrap_or_else(|| "text-embedding-3-small".to_string())
            } else {
                "text-embedding-3-small".to_string()
            };

            let answer = rag::ask(pool, kb_id, question, &emb_model, chat_model, top_k, true, &shared.app).await?;

            let mut content = vec![serde_json::json!({
                "type": "text",
                "text": answer.answer
            })];

            for source in &answer.sources {
                content.push(serde_json::json!({
                    "type": "text",
                    "text": format!("Source: {} (score: {:.2})\n{}", source.filename, source.score, source.snippet)
                }));
            }

            Ok(serde_json::json!({
                "content": content,
                "isError": false
            }))
        }

        "get_knowledge_base_stats" => {
            let kb_id = args.get("kb_id").and_then(|k| k.as_str()).ok_or("Missing kb_id")?;

            let kb_repo = KbRepository::new(pool.clone());
            let kb = kb_repo.get_kb(kb_id).await.map_err(|e| e.to_string())?;
            let docs = kb_repo.get_documents(kb_id).await.map_err(|e| e.to_string())?;

            Ok(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!(
                        "Knowledge Base: {}\nDocuments: {} (ready: {})\nChunks: {}\nTotal Tokens: {}",
                        kb.name,
                        kb.doc_count,
                        docs.iter().filter(|d| d.status == "ready").count(),
                        kb.chunk_count,
                        kb.total_tokens
                    )
                }],
                "isError": false
            }))
        }

        _ => Err(format!("Unknown tool: {}", tool_name)),
    }
}
