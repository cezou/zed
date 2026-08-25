//! The modal that turns a Notion ticket into a running Claude Code session:
//! pick the repository, name the worktree, review the brief seeded from the
//! Notion page body, attach screenshots, launch.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_ui::ticket_metadata_store::{
    self, GtrWorktree, TicketId, TicketLaunchSpec, TicketMetadataStore, TicketSessionStart,
    ticket_images_dir,
};
use anyhow::Context as _;
use chrono::Utc;
use editor::{Editor, EditorElement, EditorStyle};
use fs::Fs;
use gpui::{
    AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Image, Task,
    TextStyle, WeakEntity, actions, img, px,
};
use notion_client::TicketRef;
use notion_client::mcp::McpClient;
use notion_client::oauth_store;
use notion_client::page_body;
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Chip, ContextMenu, DropdownMenu, DropdownStyle, Render, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::clipboard_images::{self, SavedImage};
use crate::repository_registry::{self, TicketRepository};
use crate::ticket_brief::render_brief;
use crate::ticket_sync_settings::TicketSyncSettings;

actions!(
    tickets,
    [
        /// Launches the Claude Code session configured in the ticket launch
        /// modal. Bound separately from `menu::Confirm` so that Enter keeps
        /// inserting a newline in the brief.
        LaunchTicket,
    ]
);

/// How long a cached Notion page body is trusted before the modal refetches
/// it. The MCP board query leaves `TicketRef::last_edited_time` empty, so
/// there is no edit timestamp to compare against — this TTL plus the explicit
/// refresh button is the whole freshness story.
const BODY_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// The ticket has no worktree yet: cut one or attach an existing one, then
    /// launch in it.
    CreateWorktree,
    /// The ticket already has a worktree: launch one more session in it.
    AdditionalSession,
}

/// Which worktree the ticket's session should run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeChoice {
    /// Cut a new branch and worktree with `git gtr new`.
    New,
    /// Attach the ticket to `worktrees[index]`, which already exists.
    Existing(usize),
}

/// Which `claude` session the launch should start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionChoice {
    /// Write the brief into the worktree and start a fresh session on it.
    New,
    /// Run bare `claude --resume` and let Claude's own picker choose which
    /// existing session to continue. No brief, no prompt.
    Resume,
}

enum BodyState {
    Loading,
    Ready,
    Failed(SharedString),
}

/// The status a launch moves the ticket to unless the user picks another one:
/// the board's "in progress" option, recognized the same way
/// [`ui::ticket_status_color`] does — by substring, since the option strings
/// carry per-board numbering and emoji (`"3 - ⏳ In progress"`).
///
/// `None` when the board has no such option or the ticket is already there,
/// which renders as "Keep as is" and writes nothing.
fn default_target_status(current: &str, cx: &App) -> Option<SharedString> {
    TicketSyncSettings::get_global(cx)
        .notion_status_filter
        .iter()
        .find(|option| {
            option.to_lowercase().contains("progress") && option.as_str() != current
        })
        .map(|option| SharedString::from(option.clone()))
}

enum WorktreesState {
    Loading,
    Ready,
    Failed(SharedString),
}

pub struct TicketLaunchModal {
    ticket: TicketRef,
    ticket_id: TicketId,
    mode: LaunchMode,
    /// The status the ticket moves to once the session is launched, or `None`
    /// to leave it where it is. Defaults to the board's "in progress" option.
    target_status: Option<SharedString>,
    /// Kept so a successful launch can write [`Self::target_status`] to Notion
    /// through the workspace's action handler.
    workspace: WeakEntity<Workspace>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    repositories: Vec<TicketRepository>,
    selected_repository: Option<usize>,
    worktrees: Vec<GtrWorktree>,
    worktrees_state: WorktreesState,
    worktree_choice: WorktreeChoice,
    session_choice: SessionChoice,
    branch_editor: Entity<Editor>,
    brief_editor: Entity<Editor>,
    body_state: BodyState,
    attachments: Vec<SavedImage>,
    next_image_index: usize,
    error: Option<SharedString>,
    launching: bool,
    _body_task: Task<()>,
    _attachment_task: Task<()>,
    _worktrees_task: Task<()>,
}

