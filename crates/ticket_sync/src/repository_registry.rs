//! The set of repositories a ticket worktree can be cut from, plus their
//! most-recently-used ordering.
//!
//! The list itself lives in `settings.json` (`tickets_panel.repositories`)
//! because it is something a user edits by hand. The MRU ordering
//! deliberately does *not*: it changes on every single launch, and writing it
//! to `settings.json` would reformat the user's file each time. It goes into
//! `db::kvp::KeyValueStore` instead, mirroring `LAST_USED_AGENT_KEY` in
//! `agent_ui::agent_panel`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{App, AppContext as _, PathPromptOptions, Task};
use settings::{Settings as _, TicketRepositoryContent};
use util::ResultExt as _;

use crate::TicketSyncSettings;

const REPOSITORY_MRU_KEY: &str = "tickets_panel__repository_mru";

/// A repository the launch modal can cut a worktree from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketRepository {
    pub path: PathBuf,
    pub name: String,
}

impl TicketRepository {
    fn from_content(content: &TicketRepositoryContent) -> Self {
        let path = PathBuf::from(&content.path);
        let name = content
            .name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| display_name_for(&path));
        Self { path, name }
    }
}

fn display_name_for(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

/// A path is only meaningful as a map key once it is normalized: settings may
/// spell the same repository with mixed separators or a trailing one.
fn mru_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

fn read_mru(kvp: &KeyValueStore) -> HashMap<String, u64> {
    kvp.read_kvp(REPOSITORY_MRU_KEY)
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str::<HashMap<String, u64>>(&json).log_err())
        .unwrap_or_default()
}

/// Every registered repository, most recently used first. Repositories never
/// launched yet keep their `settings.json` order, after the used ones.
///
/// The legacy single `tickets_panel.repo_path` is folded in here rather than
/// migrated on disk, so a user who has not yet edited their settings still
/// sees their repository in the dropdown.
pub fn registered_repositories(cx: &App) -> Vec<TicketRepository> {
    let settings = TicketSyncSettings::get_global(cx);
    let mut repositories: Vec<TicketRepository> = settings
        .repositories
        .iter()
        .filter(|content| !content.path.trim().is_empty())
        .map(TicketRepository::from_content)
        .collect();

    if let Some(legacy_path) = settings
        .repo_path
        .as_ref()
        .map(|path| PathBuf::from(path.trim()))
        .filter(|path| !path.as_os_str().is_empty())
    {
        let legacy_key = mru_key(&legacy_path);
        if !repositories
            .iter()
            .any(|repository| mru_key(&repository.path) == legacy_key)
        {
            repositories.push(TicketRepository {
                name: display_name_for(&legacy_path),
                path: legacy_path,
            });
        }
    }

    let mru = read_mru(&KeyValueStore::global(cx));
    // Stable sort: repositories with no recorded use all compare equal and so
    // stay in the order settings.json lists them.
    repositories.sort_by_key(|repository| {
        std::cmp::Reverse(mru.get(&mru_key(&repository.path)).copied().unwrap_or(0))
    });
    repositories
}

/// Records `path` as the most recently used repository.
pub fn mark_used(path: &Path, cx: &mut App) -> Task<anyhow::Result<()>> {
    let kvp = KeyValueStore::global(cx);
    let key = mru_key(path);
    cx.background_spawn(async move {
        let mut mru = read_mru(&kvp);
        let next_rank = mru.values().copied().max().unwrap_or(0) + 1;
        mru.insert(key, next_rank);
        let json = serde_json::to_string(&mru)?;
        kvp.write_kvp(REPOSITORY_MRU_KEY.to_string(), json).await
    })
}

/// Confirms `path` is inside a git repository and returns that repository's
/// top level, so picking a subdirectory still registers the root — which is
/// what `git gtr new` needs as its working directory.
pub async fn validate_git_repository(path: PathBuf) -> anyhow::Result<PathBuf> {
    let output = smol::process::Command::new("git")
        .args(["-C"])
        .arg(&path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .with_context(|| format!("failed to run git in {}", path.display()))?;

    anyhow::ensure!(
        output.status.success(),
        "{} is not inside a git repository: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let top_level = String::from_utf8(output.stdout)
        .context("git rev-parse --show-toplevel returned non-UTF-8 output")?
        .trim()
        .to_string();
    anyhow::ensure!(
        !top_level.is_empty(),
        "git rev-parse --show-toplevel returned nothing for {}",
        path.display()
    );
    Ok(PathBuf::from(top_level))
}

/// Asks the user for a directory, validates it is a git repository, and
/// appends it to `tickets_panel.repositories`. Resolves to `None` when the
/// user dismisses the picker.
pub fn add_repository(
    fs: Arc<dyn Fs>,
    cx: &mut App,
) -> Task<anyhow::Result<Option<TicketRepository>>> {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Add repository".into()),
    });

    cx.spawn(async move |cx| {
        let Some(selected) = paths
            .await
            .context("the directory picker was closed before answering")?
            .context("failed to open a directory picker")?
        else {
            return Ok(None);
        };
        let Some(selected) = selected.into_iter().next() else {
            return Ok(None);
        };

        let repository_root = validate_git_repository(selected).await?;
        let repository = TicketRepository {
            name: display_name_for(&repository_root),
            path: repository_root.clone(),
        };

        cx.update(|cx| {
            let path_string = repository_root.to_string_lossy().to_string();
            settings::update_settings_file(fs, cx, move |settings, _cx| {
                let panel = settings.tickets_panel.get_or_insert_default();
                let repositories = panel.repositories.get_or_insert_default();
                let key = mru_key(Path::new(&path_string));
                if !repositories
                    .iter()
                    .any(|existing| mru_key(Path::new(&existing.path)) == key)
                {
                    repositories.push(TicketRepositoryContent {
                        path: path_string,
                        name: None,
                    });
                }
            });
        });

        Ok(Some(repository))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mru_key_normalizes_separators_and_case() {
        assert_eq!(
            mru_key(Path::new(r"C:\Users\dev\Repo\")),
            mru_key(Path::new("c:/users/dev/repo"))
        );
    }

    #[test]
    fn test_display_name_for_uses_the_directory_name() {
        assert_eq!(display_name_for(Path::new("/home/dev/inox")), "inox");
    }
}
