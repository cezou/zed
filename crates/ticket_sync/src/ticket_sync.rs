pub mod clipboard_images;
pub mod repository_registry;
pub mod text_input_modal;
pub mod ticket_brief;
pub mod ticket_launch_modal;
pub mod ticket_sync_settings;

use std::sync::Arc;
use std::time::Duration;

use agent_ui::ticket_metadata_store::{
    TicketDisplayFields, TicketId, TicketMetadataStore, TicketWorktreeRecord,
};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Task, TaskExt as _,
    WeakEntity, Window, actions,
};
use http_client::HttpClient;
use notion_client::mcp::McpClient;
use notion_client::mcp_board::McpBoardConfig;
use notion_client::oauth::OAuthTokens;
use notion_client::{DatabaseSchema, NotionClient, TicketRef, oauth, oauth_store, token_store};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::{AppState, Workspace};

pub use ticket_sync_settings::TicketSyncSettings;

use crate::text_input_modal::TextInputModal;
use crate::ticket_launch_modal::{LaunchMode, TicketLaunchModal};

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

pub fn init(cx: &mut App) {
    TicketSyncService::init_global(cx);
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_workspace, _: &SetPersonalAccessToken, window, cx| {
            open_set_token_modal(window, cx);
        });
        workspace.register_action(|_workspace, _: &ResolveDatabase, window, cx| {
            open_resolve_database_modal(window, cx);
        });
        workspace.register_action(|_workspace, _: &ConnectToNotion, window, cx| {
            open_connect_to_notion_modal(window, cx);
        });
        workspace.register_action(
            |workspace, action: &agent_ui::StartTicketWork, window, cx| {
                open_ticket_launch_modal(workspace, action, window, cx);
            },
        );
    })
    .detach();
}

/// Handles the sidebar's request to start work on a ticket. The sidebar only
/// knows the ticket id, so the `TicketRef` the modal wants is rebuilt from the
/// metadata store, which the Notion sync keeps current.
fn open_ticket_launch_modal(
    workspace: &mut Workspace,
    action: &agent_ui::StartTicketWork,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let ticket_id = TicketId::new(action.ticket_id.clone());
    let Some(store) = TicketMetadataStore::try_global(cx) else {
        return;
    };
    let Some(ticket) = store.read(cx).entry(&ticket_id).map(ticket_ref_from_record) else {
        log::error!("cannot launch unknown ticket {ticket_id:?}");
        return;
    };
    let mode = if action.additional_session {
        LaunchMode::AdditionalSession
    } else {
        LaunchMode::CreateWorktree
    };
    let fs = workspace.app_state().fs.clone();
    cx.defer_in(window, move |workspace, window, cx| {
        TicketLaunchModal::show(workspace, ticket, mode, fs, window, cx);
    });
}

/// Recovers the Notion-sourced view of a ticket from its stored record. The
/// store is the only ticket source that survives a restart, so the modal must
/// not depend on a live board sync having run in this session.
fn ticket_ref_from_record(record: &TicketWorktreeRecord) -> TicketRef {
    let title = record.title.to_string();
    TicketRef {
        page_id: record.ticket_id.0.to_string(),
        slug: record
            .branch_name
            .clone()
            .unwrap_or_else(|| notion_client::slugify(&title)),
        title,
        url: record.url.to_string(),
        status: record.status.as_deref().unwrap_or_default().to_string(),
        last_edited_time: String::new(),
        ticket_type: record.ticket_type.as_deref().map(str::to_string),
        issue_id: record.issue_id.as_deref().map(str::to_string),
    }
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
                let ticket_settings = settings.tickets_panel.get_or_insert_default();
                ticket_settings.notion_database_id = Some(database_id);
                if !assignee_user_id.is_empty() {
                    ticket_settings.notion_assignee_user_id = Some(assignee_user_id);
                }
                ticket_settings.notion_status_property = Some(schema.status_property);
                ticket_settings.notion_assignee_property = Some(schema.assignee_property);
                ticket_settings.notion_status_filter = Some(schema.status_options);
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
    let view_name = TicketSyncSettings::get_global(cx)
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
        let board =
            match notion_client::mcp_board::discover_board(&mut client, &page_id, &view_name).await
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
                let ticket_settings = settings.tickets_panel.get_or_insert_default();
                ticket_settings.notion_data_source_url = Some(board.data_source_url);
                ticket_settings.notion_title_property = Some(board.title_property);
                ticket_settings.notion_person_property = Some(board.person_property);
                ticket_settings.notion_issue_id_property = board.issue_id_property;
                ticket_settings.notion_status_property = Some(board.status_property);
                ticket_settings.notion_status_filter = Some(board.status_values);
                ticket_settings.notion_assignee_user_id = Some(self_user_id);
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

/// Sync status of the Notion poll. Public so a future surface can report
/// credential/board misconfiguration to the user; the sidebar currently just
/// renders whatever the metadata store holds.
#[derive(Debug, Clone)]
pub enum LoadStatus {
    NoToken,
    NoDatabase,
    Loading,
    Loaded,
    Error(SharedString),
}

/// Polls the configured Notion board and folds the results into
/// [`TicketMetadataStore`], which is what the sidebar renders from. This is a
/// process-wide global rather than per-window state so that opening a second
/// window does not start a second poll against Notion.
pub struct TicketSyncService {
    status: LoadStatus,
    _refresh_task: Task<()>,
}

struct GlobalTicketSyncService(Entity<TicketSyncService>);

impl Global for GlobalTicketSyncService {}

impl TicketSyncService {
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTicketSyncService>() {
            return;
        }
        let service = cx.new(Self::new);
        cx.set_global(GlobalTicketSyncService(service));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTicketSyncService>()
            .map(|service| service.0.clone())
    }

    pub fn status(&self) -> &LoadStatus {
        &self.status
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            status: LoadStatus::NoToken,
            _refresh_task: Task::ready(()),
        };
        this.start_refresh(cx);
        this
    }

    fn start_refresh(&mut self, cx: &mut Context<Self>) {
        let settings = TicketSyncSettings::get_global(cx).clone();
        let interval = Duration::from_secs(settings.refresh_interval_secs.max(30));
        let http_client = cx.http_client();

        self.status = LoadStatus::Loading;
        let service = cx.entity().downgrade();
        self._refresh_task = cx.spawn(async move |_this, cx| {
            // OAuth is preferred when both credentials happen to be
            // configured — it's the path that's been empirically verified
            // against a workspace where Personal Access Tokens are blocked
            // by admin policy, so it's the safer default to trust.
            let oauth_tokens = cx.update(|cx| oauth_store::load_tokens(cx)).await;
            if let Some(tokens) = oauth_tokens {
                refresh_loop_mcp(service, http_client, tokens, settings, interval, cx).await;
            } else {
                refresh_loop_rest(service, http_client, settings, interval, cx).await;
            }
        });
    }
}

