//! OAuth against Notion's public MCP authorization server
//! (`https://mcp.notion.com`), for workspaces whose admin policy blocks
//! self-service Personal Access Token / Connection creation but still allows
//! authorizing an already-registered OAuth app. Uses Dynamic Client
//! Registration (RFC 7591) so no pre-shared client id/secret is needed — the
//! client registers itself as a public, PKCE-only client at flow start.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use futures::AsyncReadExt as _;
use gpui::{App, ClipboardItem, Task};
use http_client::{AsyncBody, HttpClient, Method, Request};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

const ISSUER: &str = "https://mcp.notion.com";
const RESOURCE: &str = "https://mcp.notion.com/mcp";
/// Sent on every request: Notion's edge (Cloudflare) blocks the default
/// `urllib`/bare-HTTP-client user agent as a bot signature, so a browser-like
/// one is required even for plain JSON API calls.
const USER_AGENT: &str = "Mozilla/5.0 (compatible; ZedTicketsPanel/1.0)";

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("Notion OAuth session expired — reconnect Notion")]
    GrantInvalid,
    #[error("timed out waiting for the browser authorization (5 min)")]
    Timeout,
    #[error("OAuth error from Notion: {0}")]
    AuthorizationDenied(String),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for OAuthError {
    fn from(error: anyhow::Error) -> Self {
        OAuthError::Other(error.to_string())
    }
}

impl From<std::io::Error> for OAuthError {
    fn from(error: std::io::Error) -> Self {
        OAuthError::Other(error.to_string())
    }
}

/// Persistable OAuth state for one connected Notion workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix timestamp (seconds) after which `access_token` should be refreshed.
    pub expires_at: i64,
}

#[derive(Debug, Deserialize)]
struct OAuthMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
    client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

async fn fetch_json_get(http_client: &Arc<dyn HttpClient>, url: &str) -> Result<serde_json::Value> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .body(AsyncBody::default())
        .context("failed to build request")?;
    let mut response = http_client
        .send(request)
        .await
        .with_context(|| format!("request to {url} failed"))?;
    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;
    if !response.status().is_success() {
        bail!("{url} -> HTTP {}: {body}", response.status());
    }
    serde_json::from_str(&body).with_context(|| format!("{url} returned invalid JSON: {body}"))
}

async fn post_json(
    http_client: &Arc<dyn HttpClient>,
    url: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(AsyncBody::from(serde_json::to_vec(&body)?))
        .context("failed to build request")?;
    let mut response = http_client.send(request).await.context("request failed")?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    if !response.status().is_success() {
        bail!("{url} -> HTTP {}: {text}", response.status());
    }
    serde_json::from_str(&text).with_context(|| format!("{url} returned invalid JSON: {text}"))
}

async fn post_form(
    http_client: &Arc<dyn HttpClient>,
    url: &str,
    form: &[(&str, &str)],
) -> Result<TokenResponse, OAuthError> {
    let body = serde_urlencoded::to_string(form).context("failed to encode form body")?;
    let request = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(AsyncBody::from(body.into_bytes()))
        .context("failed to build request")?;
    let mut response = http_client
        .send(request)
        .await
        .context("token request failed")?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    if !response.status().is_success() {
        if text.contains("invalid_grant") {
            return Err(OAuthError::GrantInvalid);
        }
        return Err(OAuthError::Other(format!(
            "{url} -> HTTP {}: {text}",
            response.status()
        )));
    }
    Ok(serde_json::from_str(&text)
        .with_context(|| format!("{url} returned invalid JSON: {text}"))?)
}

async fn discover_metadata(http_client: &Arc<dyn HttpClient>) -> Result<OAuthMetadata> {
    let value = fetch_json_get(
        http_client,
        &format!("{ISSUER}/.well-known/oauth-authorization-server"),
    )
    .await?;
    serde_json::from_value(value).context("unexpected OAuth metadata shape")
}

async fn register_client(
    http_client: &Arc<dyn HttpClient>,
    metadata: &OAuthMetadata,
    redirect_uri: &str,
) -> Result<RegistrationResponse> {
    let value = post_json(
        http_client,
        &metadata.registration_endpoint,
        serde_json::json!({
            "client_name": "zed-tickets-panel",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }),
    )
    .await?;
    serde_json::from_value(value).context("unexpected client registration response shape")
}

