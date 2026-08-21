//! Headless client for the parts of the Notion API needed by the tickets
//! panel: resolving a database from a page, resolving the current user,
//! reading a database's schema, and querying/filtering its rows.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, Task};
use http_client::{AsyncBody, HttpClient, Method, Request, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub mod mcp;
pub mod mcp_board;
pub mod oauth;
pub mod oauth_store;
pub mod page_body;

const NOTION_API_BASE: &str = "https://api.notion.com/v1";
const NOTION_VERSION: &str = "2022-06-28";
const NOTION_CREDENTIALS_URL: &str = "https://api.notion.com";

/// Errors a caller can distinguish to show an actionable message.
#[derive(Debug, thiserror::Error)]
pub enum NotionError {
    #[error("Notion token is missing or invalid — set a new Personal Access Token")]
    Unauthorized,
    #[error(
        "Notion returned 404 for {0} — check the configured id and that it's shared with your integration"
    )]
    NotFound(String),
    #[error(
        "Notion says your integration can't see {0} — share it via the page's \"...\" → Connections menu"
    )]
    Forbidden(String),
    #[error("network request to Notion failed: {0}")]
    Network(String),
    #[error("failed to parse Notion response: {0}")]
    Parse(String),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for NotionError {
    fn from(error: anyhow::Error) -> Self {
        NotionError::Other(error.to_string())
    }
}

pub mod token_store {
    use super::*;

    /// Loads the stored Notion Personal Access Token, if any, from the
    /// system keychain (or the dev-mode credentials provider).
    pub fn load_token(cx: &App) -> Task<Option<Arc<str>>> {
        let provider = zed_credentials_provider::global(cx);
        cx.spawn(async move |cx| {
            match provider.read_credentials(NOTION_CREDENTIALS_URL, cx).await {
                Ok(Some((_username, password))) => String::from_utf8(password).ok().map(Arc::from),
                Ok(None) => None,
                Err(error) => {
                    log::error!("failed to read Notion token from keychain: {error}");
                    None
                }
            }
        })
    }

    /// Stores a Notion Personal Access Token in the system keychain.
    pub fn store_token(token: String, cx: &App) -> Task<Result<()>> {
        let provider = zed_credentials_provider::global(cx);
        cx.spawn(async move |cx| {
            provider
                .write_credentials(NOTION_CREDENTIALS_URL, "Bearer", token.as_bytes(), cx)
                .await
        })
    }

    /// Removes a stored Notion Personal Access Token.
    pub fn delete_token(cx: &App) -> Task<Result<()>> {
        let provider = zed_credentials_provider::global(cx);
        cx.spawn(async move |cx| {
            provider
                .delete_credentials(NOTION_CREDENTIALS_URL, cx)
                .await
        })
    }
}

/// One ticket row read from the configured Notion database, already
/// filtered to the tracked statuses and assignee.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketRef {
    /// Notion page id (UUID, stable) — the join key for anything that
    /// tracks per-ticket state (e.g. a worktree/session store).
    pub page_id: String,
    pub title: String,
    /// Canonical `notion.so` URL for the ticket page.
    pub url: String,
    /// Raw status option string as stored in Notion (may carry emoji/numbering).
    pub status: String,
    /// A lowercase-kebab-case slug derived from the title, suitable as a
    /// starting point for a branch/directory name.
    pub slug: String,
    pub last_edited_time: String,
    pub ticket_type: Option<String>,
    /// Human-facing ticket reference (Notion's `unique_id` property, e.g.
    /// `CT-1487`). Absent when the board has no such property.
    pub issue_id: Option<String>,
}

impl TicketRef {
    /// The page's UUID, recovered from its URL.
    ///
    /// [`Self::page_id`] is **not** interchangeable with this: on the
    /// OAuth/MCP path it is the URL's whole slug-plus-id segment. It stays
    /// that way on purpose — it is the primary key of the on-disk ticket
    /// store, so changing it would orphan every recorded worktree.
    pub fn notion_page_uuid(&self) -> String {
        extract_page_id(&self.url)
    }
}

/// Which Notion property type the status filter needs to target — the two
/// aren't interchangeable in the query filter JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusPropertyKind {
    Status,
    Select,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DatabaseSchema {
    pub status_property: String,
    pub status_property_kind: StatusPropertyKind,
    pub status_options: Vec<String>,
    pub assignee_property: String,
}

