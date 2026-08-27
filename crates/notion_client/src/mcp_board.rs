//! Board discovery and ticket querying through Notion's MCP `notion-fetch`
//! and `notion-query-data-sources` tools — the OAuth-token equivalent of
//! `crate::notion_client`'s REST-based `resolve_database_id`/
//! `fetch_database_schema`/`query_tickets`, feeding the same [`TicketRef`].
//!
//! `notion-fetch` returns a page/database's content as a text blob with
//! embedded pseudo-XML tags wrapping JSON payloads (confirmed empirically
//! against a live workspace), not structured JSON throughout — hence the
//! tag-scanning helpers below rather than a single `serde_json::from_str`.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::TicketRef;
use crate::mcp::{McpClient, McpError};

/// Everything needed to run the filtered ticket query once discovered from a
/// named view (e.g. "Team Board").
#[derive(Debug, Clone, PartialEq)]
pub struct McpBoardConfig {
    pub data_source_url: String,
    pub title_property: String,
    pub status_property: String,
    pub status_values: Vec<String>,
    pub person_property: String,
    /// Name of the board's `unique_id` property (what renders as `CT-1487`),
    /// when it has one. Selected as an extra column so ticket rows can show a
    /// human-facing reference.
    pub issue_id_property: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DataSourceState {
    url: String,
    schema: HashMap<String, PropertyDef>,
}

#[derive(Debug, Deserialize)]
struct PropertyDef {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    groups: Option<StatusGroups>,
}

#[derive(Debug, Default, Deserialize)]
struct StatusGroups {
    #[serde(default)]
    to_do: Vec<StatusOption>,
    #[serde(default)]
    in_progress: Vec<StatusOption>,
    #[serde(default)]
    current: Vec<StatusOption>,
    #[serde(default)]
    future: Vec<StatusOption>,
    #[serde(default)]
    complete: Vec<StatusOption>,
}

impl StatusGroups {
    fn options_for(&self, key: &str) -> &[StatusOption] {
        match key {
            "to_do" => &self.to_do,
            "in_progress" => &self.in_progress,
            "current" => &self.current,
            "future" => &self.future,
            "complete" => &self.complete,
            _ => &[],
        }
    }
}

#[derive(Debug, Deserialize)]
struct StatusOption {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewDef {
    name: String,
    #[serde(default)]
    data_source_url: Option<String>,
    #[serde(default)]
    simple_filters: Vec<SimpleFilterEntry>,
    #[serde(default)]
    group_by: Option<GroupByDef>,
}

#[derive(Debug, Deserialize)]
struct SimpleFilterEntry {
    filter: FilterDef,
}

#[derive(Debug, Deserialize)]
struct FilterDef {
    operator: String,
    property: String,
    #[serde(default)]
    value: Option<FilterValue>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FilterValue {
    Single(FilterValueEntry),
    Multiple(Vec<FilterValueEntry>),
}

#[derive(Debug, Deserialize)]
struct FilterValueEntry {
    #[serde(rename = "type")]
    kind: String,
    /// Deliberately untyped: sibling filters on the same view (e.g.
    /// `checkbox_is`, `relation_contains`) carry booleans or URLs here, and a
    /// strict `String` type would fail deserializing the whole view over an
    /// entry we don't even care about.
    value: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupByDef {
    property: String,
    property_type: String,
}

/// Maps a view filter's human-readable status group name (e.g. "In
/// progress", "To-do") to the schema's `groups` JSON key. Best-effort:
/// unrecognized group names fall back to `to_do` rather than erroring, since
/// dropping a handful of tickets from an unusual group name is preferable to
/// failing discovery entirely.
fn group_key_for(value: &str) -> &'static str {
    match value.to_lowercase().replace(['-', ' '], "_").as_str() {
        "in_progress" | "inprogress" => "in_progress",
        "complete" | "done" => "complete",
        "current" => "current",
        "future" | "next" => "future",
        _ => "to_do",
    }
}

/// Extracts every occurrence of `<tag ...>...</tag>` (non-nested) from
/// `text`, returning the tag's `url="..."` attribute (if any) alongside the
/// inner text. Notion's `notion-fetch` tool wraps JSON payloads this way
/// instead of returning structured JSON throughout.
fn extract_tagged_blocks<'a>(text: &'a str, tag: &str) -> Vec<(Option<&'a str>, &'a str)> {
    let mut out = Vec::new();
    let open_prefix = format!("<{tag}");
    let close_tag = format!("</{tag}>");
    let mut rest = text;
    let mut search_from = 0;
    while let Some(found) = rest[search_from..].find(&open_prefix) {
        let start = search_from + found;
        // Notion's fetch output wraps same-named blocks in a pluralized
        // container (e.g. `<views>` around `<view>` entries, `<data-sources>`
        // around `<data-source>` entries) — a plain substring search for
        // "<view" also matches "<views", swallowing the first real block as
        // garbage. Require a proper tag boundary (whitespace, `>`, or `/`)
        // right after the prefix so containers are skipped, not matched.
        let boundary_ok = rest[start + open_prefix.len()..]
            .chars()
            .next()
            .is_some_and(|ch| ch == '>' || ch == '/' || ch.is_whitespace());
        if !boundary_ok {
            search_from = start + open_prefix.len();
            continue;
        }
        let after = &rest[start..];
        let Some(gt) = after.find('>') else { break };
        let tag_open = &after[..gt];
        let body_start = gt + 1;
        let Some(end) = after[body_start..].find(&close_tag) else {
            break;
        };
        let inner = &after[body_start..body_start + end];
        out.push((extract_attr(tag_open, "url"), inner));
        rest = &after[body_start + end + close_tag.len()..];
        search_from = 0;
    }
    out
}

fn extract_attr<'a>(tag_open: &'a str, attr: &str) -> Option<&'a str> {
    let needle = format!("{attr}=\"");
    let start = tag_open.find(&needle)? + needle.len();
    let rest = &tag_open[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

fn fetch_text(client_result: &Value) -> &str {
    client_result
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Finds the named view (e.g. "Team Board") on the given page or database,
/// resolving its status filter into a concrete flat list of option strings
/// and identifying its person/title properties.
pub async fn discover_board(
    client: &mut McpClient,
    page_or_database_id: &str,
    view_name: &str,
) -> Result<McpBoardConfig, McpError> {
    let page_result = client
        .call_tool("notion-fetch", json!({ "id": page_or_database_id }))
        .await?;
    let page_payload = page_result.payload()?;
    let page_text = fetch_text(&page_payload);
    log::info!(
        "discover_board: page {page_or_database_id} fetch returned {} bytes: {}",
        page_text.len(),
        &page_text[..page_text.len().min(2000)]
    );

    let mut database_candidates: Vec<String> = Vec::new();
    if page_text.contains("<data-source-state>") {
        database_candidates.push(page_or_database_id.to_string());
    } else {
        for (url, _inner) in extract_tagged_blocks(page_text, "database") {
            if let Some(url) = url {
                database_candidates.push(url.to_string());
            }
        }
    }
    log::info!(
        "discover_board: found {} database candidate(s): {database_candidates:?}",
        database_candidates.len()
    );
    if database_candidates.is_empty() {
        return Err(McpError::Other(format!(
            "no databases found on {page_or_database_id}"
        )));
    }

    for candidate in database_candidates {
        let db_result = client
            .call_tool("notion-fetch", json!({ "id": candidate }))
            .await?;
        let db_payload = db_result.payload()?;
        let db_text = fetch_text(&db_payload);
        log::info!(
            "discover_board: candidate {candidate} fetch returned {} bytes: {}",
            db_text.len(),
            &db_text[..db_text.len().min(2000)]
        );

        let Some((_, state_json)) = extract_tagged_blocks(db_text, "data-source-state")
            .into_iter()
            .next()
        else {
            log::info!("discover_board: candidate {candidate} has no <data-source-state> block");
            continue;
        };
        let state: DataSourceState = serde_json::from_str(state_json).map_err(|error| {
            McpError::Other(format!("failed to parse data source schema: {error}"))
        })?;

        let view_blocks = extract_tagged_blocks(db_text, "view");
        log::info!(
            "discover_board: candidate {candidate} has {} <view> block(s): {:?}",
            view_blocks.len(),
            view_blocks
                .iter()
                .map(|(_, json)| serde_json::from_str::<ViewDef>(json)
                    .map(|v| v.name)
                    .unwrap_or_else(|error| format!("<parse error: {error}>")))
                .collect::<Vec<_>>()
        );
        for (_, view_json) in view_blocks {
            let Ok(view) = serde_json::from_str::<ViewDef>(view_json) else {
                continue;
            };
            if view.name != view_name {
                continue;
            }
            return build_board_config(view, state);
        }
    }

    Err(McpError::Other(format!(
        "view {view_name:?} not found on {page_or_database_id}"
    )))
}

/// Notion's fetch output wraps cross-reference values (e.g. a view's
/// `dataSourceUrl` field) in `{{...}}` as a placeholder convention; the
/// underlying `collection://...` id must be unwrapped before it's usable as
/// an actual `data_source_urls` argument to the query tool.
fn strip_reference_braces(value: &str) -> &str {
    value
        .strip_prefix("{{")
        .and_then(|value| value.strip_suffix("}}"))
        .unwrap_or(value)
}

fn build_board_config(view: ViewDef, state: DataSourceState) -> Result<McpBoardConfig, McpError> {
    let data_source_url =
        strip_reference_braces(&view.data_source_url.unwrap_or(state.url)).to_string();

    let mut status_property = None;
    let mut status_values = Vec::new();
    for entry in &view.simple_filters {
        if entry.filter.operator != "status_is" {
            continue;
        }
        status_property = Some(entry.filter.property.clone());
        let Some(filter_value) = &entry.filter.value else {
            continue;
        };
        let entries: Vec<&FilterValueEntry> = match filter_value {
            FilterValue::Single(entry) => vec![entry],
            FilterValue::Multiple(entries) => entries.iter().collect(),
        };
        let property_def = state.schema.get(entry.filter.property.as_str());
        for value_entry in entries {
            match value_entry.kind.as_str() {
                "is_option" => {
                    if let Some(name) = value_entry.value.as_str() {
                        status_values.push(name.to_string());
                    }
                }
                "is_group" => {
                    let Some(group_name) = value_entry.value.as_str() else {
                        continue;
                    };
                    if let Some(groups) = property_def.and_then(|def| def.groups.as_ref()) {
                        status_values.extend(
                            groups
                                .options_for(group_key_for(group_name))
                                .iter()
                                .map(|option| option.name.clone()),
                        );
                    }
                }
                _ => {}
            }
        }
    }
    status_values.sort();
    status_values.dedup();

    let status_property = status_property
        .ok_or_else(|| McpError::Other(format!("view {:?} has no status filter", view.name)))?;

    let person_property = view
        .group_by
        .as_ref()
        .filter(|group_by| group_by.property_type == "person")
        .map(|group_by| group_by.property.clone())
        .or_else(|| {
            state
                .schema
                .iter()
                .find(|(_, def)| def.kind == "person")
                .map(|(name, _)| name.clone())
        })
        .ok_or_else(|| McpError::Other("no person property found in schema".to_string()))?;

    let title_property = state
        .schema
        .iter()
        .find(|(_, def)| def.kind == "title")
        .map(|(name, _)| name.clone())
        .ok_or_else(|| McpError::Other("no title property found in schema".to_string()))?;

    let issue_id_property = state
        .schema
        .iter()
        .find(|(_, def)| def.kind == "unique_id")
        .map(|(name, _)| name.clone());

    Ok(McpBoardConfig {
        data_source_url,
        title_property,
        status_property,
        status_values,
        person_property,
        issue_id_property,
    })
}

/// Resolves the connected user's own Notion identity (for the person-column
/// filter) via `notion-fetch id="self"`.
pub async fn resolve_self_user_id(client: &mut McpClient) -> Result<String, McpError> {
    let result = client
        .call_tool("notion-fetch", json!({ "id": "self" }))
        .await?;
    let payload = result.payload()?;
    payload
        .get("self")
        .and_then(|value| value.get("user"))
        .and_then(|user| user.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| McpError::Other("self fetch had no user id".to_string()))
}

#[derive(Debug, Deserialize)]
struct QueryResultsEnvelope {
    #[serde(default)]
    results: Vec<Value>,
}

/// Queries the board for tickets assigned to `person_id` in one of
/// `config.status_values`, mapping rows into [`TicketRef`].
pub async fn query_tickets(
    client: &mut McpClient,
    config: &McpBoardConfig,
    person_id: &str,
) -> Result<Vec<TicketRef>, McpError> {
    if config.status_values.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat_n("?", config.status_values.len())
        .collect::<Vec<_>>()
        .join(", ");
    let issue_id_column = config
        .issue_id_property
        .as_deref()
        .map(|property| format!(", \"{}\"", property.replace('"', "\"\"")))
        .unwrap_or_default();
    let query = format!(
        "SELECT url, \"{title}\", \"{status}\"{issue_id_column} FROM \"{data_source}\" WHERE \"{person}\" LIKE ? AND \"{status}\" IN ({placeholders})",
        title = config.title_property,
        status = config.status_property,
        person = config.person_property,
        data_source = config.data_source_url,
    );

    let mut params = vec![Value::String(format!("%{person_id}%"))];
    params.extend(config.status_values.iter().cloned().map(Value::String));

    let result = client
        .call_tool(
            "notion-query-data-sources",
            json!({
                "data": {
                    "data_source_urls": [config.data_source_url],
                    "query": query,
                    "params": params,
                }
            }),
        )
        .await?;
    if result.is_error {
        return Err(McpError::Other(format!(
            "notion-query-data-sources returned an error: {}",
            result.joined_text()
        )));
    }
    let payload = result.payload()?;
    let envelope: QueryResultsEnvelope = serde_json::from_value(payload)
        .map_err(|error| McpError::Other(format!("unexpected query result shape: {error}")))?;

    Ok(envelope
        .results
        .iter()
        .filter_map(|row| {
            parse_row(
                row,
                &config.title_property,
                &config.status_property,
                config.issue_id_property.as_deref(),
            )
        })
        .collect())
}

/// Writes a new value into the board's status property for one page.
///
/// `page_uuid` must be the page's real UUID: on this path
/// [`TicketRef::page_id`] is the URL's slug-plus-id segment, which
/// `notion-update-page` rejects — use [`TicketRef::notion_page_uuid`].
pub async fn set_page_status(
    client: &mut McpClient,
    page_uuid: &str,
    status_property: &str,
    status: &str,
) -> Result<(), McpError> {
    let result = client
        .call_tool(
            "notion-update-page",
            json!({
                "page_id": page_uuid,
                "command": "update_properties",
                "properties": { status_property: status },
            }),
        )
        .await?;
    if result.is_error {
        return Err(McpError::Other(format!(
            "notion-update-page returned an error: {}",
            result.joined_text()
        )));
    }
    Ok(())
}

/// Looks tickets up by their page url, ignoring the board's status and
/// assignee filters.
///
/// [`query_tickets`] can only ever return what the filter matches, so a ticket
/// that moved to a status outside it — or was reassigned — silently stops being
/// reported and its last-seen status would stand forever. This is how a ticket
/// someone is still working in gets its real status back.
pub async fn query_tickets_by_url(
    client: &mut McpClient,
    config: &McpBoardConfig,
    urls: &[String],
) -> Result<Vec<TicketRef>, McpError> {
    if urls.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders = std::iter::repeat_n("?", urls.len())
        .collect::<Vec<_>>()
        .join(", ");
    let issue_id_column = config
        .issue_id_property
        .as_deref()
        .map(|property| format!(", \"{}\"", property.replace('"', "\"\"")))
        .unwrap_or_default();
    let query = format!(
        "SELECT url, \"{title}\", \"{status}\"{issue_id_column} FROM \"{data_source}\" WHERE url IN ({placeholders})",
        title = config.title_property,
        status = config.status_property,
        data_source = config.data_source_url,
    );
    let params: Vec<Value> = urls.iter().cloned().map(Value::String).collect();

    let result = client
        .call_tool(
            "notion-query-data-sources",
            json!({
                "data": {
                    "data_source_urls": [config.data_source_url],
                    "query": query,
                    "params": params,
                }
            }),
        )
        .await?;
    if result.is_error {
        return Err(McpError::Other(format!(
            "notion-query-data-sources returned an error: {}",
            result.joined_text()
        )));
    }
    let payload = result.payload()?;
    let envelope: QueryResultsEnvelope = serde_json::from_value(payload)
        .map_err(|error| McpError::Other(format!("unexpected query result shape: {error}")))?;

    Ok(envelope
        .results
        .iter()
        .filter_map(|row| {
            parse_row(
                row,
                &config.title_property,
                &config.status_property,
                config.issue_id_property.as_deref(),
            )
        })
        .collect())
}

fn parse_row(
    row: &Value,
    title_property: &str,
    status_property: &str,
    issue_id_property: Option<&str>,
) -> Option<TicketRef> {
    let raw_url = row.get("url").and_then(Value::as_str)?;
    // Derived from the *raw* url: this is the primary key of the on-disk
    // ticket store (see `TicketRef::notion_page_uuid`), so normalizing the
    // url must not shift it.
    let page_id = raw_url.rsplit('/').next().unwrap_or(raw_url).to_string();
    let url = crate::normalize_page_url(raw_url);
    let title = row
        .get(title_property)
        .and_then(Value::as_str)
        .unwrap_or("Untitled")
        .to_string();
    let status = row
        .get(status_property)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // The query engine hands a `unique_id` back either already rendered
    // (`"CT-1487"`) or as its bare number, depending on the column.
    let issue_id = issue_id_property.and_then(|property| {
        let value = row.get(property)?;
        value
            .as_str()
            .map(str::to_string)
            .or_else(|| value.as_i64().map(|number| number.to_string()))
    });

    Some(TicketRef {
        slug: crate::slugify(&title),
        page_id,
        title,
        url,
        status,
        last_edited_time: String::new(),
        ticket_type: None,
        issue_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_data_source_state() -> &'static str {
        r#"{
            "url": "collection://fake-data-source-id",
            "schema": {
                "Task": { "type": "title" },
                "Owner": { "type": "person" },
                "Status": {
                    "type": "status",
                    "groups": {
                        "to_do": [{ "name": "1 - Todo" }],
                        "in_progress": [{ "name": "2 - Doing" }, { "name": "3 - Review" }],
                        "complete": [{ "name": "4 - Done" }]
                    }
                }
            }
        }"#
    }

    fn fixture_view(name: &str, status_filter_value: &str) -> String {
        format!(
            r#"{{
                "name": "{name}",
                "dataSourceUrl": "collection://fake-data-source-id",
                "groupBy": {{ "property": "Owner", "propertyType": "person" }},
                "simpleFilters": [
                    {{
                        "filter": {{
                            "operator": "status_is",
                            "property": "Status",
                            "propertyType": "status",
                            "value": {status_filter_value}
                        }}
                    }}
                ]
            }}"#
        )
    }

    #[test]
    fn extract_tagged_blocks_finds_url_attr_and_inner_text() {
        let text =
            r#"before <data-source url="{{collection://abc}}">inner content</data-source> after"#;
        let blocks = extract_tagged_blocks(text, "data-source");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, Some("{{collection://abc}}"));
        assert_eq!(blocks[0].1, "inner content");
    }

