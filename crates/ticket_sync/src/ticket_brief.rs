//! Renders the markdown brief that seeds a ticket's Claude Code session.

use notion_client::TicketRef;

const MISSION_PLACEHOLDER: &str =
    "_No description on the Notion page — describe the mission here before launching._";

const WORKING_BEHAVIOR: &str = "\
## Working behavior
- conventional commits
- ask before destructive git operations
- no push / no MR
";

/// Builds the brief for `ticket`, embedding `body` (the Notion page body) as
/// the mission when there is one.
pub fn render_brief(ticket: &TicketRef, body: Option<&str>) -> String {
    let mut brief = format!("# Ticket: {}\n", ticket.title.trim());

    let issue_id = ticket
        .issue_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let status = Some(ticket.status.trim()).filter(|value| !value.is_empty());
    let subtitle = match (issue_id, status) {
        (Some(issue_id), Some(status)) => Some(format!("{issue_id} · {status}")),
        (Some(only), None) | (None, Some(only)) => Some(only.to_string()),
        (None, None) => None,
    };
    if let Some(subtitle) = subtitle {
        brief.push_str(&format!("_{subtitle}_\n"));
    }

    let url = ticket.url.trim();
    if !url.is_empty() {
        brief.push_str(url);
        brief.push('\n');
    }

    let mission = body
        .map(str::trim)
        .filter(|body| !body.is_empty())
        .map(demote_headings);
    brief.push_str("\n## Mission\n");
    brief.push_str(mission.as_deref().unwrap_or(MISSION_PLACEHOLDER));
    brief.push_str("\n\n");
    brief.push_str(WORKING_BEHAVIOR);
    brief
}

/// Notion bodies frequently carry their own top-level `#` headings. Left
/// alone they would close the brief's `## Mission` section and make the
/// ticket body look like a sibling of the instructions Claude must follow, so
/// each heading is pushed two levels down to nest under Mission instead.
/// Fenced code blocks are skipped: a `#` there is a comment, not a heading.
fn demote_headings(body: &str) -> String {
    let mut in_fence = false;
    body.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                return line.to_string();
            }
            if in_fence || !trimmed.starts_with('#') {
                return line.to_string();
            }
            let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
            // `####### ` is not a heading in any markdown flavor, so demoting
            // past six levels would corrupt the line rather than nest it.
            if hashes > 4 || !trimmed[hashes..].starts_with(' ') {
                return line.to_string();
            }
            format!("##{trimmed}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> TicketRef {
        TicketRef {
            page_id: "page".into(),
            title: "Fix the spider chart".into(),
            url: "https://www.notion.so/Fix-the-spider-chart-0123".into(),
            status: "In progress".into(),
            slug: "fix-the-spider-chart".into(),
            last_edited_time: String::new(),
            ticket_type: None,
            issue_id: Some("CT-1487".into()),
        }
    }

    #[test]
    fn test_render_brief_with_a_body() {
        let brief = render_brief(&ticket(), Some("The chart renders upside down."));
        assert_eq!(
            brief,
            concat!(
                "# Ticket: Fix the spider chart\n",
                "_CT-1487 · In progress_\n",
                "https://www.notion.so/Fix-the-spider-chart-0123\n",
                "\n",
                "## Mission\n",
                "The chart renders upside down.\n",
                "\n",
                "## Working behavior\n",
                "- conventional commits\n",
                "- ask before destructive git operations\n",
                "- no push / no MR\n",
            )
        );
    }

    #[test]
    fn test_render_brief_without_a_body_uses_a_placeholder() {
        let brief = render_brief(&ticket(), None);
        assert!(brief.contains(MISSION_PLACEHOLDER));
        assert!(brief.ends_with("- no push / no MR\n"));

        let blank = render_brief(&ticket(), Some("   \n  "));
        assert!(blank.contains(MISSION_PLACEHOLDER));
    }

    #[test]
    fn test_render_brief_without_an_issue_id_falls_back_to_the_status() {
        let mut ticket = ticket();
        ticket.issue_id = None;
        assert!(render_brief(&ticket, Some("body")).contains("\n_In progress_\n"));

        ticket.status = String::new();
        let brief = render_brief(&ticket, Some("body"));
        assert!(!brief.contains('_'), "{brief}");
    }

    #[test]
    fn test_render_brief_demotes_headings_in_the_body() {
        let body = "# Context\nsome text\n## Details\n```\n# not a heading\n```\n";
        let brief = render_brief(&ticket(), Some(body));
        assert!(brief.contains("\n### Context\n"), "{brief}");
        assert!(brief.contains("\n#### Details\n"), "{brief}");
        assert!(brief.contains("\n# not a heading\n"), "{brief}");
    }
}
