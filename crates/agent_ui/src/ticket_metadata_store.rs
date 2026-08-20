use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use collections::HashMap;
use db::{
    sqlez::{
        bindable::Column, domain::Domain, statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use git::repository::CreateWorktreeTarget;
use gpui::{AppContext as _, AsyncApp, Entity, Global, Task, WindowHandle};
use project::git_store::Repository;
use project::project_settings::ProjectSettings;
use remote::RemoteConnectionOptions;
use settings::Settings as _;
use ui::{App, Context, SharedString};
use util::ResultExt as _;
use workspace::{AppState, MultiWorkspace, OpenOptions, OpenResult, Workspace};

use crate::terminal_thread_metadata_store::{TerminalThreadMetadata, TerminalThreadMetadataStore};
use crate::{AgentPanel, AgentThreadSource, TerminalId};

pub fn init(cx: &mut App) {
    TicketMetadataStore::init_global(cx);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TicketId(pub SharedString);

impl TicketId {
    pub fn new(notion_page_id: impl Into<SharedString>) -> Self {
        Self(notion_page_id.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketLaunchKind {
    Initial,
    Additional,
}

impl TicketLaunchKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Additional => "additional",
        }
    }

    fn from_str(value: &str) -> Self {
        match value {
            "additional" => Self::Additional,
            _ => Self::Initial,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TicketSessionRecord {
    pub terminal_id: TerminalId,
    pub cc_session_id: Option<String>,
    pub launch_kind: TicketLaunchKind,
    pub created_at: DateTime<Utc>,
    pub last_resumed_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TicketWorktreeRecord {
    pub ticket_id: TicketId,
    pub title: SharedString,
    pub url: SharedString,
    pub worktree_path: Option<PathBuf>,
    pub branch_name: Option<String>,
    pub base_repo_root: Option<PathBuf>,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub created_at: DateTime<Utc>,
    pub sessions: Vec<TicketSessionRecord>,
}

impl TicketWorktreeRecord {
    /// The most recently active session for this ticket (by creation time),
    /// used to decide what to resume when a ticket with an existing worktree
    /// is reopened.
    pub fn most_recent_session(&self) -> Option<&TicketSessionRecord> {
        self.sessions.iter().max_by_key(|session| session.created_at)
    }

    pub fn active_session_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| session.ended_at.is_none())
            .count()
    }
}

struct GlobalTicketMetadataStore(Entity<TicketMetadataStore>);
impl Global for GlobalTicketMetadataStore {}

#[cfg(any(test, feature = "test-support"))]
pub struct TestTicketMetadataDbName(pub String);
#[cfg(any(test, feature = "test-support"))]
impl Global for TestTicketMetadataDbName {}

#[cfg(any(test, feature = "test-support"))]
impl TestTicketMetadataDbName {
    pub fn global(cx: &App) -> String {
        cx.try_global::<Self>()
            .map(|global| global.0.clone())
            .unwrap_or_else(|| {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap_or("unknown_test");
                format!("TICKET_METADATA_DB_{}", test_name)
            })
    }
}

pub struct TicketMetadataStore {
    db: TicketMetadataDb,
    tickets: HashMap<TicketId, TicketWorktreeRecord>,
    pending_ops_tx: async_channel::Sender<DbOperation>,
    _db_operations_task: Task<()>,
}

#[derive(Debug, Clone)]
enum DbOperation {
    UpsertWorktree(TicketWorktreeRow),
    UpsertSession {
        ticket_id: TicketId,
        session: TicketSessionRecord,
    },
    DeleteWorktree(TicketId),
}

impl DbOperation {
    /// Distinguishes operations for dedup purposes: two ops with the same key
    /// are collapsed to just the most recent one when flushing a batch.
    fn dedup_key(&self) -> (u8, String) {
        match self {
            DbOperation::UpsertWorktree(row) => (0, row.ticket_id.0.to_string()),
            DbOperation::UpsertSession { session, .. } => (1, session.terminal_id.to_key_string()),
            DbOperation::DeleteWorktree(ticket_id) => (2, ticket_id.0.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
struct TicketWorktreeRow {
    ticket_id: TicketId,
    title: SharedString,
    url: SharedString,
    worktree_path: Option<PathBuf>,
    branch_name: Option<String>,
    base_repo_root: Option<PathBuf>,
    remote_connection: Option<RemoteConnectionOptions>,
    created_at: DateTime<Utc>,
}

impl TicketMetadataStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTicketMetadataStore>() {
            return;
        }

        let db = TicketMetadataDb::global(cx);
        let store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalTicketMetadataStore(store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        let db_name = TestTicketMetadataDbName::global(cx);
        let db = gpui::block_on(db::open_test_db::<TicketMetadataDb>(&db_name));
        let store = cx.new(|cx| Self::new(TicketMetadataDb(db), cx));
        cx.set_global(GlobalTicketMetadataStore(store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTicketMetadataStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalTicketMetadataStore>().0.clone()
    }

    pub fn entry(&self, ticket_id: &TicketId) -> Option<&TicketWorktreeRecord> {
        self.tickets.get(ticket_id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &TicketWorktreeRecord> + '_ {
        self.tickets.values()
    }

    /// Records or refreshes a ticket's display metadata as synced from
    /// Notion. Never touches worktree/session state — safe to call on every
    /// refresh regardless of whether a worktree exists yet.
    pub fn upsert_ticket_ref(
        &mut self,
        ticket_id: TicketId,
        title: SharedString,
        url: SharedString,
        cx: &mut Context<Self>,
    ) {
        let record = if let Some(existing) = self.tickets.get(&ticket_id) {
            TicketWorktreeRecord {
                title,
                url,
                ..existing.clone()
            }
        } else {
            TicketWorktreeRecord {
                ticket_id: ticket_id.clone(),
                title,
                url,
                worktree_path: None,
                branch_name: None,
                base_repo_root: None,
                remote_connection: None,
                created_at: Utc::now(),
                sessions: Vec::new(),
            }
        };
        self.save_worktree_record(record, cx);
    }

    /// Persists a newly created worktree's location for a ticket. The ticket
    /// must already have an entry (from `upsert_ticket_ref`).
    pub fn save_worktree(
        &mut self,
        ticket_id: &TicketId,
        worktree_path: PathBuf,
        branch_name: String,
        base_repo_root: PathBuf,
        remote_connection: Option<RemoteConnectionOptions>,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(existing) = self.tickets.get(ticket_id) else {
            anyhow::bail!("cannot save a worktree for an unknown ticket {ticket_id:?}");
        };
        let record = TicketWorktreeRecord {
            worktree_path: Some(worktree_path),
            branch_name: Some(branch_name),
            base_repo_root: Some(base_repo_root),
            remote_connection,
            ..existing.clone()
        };
        self.save_worktree_record(record, cx);
        Ok(())
    }

    fn save_worktree_record(&mut self, record: TicketWorktreeRecord, cx: &mut Context<Self>) {
        let row = TicketWorktreeRow {
            ticket_id: record.ticket_id.clone(),
            title: record.title.clone(),
            url: record.url.clone(),
            worktree_path: record.worktree_path.clone(),
            branch_name: record.branch_name.clone(),
            base_repo_root: record.base_repo_root.clone(),
            remote_connection: record.remote_connection.clone(),
            created_at: record.created_at,
        };
        self.tickets.insert(record.ticket_id.clone(), record);
        self.pending_ops_tx
            .try_send(DbOperation::UpsertWorktree(row))
            .log_err();
        cx.notify();
    }

    /// Records a newly launched or resumed Claude Code session for a ticket.
    /// The caller (`AgentPanel::spawn_ticket_terminal`) is expected to call
    /// this *before* actually spawning the terminal, so a crash mid-launch
    /// still leaves a resumable record.
    pub fn add_session(
        &mut self,
        ticket_id: &TicketId,
        session: TicketSessionRecord,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let Some(existing) = self.tickets.get_mut(ticket_id) else {
            anyhow::bail!("cannot add a session for an unknown ticket {ticket_id:?}");
        };
        existing
            .sessions
            .retain(|s| s.terminal_id != session.terminal_id);
        existing.sessions.push(session.clone());
        self.pending_ops_tx
            .try_send(DbOperation::UpsertSession {
                ticket_id: ticket_id.clone(),
                session,
            })
            .log_err();
        cx.notify();
        Ok(())
    }

    pub fn delete_worktree(&mut self, ticket_id: &TicketId, cx: &mut Context<Self>) {
        self.tickets.remove(ticket_id);
        self.pending_ops_tx
            .try_send(DbOperation::DeleteWorktree(ticket_id.clone()))
            .log_err();
        cx.notify();
    }

    fn new(db: TicketMetadataDb, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let _db_operations_task = cx.background_spawn({
            let db = db.clone();
            async move {
                while let Ok(first_update) = rx.recv().await {
                    let mut updates = vec![first_update];
                    while let Ok(update) = rx.try_recv() {
                        updates.push(update);
                    }
                    let updates = Self::dedup_db_operations(updates);
                    for operation in updates {
                        match operation {
                            DbOperation::UpsertWorktree(row) => {
                                db.save_worktree(row).await.log_err();
                            }
                            DbOperation::UpsertSession { ticket_id, session } => {
                                db.save_session(ticket_id, session).await.log_err();
                            }
                            DbOperation::DeleteWorktree(ticket_id) => {
                                db.delete_worktree(ticket_id).await.log_err();
                            }
                        }
                    }
                }
            }
        });

        let mut this = Self {
            db,
            tickets: HashMap::default(),
            pending_ops_tx: tx,
            _db_operations_task,
        };
        this.reload(cx);
        this
    }

    fn dedup_db_operations(operations: Vec<DbOperation>) -> Vec<DbOperation> {
        let mut ops = HashMap::default();
        for operation in operations.into_iter().rev() {
            let key = operation.dedup_key();
            if ops.contains_key(&key) {
                continue;
            }
            ops.insert(key, operation);
        }
        ops.into_values().collect()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let (worktrees, sessions) = cx
                .background_spawn(async move {
                    let worktrees = db
                        .list_worktrees()
                        .context("Failed to fetch ticket worktrees")
                        .log_err()
                        .unwrap_or_default();
                    let sessions = db
                        .list_sessions()
                        .context("Failed to fetch ticket sessions")
                        .log_err()
                        .unwrap_or_default();
                    (worktrees, sessions)
                })
                .await;

            this.update(cx, |this, cx| {
                this.tickets.clear();
                for row in worktrees {
                    this.tickets.insert(
                        row.ticket_id.clone(),
                        TicketWorktreeRecord {
                            ticket_id: row.ticket_id,
                            title: row.title,
                            url: row.url,
                            worktree_path: row.worktree_path,
                            branch_name: row.branch_name,
                            base_repo_root: row.base_repo_root,
                            remote_connection: row.remote_connection,
                            created_at: row.created_at,
                            sessions: Vec::new(),
                        },
                    );
                }
                for (ticket_id, session) in sessions {
                    if let Some(record) = this.tickets.get_mut(&ticket_id) {
                        record.sessions.push(session);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

struct TicketMetadataDb(ThreadSafeConnection);

impl Domain for TicketMetadataDb {
    const NAME: &str = stringify!(TicketMetadataDb);

    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE IF NOT EXISTS ticket_worktrees(
            ticket_id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            url TEXT NOT NULL,
            worktree_path TEXT,
            branch_name TEXT,
            base_repo_root TEXT,
            remote_connection TEXT,
            created_at TEXT NOT NULL
        ) STRICT;

        CREATE TABLE IF NOT EXISTS ticket_sessions(
            terminal_id TEXT PRIMARY KEY,
            ticket_id TEXT NOT NULL,
            cc_session_id TEXT,
            launch_kind TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_resumed_at TEXT,
            ended_at TEXT
        ) STRICT;
    )];
}

db::static_connection!(TicketMetadataDb, []);

impl TicketMetadataDb {
    fn list_worktrees(&self) -> anyhow::Result<Vec<TicketWorktreeRow>> {
        self.select::<TicketWorktreeRow>(
            "SELECT ticket_id, title, url, worktree_path, branch_name, base_repo_root, \
            remote_connection, created_at \
            FROM ticket_worktrees \
            ORDER BY created_at DESC",
        )?()
    }

    fn list_sessions(&self) -> anyhow::Result<Vec<(TicketId, TicketSessionRecord)>> {
        self.select::<(TicketId, TicketSessionRecord)>(
            "SELECT ticket_id, terminal_id, cc_session_id, launch_kind, created_at, \
            last_resumed_at, ended_at \
            FROM ticket_sessions \
            ORDER BY created_at DESC",
        )?()
    }

    async fn save_worktree(&self, row: TicketWorktreeRow) -> anyhow::Result<()> {
        let ticket_id = row.ticket_id.0.to_string();
        let title = row.title.to_string();
        let url = row.url.to_string();
        let worktree_path = row
            .worktree_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let branch_name = row.branch_name.clone();
        let base_repo_root = row
            .base_repo_root
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let remote_connection = row
            .remote_connection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize ticket remote connection")?;
        let created_at = row.created_at.to_rfc3339();

        self.write(move |conn| {
            let sql = "INSERT INTO ticket_worktrees(ticket_id, title, url, worktree_path, branch_name, base_repo_root, remote_connection, created_at) \
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                       ON CONFLICT(ticket_id) DO UPDATE SET \
                           title = excluded.title, \
                           url = excluded.url, \
                           worktree_path = excluded.worktree_path, \
                           branch_name = excluded.branch_name, \
                           base_repo_root = excluded.base_repo_root, \
                           remote_connection = excluded.remote_connection, \
                           created_at = excluded.created_at";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&ticket_id, 1)?;
            i = stmt.bind(&title, i)?;
            i = stmt.bind(&url, i)?;
            i = stmt.bind(&worktree_path, i)?;
            i = stmt.bind(&branch_name, i)?;
            i = stmt.bind(&base_repo_root, i)?;
            i = stmt.bind(&remote_connection, i)?;
            stmt.bind(&created_at, i)?;
            stmt.exec()
        })
        .await
    }

    async fn save_session(
        &self,
        ticket_id: TicketId,
        session: TicketSessionRecord,
    ) -> anyhow::Result<()> {
        let terminal_id = session.terminal_id.to_key_string();
        let ticket_id = ticket_id.0.to_string();
        let cc_session_id = session.cc_session_id.clone();
        let launch_kind = session.launch_kind.as_str().to_string();
        let created_at = session.created_at.to_rfc3339();
        let last_resumed_at = session.last_resumed_at.map(|time| time.to_rfc3339());
        let ended_at = session.ended_at.map(|time| time.to_rfc3339());

        self.write(move |conn| {
            let sql = "INSERT INTO ticket_sessions(terminal_id, ticket_id, cc_session_id, launch_kind, created_at, last_resumed_at, ended_at) \
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                       ON CONFLICT(terminal_id) DO UPDATE SET \
                           ticket_id = excluded.ticket_id, \
                           cc_session_id = excluded.cc_session_id, \
                           launch_kind = excluded.launch_kind, \
                           created_at = excluded.created_at, \
                           last_resumed_at = excluded.last_resumed_at, \
                           ended_at = excluded.ended_at";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&terminal_id, 1)?;
            i = stmt.bind(&ticket_id, i)?;
            i = stmt.bind(&cc_session_id, i)?;
            i = stmt.bind(&launch_kind, i)?;
            i = stmt.bind(&created_at, i)?;
            i = stmt.bind(&last_resumed_at, i)?;
            stmt.bind(&ended_at, i)?;
            stmt.exec()
        })
        .await
    }

    async fn delete_worktree(&self, ticket_id: TicketId) -> anyhow::Result<()> {
        let ticket_id = ticket_id.0.to_string();
        self.write(move |conn| {
            let mut stmt =
                Statement::prepare(conn, "DELETE FROM ticket_worktrees WHERE ticket_id = ?")?;
            stmt.bind(&ticket_id, 1)?;
            stmt.exec()?;
            let mut stmt =
                Statement::prepare(conn, "DELETE FROM ticket_sessions WHERE ticket_id = ?")?;
            stmt.bind(&ticket_id, 1)?;
            stmt.exec()
        })
        .await
    }
}

impl Column for TicketWorktreeRow {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (ticket_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (title, next): (String, i32) = Column::column(statement, next)?;
        let (url, next): (String, i32) = Column::column(statement, next)?;
        let (worktree_path, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (branch_name, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (base_repo_root, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (remote_connection_json, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;

        let remote_connection = remote_connection_json
            .as_deref()
            .map(serde_json::from_str::<RemoteConnectionOptions>)
            .transpose()
            .context("deserialize ticket remote connection")?;

        Ok((
            TicketWorktreeRow {
                ticket_id: TicketId(SharedString::from(ticket_id)),
                title: SharedString::from(title),
                url: SharedString::from(url),
                worktree_path: worktree_path.map(PathBuf::from),
                branch_name,
                base_repo_root: base_repo_root.map(PathBuf::from),
                remote_connection,
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
            },
            next,
        ))
    }
}

impl Column for TicketId {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (ticket_id, next): (String, i32) = Column::column(statement, start_index)?;
        Ok((TicketId(SharedString::from(ticket_id)), next))
    }
}

impl Column for TicketSessionRecord {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (terminal_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (cc_session_id, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (launch_kind, next): (String, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (last_resumed_at, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (ended_at, next): (Option<String>, i32) = Column::column(statement, next)?;

        Ok((
            TicketSessionRecord {
                terminal_id: TerminalId::from_key_string(&terminal_id)?,
                cc_session_id,
                launch_kind: TicketLaunchKind::from_str(&launch_kind),
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                last_resumed_at: last_resumed_at
                    .map(|time| DateTime::parse_from_rfc3339(&time))
                    .transpose()?
                    .map(|time| time.with_timezone(&Utc)),
                ended_at: ended_at
                    .map(|time| DateTime::parse_from_rfc3339(&time))
                    .transpose()?
                    .map(|time| time.with_timezone(&Utc)),
            },
            next,
        ))
    }
}

/// Opens (or focuses, if already open in some window) a workspace at `path`
/// and waits for its project's initial scan to finish, mirroring the await
/// sequence `git_ui::worktree_service::open_worktree_workspace` uses before
/// touching panels. `workspace::open_paths` already implements reuse-across-
/// all-windows-else-open-new, so callers never need to check "is it open"
/// themselves.
async fn open_ticket_workspace(
    path: PathBuf,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    let OpenResult {
        window, workspace, ..
    } = cx
        .update(|cx| workspace::open_paths(&[path], app_state, OpenOptions::default(), cx))
        .await?;

    workspace
        .update(cx, |workspace, cx| {
            workspace.project().read(cx).wait_for_initial_scan(cx)
        })
        .await;

    Ok((window, workspace))
}

/// Best-effort rollback of a worktree whose creation failed partway through;
/// errors are logged, not propagated, since the caller is already returning
/// its own (more relevant) error.
async fn rollback_worktree(repo: &Entity<Repository>, path: PathBuf, cx: &mut AsyncApp) {
    let removal = repo.update(cx, |repo, _cx| repo.remove_worktree(path.clone(), true));
    match removal.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            log::error!("failed to roll back worktree at {}: {error:#}", path.display());
        }
        Err(_canceled) => {
            log::error!("failed to roll back worktree at {}: canceled", path.display());
        }
    }
}

/// Spawns a fresh `claude` CLI session for a ticket in the given worktree,
/// via the workspace's `AgentPanel`.
async fn launch_ticket_session(
    ticket_id: TicketId,
    worktree_path: PathBuf,
    seeded_message: String,
    launch_kind: TicketLaunchKind,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let (window, workspace) = open_ticket_workspace(worktree_path, app_state, cx).await?;
    let agent_panel = workspace
        .read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))
        .context("agent panel not available in this workspace")?;

    window.update(cx, |_multi_workspace, window, cx| {
        agent_panel.update(cx, |panel, cx| {
            panel.spawn_ticket_terminal(ticket_id, launch_kind, seeded_message, window, cx)
        })
    })?;
    Ok(())
}

/// Creates a git worktree for a ticket on a new branch cut from the
/// configured `repo_path`'s current `HEAD` (bypassing `zed_actions::CreateWorktree`,
/// which always creates a *detached* worktree by design), persists the
/// result into `TicketMetadataStore`, then launches the ticket's initial
/// Claude Code session in it. `ticket_id` must already have an entry in
/// `TicketMetadataStore` (from a prior `upsert_ticket_ref` sync).
pub async fn create_worktree_and_launch(
    ticket_id: TicketId,
    repo_path: PathBuf,
    branch_name: String,
    seeded_message: String,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let (_home_window, home_workspace) =
        open_ticket_workspace(repo_path, app_state.clone(), cx).await?;

    let repo = home_workspace
        .read_with(cx, |workspace, cx| {
            workspace.project().read(cx).active_repository(cx)
        })
        .context("no git repository open at the configured tickets_panel.repo_path")?;

    let worktree_directory_setting = home_workspace.read_with(cx, |_workspace, cx| {
        ProjectSettings::get_global(cx).git.worktree_directory.clone()
    });

    let (worktree_path, base_repo_root) = repo.update(cx, |repo, _cx| {
        anyhow::Ok((
            repo.path_for_new_linked_worktree(&branch_name, &worktree_directory_setting)?,
            repo.work_directory_abs_path.to_path_buf(),
        ))
    })?;

    let receiver = repo.update(cx, |repo, _cx| {
        repo.create_worktree(
            CreateWorktreeTarget::NewBranch {
                branch_name: branch_name.clone(),
                base_sha: None,
            },
            worktree_path.clone(),
        )
    });

    match receiver.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            rollback_worktree(&repo, worktree_path, cx).await;
            return Err(error);
        }
        Err(_canceled) => {
            rollback_worktree(&repo, worktree_path, cx).await;
            anyhow::bail!("worktree creation was canceled");
        }
    }

    let ticket_store = cx.update(|cx| TicketMetadataStore::global(cx));
    ticket_store.update(cx, |store, cx| {
        store.save_worktree(
            &ticket_id,
            worktree_path.clone(),
            branch_name,
            base_repo_root,
            None,
            cx,
        )
    })?;

    launch_ticket_session(
        ticket_id,
        worktree_path,
        seeded_message,
        TicketLaunchKind::Initial,
        app_state,
        cx,
    )
    .await
}

/// Opens a ticket whose worktree already exists: focuses/opens the
/// worktree's workspace and resumes its most recently active session via
/// `claude --resume <id>` (built by `AgentPanel::restore_terminal`'s
/// resume-command logic — this call site does not construct the command
/// itself).
pub async fn open_ticket(
    ticket_id: TicketId,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let store = cx.update(|cx| TicketMetadataStore::global(cx));
    let (worktree_path, most_recent_terminal_id) = store.read_with(cx, |store, _cx| {
        let entry = store
            .entry(&ticket_id)
            .context("ticket is not tracked by TicketMetadataStore")?;
        let worktree_path = entry
            .worktree_path
            .clone()
            .context("ticket has no worktree yet")?;
        let terminal_id = entry
            .most_recent_session()
            .context("ticket has a worktree but no recorded session")?
            .terminal_id;
        anyhow::Ok((worktree_path, terminal_id))
    })?;

    let (window, workspace) = open_ticket_workspace(worktree_path, app_state, cx).await?;
    let agent_panel = workspace
        .read_with(cx, |workspace, cx| workspace.panel::<AgentPanel>(cx))
        .context("agent panel not available in this workspace")?;

    let terminal_metadata = cx
        .update(|cx| TerminalThreadMetadataStore::global(cx))
        .read_with(cx, |store, _cx| store.entry(most_recent_terminal_id).cloned())
        .context("no persisted metadata for the ticket's most recent session")?;

    window.update(cx, |_multi_workspace, window, cx| {
        agent_panel.update(cx, |panel, cx| {
            panel.restore_terminal(
                terminal_metadata,
                true,
                AgentThreadSource::TicketPanel,
                None,
                window,
                cx,
            )
        })
    })?;
    Ok(())
}

/// Launches an additional, independent Claude Code session for a ticket
/// that already has a worktree (and possibly other sessions running).
pub async fn launch_additional_session(
    ticket_id: TicketId,
    seeded_message: String,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let worktree_path = cx
        .update(|cx| TicketMetadataStore::global(cx))
        .read_with(cx, |store, _cx| {
            store
                .entry(&ticket_id)
                .and_then(|entry| entry.worktree_path.clone())
        })
        .context("ticket has no worktree yet")?;

    launch_ticket_session(
        ticket_id,
        worktree_path,
        seeded_message,
        TicketLaunchKind::Additional,
        app_state,
        cx,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            TicketMetadataStore::init_global(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_upsert_and_save_worktree_round_trip(cx: &mut TestAppContext) {
        init_test(cx);

        let ticket_id = TicketId::new("notion-page-1");
        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.upsert_ticket_ref(
                    ticket_id.clone(),
                    "Fix invoice export".into(),
                    "https://notion.so/ticket-1".into(),
                    cx,
                );
            });
        });

        cx.update(|cx| {
            TicketMetadataStore::global(cx)
                .update(cx, |store, cx| {
                    store.save_worktree(
                        &ticket_id,
                        PathBuf::from("/worktrees/fix-invoice-export"),
                        "ticket/fix-invoice-export".to_string(),
                        PathBuf::from("/repo"),
                        None,
                        cx,
                    )
                })
                .unwrap();
        });

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            let entry = store.entry(&ticket_id).expect("ticket should be present");
            assert_eq!(entry.title.as_ref(), "Fix invoice export");
            assert_eq!(
                entry.worktree_path,
                Some(PathBuf::from("/worktrees/fix-invoice-export"))
            );
            assert_eq!(
                entry.branch_name.as_deref(),
                Some("ticket/fix-invoice-export")
            );
        });

        // Reload from the database to confirm persistence, not just the
        // in-memory cache.
        let db = cx.update(|cx| TicketMetadataStore::global(cx).read(cx).db.clone());
        cx.run_until_parked();
        let rows = db.list_worktrees().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ticket_id, ticket_id);
    }

    #[gpui::test]
    async fn test_add_session_and_delete_worktree_round_trip(cx: &mut TestAppContext) {
        init_test(cx);

        let ticket_id = TicketId::new("notion-page-2");
        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.upsert_ticket_ref(
                    ticket_id.clone(),
                    "Add dark mode".into(),
                    "https://notion.so/ticket-2".into(),
                    cx,
                );
            });
        });

        let terminal_id = TerminalId::new();
        cx.update(|cx| {
            TicketMetadataStore::global(cx)
                .update(cx, |store, cx| {
                    store.add_session(
                        &ticket_id,
                        TicketSessionRecord {
                            terminal_id,
                            cc_session_id: Some("session-uuid".to_string()),
                            launch_kind: TicketLaunchKind::Initial,
                            created_at: Utc::now(),
                            last_resumed_at: None,
                            ended_at: None,
                        },
                        cx,
                    )
                })
                .unwrap();
        });

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            let entry = store.entry(&ticket_id).expect("ticket should be present");
            assert_eq!(entry.sessions.len(), 1);
            assert_eq!(
                entry.most_recent_session().unwrap().cc_session_id.as_deref(),
                Some("session-uuid")
            );
        });

        cx.update(|cx| {
            TicketMetadataStore::global(cx).update(cx, |store, cx| {
                store.delete_worktree(&ticket_id, cx);
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            let store = TicketMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(store.entry(&ticket_id).is_none());
        });

        let db = cx.update(|cx| TicketMetadataStore::global(cx).read(cx).db.clone());
        cx.run_until_parked();
        assert!(db.list_worktrees().unwrap().is_empty());
    }
}
