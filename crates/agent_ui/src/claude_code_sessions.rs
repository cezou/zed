//! Local Claude Code CLI session discovery.
//!
//! Claude Code (the standalone CLI at `claude`) records each conversation as a
//! JSONL file under `~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl`.
//! The "encoded cwd" is the absolute path of the working directory with `/`
//! replaced by `-` (so `/home/u/foo` becomes `-home-u-foo`).
//!
//! This module surfaces those sessions to the Agent panel sidebar without
//! going through ACP: clicking a session opens a terminal that runs
//! `claude -r <session-id>` in the recorded cwd, so users get the full
//! native CLI (slash commands, MCP, skills, agents, hooks) instead of the
//! more limited ACP subset.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use gpui::SharedString;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// One Claude Code CLI conversation as displayed in the sidebar.
#[derive(Clone, Debug)]
pub struct ClaudeCodeSession {
    pub session_id: SharedString,
    pub cwd: PathBuf,
    /// First user prompt of the session, truncated for display.
    pub title: SharedString,
    /// Most recent user prompt, used as the secondary display line.
    pub last_user_prompt: Option<SharedString>,
    /// Timestamp of the last appended message (any type), derived from file
    /// mtime — Claude Code appends synchronously so mtime is a reliable
    /// activity signal.
    pub last_activity: DateTime<Utc>,
    /// Status derived from the final non-attachment message type plus mtime.
    pub status: SessionStatus,
    /// Git branch recorded in the session messages, if any.
    pub git_branch: Option<SharedString>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    /// Last message is from the user (Claude owes a reply) and the file was
    /// touched recently — the CLI is most likely actively responding.
    Running,
    /// Last message is from the assistant and the file is quiescent — the
    /// turn finished and the CLI is waiting for the next user prompt.
    Idle,
    /// The session ended (last message is a terminator type) or no
    /// meaningful messages were found.
    Closed,
}

#[derive(Deserialize)]
struct JsonlLine {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "gitBranch")]
    git_branch: Option<String>,
    #[serde(default)]
    message: Option<serde_json::Value>,
}

/// How recent the file must have been touched (in seconds) for an open
/// session ending in a `user` message to be classified as Running rather
/// than Closed. Chosen empirically — Claude appends an assistant chunk
/// every few seconds during generation, so >30s of silence after a user
/// prompt almost always means the CLI was killed mid-turn.
const RUNNING_RECENCY_SECS: u64 = 30;

/// Conservative cap on how many tail messages to scan when looking for the
/// last user prompt. Keeps the parse bounded for very long sessions.
const TAIL_SCAN_BYTES: u64 = 64 * 1024;

/// Returns the default `~/.claude/projects` root, or `None` if `$HOME` is
/// unset (which shouldn't happen on a real user system but we don't want
/// to panic in tests).
pub fn default_projects_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude").join("projects"))
}

/// Decodes the directory-name encoding used by Claude Code:
/// `-home-foo-bar` → `/home/foo/bar`. Returns `None` if the name doesn't
/// look like an encoded absolute path (must start with `-`).
pub fn decode_project_dir_name(name: &str) -> Option<PathBuf> {
    if !name.starts_with('-') {
        return None;
    }
    Some(PathBuf::from(name.replace('-', "/")))
}

