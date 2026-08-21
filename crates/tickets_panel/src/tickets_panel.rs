pub mod text_input_modal;
pub mod tickets_panel_settings;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_ui::ticket_metadata_store::{
    self, TicketId, TicketMetadataStore, TicketWorktreeRecord,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement, Render, SharedString, Styled, Task, WeakEntity, Window, actions,
    div, uniform_list,
};
use http_client::HttpClient;
use notion_client::mcp::McpClient;
use notion_client::mcp_board::McpBoardConfig;
use notion_client::oauth::OAuthTokens;
use notion_client::{DatabaseSchema, NotionClient, TicketRef, oauth, oauth_store, token_store};
use settings::Settings as _;
use ui::{
    Button, ButtonCommon, Chip, Clickable, Color, IconButton, IconName, InteractiveElement, Label,
    LabelCommon, ListItem, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{AppState, Workspace, dock::DockPosition, dock::Panel};

pub use tickets_panel_settings::TicketsPanelSettings;

use crate::text_input_modal::TextInputModal;

actions!(
    notion,
    [
        /// Prompts for and stores a Notion Personal Access Token in the
        /// system keychain.
        SetPersonalAccessToken,
        /// Resolves the configured Notion page into a queryable database id,
        /// assignee user id, and status/assignee property names, writing the
        /// results back into settings.
        ResolveDatabase,
        /// Connects to Notion via OAuth against its public MCP server, for
        /// workspaces whose admin policy blocks Personal Access Token and
        /// Connection creation. Discovers the configured board's schema over
        /// MCP once connected.
        ConnectToNotion,
    ]
);

actions!(
    tickets_panel,
    [
        /// Toggles the tickets panel.
        Toggle,
        /// Toggles focus on the tickets panel.
        ToggleFocus,
    ]
);

const PANEL_KEY: &str = "TicketsPanel";

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<TicketsPanel>(window, cx) {
                workspace.close_panel::<TicketsPanel>(window, cx);
            }
        });
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TicketsPanel>(window, cx);
        });
        workspace.register_action(|_workspace, _: &SetPersonalAccessToken, window, cx| {
            open_set_token_modal(window, cx);
        });
        workspace.register_action(|_workspace, _: &ResolveDatabase, window, cx| {
            open_resolve_database_modal(window, cx);
        });
        workspace.register_action(|_workspace, _: &ConnectToNotion, window, cx| {
            open_connect_to_notion_modal(window, cx);
        });
    })
    .detach();
}

fn open_set_token_modal(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.defer_in(window, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            TextInputModal::new(
                "Set Notion Personal Access Token",
                "secret_...",
                None,
                true,
                |token, _window, cx| {
                    token_store::store_token(token, cx).detach_and_log_err(cx);
                },
                window,
                cx,
            )
        });
    });
}

fn open_resolve_database_modal(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.defer_in(window, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            TextInputModal::new(
                "Resolve Notion Database",
                "Notion page URL or id",
                None,
                false,
                |page_input, _window, cx| {
                    resolve_database(page_input, cx);
                },
                window,
                cx,
            )
        });
    });
}