pub struct NotionClient {
    http_client: Arc<dyn HttpClient>,
    token: Arc<str>,
}

impl NotionClient {
    pub fn new(http_client: Arc<dyn HttpClient>, token: Arc<str>) -> Self {
        Self { http_client, token }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, NotionError> {
        let uri = format!("{NOTION_API_BASE}{path}");
        let async_body = match &body {
            Some(value) => AsyncBody::from(serde_json::to_vec(value).map_err(|error| {
                NotionError::Parse(format!("failed to serialize request body: {error}"))
            })?),
            None => AsyncBody::default(),
        };

        let mut builder = Request::builder()
            .method(method)
            .uri(&uri)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Notion-Version", NOTION_VERSION);
        if body.is_some() {
            builder = builder.header("Content-Type", "application/json");
        }
        let request = builder
            .body(async_body)
            .map_err(|error| NotionError::Other(format!("failed to build request: {error}")))?;

        let mut response = self
            .http_client
            .send(request)
            .await
            .map_err(|error| NotionError::Network(error.to_string()))?;

        let mut response_body = String::new();
        response
            .body_mut()
            .read_to_string(&mut response_body)
            .await
            .map_err(|error| NotionError::Network(error.to_string()))?;

        match response.status() {
            status if status.is_success() => serde_json::from_str(&response_body)
                .map_err(|error| NotionError::Parse(format!("{error}: {response_body}"))),
            StatusCode::UNAUTHORIZED => Err(NotionError::Unauthorized),
            StatusCode::FORBIDDEN => Err(NotionError::Forbidden(path.to_string())),
            StatusCode::NOT_FOUND => Err(NotionError::NotFound(path.to_string())),
            status => Err(NotionError::Other(format!(
                "Notion returned {status}: {response_body}"
            ))),
        }
    }

    /// Resolves a queryable database id from a Notion page id. Tries, in
    /// order: a `child_database` block on the page, the page id itself as a
    /// database id, then a title search.
    pub async fn resolve_database_id(&self, page_id: &str) -> Result<String, NotionError> {
        let children = self
            .request(Method::GET, &format!("/blocks/{page_id}/children"), None)
            .await;

        if let Ok(children) = children {
            let results = children.get("results").and_then(Value::as_array);
            if let Some(results) = results {
                for block in results {
                    if block.get("type").and_then(Value::as_str) == Some("child_database")
                        && let Some(id) = block.get("id").and_then(Value::as_str)
                    {
                        return Ok(id.to_string());
                    }
                }
            }
        }

        if self
            .request(Method::GET, &format!("/databases/{page_id}"), None)
            .await
            .is_ok()
        {
            return Ok(page_id.to_string());
        }

        let search = self
            .request(
                Method::POST,
                "/search",
                Some(json!({
                    "filter": { "value": "database", "property": "object" }
                })),
            )
            .await?;
        let results = search
            .get("results")
            .and_then(Value::as_array)
            .context("Notion search response had no results array")?;
        let database = results
            .first()
            .and_then(|entry| entry.get("id"))
            .and_then(Value::as_str)
            .context("Notion search found no database")?;
        Ok(database.to_string())
    }

    /// Resolves the Notion user id for the token's own identity (Personal
    /// Access Tokens are scoped to one person), falling back to a
    /// paginated user-list search by email if `/users/me` doesn't resolve
    /// to a matching person.
    pub async fn resolve_assignee_user_id(&self, email_hint: &str) -> Result<String, NotionError> {
        let me = self.request(Method::GET, "/users/me", None).await;
        if let Ok(me) = me
            && me.get("type").and_then(Value::as_str) == Some("person")
            && let Some(id) = me.get("id").and_then(Value::as_str)
        {
            return Ok(id.to_string());
        }

        let mut cursor: Option<String> = None;
        loop {
            let path = match &cursor {
                Some(cursor) => format!("/users?start_cursor={cursor}"),
                None => "/users".to_string(),
            };
            let page = self.request(Method::GET, &path, None).await?;
            let results = page
                .get("results")
                .and_then(Value::as_array)
                .context("Notion users response had no results array")?;
            for user in results {
                let email = user
                    .get("person")
                    .and_then(|person| person.get("email"))
                    .and_then(Value::as_str);
                if email == Some(email_hint)
                    && let Some(id) = user.get("id").and_then(Value::as_str)
                {
                    return Ok(id.to_string());
                }
            }
            let has_more = page
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !has_more {
                break;
            }
            cursor = page
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }

        Err(NotionError::Other(format!(
            "no Notion user found with email {email_hint}"
        )))
    }

    /// Reads a database's schema to discover the real status/assignee
    /// property names and the exact (possibly emoji/number-prefixed) status
    /// option strings, rather than guessing.
    pub async fn fetch_database_schema(
        &self,
        database_id: &str,
    ) -> Result<DatabaseSchema, NotionError> {
        let database = self
            .request(Method::GET, &format!("/databases/{database_id}"), None)
            .await?;
        let properties = database
            .get("properties")
            .and_then(Value::as_object)
            .context("Notion database response had no properties object")?;

        let mut status_property = None;
        let mut status_property_kind = None;
        let mut status_options = Vec::new();
        let mut assignee_property = None;

        for (name, definition) in properties {
            let kind = definition.get("type").and_then(Value::as_str);
            match kind {
                Some("status") | Some("select") => {
                    let options_key = kind.unwrap();
                    let options = definition
                        .get(options_key)
                        .and_then(|value| value.get("options"))
                        .and_then(Value::as_array);
                    if let Some(options) = options {
                        status_property = Some(name.clone());
                        status_property_kind = Some(if kind == Some("status") {
                            StatusPropertyKind::Status
                        } else {
                            StatusPropertyKind::Select
                        });
                        status_options = options
                            .iter()
                            .filter_map(|option| option.get("name").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect();
                    }
                }
                Some("people") => {
                    assignee_property = Some(name.clone());
                }
                _ => {}
            }
        }

        Ok(DatabaseSchema {
            status_property: status_property
                .context("could not find a status/select property in the database schema")?,
            status_property_kind: status_property_kind
                .context("could not determine the status property's type")?,
            status_options,
            assignee_property: assignee_property
                .context("could not find a people property in the database schema")?,
        })
    }

    /// Queries the database for tickets matching the schema's status
    /// options and the given assignee, paginating through all results.
    pub async fn query_tickets(
        &self,
        database_id: &str,
        schema: &DatabaseSchema,
        assignee_user_id: &str,
        status_filter: &[String],
    ) -> Result<Vec<TicketRef>, NotionError> {
        let status_key = match schema.status_property_kind {
            StatusPropertyKind::Status => "status",
            StatusPropertyKind::Select => "select",
        };
        let status_or: Vec<Value> = status_filter
            .iter()
            .map(|status| {
                json!({
                    "property": schema.status_property,
                    status_key: { "equals": status }
                })
            })
            .collect();

        let filter = json!({
            "and": [
                { "or": status_or },
                {
                    "property": schema.assignee_property,
                    "people": { "contains": assignee_user_id }
                }
            ]
        });

        let mut tickets = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut body = json!({ "filter": filter.clone(), "page_size": 100 });
            if let Some(cursor) = &cursor {
                body["start_cursor"] = json!(cursor);
            }
            let page = self
                .request(
                    Method::POST,
                    &format!("/databases/{database_id}/query"),
                    Some(body),
                )
                .await?;
            let results = page
                .get("results")
                .and_then(Value::as_array)
                .context("Notion query response had no results array")?;
            for page_value in results {
                tickets.push(parse_ticket(page_value, &schema.status_property)?);
            }
            let has_more = page
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !has_more {
                break;
            }
            cursor = page
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }

        Ok(tickets)
    }

    /// Runs [`Self::query_tickets`] once and reports the result via
    /// `on_result`. Single call, no scheduling — see [`refresh_loop`] for a
    /// recurring background refresh.
    pub fn refresh_once(
        self: Arc<Self>,
        database_id: String,
        schema: DatabaseSchema,
        assignee_user_id: String,
        status_filter: Vec<String>,
        on_result: impl Fn(Result<Vec<TicketRef>, NotionError>) + Send + 'static,
        cx: &App,
    ) -> Task<()> {
        cx.background_spawn(async move {
            let result = self
                .query_tickets(&database_id, &schema, &assignee_user_id, &status_filter)
                .await;
            on_result(result);
        })
    }

    /// Recurring refresh on `interval`. The returned [`Task`] runs until
    /// dropped — the caller owns its lifetime (e.g. stores it on a panel and
    /// drops/replaces it to cancel).
    pub fn refresh_loop(
        self: Arc<Self>,
        database_id: String,
        schema: DatabaseSchema,
        assignee_user_id: String,
        status_filter: Vec<String>,
        interval: Duration,
        on_result: impl Fn(Result<Vec<TicketRef>, NotionError>) + Send + 'static,
        cx: &App,
    ) -> Task<()> {
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            loop {
                let result = self
                    .query_tickets(&database_id, &schema, &assignee_user_id, &status_filter)
                    .await;
                on_result(result);
                executor.timer(interval).await;
            }
        })
    }
}

