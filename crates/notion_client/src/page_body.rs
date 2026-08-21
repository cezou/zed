//! Reads a Notion page's body over MCP and renders it as plain markdown.
//!
//! `notion-fetch` returns neither blocks nor markdown: it returns a JSON
//! envelope whose `text` field is an XML-ish "view" of the page. The readable
//! body lives inside `<content>…</content>`, interleaved with Notion's own
//! markup (`<span>`, tables, `<mention-user/>`, media tags). Everything here
//! exists to get from that blob to something a human can edit in a prompt.

use serde_json::json;

use crate::mcp::{McpClient, McpError};

#[derive(Debug, Clone, PartialEq)]
pub struct PageBody {
    pub title: String,
    pub url: String,
    /// The page's `<content>` block, de-tagged to plain markdown-ish text.
    /// Empty for a page with no body.
    pub markdown: String,
}

/// Fetches a Notion page's body.
///
/// `id` must be a bare page id or a full `notion.so` URL. Pass
/// [`crate::extract_page_id`] applied to the ticket's **url** — on the
/// OAuth/MCP query path `TicketRef::page_id` is the URL's slug-plus-id
/// segment, which `notion-fetch` does not accept.
pub async fn fetch_page_body(client: &mut McpClient, id: &str) -> Result<PageBody, McpError> {
    // `notion-fetch` validates `id` at the top level — unlike
    // `notion-query-data-sources`, it rejects a `{"data": {…}}` wrapper.
    let result = client
        .call_tool("notion-fetch", json!({ "id": id }))
        .await?;

    if result.is_error {
        return Err(McpError::Other(format!(
            "notion-fetch returned an error for {id}: {}",
            result.joined_text()
        )));
    }

    // A payload that isn't JSON is presumably already the body itself, so
    // degrade to cleaning the raw text rather than failing the whole fetch.
    let Ok(payload) = result.payload() else {
        return Ok(PageBody {
            title: String::new(),
            url: String::new(),
            markdown: clean_body(&result.joined_text()),
        });
    };

    let string_field = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };

    Ok(PageBody {
        title: string_field("title"),
        url: string_field("url"),
        markdown: body_from_view_text(&string_field("text")),
    })
}

/// Unwraps the readable part of a `notion-fetch` view blob.
pub fn body_from_view_text(view_text: &str) -> String {
    if view_text.trim().is_empty() {
        return String::new();
    }

    if let Some(inner) = first_tagged_block(view_text, "content") {
        return clean_body(inner);
    }

    // No `<content>` wrapper: drop the metadata blocks and keep what's left.
    let mut stripped = view_text.to_string();
    for tag in ["properties", "ancestor-path"] {
        stripped = remove_tagged_blocks(&stripped, tag);
    }
    clean_body(&stripped)
}

/// Inner text of the first non-nested `<tag …>…</tag>` occurrence.
fn first_tagged_block<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open_prefix = format!("<{tag}");
    let close_tag = format!("</{tag}>");
    let mut search_from = 0;
    loop {
        let start = search_from + text[search_from..].find(&open_prefix)?;
        let after_prefix = start + open_prefix.len();
        // `<content` also prefixes a hypothetical `<contents>` container;
        // require a real tag boundary so the container isn't mistaken for the
        // block, the way `<views>` shadows `<view>` elsewhere in this format.
        let boundary_ok = text[after_prefix..]
            .chars()
            .next()
            .is_some_and(|ch| ch == '>' || ch == '/' || ch.is_whitespace());
        if !boundary_ok {
            search_from = after_prefix;
            continue;
        }
        let open_end = start + text[start..].find('>')?;
        let body_start = open_end + 1;
        let end = body_start + text[body_start..].find(&close_tag)?;
        return Some(&text[body_start..end]);
    }
}

/// Deletes every `<tag …>…</tag>` block, contents included.
fn remove_tagged_blocks(text: &str, tag: &str) -> String {
    let open_prefix = format!("<{tag}");
    let close_tag = format!("</{tag}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(start) = rest.find(&open_prefix) else {
            out.push_str(rest);
            return out;
        };
        let Some(open_end) = rest[start..].find('>').map(|offset| start + offset) else {
            out.push_str(rest);
            return out;
        };
        let body_start = open_end + 1;
        let Some(end) = rest[body_start..]
            .find(&close_tag)
            .map(|offset| body_start + offset)
        else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        rest = &rest[end + close_tag.len()..];
    }
}

/// Turns a raw view fragment into readable text: image markdown out (Notion's
/// signed S3 URLs run to kilobytes and drown the prose), markup out, blank
/// runs collapsed.
pub fn clean_body(raw: &str) -> String {
    let without_images = remove_image_markdown(raw);
    let text = strip_tags(&without_images);
    collapse_whitespace(&text)
}