fn resolve_database(page_input: String, cx: &mut App) {
    let http_client = cx.http_client();
    let fs = AppState::try_global(cx).map(|state| state.fs.clone());
    cx.spawn(async move |cx| {
        let token_task = cx.update(|cx| token_store::load_token(cx));
        let Some(token) = token_task.await else {
            log::error!(
                "resolve_database: no Notion token configured — run `notion: Set Personal Access Token` first"
            );
            return;
        };
        let client = NotionClient::new(http_client, token);
        let page_id = extract_page_id(&page_input);

        let database_id = match client.resolve_database_id(&page_id).await {
            Ok(id) => id,
            Err(error) => {
                log::error!("resolve_database: failed to resolve database id: {error}");
                return;
            }
        };
        let schema = match client.fetch_database_schema(&database_id).await {
            Ok(schema) => schema,
            Err(error) => {
                log::error!("resolve_database: failed to fetch database schema: {error}");
                return;
            }
        };
        let assignee_user_id = match client.resolve_assignee_user_id("").await {
            Ok(id) => id,
            Err(error) => {
                log::error!(
                    "resolve_database: failed to resolve assignee user id: {error} — \
                    set `tickets_panel.notion_assignee_user_id` manually in settings"
                );
                String::new()
            }
        };

        let Some(fs) = fs else {
            log::error!("resolve_database: no AppState available to write settings back");
            return;
        };
        cx.update(|cx| {
            settings::update_settings_file(fs, cx, move |settings, _cx| {
                let panel = settings.tickets_panel.get_or_insert_default();
                panel.notion_database_id = Some(database_id);
                if !assignee_user_id.is_empty() {
                    panel.notion_assignee_user_id = Some(assignee_user_id);
                }
                panel.notion_status_property = Some(schema.status_property);
                panel.notion_assignee_property = Some(schema.assignee_property);
                panel.notion_status_filter = Some(schema.status_options);
            });
        });
    })
    .detach();
}

fn open_connect_to_notion_modal(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.defer_in(window, |workspace, window, cx| {
        workspace.toggle_modal(window, cx, |window, cx| {
            TextInputModal::new(
                "Connect to Notion",
                "Notion page or database URL/id",
                None,
                false,
                |page_input, _window, cx| {
                    connect_to_notion(page_input, cx);
                },
                window,
                cx,
            )
        });
    });
}

/// Runs the OAuth flow against Notion's public MCP server, stores the
/// resulting tokens, then discovers the configured board's schema (data
/// source, status/person/title properties, and the status values the named
/// view filters to) and the caller's own Notion user id, writing everything
/// back into settings. Used instead of [`resolve_database`] when the
/// workspace doesn't permit Personal Access Token/Connection creation.
fn connect_to_notion(page_input: String, cx: &mut App) {
    let http_client = cx.http_client();
    let fs = AppState::try_global(cx).map(|state| state.fs.clone());
    let view_name = TicketsPanelSettings::get_global(cx)
        .notion_board_view_name
        .clone();
    cx.spawn(async move |cx| {
        let oauth_task = cx.update(|cx| oauth::run_oauth_flow(http_client.clone(), cx));
        let tokens = match oauth_task.await {
            Ok(tokens) => tokens,
            Err(error) => {
                log::error!("connect_to_notion: OAuth flow failed: {error}");
                return;
            }
        };
        let store_task = cx.update(|cx| oauth_store::store_tokens(&tokens, cx));
        if let Err(error) = store_task.await {
            log::error!("connect_to_notion: failed to store OAuth tokens: {error}");
        }

        let mut client = McpClient::new(http_client, tokens);
        if let Err(error) = client.initialize().await {
            log::error!("connect_to_notion: MCP initialize failed: {error}");
            return;
        }

        let page_id = extract_page_id(&page_input);
        let board = match notion_client::mcp_board::discover_board(&mut client, &page_id, &view_name).await
        {
            Ok(board) => board,
            Err(error) => {
                log::error!("connect_to_notion: failed to discover board: {error}");
                return;
            }
        };
        let self_user_id = match notion_client::mcp_board::resolve_self_user_id(&mut client).await {
            Ok(id) => id,
            Err(error) => {
                log::error!("connect_to_notion: failed to resolve self user id: {error}");
                return;
            }
        };

        let Some(fs) = fs else {
            log::error!("connect_to_notion: no AppState available to write settings back");
            return;
        };
        cx.update(|cx| {
            settings::update_settings_file(fs, cx, move |settings, _cx| {
                let panel = settings.tickets_panel.get_or_insert_default();
                panel.notion_data_source_url = Some(board.data_source_url);
                panel.notion_title_property = Some(board.title_property);
                panel.notion_person_property = Some(board.person_property);
                panel.notion_status_property = Some(board.status_property);
                panel.notion_status_filter = Some(board.status_values);
                panel.notion_assignee_user_id = Some(self_user_id);
            });
        });
    })
    .detach();
}

