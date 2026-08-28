use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use collections::{HashMap, HashSet};
use db::{
    sqlez::{
        bindable::Column, domain::Domain, statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
    sqlez_macros::sql,
};
use futures::{FutureExt, future::Shared};
use gpui::{AppContext as _, Entity, Global, Task};
use remote::{RemoteConnectionOptions, same_remote_connection_identity};
use ui::{App, Context, SharedString};
use util::ResultExt as _;
use workspace::PathList;

use crate::{TerminalId, thread_metadata_store::WorktreePaths};

pub fn init(cx: &mut App) {
    TerminalThreadMetadataStore::init_global(cx);
}

struct GlobalTerminalThreadMetadataStore(Entity<TerminalThreadMetadataStore>);
impl Global for GlobalTerminalThreadMetadataStore {}

#[cfg(any(test, feature = "test-support"))]
pub struct TestTerminalMetadataDbName(pub String);
#[cfg(any(test, feature = "test-support"))]
impl Global for TestTerminalMetadataDbName {}

#[cfg(any(test, feature = "test-support"))]
impl TestTerminalMetadataDbName {
    pub fn global(cx: &App) -> String {
        cx.try_global::<Self>()
            .map(|global| global.0.clone())
            .unwrap_or_else(|| {
                let thread = std::thread::current();
                let test_name = thread.name().unwrap_or("unknown_test");
                format!("TERMINAL_THREAD_METADATA_DB_{}", test_name)
            })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalThreadMetadata {
    pub terminal_id: TerminalId,
    pub title: SharedString,
    pub custom_title: Option<SharedString>,
    pub created_at: DateTime<Utc>,
    pub worktree_paths: WorktreePaths,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub working_directory: Option<PathBuf>,
    /// Shell command line typed into the terminal right after it was spawned
    /// (e.g. a `claude` invocation for a ticket-launched session). `None` for
    /// plain agent-panel terminals, which start a blank shell.
    pub initial_command: Option<String>,
    /// The `claude` CLI session id this terminal was launched or last resumed
    /// with, if any. Used to build a `claude --resume <id>` command when the
    /// terminal is respawned after being closed.
    pub cc_session_id: Option<String>,
}

impl TerminalThreadMetadata {
    pub fn folder_paths(&self) -> &PathList {
        self.worktree_paths.folder_path_list()
    }

    pub fn main_worktree_paths(&self) -> &PathList {
        self.worktree_paths.main_worktree_path_list()
    }

    pub fn display_title(&self) -> SharedString {
        compose_terminal_thread_title(
            self.title.as_ref(),
            self.custom_title.as_ref().map(|title| title.as_ref()),
        )
    }
}

pub(crate) fn compose_terminal_thread_title(
    terminal_title: &str,
    custom_title: Option<&str>,
) -> SharedString {
    let Some(custom_title) = custom_title.filter(|title| !title.trim().is_empty()) else {
        return SharedString::from(terminal_title.to_string());
    };

    if let Some(prefix) = terminal_title_prefix(terminal_title) {
        SharedString::from(format!("{prefix}{custom_title}"))
    } else {
        SharedString::from(custom_title.to_string())
    }
}

pub fn terminal_title_without_prefix(title: &str) -> &str {
    terminal_title_prefix(title)
        .map(|prefix| &title[prefix.len()..])
        .unwrap_or(title)
}

/// What the `claude` CLI is doing right now, decoded from the terminal title.
///
/// Claude Code reports its own state through the terminal title, as
/// `ESC ] 0 ; <glyph> <task summary> BEL`: while it works the glyph cycles
/// through the quadrant-circle spinner frames (`◐ ◑ ◒ ◓`) roughly twice a
/// second, and once it stops it settles on the asterisk family (`✳ ✻ ✽ ✶`).
/// That is the only liveness signal available for a CLI running inside a pty —
/// the terminal itself stays open either way, which is why "the terminal
/// exists" cannot stand in for "the agent is working".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ClaudeActivity {
    /// The title carries no glyph Claude Code is known to use, so the terminal
    /// is running something else (a plain shell, a build, ...).
    #[default]
    Unknown,
    Working,
    Idle,
}

/// The spinner frames Claude Code animates while a turn is in flight.
const CLAUDE_WORKING_GLYPHS: [char; 4] = ['\u{25d0}', '\u{25d1}', '\u{25d2}', '\u{25d3}'];
/// The resting glyphs Claude Code settles on between turns.
const CLAUDE_IDLE_GLYPHS: [char; 4] = ['\u{2733}', '\u{273b}', '\u{273d}', '\u{2736}'];

/// The title Claude Code shows before the first prompt of a session, which is
/// a placeholder rather than a task summary.
const CLAUDE_PLACEHOLDER_TASK: &str = "Claude Code";

pub fn claude_activity(terminal_title: &str) -> ClaudeActivity {
    let Some(glyph) = terminal_title_prefix(terminal_title)
        .and_then(|prefix| prefix.trim_end().chars().next_back())
    else {
        return ClaudeActivity::Unknown;
    };

    if CLAUDE_WORKING_GLYPHS.contains(&glyph) {
        ClaudeActivity::Working
    } else if CLAUDE_IDLE_GLYPHS.contains(&glyph) {
        ClaudeActivity::Idle
    } else {
        ClaudeActivity::Unknown
    }
}

/// The task summary Claude Code appended to its status glyph, with the
/// pre-first-prompt placeholder treated as "no task yet".
pub fn claude_task_summary(terminal_title: &str) -> Option<&str> {
    if claude_activity(terminal_title) == ClaudeActivity::Unknown {
        return None;
    }
    let summary = terminal_title_without_prefix(terminal_title).trim();
    (!summary.is_empty() && summary != CLAUDE_PLACEHOLDER_TASK).then_some(summary)
}

/// How many lines of terminal output to hand [`claude_response_excerpt`].
///
/// Enough to reach back over a tool call and its result to the prose above it,
/// without scanning a whole screenful for nothing.
pub const CLAUDE_EXCERPT_LINES: usize = 40;

/// How much of the response to carry into a notification body. Both XDG and
/// Windows truncate long bodies themselves, at lengths neither documents, so
/// the cut is made here where it can land on a word boundary.
const CLAUDE_EXCERPT_MAX_CHARS: usize = 200;

/// The glyphs Claude Code prefixes its own messages and its tool calls with.
const CLAUDE_MESSAGE_MARKERS: [char; 2] = ['\u{23fa}', '\u{25cf}'];

/// The last thing Claude Code said, recovered from what it drew on screen.
///
/// The transcript on disk would be a more faithful source, but its location and
/// shape are Claude Code's private business, whereas the screen is the contract
/// it keeps with the user. So this reads the rendered output: every message and
/// every tool call is prefixed with a marker glyph, and the input box below them
/// is drawn with box-drawing characters. Walking backwards to the last marker
/// that is not a tool call, then forward to the first line of box drawing,
/// yields the prose Claude finished on.
///
/// This is a heuristic over a TUI that is free to change, so callers must have
/// something to fall back on: `None` means "nothing recognizable", not "nothing
/// happened".
///
/// Expects the output of `terminal::Terminal::last_n_non_empty_lines`, which
/// unwraps soft-wrapped rows into one logical line each and drops blank lines.
pub fn claude_response_excerpt(lines: &[String]) -> Option<String> {
    let message_start = lines.iter().enumerate().rev().find_map(|(index, line)| {
        let message = strip_message_marker(line)?;
        (!is_tool_call(message)).then_some(index)
    })?;

    let mut excerpt = String::new();
    for line in lines.get(message_start..)? {
        let text = match strip_message_marker(line) {
            Some(message) if excerpt.is_empty() => message,
            // A further marker opens the next message or a tool call, so the
            // message being read ends here.
            Some(_) => break,
            None if is_chrome(line) => break,
            None => line.trim(),
        };
        if text.is_empty() {
            continue;
        }
        if !excerpt.is_empty() {
            excerpt.push(' ');
        }
        excerpt.push_str(text);
    }

    let excerpt = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
    (!excerpt.is_empty()).then(|| truncate_on_word_boundary(&excerpt, CLAUDE_EXCERPT_MAX_CHARS))
}

/// The text after a message or tool-call marker, or `None` if the line carries
/// no marker.
fn strip_message_marker(line: &str) -> Option<&str> {
    let line = line.trim();
    let marker = line.chars().next()?;
    if !CLAUDE_MESSAGE_MARKERS.contains(&marker) {
        return None;
    }
    Some(line[marker.len_utf8()..].trim_start())
}

/// Whether a marked line is a tool call (`Bash(cargo test)`) rather than prose.
fn is_tool_call(message: &str) -> bool {
    let name_length = message
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
        .count();
    name_length > 0
        && message
            .get(name_length..)
            .is_some_and(|rest| rest.starts_with('('))
}

/// Whether a line belongs to Claude Code's frame rather than its output: the
/// input box, a tool result's hook, the mode indicators, the status glyph.
fn is_chrome(line: &str) -> bool {
    let Some(first) = line.trim_start().chars().next() else {
        return true;
    };
    matches!(first, '\u{2500}'..='\u{257f}' | '\u{23b8}'..='\u{23bf}' | '\u{23f4}'..='\u{23f7}')
        || CLAUDE_WORKING_GLYPHS.contains(&first)
        || CLAUDE_IDLE_GLYPHS.contains(&first)
}

fn truncate_on_word_boundary(text: &str, max_characters: usize) -> String {
    if text.chars().count() <= max_characters {
        return text.to_string();
    }
    let cut = text
        .char_indices()
        .nth(max_characters)
        .map_or(text.len(), |(index, _)| index);
    let head = &text[..cut];
    // A word longer than the whole budget has no boundary to fall back on.
    let head = head
        .rfind(char::is_whitespace)
        .map_or(head, |space| &head[..space]);
    format!("{}\u{2026}", head.trim_end())
}

pub fn terminal_title_prefix(title: &str) -> Option<&str> {
    let mut prefix_byte_len = 0;
    let mut saw_prefix_character = false;
    let mut saw_whitespace_after_prefix = false;

    let mut chars = title.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_alphanumeric() {
            return None;
        }

        if character.is_whitespace() {
            if !saw_prefix_character {
                return None;
            }

            prefix_byte_len += character.len_utf8();
            saw_whitespace_after_prefix = true;

            while let Some(character) = chars.peek() {
                if !character.is_whitespace() {
                    break;
                }

                prefix_byte_len += character.len_utf8();
                chars.next();
            }

            break;
        }

        saw_prefix_character = true;
        prefix_byte_len += character.len_utf8();
    }

    if saw_whitespace_after_prefix {
        Some(&title[..prefix_byte_len])
    } else {
        None
    }
}

pub struct TerminalThreadMetadataStore {
    db: TerminalThreadMetadataDb,
    terminals: HashMap<TerminalId, TerminalThreadMetadata>,
    terminals_by_paths: HashMap<PathList, HashSet<TerminalId>>,
    terminals_by_main_paths: HashMap<PathList, HashSet<TerminalId>>,
    reload_task: Option<Shared<Task<()>>>,
    pending_terminal_ops_tx: async_channel::Sender<DbOperation>,
    _db_operations_task: Task<()>,
}

#[derive(Debug, PartialEq)]
enum DbOperation {
    Upsert(TerminalThreadMetadata),
    Delete(TerminalId),
}

impl DbOperation {
    fn id(&self) -> TerminalId {
        match self {
            DbOperation::Upsert(metadata) => metadata.terminal_id,
            DbOperation::Delete(terminal_id) => *terminal_id,
        }
    }
}

impl TerminalThreadMetadataStore {
    #[cfg(not(any(test, feature = "test-support")))]
    pub fn init_global(cx: &mut App) {
        if cx.has_global::<GlobalTerminalThreadMetadataStore>() {
            return;
        }

        let db = TerminalThreadMetadataDb::global(cx);
        let terminal_store = cx.new(|cx| Self::new(db, cx));
        cx.set_global(GlobalTerminalThreadMetadataStore(terminal_store));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn init_global(cx: &mut App) {
        let db_name = TestTerminalMetadataDbName::global(cx);
        let db = gpui::block_on(db::open_test_db::<TerminalThreadMetadataDb>(&db_name));
        let terminal_store = cx.new(|cx| Self::new(TerminalThreadMetadataDb(db), cx));
        cx.set_global(GlobalTerminalThreadMetadataStore(terminal_store));
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalTerminalThreadMetadataStore>()
            .map(|store| store.0.clone())
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalTerminalThreadMetadataStore>().0.clone()
    }

    pub fn entry(&self, terminal_id: TerminalId) -> Option<&TerminalThreadMetadata> {
        self.terminals.get(&terminal_id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &TerminalThreadMetadata> + '_ {
        self.terminals.values()
    }

    pub fn reload_task(&self) -> Shared<Task<()>> {
        self.reload_task
            .clone()
            .unwrap_or_else(|| Task::ready(()).shared())
    }

    pub fn entries_for_path<'a>(
        &'a self,
        path_list: &PathList,
        remote_connection: Option<&'a RemoteConnectionOptions>,
    ) -> impl Iterator<Item = &'a TerminalThreadMetadata> + 'a {
        self.terminals_by_paths
            .get(path_list)
            .into_iter()
            .flatten()
            .filter_map(|id| self.terminals.get(id))
            .filter(move |terminal| {
                same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
            })
    }

    pub fn entries_for_main_worktree_path<'a>(
        &'a self,
        path_list: &PathList,
        remote_connection: Option<&'a RemoteConnectionOptions>,
    ) -> impl Iterator<Item = &'a TerminalThreadMetadata> + 'a {
        self.terminals_by_main_paths
            .get(path_list)
            .into_iter()
            .flatten()
            .filter_map(|id| self.terminals.get(id))
            .filter(move |terminal| {
                same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
            })
    }

    pub fn path_is_referenced_by_terminal(
        &self,
        terminal_id: Option<TerminalId>,
        path: &Path,
        remote_connection: Option<&RemoteConnectionOptions>,
    ) -> bool {
        self.entries().any(|terminal| {
            Some(terminal.terminal_id) != terminal_id
                && same_remote_connection_identity(
                    terminal.remote_connection.as_ref(),
                    remote_connection,
                )
                && terminal
                    .folder_paths()
                    .paths()
                    .iter()
                    .any(|folder_path| folder_path.as_path() == path)
        })
    }

    pub fn save(&mut self, metadata: TerminalThreadMetadata, cx: &mut Context<Self>) {
        self.save_internal(metadata);
        cx.notify();
    }

    pub fn change_worktree_paths(
        &mut self,
        current_folder_paths: &PathList,
        remote_connection: Option<&RemoteConnectionOptions>,
        mutate: impl Fn(&mut WorktreePaths),
        cx: &mut Context<Self>,
    ) {
        let terminal_ids: Vec<_> = self
            .terminals_by_paths
            .get(current_folder_paths)
            .into_iter()
            .flatten()
            .filter(|id| {
                self.terminals.get(id).is_some_and(|terminal| {
                    same_remote_connection_identity(
                        terminal.remote_connection.as_ref(),
                        remote_connection,
                    )
                })
            })
            .copied()
            .collect();

        if terminal_ids.is_empty() {
            return;
        }

        for terminal_id in terminal_ids {
            if let Some(mut terminal) = self.terminals.get(&terminal_id).cloned() {
                mutate(&mut terminal.worktree_paths);
                self.save_internal(terminal);
            }
        }

        cx.notify();
    }

    fn save_internal(&mut self, metadata: TerminalThreadMetadata) {
        if let Some(existing) = self.terminals.get(&metadata.terminal_id) {
            if existing.folder_paths() != metadata.folder_paths()
                && let Some(ids) = self.terminals_by_paths.get_mut(existing.folder_paths())
            {
                ids.remove(&metadata.terminal_id);
            }

            if existing.main_worktree_paths() != metadata.main_worktree_paths()
                && let Some(ids) = self
                    .terminals_by_main_paths
                    .get_mut(existing.main_worktree_paths())
            {
                ids.remove(&metadata.terminal_id);
            }
        }

        self.cache_terminal_metadata(metadata.clone());
        self.pending_terminal_ops_tx
            .try_send(DbOperation::Upsert(metadata))
            .log_err();
    }

    fn cache_terminal_metadata(&mut self, metadata: TerminalThreadMetadata) {
        self.terminals
            .insert(metadata.terminal_id, metadata.clone());

        self.terminals_by_paths
            .entry(metadata.folder_paths().clone())
            .or_default()
            .insert(metadata.terminal_id);

        if !metadata.main_worktree_paths().is_empty() {
            self.terminals_by_main_paths
                .entry(metadata.main_worktree_paths().clone())
                .or_default()
                .insert(metadata.terminal_id);
        }
    }

    pub fn delete(&mut self, terminal_id: TerminalId, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminals.remove(&terminal_id) {
            if let Some(ids) = self.terminals_by_paths.get_mut(terminal.folder_paths()) {
                ids.remove(&terminal_id);
            }
            if !terminal.main_worktree_paths().is_empty()
                && let Some(ids) = self
                    .terminals_by_main_paths
                    .get_mut(terminal.main_worktree_paths())
            {
                ids.remove(&terminal_id);
            }
        }
        self.pending_terminal_ops_tx
            .try_send(DbOperation::Delete(terminal_id))
            .log_err();
        cx.notify();
    }

    fn new(db: TerminalThreadMetadataDb, cx: &mut Context<Self>) -> Self {
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
                            DbOperation::Upsert(metadata) => {
                                db.save(metadata).await.log_err();
                            }
                            DbOperation::Delete(terminal_id) => {
                                db.delete(terminal_id).await.log_err();
                            }
                        }
                    }
                }
            }
        });

        let mut this = Self {
            db,
            terminals: HashMap::default(),
            terminals_by_paths: HashMap::default(),
            terminals_by_main_paths: HashMap::default(),
            reload_task: None,
            pending_terminal_ops_tx: tx,
            _db_operations_task,
        };
        this.reload(cx);
        this
    }

    fn dedup_db_operations(operations: Vec<DbOperation>) -> Vec<DbOperation> {
        let mut ops = HashMap::default();
        for operation in operations.into_iter().rev() {
            if ops.contains_key(&operation.id()) {
                continue;
            }
            ops.insert(operation.id(), operation);
        }
        ops.into_values().collect()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        self.reload_task = Some(
            cx.spawn(async move |this, cx| {
                let rows = cx
                    .background_spawn(async move {
                        db.list()
                            .context("Failed to fetch terminal thread metadata")
                    })
                    .await
                    .log_err()
                    .unwrap_or_default();

                this.update(cx, |this, cx| {
                    this.terminals.clear();
                    this.terminals_by_paths.clear();
                    this.terminals_by_main_paths.clear();

                    for row in rows {
                        this.cache_terminal_metadata(row);
                    }

                    cx.notify();
                })
                .ok();
            })
            .shared(),
        );
    }
}