async fn refresh_loop_rest(
    service: WeakEntity<TicketSyncService>,
    http_client: Arc<dyn HttpClient>,
    settings: TicketSyncSettings,
    interval: Duration,
    cx: &mut AsyncApp,
) {
    let Some(database_id) = settings.notion_database_id.clone() else {
        service
            .update(cx, |service, cx| {
                service.status = LoadStatus::NoDatabase;
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
        service
            .update(cx, |service, cx| {
                service.status = LoadStatus::NoDatabase;
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
        service
            .update(cx, |service, cx| {
                service.status = LoadStatus::NoToken;
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
        let updated = service
            .update(cx, |service, cx| {
                match result {
                    Ok(tickets) => {
                        upsert_tickets_into_store(&tickets, cx);
                        service.status = LoadStatus::Loaded;
                    }
                    Err(error) => {
                        service.status = LoadStatus::Error(error.to_string().into());
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
    service: WeakEntity<TicketSyncService>,
    http_client: Arc<dyn HttpClient>,
    mut tokens: OAuthTokens,
    settings: TicketSyncSettings,
    interval: Duration,
    cx: &mut AsyncApp,
) {
    let (Some(data_source_url), Some(title_property), Some(person_property), Some(status_property)) = (
        settings.notion_data_source_url.clone(),
        settings.notion_title_property.clone(),
        settings.notion_person_property.clone(),
        settings.notion_status_property.clone(),
    ) else {
        service
            .update(cx, |service, cx| {
                service.status = LoadStatus::NoDatabase;
                cx.notify();
            })
            .ok();
        return;
    };
    let Some(person_id) = settings.notion_assignee_user_id.clone() else {
        service
            .update(cx, |service, cx| {
                service.status = LoadStatus::NoDatabase;
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
        issue_id_property: settings.notion_issue_id_property.clone(),
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

        let updated = service
            .update(cx, |service, cx| {
                match result {
                    Ok(tickets) => {
                        upsert_tickets_into_store(&tickets, cx);
                        service.status = LoadStatus::Loaded;
                    }
                    Err(error) => {
                        service.status = LoadStatus::Error(error.to_string().into());
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
        let fields = TicketDisplayFields {
            title: ticket.title.clone().into(),
            url: ticket.url.clone().into(),
            status: non_empty(&ticket.status),
            ticket_type: ticket.ticket_type.as_deref().and_then(non_empty),
            issue_id: ticket.issue_id.as_deref().and_then(non_empty),
        };
        store.update(cx, |store, cx| {
            store.upsert_ticket_ref(TicketId::new(ticket.page_id.clone()), fields.clone(), cx);
        });
    }
}

/// Notion's board query reports a missing text property as an empty string;
/// storing that as `Some("")` would make "unknown" indistinguishable from a
/// genuinely blank value downstream.
fn non_empty(value: &str) -> Option<SharedString> {
    let value = value.trim();
    (!value.is_empty()).then(|| SharedString::from(value.to_string()))
}