fn parse_ticket(page: &Value, status_property: &str) -> Result<TicketRef, NotionError> {
    let page_id = page
        .get("id")
        .and_then(Value::as_str)
        .context("ticket page had no id")?
        .to_string();
    let url = page
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let last_edited_time = page
        .get("last_edited_time")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let properties = page
        .get("properties")
        .and_then(Value::as_object)
        .context("ticket page had no properties object")?;

    let title = properties
        .values()
        .find_map(|property| {
            if property.get("type").and_then(Value::as_str) != Some("title") {
                return None;
            }
            property
                .get("title")
                .and_then(Value::as_array)
                .and_then(|rich_text| joined_plain_text(rich_text))
        })
        .unwrap_or_else(|| "Untitled".to_string());

    let status_property_value = properties.get(status_property);
    let status = status_property_value
        .and_then(|property| property.get("status").or_else(|| property.get("select")))
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let ticket_type = properties
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("type"))
        .and_then(|(_, property)| property.get("select"))
        .and_then(|select| select.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // Notion renders a `unique_id` property as `<prefix>-<number>`; the API
    // hands the two back separately.
    let issue_id = properties
        .values()
        .filter(|property| property.get("type").and_then(Value::as_str) == Some("unique_id"))
        .find_map(|property| {
            let unique_id = property.get("unique_id")?;
            let number = unique_id.get("number").and_then(Value::as_i64)?;
            match unique_id.get("prefix").and_then(Value::as_str) {
                Some(prefix) if !prefix.is_empty() => Some(format!("{prefix}-{number}")),
                _ => Some(number.to_string()),
            }
        });

    Ok(TicketRef {
        page_id,
        slug: slugify(&title),
        title,
        url,
        status,
        last_edited_time,
        ticket_type,
        issue_id,
    })
}