fn pkce_pair() -> (String, String) {
    let mut verifier_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut verifier_bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn random_state() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct CallbackResult {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Blocks the calling (background) thread until the OAuth redirect arrives on
/// `server`, or the retry budget is exhausted. Mirrors the polling shape
/// `crates/client/src/client.rs`'s `authenticate_with_browser` already uses
/// for Zed's own sign-in flow.
fn wait_for_callback(server: &tiny_http::Server, expected_state: &str) -> Result<String, OAuthError> {
    for _ in 0..300 {
        if let Some(request) = server
            .recv_timeout(Duration::from_secs(1))
            .map_err(|error| OAuthError::Other(error.to_string()))?
        {
            let url = url::Url::parse(&format!("http://localhost{}", request.url()))
                .context("failed to parse callback url")?;
            let mut result = CallbackResult {
                code: None,
                state: None,
                error: None,
            };
            for (key, value) in url.query_pairs() {
                match key.as_ref() {
                    "code" => result.code = Some(value.into_owned()),
                    "state" => result.state = Some(value.into_owned()),
                    "error" => result.error = Some(value.into_owned()),
                    _ => {}
                }
            }

            let body = "<html><body><h2>Connected to Notion</h2><p>You can close this tab and return to Zed.</p></body></html>";
            request
                .respond(
                    tiny_http::Response::from_string(body)
                        .with_header(
                            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html"[..])
                                .expect("static header is valid"),
                        )
                        .with_status_code(200),
                )
                .context("failed to respond to OAuth callback request")?;

            if let Some(error) = result.error {
                return Err(OAuthError::AuthorizationDenied(error));
            }
            let code = result.code.context("callback had no code")?;
            if result.state.as_deref() != Some(expected_state) {
                return Err(OAuthError::Other(
                    "OAuth callback state mismatch".to_string(),
                ));
            }
            return Ok(code);
        }
    }
    Err(OAuthError::Timeout)
}

/// Runs the full flow: metadata discovery → dynamic client registration →
/// PKCE → open the user's browser → wait for the local callback → exchange
/// the code for tokens. The browser step needs a live `App`, so this must run
/// on a session with a display; there's no headless fallback.
pub fn run_oauth_flow(http_client: Arc<dyn HttpClient>, cx: &App) -> Task<Result<OAuthTokens, OAuthError>> {
    cx.spawn(async move |cx| {
        let background = cx.background_executor().clone();
        let http_client_for_bg = http_client.clone();

        let (open_url_tx, open_url_rx) = futures::channel::oneshot::channel::<String>();
        cx.update(|cx| {
            cx.spawn(async move |cx| {
                if let Ok(url) = open_url_rx.await {
                    log::info!("Notion OAuth: open this URL to authorize: {url}");
                    cx.update(|cx| {
                        // Copy unconditionally rather than only on a detected
                        // open_url failure: opening silently no-ops in some
                        // environments (no browser/portal backend) with no
                        // observable error, so a fallback the user can always
                        // paste is more reliable than trying to detect success.
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                        cx.open_url(&url);
                    });
                }
            })
            .detach();
        });

        background
            .spawn(async move {
                let metadata = discover_metadata(&http_client_for_bg).await?;

                let server = tiny_http::Server::http("127.0.0.1:0")
                    .map_err(|error| anyhow!(error).context("failed to bind OAuth callback port"))?;
                let port = server
                    .server_addr()
                    .to_ip()
                    .context("callback server not bound to a TCP address")?
                    .port();
                let redirect_uri = format!("http://localhost:{port}/callback");

                let registration = register_client(&http_client_for_bg, &metadata, &redirect_uri).await?;
                let (verifier, challenge) = pkce_pair();
                let state = random_state();

                let auth_url = format!(
                    "{}?{}",
                    metadata.authorization_endpoint,
                    serde_urlencoded::to_string([
                        ("client_id", registration.client_id.as_str()),
                        ("redirect_uri", redirect_uri.as_str()),
                        ("response_type", "code"),
                        ("code_challenge", challenge.as_str()),
                        ("code_challenge_method", "S256"),
                        ("state", state.as_str()),
                        ("resource", RESOURCE),
                        ("scope", "mcp"),
                    ])
                    .context("failed to encode authorization url")?
                );
                open_url_tx.send(auth_url).ok();

                let code = wait_for_callback(&server, &state)?;

                let mut form = vec![
                    ("grant_type", "authorization_code"),
                    ("code", code.as_str()),
                    ("redirect_uri", redirect_uri.as_str()),
                    ("client_id", registration.client_id.as_str()),
                    ("code_verifier", verifier.as_str()),
                ];
                if let Some(secret) = registration.client_secret.as_deref() {
                    form.push(("client_secret", secret));
                }
                let token_response = post_form(&http_client_for_bg, &metadata.token_endpoint, &form).await?;

                Ok(OAuthTokens {
                    client_id: registration.client_id,
                    client_secret: registration.client_secret,
                    access_token: token_response.access_token,
                    refresh_token: token_response.refresh_token,
                    expires_at: now_unix() + token_response.expires_in,
                })
            })
            .await
    })
}

/// Exchanges a refresh token for a new access token. Returns
/// [`OAuthError::GrantInvalid`] when the refresh token itself is dead —
/// callers should clear the stored tokens and prompt the user to reconnect
/// rather than retry.
pub async fn refresh_tokens(
    http_client: &Arc<dyn HttpClient>,
    previous: &OAuthTokens,
) -> Result<OAuthTokens, OAuthError> {
    let Some(refresh_token) = previous.refresh_token.as_deref() else {
        return Err(OAuthError::GrantInvalid);
    };
    let metadata = discover_metadata(http_client).await?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", previous.client_id.as_str()),
    ];
    if let Some(secret) = previous.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let token_response = post_form(http_client, &metadata.token_endpoint, &form).await?;
    Ok(OAuthTokens {
        client_id: previous.client_id.clone(),
        client_secret: previous.client_secret.clone(),
        access_token: token_response.access_token,
        refresh_token: token_response.refresh_token.or_else(|| previous.refresh_token.clone()),
        expires_at: now_unix() + token_response.expires_in,
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
