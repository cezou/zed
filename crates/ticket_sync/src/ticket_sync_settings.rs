use gpui::Pixels;
use settings::RegisterSetting;
pub use settings::{DockSide, Settings};

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct TicketsPanelSettings {
    pub dock: DockSide,
    pub default_width: Pixels,
    pub notion_database_id: Option<String>,
    pub notion_assignee_user_id: Option<String>,
    pub notion_status_filter: Vec<String>,
    pub notion_status_property: Option<String>,
    pub notion_assignee_property: Option<String>,
    pub refresh_interval_secs: u64,
    pub repo_path: Option<String>,
    pub notion_data_source_url: Option<String>,
    pub notion_title_property: Option<String>,
    pub notion_person_property: Option<String>,
    pub notion_board_view_name: String,
}

impl Settings for TicketsPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let panel = content.tickets_panel.as_ref().unwrap();
        Self {
            dock: panel.dock.unwrap(),
            default_width: panel.default_width.map(gpui::px).unwrap(),
            notion_database_id: panel.notion_database_id.clone(),
            notion_assignee_user_id: panel.notion_assignee_user_id.clone(),
            notion_status_filter: panel.notion_status_filter.clone().unwrap_or_default(),
            notion_status_property: panel.notion_status_property.clone(),
            notion_assignee_property: panel.notion_assignee_property.clone(),
            refresh_interval_secs: panel.refresh_interval_secs.unwrap(),
            repo_path: panel.repo_path.clone(),
            notion_data_source_url: panel.notion_data_source_url.clone(),
            notion_title_property: panel.notion_title_property.clone(),
            notion_person_property: panel.notion_person_property.clone(),
            notion_board_view_name: panel
                .notion_board_view_name
                .clone()
                .unwrap_or_else(|| "Team Board".to_string()),
        }
    }
}