fn joined_plain_text(rich_text: &[Value]) -> Option<String> {
    let text: String = rich_text
        .iter()
        .filter_map(|segment| segment.get("plain_text").and_then(Value::as_str))
        .collect();
    if text.is_empty() { None } else { Some(text) }
}

/// Kebab-cases a ticket title into a starting point for a branch or
/// directory name. Public so callers that rebuild a `TicketRef` outside this
/// crate derive the same slug the board query would have.
pub fn slugify(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    let mut last_was_dash = false;
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// Notion page URLs embed the 32-hex-character page id at the very end of
/// the last path segment's slug (with the title's own words/dashes stripped
/// out ahead of it, e.g. `.../My-Page-Title-0123456789abcdef0123456789abcdef`) —
/// so the id is recovered by taking the last 32 alphanumeric characters
/// rather than trying to parse the slug structurally.
///
/// Needed wherever a page id is handed to a Notion API or MCP tool: on the
/// OAuth/MCP query path [`TicketRef::page_id`] is that whole slug-plus-id
/// segment, which `notion-fetch` rejects.
pub fn extract_page_id(input: &str) -> String {
    let trimmed = input.trim();
    let without_query = trimmed.split('?').next().unwrap_or(trimmed);
    let last_segment = without_query.rsplit('/').next().unwrap_or(without_query);
    let cleaned: String = last_segment
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    let tail = if cleaned.len() >= 32 {
        &cleaned[cleaned.len() - 32..]
    } else {
        return trimmed.to_string();
    };
    format!(
        "{}-{}-{}-{}-{}",
        &tail[0..8],
        &tail[8..12],
        &tail[12..16],
        &tail[16..20],
        &tail[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_page_id_from_a_slugged_url() {
        assert_eq!(
            extract_page_id(
                "https://www.notion.so/acme/Fix-The-Thing-0123456789abcdef0123456789abcdef?pvs=4"
            ),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
    }

    #[test]
    fn extract_page_id_from_a_bare_id() {
        assert_eq!(
            extract_page_id("0123456789abcdef0123456789abcdef"),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
    }

    #[test]
    fn extract_page_id_leaves_a_hyphenated_uuid_alone() {
        let uuid = "01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(extract_page_id(uuid), uuid);
    }

    #[test]
    fn extract_page_id_passes_through_anything_too_short() {
        assert_eq!(extract_page_id("  not-an-id  "), "not-an-id");
    }

    #[test]
    fn slugify_basic_title() {
        assert_eq!(slugify("Fix invoice PDF export"), "fix-invoice-pdf-export");
        assert_eq!(slugify("  spaced -- out!! "), "spaced-out");
        assert_eq!(slugify("Déjà vu"), "d-j-vu");
    }

    #[test]
    fn parse_ticket_from_fixture() {
        let page = serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "url": "https://www.notion.so/Fix-thing-1111111111111111111111111111",
            "last_edited_time": "2026-08-01T12:00:00.000Z",
            "properties": {
                "Name": {
                    "type": "title",
                    "title": [{ "plain_text": "Fix invoice PDF export" }]
                },
                "Status": {
                    "type": "status",
                    "status": { "name": "2 - 🏁 Ready for dev" }
                },
                "Type": {
                    "type": "select",
                    "select": { "name": "Bug" }
                }
            }
        });

        let ticket = parse_ticket(&page, "Status").expect("ticket should parse");
        assert_eq!(ticket.page_id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(ticket.title, "Fix invoice PDF export");
        assert_eq!(ticket.status, "2 - 🏁 Ready for dev");
        assert_eq!(ticket.ticket_type.as_deref(), Some("Bug"));
        assert_eq!(ticket.slug, "fix-invoice-pdf-export");
    }

    #[test]
    fn parse_ticket_without_type() {
        let page = serde_json::json!({
            "id": "22222222-2222-2222-2222-222222222222",
            "url": "https://www.notion.so/Other-thing-2222222222222222222222222222",
            "last_edited_time": "2026-08-02T12:00:00.000Z",
            "properties": {
                "Name": {
                    "type": "title",
                    "title": [{ "plain_text": "Other thing" }]
                },
                "Status": {
                    "type": "status",
                    "status": { "name": "3 - In progress" }
                }
            }
        });

        let ticket = parse_ticket(&page, "Status").expect("ticket should parse");
        assert_eq!(ticket.ticket_type, None);
        assert_eq!(ticket.status, "3 - In progress");
    }

    #[test]
    fn resolve_database_id_finds_child_database_block() {
        let children = serde_json::json!({
            "results": [
                { "type": "paragraph", "id": "aaaa" },
                { "type": "child_database", "id": "bbbb-database-id" }
            ]
        });
        let block = children["results"][1].clone();
        assert_eq!(block["type"].as_str(), Some("child_database"));
        assert_eq!(block["id"].as_str(), Some("bbbb-database-id"));
    }

    #[test]
    fn fetch_database_schema_parses_status_and_people_properties() {
        let database = serde_json::json!({
            "properties": {
                "Status": {
                    "type": "status",
                    "status": {
                        "options": [
                            { "name": "1 - Backlog" },
                            { "name": "2 - 🏁 Ready for dev" }
                        ]
                    }
                },
                "Assignee": { "type": "people" },
                "Name": { "type": "title" }
            }
        });
        let properties = database.get("properties").unwrap().as_object().unwrap();

        let mut status_property = None;
        let mut status_options = Vec::new();
        let mut assignee_property = None;
        for (name, definition) in properties {
            match definition.get("type").and_then(Value::as_str) {
                Some("status") => {
                    status_property = Some(name.clone());
                    status_options = definition["status"]["options"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|option| option["name"].as_str().unwrap().to_string())
                        .collect();
                }
                Some("people") => assignee_property = Some(name.clone()),
                _ => {}
            }
        }

        assert_eq!(status_property.as_deref(), Some("Status"));
        assert_eq!(assignee_property.as_deref(), Some("Assignee"));
        assert_eq!(
            status_options,
            vec![
                "1 - Backlog".to_string(),
                "2 - 🏁 Ready for dev".to_string()
            ]
        );
    }
}