/// Scans the given Claude Code projects root and returns every session it
/// can parse. Sessions whose JSONL is malformed or empty are silently
/// skipped — this is a best-effort UI listing, not a parser of record.
pub fn scan_all(projects_root: &Path) -> Result<Vec<ClaudeCodeSession>> {
    let mut sessions = Vec::new();
    let entries = fs::read_dir(projects_root)
        .with_context(|| format!("reading {}", projects_root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let fallback_cwd = decode_project_dir_name(&dir_name);
        let session_files = match fs::read_dir(&path) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for file in session_files.flatten() {
            let session_path = file.path();
            if session_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let session_id = match session_path.file_stem().and_then(|s| s.to_str()) {
                Some(id) => SharedString::from(id.to_string()),
                None => continue,
            };
            match parse_session(&session_path, session_id, fallback_cwd.clone()) {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(_) => {}
            }
        }
    }
    Ok(sessions)
}

/// Reads the head (for title/cwd) and a bounded tail (for last activity and
/// status) of a session JSONL, returning a `ClaudeCodeSession` if at least
/// one parseable message was found.
fn parse_session(
    path: &Path,
    session_id: SharedString,
    fallback_cwd: Option<PathBuf>,
) -> Result<Option<ClaudeCodeSession>> {
    let metadata = fs::metadata(path)?;
    let last_activity = DateTime::<Utc>::from(metadata.modified()?);
    let file_size = metadata.len();

    let mut head = fs::File::open(path)?;
    let mut head_reader = BufReader::new(&mut head);

    let mut title: Option<SharedString> = None;
    let mut cwd: Option<PathBuf> = fallback_cwd;
    let mut git_branch: Option<SharedString> = None;

    let mut line = String::new();
    // Scan up to ~50 head lines for a user message + cwd/git_branch.
    for _ in 0..50 {
        line.clear();
        if head_reader.read_line(&mut line)? == 0 {
            break;
        }
        let parsed: JsonlLine = match serde_json::from_str(line.trim()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if cwd.is_none()
            && let Some(c) = parsed.cwd.as_deref()
        {
            cwd = Some(PathBuf::from(c));
        }
        if git_branch.is_none()
            && let Some(b) = parsed.git_branch.as_deref()
        {
            git_branch = Some(SharedString::from(b.to_string()));
        }
        if title.is_none()
            && parsed.r#type.as_deref() == Some("user")
            && let Some(text) = extract_user_text(&parsed.message)
        {
            title = Some(truncate_for_title(&text).into());
        }
        if title.is_some() && cwd.is_some() && git_branch.is_some() {
            break;
        }
    }

    let cwd = match cwd {
        Some(c) => c,
        None => return Ok(None),
    };

    let (last_user_prompt, last_type) = scan_tail(path, file_size)?;
    let status = classify_status(last_type.as_deref(), last_activity);
    let title = title.unwrap_or_else(|| SharedString::from(session_id.clone()));

    Ok(Some(ClaudeCodeSession {
        session_id,
        cwd,
        title,
        last_user_prompt,
        last_activity,
        status,
        git_branch,
    }))
}

/// Reads the final `TAIL_SCAN_BYTES` of the file, finds the last `user` and
/// the last non-attachment message type within that window.
fn scan_tail(path: &Path, file_size: u64) -> Result<(Option<SharedString>, Option<String>)> {
    let start = file_size.saturating_sub(TAIL_SCAN_BYTES);
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let reader = BufReader::new(file);
    let mut last_user_prompt: Option<SharedString> = None;
    let mut last_type: Option<String> = None;
    let mut first = true;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        // If we started mid-line (non-zero offset), the first read may be a
        // partial record — skip it.
        if first && start != 0 {
            first = false;
            continue;
        }
        first = false;
        let parsed: JsonlLine = match serde_json::from_str(line.trim()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        match parsed.r#type.as_deref() {
            Some("user") => {
                if let Some(text) = extract_user_text(&parsed.message) {
                    last_user_prompt = Some(truncate_for_title(&text).into());
                }
                last_type = Some("user".to_string());
            }
            Some("assistant") => {
                last_type = Some("assistant".to_string());
            }
            Some(other) if other != "attachment" && other != "queue-operation" => {
                last_type = Some(other.to_string());
            }
            _ => {}
        }
    }
    Ok((last_user_prompt, last_type))
}

fn classify_status(last_type: Option<&str>, last_activity: DateTime<Utc>) -> SessionStatus {
    let age = SystemTime::now()
        .duration_since(SystemTime::from(last_activity))
        .unwrap_or(Duration::ZERO);
    match last_type {
        Some("user") if age.as_secs() < RUNNING_RECENCY_SECS => SessionStatus::Running,
        Some("user") => SessionStatus::Closed,
        Some("assistant") => SessionStatus::Idle,
        _ => SessionStatus::Closed,
    }
}

/// Extracts a human-readable text snippet from a user message value. The
/// `content` field can be either a string or a list of typed blocks; we
/// concatenate every `text` block we find.
fn extract_user_text(message: &Option<serde_json::Value>) -> Option<String> {
    let msg = message.as_ref()?;
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(blocks) = content.as_array() {
        let mut out = String::new();
        for block in blocks {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(text);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

fn truncate_for_title(text: &str) -> String {
    const MAX: usize = 80;
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX).collect();
    out.push('…');
    out
}

/// Groups sessions by cwd so the sidebar can place them under the matching
/// project/worktree header without re-walking the input list.
pub fn group_by_cwd(
    sessions: impl IntoIterator<Item = ClaudeCodeSession>,
) -> HashMap<PathBuf, Vec<ClaudeCodeSession>> {
    let mut map: HashMap<PathBuf, Vec<ClaudeCodeSession>> = HashMap::new();
    for session in sessions {
        map.entry(session.cwd.clone()).or_default().push(session);
    }
    for sessions in map.values_mut() {
        sessions.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_typical_project_dir_name() {
        assert_eq!(
            decode_project_dir_name("-home-cviegas-Documents-Perso-PredictGodAI"),
            Some(PathBuf::from("/home/cviegas/Documents/Perso/PredictGodAI"))
        );
    }

    #[test]
    fn rejects_non_encoded_names() {
        assert!(decode_project_dir_name("not-encoded").is_none());
        assert!(decode_project_dir_name("/home/foo").is_none());
        assert!(decode_project_dir_name("").is_none());
    }

    #[test]
    fn truncates_long_titles() {
        let s = "a".repeat(200);
        let t = truncate_for_title(&s);
        assert!(t.chars().count() <= 81);
        assert!(t.ends_with('…'));
    }
}
