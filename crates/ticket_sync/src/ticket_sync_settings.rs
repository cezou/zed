use settings::RegisterSetting;
pub use settings::Settings;
pub use settings::TicketRepositoryContent;

/// Runtime view of the `tickets_panel` settings key.
///
/// The key keeps its old name even though the crate is now `ticket_sync` and
/// the dock panel is gone: renaming it would silently unconfigure every
/// existing `settings.json` (Notion board ids, repositories, `repo_path`).
///
/// Every field of the underlying content struct is optional, so a user
/// settings file that predates a key — or a `default.json` that has drifted —
/// must degrade to a default rather than panic during startup.
#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct TicketSyncSettings {
    pub notion_database_id: Option<String>,
    pub notion_assignee_user_id: Option<String>,
    pub notion_status_filter: Vec<String>,
    pub notion_status_property: Option<String>,
    pub notion_assignee_property: Option<String>,
    pub refresh_interval_secs: u64,
    pub repo_path: Option<String>,
    pub repositories: Vec<TicketRepositoryContent>,
    pub notion_data_source_url: Option<String>,
    pub notion_title_property: Option<String>,
    pub notion_person_property: Option<String>,
    pub notion_issue_id_property: Option<String>,
    pub notion_board_view_name: String,
}

const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300;
const DEFAULT_BOARD_VIEW_NAME: &str = "Team Board";

impl Settings for TicketSyncSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let ticket_settings = content.tickets_panel.clone().unwrap_or_default();
        Self {
            notion_database_id: ticket_settings.notion_database_id,
            notion_assignee_user_id: ticket_settings.notion_assignee_user_id,
            notion_status_filter: ticket_settings.notion_status_filter.unwrap_or_default(),
            notion_status_property: ticket_settings.notion_status_property,
            notion_assignee_property: ticket_settings.notion_assignee_property,
            refresh_interval_secs: ticket_settings
                .refresh_interval_secs
                .unwrap_or(DEFAULT_REFRESH_INTERVAL_SECS),
            repo_path: ticket_settings.repo_path,
            repositories: ticket_settings.repositories.unwrap_or_default(),
            notion_data_source_url: ticket_settings.notion_data_source_url,
            notion_title_property: ticket_settings.notion_title_property,
            notion_person_property: ticket_settings.notion_person_property,
            notion_issue_id_property: ticket_settings.notion_issue_id_property,
            notion_board_view_name: ticket_settings
                .notion_board_view_name
                .unwrap_or_else(|| DEFAULT_BOARD_VIEW_NAME.to_string()),
        }
    }
}
