//! rutracker-mcp — Model Context Protocol server over stdio.
//!
//! Phase 5 deliverable. We implement a minimal subset of the MCP protocol (initialize,
//! tools/list, tools/call) over JSON-RPC 2.0 on stdin/stdout. This avoids the pre-1.0
//! `rmcp` SDK and its API churn risk, at the cost of ~200 LOC of hand-rolled protocol
//! scaffolding. See plan §5 for the fallback rationale.
//!
//! Library-level handlers are test-driven with wiremock. The `rutracker-mcp` binary
//! in `main.rs` is a thin stdio loop over these handlers.

use rutracker_cli::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

pub mod dispatch;

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "rutracker-mcp";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// JSON-RPC 2.0 request.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// JSON-RPC 2.0 response (success or error).
#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Response {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// Handler wires an MCP request to a cli handler.
pub async fn handle_request(req: Request, cfg: &CliConfig) -> Response {
    let id = req.id.clone();

    // Notifications (no id) → no response per JSON-RPC spec. Ignore here.
    match req.method.as_str() {
        "initialize" => Response::ok(id, initialize_result()),
        "tools/list" => Response::ok(id, tools_list_result()),
        "tools/call" => match dispatch::dispatch_tool_call(&req.params, cfg).await {
            Ok(text) => Response::ok(id, tool_call_result(&text)),
            Err(e) => Response::err(id, -32000, format!("tool call failed: {e}")),
        },
        "ping" => Response::ok(id, json!({})),
        _ => Response::err(id, -32601, format!("method not found: {}", req.method)),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
    })
}

fn tools_list_result() -> Value {
    json!({ "tools": tool_schemas() })
}

fn tool_call_result(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

fn tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "search",
            "description": "Search RuTracker for torrents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query (Russian or English)." },
                    "category": { "type": "string", "description": "Optional forum/category ID." },
                    "sort_by": { "type": "string", "enum": ["seeders", "size", "downloads", "registered"], "default": "seeders" },
                    "order": { "type": "string", "enum": ["desc", "asc"], "default": "desc" },
                    "page": { "type": "integer", "minimum": 1, "default": 1 }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "get_topic",
            "description": "Get detailed information about a RuTracker topic.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic_id": { "type": "integer", "description": "Numeric topic ID." },
                    "include_comments": { "type": "boolean", "default": false },
                    "max_comment_pages": { "type": "integer", "minimum": 1, "default": 1 }
                },
                "required": ["topic_id"]
            }
        }),
        json!({
            "name": "browse_forum",
            "description": "List torrents in a forum/category without a query.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "category_id": { "type": "string" },
                    "page": { "type": "integer", "minimum": 1, "default": 1 },
                    "sort_by": { "type": "string", "default": "seeders" },
                    "order": { "type": "string", "default": "desc" }
                },
                "required": ["category_id"]
            }
        }),
        json!({
            "name": "list_categories",
            "description": "List all forum categories and subforums.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "download_torrent",
            "description": "Download a .torrent file to disk.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "topic_id": { "type": "integer" },
                    "dest_dir": { "type": "string", "description": "Destination directory (must resolve under $HOME or CWD)." }
                },
                "required": ["topic_id", "dest_dir"]
            }
        }),
    ]
}