/// Drops `![alt](url)` spans. Hand-rolled rather than regex-based so the crate
/// keeps its current dependency set.
fn remove_image_markdown(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        let Some(bang) = rest.find('!') else {
            out.push_str(rest);
            return out;
        };
        match image_span_len(&rest[bang..]) {
            Some(len) => {
                out.push_str(&rest[..bang]);
                rest = &rest[bang + len..];
            }
            None => {
                out.push_str(&rest[..bang + 1]);
                rest = &rest[bang + 1..];
            }
        }
    }
}

/// Byte length of a `![…](…)` span at the start of `rest`, if there is one.
fn image_span_len(rest: &str) -> Option<usize> {
    if !rest.starts_with("![") {
        return None;
    }
    let alt_end = rest.find("](")?;
    // An alt text spanning lines is far more likely to be prose that happens
    // to contain `![` than a real image span.
    if rest[2..alt_end].contains('\n') {
        return None;
    }
    let url_end = rest[alt_end + 2..].find(')')?;
    Some(alt_end + 2 + url_end + 1)
}

/// Removes XML/HTML markup, keeping inner text and the block structure that
/// makes the result readable.
fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>').map(|offset| start + offset) else {
            // A stray `<` with no closing `>` is literal text, not markup.
            out.push_str(&rest[start..]);
            return decode_entities(&out);
        };
        out.push_str(tag_replacement(&rest[start + 1..end]));
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    decode_entities(&out)
}

/// What a tag leaves behind so the text keeps its shape.
fn tag_replacement(tag_body: &str) -> &'static str {
    let closing = tag_body.starts_with('/');
    let name = tag_body
        .trim_start_matches('/')
        .split(|ch: char| ch.is_whitespace() || ch == '/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    match name.as_str() {
        "br" => "\n",
        "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote"
            if closing =>
        {
            "\n"
        }
        "td" | "th" if closing => " ",
        _ => "",
    }
}

fn decode_entities(raw: &str) -> String {
    raw.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&#039;", "'")
        // Last: an escaped ampersand must not resurrect the entities above.
        .replace("&amp;", "&")
}

fn collapse_whitespace(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_blank_line = false;
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            pending_blank_line = true;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if pending_blank_line {
                out.push('\n');
            }
        }
        pending_blank_line = false;
        out.push_str(trimmed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_content_block() {
        let view = concat!(
            "<page url=\"https://notion.so/x\">",
            "<properties>{\"Status\":\"3 - In progress\"}</properties>",
            "<ancestor-path>Daily Board</ancestor-path>",
            "<content>- first bullet\n- second bullet</content>",
            "</page>",
        );
        assert_eq!(body_from_view_text(view), "- first bullet\n- second bullet");
    }

    #[test]
    fn falls_back_to_the_page_when_there_is_no_content_block() {
        let view = concat!(
            "<page><properties>{\"Status\":\"x\"}</properties>",
            "<p>Body paragraph</p></page>",
        );
        assert_eq!(body_from_view_text(view), "Body paragraph");
    }

    #[test]
    fn empty_view_yields_an_empty_body() {
        assert_eq!(body_from_view_text("   "), "");
        assert_eq!(body_from_view_text("<content></content>"), "");
    }

    #[test]
    fn drops_image_markdown_but_keeps_surrounding_prose() {
        let raw = "before ![shot](https://prod-files-secure.s3.amazonaws.com/a?x=1) after";
        assert_eq!(clean_body(raw), "before  after");
    }

    #[test]
    fn keeps_a_bang_that_is_not_an_image_span() {
        assert_eq!(
            clean_body("wow! [link](https://x)"),
            "wow! [link](https://x)"
        );
    }

    #[test]
    fn structural_tags_become_line_breaks_and_table_cells_spaces() {
        let raw = "<p>one</p><p>two</p><table><tr><td>a</td><td>b</td></tr></table>";
        assert_eq!(clean_body(raw), "one\ntwo\na b");
    }

    #[test]
    fn decodes_entities_without_resurrecting_markup() {
        assert_eq!(clean_body("a &amp;lt; b"), "a &lt; b");
        assert_eq!(clean_body("5 &lt; 6 &amp;&amp; 7 &gt; 6"), "5 < 6 && 7 > 6");
        assert_eq!(
            clean_body("Fran&#39;s &quot;quote&quot;"),
            "Fran's \"quote\""
        );
    }

    #[test]
    fn collapses_runs_of_blank_lines() {
        assert_eq!(clean_body("a\n\n\n\nb\n   \nc"), "a\n\nb\n\nc");
    }

    #[test]
    fn strips_notion_span_wrappers_around_prose() {
        let raw = "<span discussion-urls=\"x\">Changer l&#39;affichage</span>";
        assert_eq!(clean_body(raw), "Changer l'affichage");
    }

    #[test]
    fn a_stray_angle_bracket_stays_literal() {
        assert_eq!(clean_body("a < b and c"), "a < b and c");
    }

    #[test]
    fn a_container_tag_does_not_shadow_the_content_block() {
        let view = "<contents><content>real body</content></contents>";
        assert_eq!(body_from_view_text(view), "real body");
    }
}