/// Notion page URLs embed the 32-hex-character page id at the very end of
/// the last path segment's slug (with the title's own words/dashes stripped
/// out ahead of it, e.g. `.../My-Page-Title-0123456789abcdef0123456789abcdef`) —
/// so the id is recovered by taking the last 32 alphanumeric characters
/// rather than trying to parse the slug structurally.
fn extract_page_id(input: &str) -> String {
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

#[derive(Debug, Clone)]
enum WorktreeState {
    None,
    Creating,
    Ready { path: PathBuf, active_sessions: usize },
    Error(SharedString),
}

#[derive(Debug, Clone)]
enum LoadStatus {
    NoToken,
    NoDatabase,
    Loading,
    Loaded,
    Error(SharedString),
}

pub struct TicketsPanel {
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    tickets: Vec<TicketRef>,
    load_status: LoadStatus,
    pending_creation: HashSet<TicketId>,
    creation_errors: HashMap<TicketId, SharedString>,
    _refresh_task: Task<()>,
}

impl TicketsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        let mut cx = cx;
        workspace.update_in(&mut cx, |_workspace, window, cx| {
            cx.new(|cx| Self::new(workspace.clone(), window, cx))
        })
    }

    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            tickets: Vec::new(),
            load_status: LoadStatus::NoToken,
            pending_creation: HashSet::default(),
            creation_errors: HashMap::default(),
            _refresh_task: Task::ready(()),
        };
        this.start_refresh(window, cx);
        this
    }

    fn start_refresh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let settings = TicketsPanelSettings::get_global(cx).clone();
        let interval = Duration::from_secs(settings.refresh_interval_secs.max(30));
        let http_client = cx.http_client();

        self.load_status = LoadStatus::Loading;
        let panel = cx.entity().downgrade();
        self._refresh_task = cx.spawn(async move |_this, cx| {
            // OAuth is preferred when both credentials happen to be
            // configured — it's the path that's been empirically verified
            // against a workspace where Personal Access Tokens are blocked
            // by admin policy, so it's the safer default to trust.
            let oauth_tokens = cx.update(|cx| oauth_store::load_tokens(cx)).await;
            if let Some(tokens) = oauth_tokens {
                refresh_loop_mcp(panel, http_client, tokens, settings, interval, cx).await;
            } else {
                refresh_loop_rest(panel, http_client, settings, interval, cx).await;
            }
        });
    }

    fn worktree_state(&self, ticket_id: &TicketId, cx: &App) -> WorktreeState {
        if self.pending_creation.contains(ticket_id) {
            return WorktreeState::Creating;
        }
        if let Some(message) = self.creation_errors.get(ticket_id) {
            return WorktreeState::Error(message.clone());
        }
        let Some(store) = TicketMetadataStore::try_global(cx) else {
            return WorktreeState::None;
        };
        let store = store.read(cx);
        match store.entry(ticket_id) {
            Some(TicketWorktreeRecord {
                worktree_path: Some(path),
                ..
            }) => WorktreeState::Ready {
                path: path.clone(),
                active_sessions: store
                    .entry(ticket_id)
                    .map(|entry| entry.active_session_count())
                    .unwrap_or(0),
            },
            _ => WorktreeState::None,
        }
    }

    fn open_create_worktree_modal(
        &mut self,
        ticket: TicketRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let panel = cx.entity().downgrade();
        let default_name = ticket.slug.clone();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                TextInputModal::new(
                    "Create worktree",
                    "branch/directory name",
                    Some(default_name.clone().into()),
                    false,
                    move |branch_name, window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.create_worktree_for(ticket.clone(), branch_name, window, cx);
                            })
                            .ok();
                    },
                    window,
                    cx,
                )
            });
        });
    }

    fn create_worktree_for(
        &mut self,
        ticket: TicketRef,
        branch_name: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ticket_id = TicketId::new(ticket.page_id.clone());
        let Some(repo_path) = TicketsPanelSettings::get_global(cx).repo_path.clone() else {
            self.creation_errors.insert(
                ticket_id,
                "Set `tickets_panel.repo_path` in settings first".into(),
            );
            cx.notify();
            return;
        };
        let Some(app_state) = AppState::try_global(cx) else {
            return;
        };

        self.pending_creation.insert(ticket_id.clone());
        self.creation_errors.remove(&ticket_id);
        cx.notify();

        let seeded_message = format!("Ticket: {}\n{}", ticket.title, ticket.url);
        let repo_path = PathBuf::from(repo_path);
        let panel = cx.entity().downgrade();
        cx.spawn(async move |_this, cx| {
            let result = ticket_metadata_store::create_worktree_and_launch(
                ticket_id.clone(),
                repo_path,
                branch_name,
                seeded_message,
                app_state,
                cx,
            )
            .await;
            panel
                .update(cx, |panel, cx| {
                    panel.pending_creation.remove(&ticket_id);
                    if let Err(error) = result {
                        panel
                            .creation_errors
                            .insert(ticket_id, error.to_string().into());
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn open_ticket(&mut self, ticket: TicketRef, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(app_state) = AppState::try_global(cx) else {
            return;
        };
        let ticket_id = TicketId::new(ticket.page_id.clone());
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            ticket_metadata_store::open_ticket(ticket_id, app_state, cx)
                .await
                .log_err();
        })
        .detach();
    }

    fn launch_additional_session(
        &mut self,
        ticket: TicketRef,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(app_state) = AppState::try_global(cx) else {
            return;
        };
        let ticket_id = TicketId::new(ticket.page_id.clone());
        let seeded_message = format!("Ticket: {}\n{}", ticket.title, ticket.url);
        cx.spawn(async move |_this, cx: &mut AsyncApp| {
            ticket_metadata_store::launch_additional_session(
                ticket_id,
                seeded_message,
                app_state,
                cx,
            )
            .await
            .log_err();
        })
        .detach();
    }

    fn status_color(status: &str) -> Color {
        let normalized = status.to_lowercase();
        if normalized.contains("prod") {
            Color::Error
        } else if normalized.contains("waiting") {
            Color::Warning
        } else if normalized.contains("review") {
            Color::Accent
        } else if normalized.contains("progress") {
            Color::Info
        } else {
            Color::Muted
        }
    }

    fn render_empty_state(&self, _cx: &Context<Self>) -> impl IntoElement {
        let message = match &self.load_status {
            LoadStatus::NoToken => {
                "No Notion token configured — run `notion: Set Personal Access Token`.".to_string()
            }
            LoadStatus::NoDatabase => {
                "No Notion database configured — run `notion: Resolve Database`.".to_string()
            }
            LoadStatus::Loading => "Loading tickets…".to_string(),
            LoadStatus::Loaded => "No tickets match your filters.".to_string(),
            LoadStatus::Error(error) => format!("Notion error: {error}"),
        };
        div().p_4().child(Label::new(message).color(Color::Muted))
    }

    fn render_row(
        &self,
        ticket: TicketRef,
        panel: WeakEntity<Self>,
        cx: &App,
    ) -> ListItem {
        let ticket_id = TicketId::new(ticket.page_id.clone());
        let state = self.worktree_state(&ticket_id, cx);
        let status_chip = Chip::new(ticket.status.clone())
            .label_color(Self::status_color(&ticket.status));
        let type_chip = ticket
            .ticket_type
            .clone()
            .map(|ticket_type| Chip::new(ticket_type).label_color(Color::Muted));

        let end_slot: gpui::AnyElement = match state {
            WorktreeState::None => {
                let ticket_for_click = ticket.clone();
                let panel = panel.clone();
                Button::new(format!("create-worktree-{}", ticket.page_id), "Create worktree")
                    .on_click(move |_, window, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.open_create_worktree_modal(
                                    ticket_for_click.clone(),
                                    window,
                                    cx,
                                );
                            })
                            .ok();
                    })
                    .into_any_element()
            }
            WorktreeState::Creating => {
                Button::new(format!("creating-{}", ticket.page_id), "Creating…")
                    .disabled(true)
                    .into_any_element()
            }
            WorktreeState::Ready { path, active_sessions } => {
                let label = if active_sessions > 0 {
                    format!("Open ({active_sessions})")
                } else {
                    "Open".to_string()
                };
                let path_tooltip = SharedString::from(path.display().to_string());
                let ticket_for_open = ticket.clone();
                let panel_for_open = panel.clone();
                let ticket_for_extra = ticket.clone();
                let panel_for_extra = panel.clone();
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new(format!("open-{}", ticket.page_id), label)
                            .tooltip(Tooltip::text(path_tooltip))
                            .on_click(move |_, window, cx| {
                                panel_for_open
                                    .update(cx, |panel, cx| {
                                        panel.open_ticket(ticket_for_open.clone(), window, cx);
                                    })
                                    .ok();
                            }),
                    )
                    .child(
                        IconButton::new(format!("extra-session-{}", ticket.page_id), IconName::Plus)
                            .tooltip(Tooltip::text("Launch an additional session"))
                            .on_click(move |_, window, cx| {
                                panel_for_extra
                                    .update(cx, |panel, cx| {
                                        panel.launch_additional_session(
                                            ticket_for_extra.clone(),
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            }),
                    )
                    .into_any_element()
            }
            WorktreeState::Error(message) => {
                IconButton::new(format!("worktree-error-{}", ticket.page_id), IconName::Warning)
                    .icon_color(Color::Error)
                    .tooltip(Tooltip::text(message))
                    .into_any_element()
            }
        };

        ListItem::new(format!("ticket-{}", ticket.page_id))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(Label::new(ticket.title.clone()))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(status_chip)
                            .children(type_chip),
                    ),
            )
            .end_slot(end_slot)
    }
}