impl EventEmitter<DismissEvent> for TicketLaunchModal {}
impl ModalView for TicketLaunchModal {}

impl Focusable for TicketLaunchModal {
    /// Focus lands on an editor rather than the modal container so the
    /// `TicketLaunch > Editor` keymap contexts apply; the container is still
    /// the ancestor every action bubbles through.
    ///
    /// Resuming is the exception: it renders neither editor, and focusing a
    /// handle whose element is not in the tree would leave the modal unable to
    /// dispatch its own actions, so focus goes to the container — whose
    /// `TicketLaunch` context carries the same bindings.
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.resuming() {
            return self.focus_handle.clone();
        }
        match self.mode {
            LaunchMode::CreateWorktree if self.worktree_choice == WorktreeChoice::New => {
                self.branch_editor.focus_handle(cx)
            }
            LaunchMode::CreateWorktree | LaunchMode::AdditionalSession => {
                self.brief_editor.focus_handle(cx)
            }
        }
    }
}

impl TicketLaunchModal {
    /// Takes `&mut Workspace` rather than its handle because every call site
    /// is already inside a workspace update (they all run from `cx.defer_in`,
    /// whose closure is handed the borrow). Re-entering through
    /// `Entity::update` here would double-lease the entity and abort the
    /// process.
    pub fn show(
        workspace: &mut Workspace,
        ticket: TicketRef,
        mode: LaunchMode,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let workspace_handle = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            Self::new(ticket, mode, workspace_handle, fs, window, cx)
        });
    }

    fn new(
        ticket: TicketRef,
        mode: LaunchMode,
        workspace: WeakEntity<Workspace>,
        fs: Arc<dyn Fs>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ticket_id = TicketId::new(ticket.page_id.clone());
        let target_status = default_target_status(&ticket.status, cx);
        let repositories = repository_registry::registered_repositories(cx);
        let selected_repository = (!repositories.is_empty()).then_some(0);

        let branch_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("branch/directory name", window, cx);
            editor.set_text(ticket.slug.clone(), window, cx);
            editor
        });
        let brief_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(8, 24, window, cx);
            editor.set_use_autoclose(false);
            editor.set_show_gutter(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_placeholder_text("Describe the mission for this ticket", window, cx);
            editor
        });

        let mut this = Self {
            ticket,
            ticket_id,
            mode,
            target_status,
            workspace,
            fs,
            focus_handle: cx.focus_handle(),
            repositories,
            selected_repository,
            worktrees: Vec::new(),
            worktrees_state: WorktreesState::Ready,
            worktree_choice: WorktreeChoice::New,
            session_choice: SessionChoice::New,
            branch_editor,
            brief_editor,
            body_state: BodyState::Loading,
            attachments: Vec::new(),
            next_image_index: 1,
            error: None,
            launching: false,
            _body_task: Task::ready(()),
            _attachment_task: Task::ready(()),
            _worktrees_task: Task::ready(()),
        };
        this.load_body(false, window, cx);
        this.load_attachment_index(cx);
        if mode == LaunchMode::CreateWorktree {
            this.load_worktrees(cx);
        }
        this
    }

    /// Lists the selected repository's worktrees so the ticket can be attached
    /// to one instead of cutting a new one.
    ///
    /// Every reload resets the choice back to `New`: a `WorktreeChoice::Existing`
    /// index only means something for the listing it was picked from.
    fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        self.worktrees = Vec::new();
        self.worktree_choice = WorktreeChoice::New;
        self.session_choice = SessionChoice::New;

        let Some(repository) = self.selected_repository().cloned() else {
            self.worktrees_state = WorktreesState::Ready;
            cx.notify();
            return;
        };

        self.worktrees_state = WorktreesState::Loading;
        cx.notify();

        self._worktrees_task = cx.spawn(async move |this, cx| {
            let listed = ticket_metadata_store::existing_worktrees(&repository.path).await;
            this.update(cx, |this, cx| {
                match listed {
                    Ok(worktrees) => {
                        this.worktrees = worktrees;
                        this.worktrees_state = WorktreesState::Ready;
                    }
                    Err(error) => {
                        this.worktrees_state = WorktreesState::Failed(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn selected_worktree(&self) -> Option<&GtrWorktree> {
        match self.worktree_choice {
            WorktreeChoice::New => None,
            WorktreeChoice::Existing(index) => self.worktrees.get(index),
        }
    }

    /// Resuming only makes sense on a worktree that already exists: one about
    /// to be cut has no Claude history to pick from.
    fn can_resume(&self) -> bool {
        match self.mode {
            LaunchMode::CreateWorktree => self.selected_worktree().is_some(),
            LaunchMode::AdditionalSession => true,
        }
    }

    fn resuming(&self) -> bool {
        self.can_resume() && self.session_choice == SessionChoice::Resume
    }

    fn launch_spec(&self, cx: &App) -> TicketLaunchSpec {
        TicketLaunchSpec {
            brief_markdown: self.brief_editor.read(cx).text(cx),
            attachments: self
                .attachments
                .iter()
                .map(|attachment| attachment.path.clone())
                .collect(),
        }
    }

    fn session_start(&self, cx: &App) -> TicketSessionStart {
        if self.resuming() {
            TicketSessionStart::ResumePicker
        } else {
            TicketSessionStart::Brief(self.launch_spec(cx))
        }
    }

    /// Seeds `next_image_index` from what is already on disk, so reopening the
    /// modal for a ticket appends attachments rather than clobbering them.
    fn load_attachment_index(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let images_dir = ticket_images_dir(&self.ticket_id);
        self._attachment_task = cx.spawn(async move |this, cx| {
            let index = clipboard_images::next_image_index(&fs, &images_dir).await;
            this.update(cx, |this, cx| {
                this.next_image_index = index;
                cx.notify();
            })
            .log_err();
        });
    }

    fn cached_body(&self, cx: &App) -> Option<String> {
        let store = TicketMetadataStore::try_global(cx)?;
        let entry = store.read(cx).entry(&self.ticket_id)?;
        let fetched_at = entry.body_fetched_at?;
        let age = Utc::now().signed_duration_since(fetched_at).to_std().ok()?;
        (age < BODY_CACHE_TTL).then(|| entry.body_markdown.clone())?
    }

    fn load_body(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !force && let Some(body) = self.cached_body(cx) {
            self.set_brief(Some(&body), window, cx);
            self.body_state = BodyState::Ready;
            cx.notify();
            return;
        }

        self.body_state = BodyState::Loading;
        cx.notify();

        let http_client = cx.http_client();
        let page_uuid = self.ticket.notion_page_uuid();
        self._body_task = cx.spawn_in(window, async move |this, cx| {
            let result = async {
                let tokens = cx
                    .update(|_window, cx| oauth_store::load_tokens(cx))?
                    .await
                    .context(
                        "not connected to Notion — run `notion: Connect to Notion` to fetch \
                         ticket bodies",
                    )?;
                let mut client = McpClient::new(http_client, tokens);
                client.initialize().await?;
                let body = page_body::fetch_page_body(&mut client, &page_uuid).await?;
                anyhow::Ok(body.markdown)
            }
            .await;

            this.update_in(cx, |this, window, cx| match result {
                Ok(markdown) => {
                    if let Some(store) = TicketMetadataStore::try_global(cx) {
                        store
                            .update(cx, |store, cx| {
                                store.save_body(&this.ticket_id, markdown.clone(), cx)
                            })
                            .log_err();
                    }
                    this.set_brief(Some(&markdown), window, cx);
                    this.body_state = BodyState::Ready;
                    cx.notify();
                }
                Err(error) => {
                    this.set_brief(None, window, cx);
                    this.body_state = BodyState::Failed(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .log_err();
        });
    }

    fn set_brief(&mut self, body: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let brief = render_brief(&self.ticket, body);
        self.brief_editor.update(cx, |editor, cx| {
            editor.set_text(brief, window, cx);
        });
    }

    fn selected_repository(&self) -> Option<&TicketRepository> {
        self.repositories.get(self.selected_repository?)
    }

    fn add_repository(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            let added = cx
                .update(|cx| repository_registry::add_repository(fs, cx))
                .await;
            this.update(cx, |this, cx| {
                match added {
                    Ok(Some(repository)) => {
                        this.repositories = repository_registry::registered_repositories(cx);
                        this.selected_repository = this
                            .repositories
                            .iter()
                            .position(|candidate| candidate.path == repository.path);
                        this.error = None;
                        this.load_worktrees(cx);
                    }
                    // The user dismissed the directory picker.
                    Ok(None) => {}
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn paste(&mut self, _: &editor::actions::Paste, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(images) = clipboard_images::read_clipboard_images(cx) else {
            return;
        };
        // Only swallow the paste once an image is actually in hand, so pasting
        // text into the brief keeps working.
        cx.stop_propagation();
        self.save_attachments(images, cx);
    }

    fn save_attachments(&mut self, images: Vec<Image>, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let images_dir = ticket_images_dir(&self.ticket_id);
        let start_index = self.next_image_index;
        self.next_image_index += images.len();
        self.error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_spawn(clipboard_images::save_images(
                    fs,
                    images_dir,
                    start_index,
                    images,
                ))
                .await;
            this.update(cx, |this, cx| {
                match saved {
                    Ok(saved) => this.attachments.extend(saved),
                    Err(error) => this.error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn remove_attachment(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.attachments
            .retain(|attachment| attachment.path != path);
        cx.notify();

        let fs = self.fs.clone();
        cx.background_spawn(async move {
            fs.remove_file(&path, fs::RemoveOptions::default())
                .await
                .log_err();
        })
        .detach();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn launch(&mut self, _: &LaunchTicket, _window: &mut Window, cx: &mut Context<Self>) {
        if self.launching {
            return;
        }
        let fs = self.fs.clone();
        let ticket_id = self.ticket_id.clone();

        let Some(app_state) = workspace::AppState::try_global(cx) else {
            self.error = Some("the workspace is not initialized yet".into());
            cx.notify();
            return;
        };

        let task = match self.mode {
            LaunchMode::CreateWorktree => {
                let Some(repository) = self.selected_repository().cloned() else {
                    self.error = Some("Add a repository to cut this ticket's worktree from".into());
                    cx.notify();
                    return;
                };
                match self.worktree_choice {
                    WorktreeChoice::New => {
                        let branch_name = self.branch_editor.read(cx).text(cx).trim().to_string();
                        if branch_name.is_empty() {
                            self.error = Some("Give the worktree a branch name first".into());
                            cx.notify();
                            return;
                        }
                        let spec = self.launch_spec(cx);
                        repository_registry::mark_used(&repository.path, cx).detach_and_log_err(cx);
                        cx.spawn(async move |_this, cx| {
                            ticket_metadata_store::create_worktree_and_launch(
                                ticket_id,
                                repository.path,
                                branch_name,
                                spec,
                                fs,
                                app_state,
                                cx,
                            )
                            .await
                        })
                    }
                    WorktreeChoice::Existing(index) => {
                        let Some(worktree) = self.worktrees.get(index).cloned() else {
                            self.error = Some("Pick the worktree to attach this ticket to".into());
                            cx.notify();
                            return;
                        };
                        let start = self.session_start(cx);
                        repository_registry::mark_used(&repository.path, cx).detach_and_log_err(cx);
                        cx.spawn(async move |_this, cx| {
                            ticket_metadata_store::attach_worktree_and_launch(
                                ticket_id,
                                repository.path,
                                worktree.path,
                                worktree.branch,
                                start,
                                fs,
                                app_state,
                                cx,
                            )
                            .await
                        })
                    }
                }
            }
            LaunchMode::AdditionalSession => {
                let start = self.session_start(cx);
                cx.spawn(async move |_this, cx| {
                    ticket_metadata_store::launch_additional_session(
                        ticket_id, start, fs, app_state, cx,
                    )
                    .await
                })
            }
        };

        self.launching = true;
        self.error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.launching = false;
                match result {
                    // Only once the session is actually running: a ticket
                    // whose worktree failed to be cut has not been started.
                    Ok(()) => {
                        this.apply_target_status(cx);
                        cx.emit(DismissEvent)
                    }
                    Err(error) => {
                        this.error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn brief_editor_style(cx: &App) -> EditorStyle {
        // `git_ui::git_panel::git_commit_editor_style` is the model here; it is
        // `pub(crate)` to `git_ui`, so it is duplicated rather than having a
        // foreign crate's visibility widened for one caller.
        let settings = ThemeSettings::get_global(cx);
        let font_size = settings.buffer_font_size(cx);
        EditorStyle {
            background: cx.theme().colors().editor_background,
            local_player: cx.theme().players().local(),
            text: TextStyle {
                color: cx.theme().colors().text,
                font_family: settings.buffer_font.family.clone(),
                font_fallbacks: settings.buffer_font.fallbacks.clone(),
                font_features: settings.buffer_font.features.clone(),
                font_size: font_size.into(),
                font_weight: settings.buffer_font.weight,
                line_height: (font_size * settings.buffer_line_height.value()).into(),
                ..Default::default()
            },
            syntax: cx.theme().syntax().clone(),
            ..Default::default()
        }
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let refreshing = matches!(self.body_state, BodyState::Loading);
        let status_transition = (!self.ticket.status.is_empty())
            .then(|| self.render_status_transition(window, cx))
            .map(IntoElement::into_any_element);
        h_flex()
            .w_full()
            .gap_2()
            .justify_between()
            .child(
                v_flex()
                    .gap_1()
                    .child(Headline::new(self.ticket.title.clone()).size(HeadlineSize::XSmall))
                    .child(
                        h_flex()
                            .gap_1()
                            .when_some(
                                self.ticket
                                    .issue_id
                                    .clone()
                                    .filter(|issue_id| !issue_id.is_empty()),
                                |this, issue_id| {
                                    this.child(Chip::new(issue_id).label_color(Color::Muted))
                                },
                            )
                            .when_some(status_transition, |this, transition| {
                                this.child(transition)
                            }),
                    ),
            )
            .child(
                IconButton::new("refresh-from-notion", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .disabled(refreshing)
                    .tooltip(Tooltip::text("Refresh from Notion"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.load_body(true, window, cx);
                    })),
            )
    }

    /// Writes the picked target status to Notion, through the workspace's
    /// action handler so a failure surfaces as a notification — the modal is
    /// already dismissing and has nowhere left to show one.
    fn apply_target_status(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.target_status.clone() else {
            return;
        };
        if status == self.ticket.status {
            return;
        }
        let action = agent_ui::SetTicketStatus {
            ticket_id: self.ticket_id.0.to_string(),
            status: status.to_string(),
        };
        self.workspace
            .update(cx, |workspace, cx| {
                crate::set_ticket_status(workspace, &action, cx);
            })
            .ok();
    }

    /// The ticket's current status, and the one launching will move it to.
    ///
    /// Rendered as `before → after` with the second half a picker, so the
    /// automatic move to "in progress" is visible up front and overridable
    /// (including to "Keep as is") without leaving the modal.
    fn render_status_transition(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let options = TicketSyncSettings::get_global(cx).notion_status_filter.clone();
        let current = self.ticket.status.clone();
        let this = cx.entity().downgrade();
        let target_label = self
            .target_status
            .clone()
            .unwrap_or_else(|| SharedString::from("Keep as is"));

        h_flex()
            .gap_1()
            .child(Chip::new(current.clone()).label_color(Color::Accent))
            .child(
                Icon::new(IconName::ArrowRight)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                DropdownMenu::new(
                    "ticket-launch-target-status",
                    target_label,
                    ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                        {
                            let this = this.clone();
                            menu = menu.entry("Keep as is", None, move |_window, cx| {
                                this.update(cx, |this, cx| {
                                    this.target_status = None;
                                    cx.notify();
                                })
                                .ok();
                            });
                        }
                        menu = menu.separator();
                        for option in &options {
                            if *option == current {
                                continue;
                            }
                            let this = this.clone();
                            let option = SharedString::from(option.clone());
                            menu = menu.entry(option.clone(), None, move |_window, cx| {
                                this.update(cx, |this, cx| {
                                    this.target_status = Some(option.clone());
                                    cx.notify();
                                })
                                .ok();
                            });
                        }
                        menu
                    }),
                )
                .style(DropdownStyle::Ghost)
                .trigger_size(ButtonSize::Compact),
            )
    }

    fn render_repository_picker(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = self
            .selected_repository()
            .map(|repository| SharedString::from(repository.name.clone()))
            .unwrap_or_else(|| SharedString::from("Select a repository…"));
        let repositories = self.repositories.clone();
        let this = cx.entity().downgrade();

        DropdownMenu::new(
            "ticket-launch-repository",
            label,
            ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                for (index, repository) in repositories.iter().enumerate() {
                    let this = this.clone();
                    menu = menu.entry(repository.name.clone(), None, move |_window, cx| {
                        this.update(cx, |this, cx| {
                            this.selected_repository = Some(index);
                            this.load_worktrees(cx);
                        })
                        .ok();
                    });
                }
                menu = menu.separator();
                menu.entry("Add a repository…", None, move |_window, cx| {
                    this.update(cx, |this, cx| this.add_repository(cx)).ok();
                })
            }),
        )
        .style(DropdownStyle::Outlined)
        .full_width(true)
    }

    fn render_worktree_picker(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = self
            .selected_worktree()
            .map(|worktree| SharedString::from(worktree.branch.clone()))
            .unwrap_or_else(|| SharedString::from("New worktree"));
        let worktrees = self.worktrees.clone();
        let this = cx.entity().downgrade();

        DropdownMenu::new(
            "ticket-launch-worktree",
            label,
            ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
                {
                    let this = this.clone();
                    menu = menu.entry("New worktree", None, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.worktree_choice = WorktreeChoice::New;
                            this.refocus(window, cx);
                        })
                        .ok();
                    });
                }
                if !worktrees.is_empty() {
                    menu = menu.separator();
                }
                for (index, worktree) in worktrees.iter().enumerate() {
                    let this = this.clone();
                    menu = menu.entry(worktree.branch.clone(), None, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.worktree_choice = WorktreeChoice::Existing(index);
                            this.refocus(window, cx);
                        })
                        .ok();
                    });
                }
                menu
            }),
        )
        .style(DropdownStyle::Outlined)
        .full_width(true)
    }

    /// The path of the attached worktree, or why the listing is unavailable —
    /// the dropdown only ever shows branch names.
    fn render_worktree_status(&self, _cx: &Context<Self>) -> Option<AnyElement> {
        match &self.worktrees_state {
            WorktreesState::Loading => Some(
                Label::new("Listing the repository's worktrees…")
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            WorktreesState::Failed(message) => Some(
                Label::new(format!("Couldn't list worktrees: {message}"))
                    .size(LabelSize::Small)
                    .color(Color::Warning)
                    .into_any_element(),
            ),
            WorktreesState::Ready => self.selected_worktree().map(|worktree| {
                Label::new(worktree.path.to_string_lossy().to_string())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            }),
        }
    }

    fn render_session_picker(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let label = match self.session_choice {
            SessionChoice::New => "Start a new session",
            SessionChoice::Resume => "Resume an existing session",
        };
        let this = cx.entity().downgrade();

        DropdownMenu::new(
            "ticket-launch-session",
            SharedString::from(label),
            ContextMenu::build(window, cx, move |menu, _window, _cx| {
                let choices = [
                    ("Start a new session", SessionChoice::New),
                    ("Resume an existing session", SessionChoice::Resume),
                ];
                choices.into_iter().fold(menu, |menu, (label, choice)| {
                    let this = this.clone();
                    menu.entry(label, None, move |window, cx| {
                        this.update(cx, |this, cx| {
                            this.session_choice = choice;
                            this.refocus(window, cx);
                        })
                        .ok();
                    })
                })
            }),
        )
        .style(DropdownStyle::Outlined)
        .full_width(true)
    }

    /// Moves focus to whatever the new choice renders, since switching to or
    /// away from a resume unmounts the editor that had it.
    fn refocus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn render_brief(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let banner = match &self.body_state {
            BodyState::Loading => Some(
                h_flex()
                    .gap_1()
                    .child(Label::new("Fetching the Notion page…").size(LabelSize::Small))
                    .into_any_element(),
            ),
            BodyState::Ready => None,
            BodyState::Failed(message) => Some(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(
                        Label::new(format!("Couldn't fetch the Notion page: {message}"))
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Button::new("retry-notion-fetch", "Retry").on_click(cx.listener(
                            |this, _, window, cx| {
                                this.load_body(true, window, cx);
                            },
                        )),
                    )
                    .into_any_element(),
            ),
        };

        v_flex().w_full().gap_1().children(banner).child(
            div()
                .w_full()
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().editor_background)
                .child(EditorElement::new(
                    &self.brief_editor,
                    Self::brief_editor_style(cx),
                )),
        )
    }

    fn render_attachments(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.attachments.is_empty() {
            return None;
        }
        let mut strip = h_flex().w_full().gap_2().flex_wrap();
        for attachment in &self.attachments {
            let path = attachment.path.clone();
            strip = strip.child(
                div()
                    .relative()
                    .child(
                        img(path.clone())
                            .h_16()
                            .w_16()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border_variant),
                    )
                    .child(
                        div().absolute().top_0().right_0().child(
                            IconButton::new(
                                SharedString::from(format!("remove-{}", attachment.file_name)),
                                IconName::Close,
                            )
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Remove attachment"))
                            .on_click(cx.listener(
                                move |this, _, _window, cx| {
                                    this.remove_attachment(path.clone(), cx);
                                },
                            )),
                        ),
                    ),
            );
        }
        Some(strip)
    }

    /// Attachments whose file name would end an `@` mention early. Naming is
    /// under our control (`img-<n>.<ext>`), so this should always be empty —
    /// but a mention that swallows only half a path fails silently on Claude's
    /// side, so the case is surfaced rather than assumed away.
    fn unmentionable_attachments(&self) -> Vec<SharedString> {
        self.attachments
            .iter()
            .filter(|attachment| attachment.file_name.contains(char::is_whitespace))
            .map(|attachment| SharedString::from(attachment.file_name.clone()))
            .collect()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let launch_label = match (self.launching, self.resuming()) {
            (true, _) => "Launching…",
            (false, true) => "Resume",
            (false, false) => "Launch",
        };
        h_flex()
            .w_full()
            .gap_2()
            .justify_between()
            .child(div().flex_1().when_some(self.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            }))
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("cancel", "Cancel").on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&menu::Cancel, window, cx)),
                    ))
                    .child(
                        Button::new("launch", launch_label)
                            .disabled(self.launching)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.launch(&LaunchTicket, window, cx)
                            })),
                    ),
            )
    }
}