struct TerminalThreadMetadataDb(ThreadSafeConnection);

impl Domain for TerminalThreadMetadataDb {
    const NAME: &str = stringify!(TerminalThreadMetadataDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE IF NOT EXISTS sidebar_terminal_threads(
                terminal_id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                custom_title TEXT,
                created_at TEXT NOT NULL,
                working_directory TEXT,
                folder_paths TEXT,
                folder_paths_order TEXT,
                main_worktree_paths TEXT,
                main_worktree_paths_order TEXT,
                remote_connection TEXT
            ) STRICT;
        ),
        sql!(ALTER TABLE sidebar_terminal_threads ADD COLUMN initial_command TEXT),
        sql!(ALTER TABLE sidebar_terminal_threads ADD COLUMN cc_session_id TEXT),
    ];
}

db::static_connection!(TerminalThreadMetadataDb, []);

impl TerminalThreadMetadataDb {
    pub fn list(&self) -> anyhow::Result<Vec<TerminalThreadMetadata>> {
        self.select::<TerminalThreadMetadata>(
            "SELECT terminal_id, title, custom_title, created_at, \
            working_directory, folder_paths, folder_paths_order, main_worktree_paths, \
            main_worktree_paths_order, remote_connection, initial_command, cc_session_id \
            FROM sidebar_terminal_threads \
            ORDER BY created_at DESC",
        )?()
    }