/// Build a CliConfig suitable for MCP use: always text format, no file output, stdout
/// suppressed (critical — stdout is reserved for JSON-RPC frames), cookies loaded.
pub fn cli_config_for_mcp(base_url: String, cookies: HashMap<String, String>) -> CliConfig {
    CliConfig {
        base_url,
        format: OutputFormat::Text,
        out: None,
        cookies,
        emit_stdout: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const FORUM_FIXTURE: &[u8] = include_bytes!("../../parser/tests/fixtures/forum-sample.html");

    fn parse_req(body: &str) -> Request {
        serde_json::from_str(body).unwrap()
    }

    #[tokio::test]
    async fn test_initialize_returns_server_info() {
        let cfg = cli_config_for_mcp("https://example.test/forum/".into(), HashMap::new());
        let req = parse_req(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        let resp = handle_request(req, &cfg).await;
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "rutracker-mcp");
        assert!(result["protocolVersion"].is_string());
        assert!(result["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn test_all_five_tools_registered() {
        let cfg = cli_config_for_mcp("https://example.test/forum/".into(), HashMap::new());
        let req = parse_req(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#);
        let resp = handle_request(req, &cfg).await;
        let tools = resp.result.unwrap();
        let names: Vec<String> = tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        let expected = vec![
            "search".to_string(),
            "get_topic".to_string(),
            "browse_forum".to_string(),
            "list_categories".to_string(),
            "download_torrent".to_string(),
        ];
        assert_eq!(names, expected);
    }

    #[tokio::test]
    async fn test_unknown_method_returns_method_not_found() {
        let cfg = cli_config_for_mcp("https://example.test/forum/".into(), HashMap::new());
        let req = parse_req(r#"{"jsonrpc":"2.0","id":3,"method":"does_not_exist","params":{}}"#);
        let resp = handle_request(req, &cfg).await;
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
    }

    #[tokio::test]
    async fn test_tools_call_search_returns_text_content() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path("/forum/tracker.php"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(FORUM_FIXTURE.to_vec()))
            .mount(&server)
            .await;
        let cfg = cli_config_for_mcp(format!("{}/forum/", server.uri()), HashMap::new());
        let req_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "search",
                "arguments": { "query": "2026", "category": "252" }
            }
        });
        let req: Request = serde_json::from_value(req_body).unwrap();
        let resp = handle_request(req, &cfg).await;
        assert!(
            resp.error.is_none(),
            "expected success, got {:?}",
            resp.error
        );
        let content = &resp.result.unwrap()["content"][0];
        assert_eq!(content["type"], "text");
        let text = content["text"].as_str().unwrap();
        assert!(
            text.starts_with("Found 50 results:"),
            "text content shape: {}",
            &text[..80.min(text.len())]
        );
    }

    /// US-008: a `tools/call` for an unknown tool name must surface as a
    /// JSON-RPC error with code -32000 and a message containing the failure
    /// detail. Covers L87.
    #[tokio::test]
    async fn test_tools_call_unknown_tool_returns_error_response() {
        let cfg = cli_config_for_mcp("https://example.test/forum/".into(), HashMap::new());
        let req_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": { "name": "does_not_exist_tool", "arguments": {} }
        });
        let req: Request = serde_json::from_value(req_body).unwrap();
        let resp = handle_request(req, &cfg).await;
        let err = resp.error.expect("dispatch failure must surface as error");
        assert_eq!(err.code, -32000);
        assert!(
            err.message.starts_with("tool call failed"),
            "error message must say `tool call failed`, got: {}",
            err.message
        );
    }

    /// US-008: `Response::err` constructs an error response with no
    /// result set, preserving the caller's id value. Covers the bare
    /// error constructor explicitly.
    #[test]
    fn test_response_err_shape() {
        let r = Response::err(Some(Value::from(42)), -32001, "oops");
        assert!(r.result.is_none());
        let e = r.error.expect("err response must have an error body");
        assert_eq!(e.code, -32001);
        assert_eq!(e.message, "oops");
        assert!(e.data.is_none());
        assert_eq!(r.id, Some(Value::from(42)));
    }

    /// US-008: `ping` method returns an empty JSON object on success.
    /// Covers the ping arm at L89.
    #[tokio::test]
    async fn test_ping_returns_empty_object() {
        let cfg = cli_config_for_mcp("https://example.test/forum/".into(), HashMap::new());
        let req = parse_req(r#"{"jsonrpc":"2.0","id":5,"method":"ping","params":{}}"#);
        let resp = handle_request(req, &cfg).await;
        assert!(resp.error.is_none());
        let result = resp.result.unwrap();
        assert!(result.is_object());
        assert_eq!(
            result.as_object().unwrap().len(),
            0,
            "ping response body must be an empty object"
        );
    }

    #[tokio::test]
    async fn test_stdio_handshake_initialize() {
        // Snapshot test for the initialize → response roundtrip shape.
        let cfg = cli_config_for_mcp("https://example.test/forum/".into(), HashMap::new());
        let req: Request = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        )
        .unwrap();
        let resp = handle_request(req, &cfg).await;
        let serialized = serde_json::to_string(&resp).unwrap();
        let v: Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 0);
        assert!(v["result"]["protocolVersion"].is_string());
    }
}
