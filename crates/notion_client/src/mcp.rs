//! Minimal MCP (Model Context Protocol) client over Streamable HTTP for
//! Notion's remote MCP server (`https://mcp.notion.com/mcp`). This is the
//! query surface an OAuth-obtained token is actually scoped to — it is not
//! interchangeable with the classic REST API in `crate::notion_client`.

use std::sync::Arc;

use anyhow::Context as _;
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::oauth::{OAuthError, OAuthTokens};

const MCP_URL: &str = "https://mcp.notion.com/mcp";
const PROTOCOL_VERSION: &str = "2025-03-26";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; ZedTicketsPanel/1.0)";

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("Notion MCP error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for McpError {
    fn from(error: anyhow::Error) -> Self {
        McpError::Other(error.to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolResult {
    #[serde(default)]
    pub content: Vec<ToolContent>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
    /// Some MCP tools may return their payload here instead of JSON-encoded
    /// inside `content[].text` — not confirmed either way for Notion's tools
    /// from this environment (no live network access to verify), so
    /// [`Self::payload`] checks both and callers should prefer it over
    /// reading either field directly.
    #[serde(default, rename = "structuredContent")]
    pub structured_content: Option<Value>,
}

impl ToolResult {
    /// Concatenates every text part of the result, newline-joined — mirrors
    /// the reference implementation's `joinTextContent`.
    pub fn joined_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| part.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The tool's JSON payload, preferring `structuredContent` when present
    /// and falling back to parsing the joined text content as JSON.
    pub fn payload(&self) -> Result<Value, McpError> {
        if let Some(structured) = &self.structured_content {
            return Ok(structured.clone());
        }
        let text = self.joined_text();
        serde_json::from_str(&text)
            .map_err(|error| McpError::Other(format!("tool result was not JSON: {error}: {text}")))
    }
}

/// Single-shot MCP session: one client handles one `initialize` + a handful
/// of `tools/call` requests, then is dropped. Notion's server is stateless
/// enough across short-lived sessions that there's no need to keep a
/// long-lived connection open, mirroring the reference implementation.
pub struct McpClient {
    http_client: Arc<dyn HttpClient>,
    tokens: OAuthTokens,
    session_id: Option<String>,
    next_id: u64,
    tokens_refreshed: bool,
}

impl McpClient {
    pub fn new(http_client: Arc<dyn HttpClient>, tokens: OAuthTokens) -> Self {
        Self {
            http_client,
            tokens,
            session_id: None,
            next_id: 1,
            tokens_refreshed: false,
        }
    }

    /// Returns the current tokens if a 401 triggered a refresh during this
    /// client's lifetime, so the caller can persist the new tokens. Returns
    /// `None` (not just the unchanged tokens) when nothing changed, so the
    /// caller can skip an unnecessary keychain write.
    pub fn refreshed_tokens(&self) -> Option<&OAuthTokens> {
        self.tokens_refreshed.then_some(&self.tokens)
    }

    pub async fn initialize(&mut self) -> Result<(), McpError> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "zed-tickets-panel", "version": "0.1" },
            }),
        )
        .await?;
        self.notify("notifications/initialized", None).await
    }

    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<ToolResult, McpError> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        serde_json::from_value(result)
            .map_err(|error| McpError::Other(format!("unexpected tool result shape: {error}")))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let (status, text, session_id) = self.send(&body).await?;
        if status == http_client::StatusCode::UNAUTHORIZED {
            let refreshed = crate::oauth::refresh_tokens(&self.http_client, &self.tokens).await?;
            self.tokens = refreshed;
            self.tokens_refreshed = true;
            let (status, text, session_id) = self.send(&body).await?;
            if !status.is_success() {
                return Err(McpError::Other(format!("Notion MCP HTTP {status}: {text}")));
            }
            if let Some(session_id) = session_id {
                self.session_id = Some(session_id);
            }
            return parse_rpc_result(&text);
        }
        if !status.is_success() {
            return Err(McpError::Other(format!("Notion MCP HTTP {status}: {text}")));
        }
        if let Some(session_id) = session_id {
            self.session_id = Some(session_id);
        }
        parse_rpc_result(&text)
    }

    /// Fire-and-forget: the server may answer with 202 Accepted and an empty
    /// body, which is not an error.
    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<(), McpError> {
        // JSON-RPC 2.0 requires `params`, if present, to be an array or
        // object — never `null` — so omit the field entirely rather than
        // serializing `None` as `"params": null`, which Notion's server
        // rejects as an invalid JSON-RPC message.
        let mut body = json!({ "jsonrpc": "2.0", "method": method });
        if let Some(params) = params {
            body["params"] = params;
        }
        let (status, text, _session_id) = self.send(&body).await?;
        if !status.is_success() {
            return Err(McpError::Other(format!("Notion MCP HTTP {status}: {text}")));
        }
        Ok(())
    }

    async fn send(
        &self,
        body: &Value,
    ) -> Result<(http_client::StatusCode, String, Option<String>), McpError> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(MCP_URL)
            .header("User-Agent", USER_AGENT)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header(
                "Authorization",
                format!("Bearer {}", self.tokens.access_token),
            );
        if let Some(session_id) = &self.session_id {
            builder = builder.header("Mcp-Session-Id", session_id.clone());
        }
        let request = builder
            .body(AsyncBody::from(
                serde_json::to_vec(body).context("failed to encode request")?,
            ))
            .context("failed to build request")?;

        let mut response = self
            .http_client
            .send(request)
            .await
            .context("Notion MCP request failed")?;
        let status = response.status();
        let session_id = response
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut text = String::new();
        response.body_mut().read_to_string(&mut text).await.ok();
        Ok((status, text, session_id))
    }
}