    pub async fn save(&self, row: TerminalThreadMetadata) -> anyhow::Result<()> {
        let terminal_id = row.terminal_id.to_key_string();
        let title = row.title.to_string();
        let custom_title = row.custom_title.as_ref().map(ToString::to_string);
        let created_at = row.created_at.to_rfc3339();
        let working_directory = row
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let serialized = row.folder_paths().serialize();
        let (folder_paths, folder_paths_order) = if row.folder_paths().is_empty() {
            (None, None)
        } else {
            (Some(serialized.paths), Some(serialized.order))
        };
        let main_serialized = row.main_worktree_paths().serialize();
        let (main_worktree_paths, main_worktree_paths_order) =
            if row.main_worktree_paths().is_empty() {
                (None, None)
            } else {
                (Some(main_serialized.paths), Some(main_serialized.order))
            };
        let remote_connection = row
            .remote_connection
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("serialize terminal thread remote connection")?;
        let initial_command = row.initial_command.clone();
        let cc_session_id = row.cc_session_id.clone();

        self.write(move |conn| {
            let sql = "INSERT INTO sidebar_terminal_threads(terminal_id, title, custom_title, created_at, working_directory, folder_paths, folder_paths_order, main_worktree_paths, main_worktree_paths_order, remote_connection, initial_command, cc_session_id) \
                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                       ON CONFLICT(terminal_id) DO UPDATE SET \
                           title = excluded.title, \
                           custom_title = excluded.custom_title, \
                           created_at = excluded.created_at, \
                           working_directory = excluded.working_directory, \
                           folder_paths = excluded.folder_paths, \
                           folder_paths_order = excluded.folder_paths_order, \
                           main_worktree_paths = excluded.main_worktree_paths, \
                           main_worktree_paths_order = excluded.main_worktree_paths_order, \
                           remote_connection = excluded.remote_connection, \
                           initial_command = excluded.initial_command, \
                           cc_session_id = excluded.cc_session_id";
            let mut stmt = Statement::prepare(conn, sql)?;
            let mut i = stmt.bind(&terminal_id, 1)?;
            i = stmt.bind(&title, i)?;
            i = stmt.bind(&custom_title, i)?;
            i = stmt.bind(&created_at, i)?;
            i = stmt.bind(&working_directory, i)?;
            i = stmt.bind(&folder_paths, i)?;
            i = stmt.bind(&folder_paths_order, i)?;
            i = stmt.bind(&main_worktree_paths, i)?;
            i = stmt.bind(&main_worktree_paths_order, i)?;
            i = stmt.bind(&remote_connection, i)?;
            i = stmt.bind(&initial_command, i)?;
            stmt.bind(&cc_session_id, i)?;
            stmt.exec()
        })
        .await
    }