async fn refresh_loop_rest(
    panel: WeakEntity<TicketsPanel>,
    http_client: Arc<dyn HttpClient>,
    settings: TicketsPanelSettings,
    interval: Duration,
    cx: &mut AsyncApp,
) {
    let Some(database_id) = settings.notion_database_id.clone() else {
        panel
            .update(cx, |panel, cx| {
                panel.load_status = LoadStatus::NoDatabase;
                cx.notify();
            })
            .ok();
        return;
    };
    let (Some(assignee_user_id), Some(status_property), Some(assignee_property)) = (
        settings.notion_assignee_user_id.clone(),
        settings.notion_status_property.clone(),
        settings.notion_assignee_property.clone(),
    ) else {
        panel
            .update(cx, |panel, cx| {
                panel.load_status = LoadStatus::NoDatabase;
                cx.notify();
            })
            .ok();
        return;
    };
    let status_options = settings.notion_status_filter.clone();
    let schema = DatabaseSchema {
        status_property,
        status_property_kind: notion_client::StatusPropertyKind::Status,
        status_options: status_options.clone(),
        assignee_property,
    };

    let token_task = cx.update(|cx| token_store::load_token(cx));
    let Some(token) = token_task.await else {
        panel
            .update(cx, |panel, cx| {
                panel.load_status = LoadStatus::NoToken;
                cx.notify();
            })
            .ok();
        return;
    };

    let client = NotionClient::new(http_client, token);
    loop {
        let result = client
            .query_tickets(&database_id, &schema, &assignee_user_id, &status_options)
            .await;
        let updated = panel
            .update(cx, |panel, cx| {
                match result {
                    Ok(tickets) => {
                        upsert_tickets_into_store(&tickets, cx);
                        panel.tickets = tickets;
                        panel.load_status = LoadStatus::Loaded;
                    }
                    Err(error) => {
                        panel.load_status = LoadStatus::Error(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .is_ok();
        if !updated {
            break;
        }
        cx.background_executor().timer(interval).await;
    }
}

async fn refresh_loop_mcp(
    panel: WeakEntity<TicketsPanel>,
    http_client: Arc<dyn HttpClient>,
    mut tokens: OAuthTokens,
    settings: TicketsPanelSettings,
    interval: Duration,
    cx: &mut AsyncApp,
) {
    let (Some(data_source_url), Some(title_property), Some(person_property), Some(status_property)) = (
        settings.notion_data_source_url.clone(),
        settings.notion_title_property.clone(),
        settings.notion_person_property.clone(),
        settings.notion_status_property.clone(),
    ) else {
        panel
            .update(cx, |panel, cx| {
                panel.load_status = LoadStatus::NoDatabase;
                cx.notify();
            })
            .ok();
        return;
    };
    let Some(person_id) = settings.notion_assignee_user_id.clone() else {
        panel
            .update(cx, |panel, cx| {
                panel.load_status = LoadStatus::NoDatabase;
                cx.notify();
            })
            .ok();
        return;
    };
    let config = McpBoardConfig {
        data_source_url,
        title_property,
        status_property,
        status_values: settings.notion_status_filter.clone(),
        person_property,
    };

    loop {
        let mut client = McpClient::new(http_client.clone(), tokens.clone());
        let result = async {
            client.initialize().await?;
            notion_client::mcp_board::query_tickets(&mut client, &config, &person_id).await
        }
        .await;
        if let Some(refreshed) = client.refreshed_tokens() {
            tokens = refreshed.clone();
            let store_task = cx.update(|cx| oauth_store::store_tokens(&tokens, cx));
            store_task.await.log_err();
        }

        let updated = panel
            .update(cx, |panel, cx| {
                match result {
                    Ok(tickets) => {
                        upsert_tickets_into_store(&tickets, cx);
                        panel.tickets = tickets;
                        panel.load_status = LoadStatus::Loaded;
                    }
                    Err(error) => {
                        panel.load_status = LoadStatus::Error(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .is_ok();
        if !updated {
            break;
        }
        cx.background_executor().timer(interval).await;
    }
}

fn upsert_tickets_into_store(tickets: &[TicketRef], cx: &mut App) {
    let Some(store) = TicketMetadataStore::try_global(cx) else {
        return;
    };
    for ticket in tickets {
        store.update(cx, |store, cx| {
            store.upsert_ticket_ref(
                TicketId::new(ticket.page_id.clone()),
                ticket.title.clone().into(),
                ticket.url.clone().into(),
                cx,
            );
        });
    }
}

impl Focusable for TicketsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<workspace::dock::PanelEvent> for TicketsPanel {}

impl Render for TicketsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.tickets.is_empty() {
            return div()
                .size_full()
                .track_focus(&self.focus_handle)
                .child(self.render_empty_state(cx))
                .into_any_element();
        }

        let tickets = self.tickets.clone();
        let panel = cx.entity().downgrade();
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(
                uniform_list(
                    "tickets-panel-list",
                    tickets.len(),
                    move |range, _window, cx| {
                        range
                            .filter_map(|index| tickets.get(index).cloned())
                            .filter_map(|ticket| {
                                panel
                                    .upgrade()
                                    .map(|panel_entity| {
                                        panel_entity.read(cx).render_row(
                                            ticket,
                                            panel.clone(),
                                            cx,
                                        )
                                    })
                            })
                            .collect()
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

impl Panel for TicketsPanel {
    fn persistent_name() -> &'static str {
        "Tickets Panel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, cx: &App) -> DockPosition {
        match TicketsPanelSettings::get_global(cx).dock {
            settings::DockSide::Left => DockPosition::Left,
            settings::DockSide::Right => DockPosition::Right,
        }
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        let Some(app_state) = AppState::try_global(cx) else {
            return;
        };
        settings::update_settings_file(app_state.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left | DockPosition::Bottom => settings::DockSide::Left,
                DockPosition::Right => settings::DockSide::Right,
            };
            settings.tickets_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, _: &Window, cx: &App) -> gpui::Pixels {
        TicketsPanelSettings::get_global(cx).default_width
    }

    fn icon(&self, _: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _: &App) -> Option<&'static str> {
        Some("Notion Tickets")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        50
    }
}