/// The MCP server answers with either a single JSON object or an SSE stream
/// of `data:`-prefixed frames; both are valid per the Streamable HTTP
/// transport spec, so both must be handled.
fn parse_rpc_result(text: &str) -> Result<Value, McpError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    let envelope: Value = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(trimmed)
            .map_err(|error| McpError::Other(format!("invalid JSON-RPC response: {error}")))?
    } else {
        parse_sse(trimmed)?
    };

    if let Some(error) = envelope.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        return Err(McpError::Rpc { code, message });
    }
    Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
}

/// Collects every `data:` line in the last parseable SSE block (per the SSE
/// spec, multiple `data:` lines within one block are newline-joined before
/// parsing).
fn parse_sse(text: &str) -> Result<Value, McpError> {
    for block in text.rsplit("\n\n") {
        let data_lines: Vec<&str> = block
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim))
            .collect();
        if data_lines.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str(&data_lines.join("\n")) {
            return Ok(value);
        }
    }
    Err(McpError::Other(format!(
        "no JSON-RPC message found in SSE response: {text}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_json_result() {
        let text = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hi"}]}}"#;
        let result = parse_rpc_result(text).expect("should parse");
        assert_eq!(result["content"][0]["text"], "hi");
    }

    #[test]
    fn parse_plain_json_error() {
        let text = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"boom"}}"#;
        match parse_rpc_result(text) {
            Err(McpError::Rpc { code, message }) => {
                assert_eq!(code, -32000);
                assert_eq!(message, "boom");
            }
            other => panic!("expected Rpc error, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_wrapped_result() {
        let text =
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let result = parse_rpc_result(text).expect("should parse");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn tool_result_joins_text_content() {
        let result = ToolResult {
            content: vec![
                ToolContent {
                    content_type: "text".into(),
                    text: Some("line one".into()),
                },
                ToolContent {
                    content_type: "text".into(),
                    text: Some("line two".into()),
                },
            ],
            is_error: false,
            structured_content: None,
        };
        assert_eq!(result.joined_text(), "line one\nline two");
    }
}
