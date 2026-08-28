//! Operating-system notifications for `claude` sessions that stopped working
//! and are waiting on the user.
//!
//! A ticket's session runs in a workspace that is usually not the one on
//! screen — often not even in the focused application — so the in-app pop-up
//! the agent panel already shows cannot be seen at the moment it matters. A
//! system notification can, and clicking it brings the session's workspace
//! forward and focuses the session that posted it.

use collections::HashMap;
use gpui::{
    App, Global, SharedString, SystemNotification, SystemNotificationResponse, WeakEntity,
    WindowHandle,
};
use util::ResultExt as _;
use workspace::{MultiWorkspace, Workspace};

use crate::agent_panel::{AgentPanel, TerminalId};

/// Where a posted notification should take the user back to.
///
/// Captured when the notification is posted rather than looked up when it is
/// clicked: the session that finished is the one that knows which window and
/// workspace it belongs to, and by click time the panel may no longer be the
/// active one anywhere.
pub(crate) struct SessionNotificationTarget {
    pub window: WindowHandle<MultiWorkspace>,
    pub workspace: WeakEntity<Workspace>,
    pub terminal_id: TerminalId,
}

#[derive(Default)]
struct SessionNotifications {
    targets: HashMap<SharedString, SessionNotificationTarget>,
}

impl Global for SessionNotifications {}

/// Registers the process-wide handler that routes notification clicks.
///
/// `App::on_system_notification_response` keeps a single callback, so a second
/// caller anywhere in the app would silently displace this one. Everything that
/// posts a system notification therefore goes through this module.
pub(crate) fn init(cx: &mut App) {
    cx.on_system_notification_response(|response: SystemNotificationResponse, cx| {
        // No action buttons are offered, so any activation is "take me there".
        let Some(target) = cx
            .default_global::<SessionNotifications>()
            .targets
            .remove(&response.tag)
        else {
            return;
        };
        activate_session(target, cx);
    });
}

/// Posts (or replaces) the notification for a session that finished a turn.
pub(crate) fn show(
    target: SessionNotificationTarget,
    title: SharedString,
    body: SharedString,
    cx: &mut App,
) {
    let tag = tag_for_terminal(target.terminal_id);
    cx.global_mut::<SessionNotifications>()
        .targets
        .insert(tag.clone(), target);
    cx.show_system_notification(SystemNotification {
        tag,
        title,
        body,
        actions: Vec::new(),
    });
}

/// Retracts a session's notification, so a session the user has already caught
/// up with does not leave a stale entry in the notification center.
pub(crate) fn dismiss(terminal_id: TerminalId, cx: &mut App) {
    let tag = tag_for_terminal(terminal_id);
    if cx
        .default_global::<SessionNotifications>()
        .targets
        .remove(&tag)
        .is_none()
    {
        return;
    }
    cx.dismiss_system_notification(&tag);
}

/// One notification per session: a new end-of-turn replaces the session's
/// previous notification instead of stacking another one behind it.
fn tag_for_terminal(terminal_id: TerminalId) -> SharedString {
    SharedString::from(format!(
        "claude-session-waiting:{}",
        terminal_id.to_key_string()
    ))
}

/// Brings the session forward: its window, then its workspace within that
/// window, then the terminal itself inside the agent panel.
///
/// Callers already inside a window's own update must wrap this in `cx.defer`;
/// it is not deferred here because the notification-response callback is handed
/// an `App` outside of any update, where a deferred effect would sit unflushed
/// until something else happened to run one.
pub(crate) fn activate_session(target: SessionNotificationTarget, cx: &mut App) {
    let SessionNotificationTarget {
        window,
        workspace,
        terminal_id,
    } = target;
    cx.activate(true);
    window
        .update(cx, |multi_workspace, window, cx| {
            window.activate_window();

            let Some(workspace) = workspace.upgrade() else {
                return;
            };
            multi_workspace.activate(workspace.clone(), None, window, cx);

            workspace.update(cx, |workspace, cx| {
                workspace.reveal_panel::<AgentPanel>(window, cx);
                if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                    panel.update(cx, |panel, cx| {
                        panel.activate_terminal(terminal_id, true, window, cx);
                    });
                }
                workspace.focus_panel::<AgentPanel>(window, cx);
            });
        })
        .log_err();
}