    pub async fn delete(&self, terminal_id: TerminalId) -> anyhow::Result<()> {
        let terminal_id = terminal_id.to_key_string();
        self.write(move |conn| {
            let mut stmt = Statement::prepare(
                conn,
                "DELETE FROM sidebar_terminal_threads WHERE terminal_id = ?",
            )?;
            stmt.bind(&terminal_id, 1)?;
            stmt.exec()
        })
        .await
    }
}

impl Column for TerminalThreadMetadata {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (terminal_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (title, next): (String, i32) = Column::column(statement, next)?;
        let (custom_title, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (working_directory, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (main_worktree_paths_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (main_worktree_paths_order_str, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (remote_connection_json, next): (Option<String>, i32) =
            Column::column(statement, next)?;
        let (initial_command, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (cc_session_id, next): (Option<String>, i32) = Column::column(statement, next)?;

        let folder_paths = folder_paths_str
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: folder_paths_order_str.unwrap_or_default(),
                })
            })
            .unwrap_or_default();

        let main_worktree_paths = main_worktree_paths_str
            .map(|paths| {
                PathList::deserialize(&util::path_list::SerializedPathList {
                    paths,
                    order: main_worktree_paths_order_str.unwrap_or_default(),
                })
            })
            .unwrap_or_default();

        let remote_connection = remote_connection_json
            .as_deref()
            .map(serde_json::from_str::<RemoteConnectionOptions>)
            .transpose()
            .context("deserialize terminal thread remote connection")?;

        let worktree_paths = WorktreePaths::from_path_lists(main_worktree_paths, folder_paths)
            .unwrap_or_else(|_| WorktreePaths::default());

        Ok((
            TerminalThreadMetadata {
                terminal_id: TerminalId::from_key_string(&terminal_id)?,
                title: SharedString::from(title),
                custom_title: custom_title
                    .filter(|title| !title.trim().is_empty())
                    .map(SharedString::from),
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                worktree_paths,
                remote_connection,
                working_directory: working_directory.map(PathBuf::from),
                initial_command,
                cc_session_id,
            },
            next,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::path::Path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            TerminalThreadMetadataStore::init_global(cx);
        });
        cx.run_until_parked();
    }

    fn metadata(title: &str, worktree_paths: WorktreePaths) -> TerminalThreadMetadata {
        let now = Utc::now();
        TerminalThreadMetadata {
            terminal_id: TerminalId::new(),
            title: SharedString::from(title.to_string()),
            custom_title: None,
            created_at: now,
            worktree_paths,
            remote_connection: None,
            working_directory: None,
            initial_command: None,
            cc_session_id: None,
        }
    }

    #[test]
    fn test_terminal_title_prefix_preserves_non_alphanumeric_prefixes() {
        assert_eq!(terminal_title_prefix("✳ Thinking"), Some("✳ "));
        assert_eq!(terminal_title_prefix(">>>   Thinking"), Some(">>>   "));
        assert_eq!(terminal_title_prefix("⠋ Running"), Some("⠋ "));
        assert_eq!(terminal_title_prefix("* Claude"), Some("* "));
        assert_eq!(terminal_title_prefix("✳Thinking"), None);
        assert_eq!(terminal_title_prefix("Thinking"), None);
        assert_eq!(terminal_title_prefix(" Thinking"), None);
        assert_eq!(terminal_title_prefix("✳"), None);
        assert_eq!(terminal_title_prefix("v1 Running"), None);
    }

    #[test]
    fn test_claude_activity_reads_the_status_glyph() {
        // The frames Claude Code animates while a turn is in flight, captured
        // from the titles it writes: `◐ Sleep 20 seconds bash`, then `◑ ...`.
        for title in ["◐ Sleep 20 seconds bash", "◑ Sleep 20 seconds bash"] {
            assert_eq!(claude_activity(title), ClaudeActivity::Working, "{title}");
        }
        // The same summary, once the turn is over.
        assert_eq!(
            claude_activity("✳ Sleep 20 seconds bash"),
            ClaudeActivity::Idle
        );
        assert_eq!(claude_activity("✳ Claude Code"), ClaudeActivity::Idle);
        // A plain shell, a build, anything that is not Claude Code.
        assert_eq!(claude_activity("~/src/zed"), ClaudeActivity::Unknown);
        assert_eq!(claude_activity("zsh"), ClaudeActivity::Unknown);
        assert_eq!(claude_activity("⠋ Running"), ClaudeActivity::Unknown);
    }

    #[test]
    fn test_claude_task_summary_drops_the_glyph_and_the_placeholder() {
        assert_eq!(
            claude_task_summary("◐ Prime the availability pool again"),
            Some("Prime the availability pool again")
        );
        assert_eq!(
            claude_task_summary("✳ Prime the availability pool again"),
            Some("Prime the availability pool again")
        );
        // Before the first prompt of a session there is no task to report.
        assert_eq!(claude_task_summary("✳ Claude Code"), None);
        assert_eq!(claude_task_summary("zsh"), None);
    }

    /// The screen Claude Code leaves behind after a turn, as
    /// `last_n_non_empty_lines` reports it: blank lines dropped, soft wraps
    /// already joined. `\u{23fa}` is the message marker, `\u{23bf}` the tool
    /// result hook, and the box drawing is the input prompt below the output.
    fn screen(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    #[test]
    fn test_claude_response_excerpt_reads_the_last_message() {
        let lines = screen(&[
            "\u{23fa} Bash(cargo test -p agent_ui)",
            "  \u{23bf}  running 12 tests",
            "\u{23fa} The twelve tests pass.",
            "  Nothing else needed changing.",
            "\u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}",
            "\u{2502} > \u{2502}",
            "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}",
            "  ? for shortcuts",
        ]);
        assert_eq!(
            claude_response_excerpt(&lines).as_deref(),
            Some("The twelve tests pass. Nothing else needed changing.")
        );
    }

    #[test]
    fn test_claude_response_excerpt_walks_back_over_a_trailing_tool_call() {
        // A turn that ended on a tool call still has prose above it, which is
        // what the user needs to read.
        let lines = screen(&[
            "\u{23fa} I will check the worktree state first.",
            "\u{23fa} Bash(git status --porcelain)",
            "  \u{23bf}  (No content)",
            "\u{2502} > \u{2502}",
        ]);
        assert_eq!(
            claude_response_excerpt(&lines).as_deref(),
            Some("I will check the worktree state first.")
        );
    }

    #[test]
    fn test_claude_response_excerpt_ignores_a_screen_with_no_message() {
        // A prompt waiting for its first instruction, and a plain shell.
        assert_eq!(
            claude_response_excerpt(&screen(&[
                "\u{2502} > \u{2502}",
                "  ? for shortcuts",
                "\u{2733} Claude Code",
            ])),
            None
        );
        assert_eq!(
            claude_response_excerpt(&screen(&["~/src/zed", "zsh"])),
            None
        );
        assert_eq!(claude_response_excerpt(&[]), None);
    }

    #[test]
    fn test_claude_response_excerpt_truncates_on_a_word_boundary() {
        let sentence = "mot ".repeat(CLAUDE_EXCERPT_MAX_CHARS);
        let excerpt = claude_response_excerpt(&screen(&[&format!("\u{23fa} {sentence}")]))
            .expect("the marked line carries prose");
        assert!(excerpt.ends_with("mot\u{2026}"), "{excerpt}");
        assert!(
            excerpt.chars().count() <= CLAUDE_EXCERPT_MAX_CHARS + 1,
            "{} characters",
            excerpt.chars().count()
        );

        // A word longer than the budget has no boundary to cut on, so it is
        // cut at the budget rather than carried whole.
        let long_word = "s".repeat(CLAUDE_EXCERPT_MAX_CHARS + 20);
        let expected = format!("{}\u{2026}", "s".repeat(CLAUDE_EXCERPT_MAX_CHARS));
        assert_eq!(
            claude_response_excerpt(&screen(&[&format!("\u{23fa} {long_word}")])).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn test_is_tool_call_separates_calls_from_prose() {
        assert!(is_tool_call("Bash(cargo test)"));
        assert!(is_tool_call("Read(crates/agent_ui/src/agent_panel.rs)"));
        assert!(!is_tool_call("The build (finally) passes."));
        assert!(!is_tool_call("(No content)"));
        assert!(!is_tool_call(""));
    }

    #[test]
    fn test_terminal_thread_display_title_combines_raw_and_custom_titles() {
        let mut metadata = metadata(
            "⠋ Thinking",
            WorktreePaths::from_folder_paths(&PathList::default()),
        );
        metadata.custom_title = Some("Fix bug".into());
        assert_eq!(metadata.display_title().as_ref(), "⠋ Fix bug");

        metadata.title = "Thinking".into();
        assert_eq!(metadata.display_title().as_ref(), "Fix bug");
    }

    #[gpui::test]
    async fn test_change_worktree_paths_reindexes_terminal_metadata(cx: &mut TestAppContext) {
        init_test(cx);

        let old_main_paths = PathList::new(&[Path::new("/repo")]);
        let old_folder_paths = PathList::new(&[Path::new("/repo-feature")]);
        let new_main_path = Path::new("/repo");
        let new_folder_path = Path::new("/repo-feature-renamed");
        let new_folder_paths = PathList::new(&[new_folder_path]);
        let metadata = metadata(
            "Dev Server",
            WorktreePaths::from_path_lists(old_main_paths.clone(), old_folder_paths.clone())
                .unwrap(),
        );
        let terminal_id = metadata.terminal_id;

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.save(metadata, cx);
            });
        });

        cx.update(|cx| {
            TerminalThreadMetadataStore::global(cx).update(cx, |store, cx| {
                store.change_worktree_paths(
                    &old_folder_paths,
                    None,
                    |paths| {
                        paths.add_path(new_main_path, new_folder_path);
                        paths.remove_folder_path(Path::new("/repo-feature"));
                    },
                    cx,
                );
            });
        });

        cx.update(|cx| {
            let store = TerminalThreadMetadataStore::global(cx);
            let store = store.read(cx);
            assert!(
                store
                    .entries_for_path(&old_folder_paths, None)
                    .next()
                    .is_none()
            );
            assert_eq!(
                store
                    .entries_for_path(&new_folder_paths, None)
                    .map(|entry| entry.terminal_id)
                    .collect::<Vec<_>>(),
                vec![terminal_id]
            );
            assert_eq!(
                store
                    .entry(terminal_id)
                    .unwrap()
                    .main_worktree_paths()
                    .paths(),
                old_main_paths.paths()
            );
        });
    }
}
