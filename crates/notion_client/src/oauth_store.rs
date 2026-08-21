//! Keychain-backed storage for OAuth tokens obtained via
//! [`crate::oauth::run_oauth_flow`]. Sibling to `token_store` (the PAT
//! equivalent) — same shape, different credential URL so the two don't
//! collide when both happen to be configured.

use anyhow::{Context as _, Result};
use gpui::{App, Task};

use crate::oauth::OAuthTokens;

const MCP_CREDENTIALS_URL: &str = "https://mcp.notion.com";

/// Loads the stored OAuth tokens, if any, from the system keychain (or the
/// dev-mode credentials provider).
pub fn load_tokens(cx: &App) -> Task<Option<OAuthTokens>> {
    let provider = zed_credentials_provider::global(cx);
    cx.spawn(async move |cx| {
        let (_username, password) = match provider.read_credentials(MCP_CREDENTIALS_URL, cx).await {
            Ok(Some(credentials)) => credentials,
            Ok(None) => return None,
            Err(error) => {
                log::error!("failed to read Notion OAuth tokens from keychain: {error}");
                return None;
            }
        };
        serde_json::from_slice(&password)
            .map_err(|error| log::error!("failed to parse stored Notion OAuth tokens: {error}"))
            .ok()
    })
}

/// Stores OAuth tokens in the system keychain, replacing any previous value.
pub fn store_tokens(tokens: &OAuthTokens, cx: &App) -> Task<Result<()>> {
    let provider = zed_credentials_provider::global(cx);
    let payload = serde_json::to_vec(tokens).context("failed to serialize OAuth tokens");
    cx.spawn(async move |cx| {
        let payload = payload?;
        provider
            .write_credentials(MCP_CREDENTIALS_URL, "oauth", &payload, cx)
            .await
    })
}

/// Removes stored OAuth tokens (e.g. after a dead refresh grant, to prompt
/// the user to reconnect).
pub fn delete_tokens(cx: &App) -> Task<Result<()>> {
    let provider = zed_credentials_provider::global(cx);
    cx.spawn(async move |cx| provider.delete_credentials(MCP_CREDENTIALS_URL, cx).await)
}