    #[test]
    fn extract_tagged_blocks_skips_pluralized_wrapper_container() {
        // Notion's real fetch output wraps every <view> in a <views> container
        // — a naive "<view" substring search also matches "<views>" itself,
        // swallowing the first real view as garbage. Regression test for that
        // exact bug (caught via live testing against a real workspace).
        let text = "<views>\n<view url=\"{{view://first}}\">\none</view>\n<view url=\"{{view://second}}\">two</view>\n</views>";
        let blocks = extract_tagged_blocks(text, "view");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], (Some("{{view://first}}"), "\none"));
        assert_eq!(blocks[1], (Some("{{view://second}}"), "two"));
    }

    #[test]
    fn extract_tagged_blocks_handles_multiple_blocks_without_attrs() {
        let text = "<view>one</view>text<view>two</view>";
        let blocks = extract_tagged_blocks(text, "view");
        assert_eq!(
            blocks.iter().map(|b| b.1).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
    }

    #[test]
    fn build_board_config_expands_is_group_and_is_option() {
        let state: DataSourceState = serde_json::from_str(fixture_data_source_state()).unwrap();
        let view_json = fixture_view(
            "Team Board",
            r#"[{"type": "is_group", "value": "In progress"}, {"type": "is_option", "value": "1 - Todo"}]"#,
        );
        let view: ViewDef = serde_json::from_str(&view_json).unwrap();

        let config = build_board_config(view, state).expect("should build config");
        assert_eq!(config.data_source_url, "collection://fake-data-source-id");
        assert_eq!(config.title_property, "Task");
        assert_eq!(config.status_property, "Status");
        assert_eq!(config.person_property, "Owner");
        let mut expected = vec![
            "1 - Todo".to_string(),
            "2 - Doing".to_string(),
            "3 - Review".to_string(),
        ];
        expected.sort();
        assert_eq!(config.status_values, expected);
    }

    #[test]
    fn build_board_config_ignores_sibling_non_status_filters() {
        // A view with a checkbox_is filter (boolean value) alongside the
        // status_is filter must still parse — this is the exact shape that
        // would break a naive `value: String`-typed FilterValueEntry.
        let state: DataSourceState = serde_json::from_str(fixture_data_source_state()).unwrap();
        let view_json = r#"{
                "name": "Mixed Filters",
                "dataSourceUrl": "collection://fake-data-source-id",
                "simpleFilters": [
                    {
                        "filter": {
                            "operator": "checkbox_is",
                            "property": "Blocked",
                            "propertyType": "checkbox",
                            "value": { "type": "exact", "value": false }
                        }
                    },
                    {
                        "filter": {
                            "operator": "status_is",
                            "property": "Status",
                            "propertyType": "status",
                            "value": {"type": "is_option", "value": "4 - Done"}
                        }
                    },
                    {
                        "filter": {
                            "operator": "is_empty",
                            "property": "Owner",
                            "propertyType": "person"
                        }
                    }
                ]
            }"#;
        let view: ViewDef =
            serde_json::from_str(&view_json).expect("mixed-filter view should parse");
        let config = build_board_config(view, state).expect("should build config");
        assert_eq!(config.status_values, vec!["4 - Done".to_string()]);
    }

    #[test]
    fn query_result_row_parses_into_ticket_ref() {
        let row = serde_json::json!({
            "url": "https://www.notion.so/fake-workspace/Fake-Ticket-Title-00000000000000000000000000000000",
            "Task": "Fake Ticket Title",
            "Status": "2 - Doing"
        });
        let ticket = parse_row(&row, "Task", "Status", None).expect("row should parse");
        assert_eq!(ticket.title, "Fake Ticket Title");
        assert_eq!(ticket.status, "2 - Doing");
        assert_eq!(ticket.slug, "fake-ticket-title");
        assert_eq!(ticket.issue_id, None);
        // `page_id` stays the URL's slug-plus-id segment (it keys the ticket
        // store); the UUID for API calls comes from `notion_page_uuid`.
        assert_eq!(
            ticket.page_id,
            "Fake-Ticket-Title-00000000000000000000000000000000"
        );
        assert_eq!(
            ticket.notion_page_uuid(),
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn query_result_row_repairs_a_bare_id_url() {
        let row = serde_json::json!({
            "url": "https://app.notion.com/00000000000000000000000000000000",
            "Task": "Fake Ticket Title",
            "Status": "2 - Doing"
        });
        let ticket = parse_row(&row, "Task", "Status", None).expect("row should parse");
        assert_eq!(
            ticket.url,
            "https://app.notion.com/p/00000000000000000000000000000000"
        );
        assert_eq!(ticket.page_id, "00000000000000000000000000000000");
    }

    #[test]
    fn query_result_row_reads_a_rendered_issue_id() {
        let row = serde_json::json!({
            "url": "https://www.notion.so/w/T-00000000000000000000000000000000",
            "Task": "T",
            "Status": "2 - Doing",
            "ID": "CT-1487"
        });
        let ticket = parse_row(&row, "Task", "Status", Some("ID")).expect("row should parse");
        assert_eq!(ticket.issue_id.as_deref(), Some("CT-1487"));
    }

    #[test]
    fn query_result_row_reads_a_bare_numeric_issue_id() {
        let row = serde_json::json!({
            "url": "https://www.notion.so/w/T-00000000000000000000000000000000",
            "Task": "T",
            "Status": "2 - Doing",
            "ID": 1487
        });
        let ticket = parse_row(&row, "Task", "Status", Some("ID")).expect("row should parse");
        assert_eq!(ticket.issue_id.as_deref(), Some("1487"));
    }

    #[test]
    fn build_board_config_discovers_a_unique_id_property() {
        let state: DataSourceState = serde_json::from_str(
            r#"{
                "url": "collection://fake",
                "schema": {
                    "Task": { "type": "title" },
                    "Owner": { "type": "person" },
                    "ID": { "type": "unique_id" },
                    "Status": { "type": "status", "groups": { "to_do": [{ "name": "1 - Todo" }] } }
                }
            }"#,
        )
        .expect("state should parse");
        let view: ViewDef = serde_json::from_str(&fixture_view(
            "Team Board",
            r#"{"type":"is_group","value":"To-do"}"#,
        ))
        .expect("view should parse");
        let config = build_board_config(view, state).expect("should build config");
        assert_eq!(config.issue_id_property.as_deref(), Some("ID"));
    }
}