impl Render for TicketLaunchModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let creating_worktree = self.mode == LaunchMode::CreateWorktree;
        let cutting_a_branch = creating_worktree && self.worktree_choice == WorktreeChoice::New;
        let resuming = self.resuming();
        v_flex()
            .id("ticket-launch-modal")
            .key_context("TicketLaunch")
            .track_focus(&self.focus_handle)
            .capture_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::launch))
            .elevation_3(cx)
            .w(px(520.))
            .p(DynamicSpacing::Base12.rems(cx))
            .gap_3()
            .child(self.render_header(window, cx))
            .when(creating_worktree, |this| {
                this.child(self.render_repository_picker(window, cx))
                    .child(self.render_worktree_picker(window, cx))
                    .children(self.render_worktree_status(cx))
            })
            .when(cutting_a_branch, |this| {
                this.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(self.branch_editor.clone()),
                )
            })
            .when(self.can_resume(), |this| {
                this.child(self.render_session_picker(window, cx))
            })
            .when(resuming, |this| {
                this.child(
                    Label::new(
                        "Claude will show its own session picker in the worktree — no brief is \
                         sent.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .when(!resuming, |this| {
                this.child(self.render_brief(cx))
                    .children(self.render_attachments(cx))
                    .children(self.unmentionable_attachments().into_iter().map(|name| {
                        Label::new(format!(
                            "{name} has a space in its name; Claude will be told to open it as a \
                             file."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Warning)
                    }))
            })
            .child(self.render_footer(cx))
    }
}
