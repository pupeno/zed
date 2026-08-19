use db::kvp::KeyValueStore;
use dev_container::{find_configs_in_snapshot, is_supported_dev_container_source_connection};
use gpui::{App, SharedString, Window};
use project::{Project, WorktreeId};
use remote::RemoteConnectionOptions;
use std::path::Path;
use std::sync::LazyLock;
use ui::Tooltip;
use ui::prelude::*;
use util::ResultExt;
use util::rel_path::RelPath;
use workspace::Workspace;
use workspace::notifications::NotificationId;
use workspace::notifications::simple_message_notification::MessageNotification;
use worktree::UpdatedEntriesSet;

const DEV_CONTAINER_SUGGEST_KEY: &str = "dev_container_suggest_dismissed";

fn devcontainer_json_path() -> &'static RelPath {
    static PATH: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::from_unix_str(".devcontainer.json").expect("valid path"));
    *PATH
}

fn project_devcontainer_key(project_path: &str) -> String {
    format!("{}_{}", DEV_CONTAINER_SUGGEST_KEY, project_path)
}

/// Returns the path used to remember the user's "Don't Show Again" choice for a
/// worktree's dev container suggestion. This is keyed on the repository's common
/// Git directory rather than the worktree's own path, so that dismissing the
/// suggestion in one git worktree also suppresses it in sibling worktrees of the
/// same repository. Falls back to the worktree path when it isn't part of a Git
/// repository.
fn dismiss_path_for_worktree(
    project: &gpui::Entity<Project>,
    worktree_abs_path: &Path,
    cx: &App,
) -> String {
    let common_dir = project
        .read(cx)
        .repositories(cx)
        .values()
        .filter_map(|repo| {
            let repo = repo.read(cx);
            let work_dir = repo.work_directory_abs_path.clone();
            // The folder opened in Zed isn't necessarily the repo root; it may be
            // a subdirectory of it, e.g. opening `~/code/myrepo/backend` when the
            // repo lives at `~/code/myrepo`. So match any repo whose work directory
            // contains the folder. Nested repos can produce multiple matches, e.g.
            // opening `~/code/myrepo/vendor/lib` where `vendor/lib` is a submodule
            // matches both `myrepo` and the submodule; `max_by_key` then picks the
            // innermost match (the submodule), which the folder actually belongs to.
            worktree_abs_path
                .starts_with(work_dir.as_ref())
                .then(|| (work_dir.as_os_str().len(), repo.common_dir_abs_path.clone()))
        })
        .max_by_key(|(work_dir_len, _)| *work_dir_len)
        .map(|(_, common_dir)| common_dir);

    match common_dir {
        Some(common_dir) => common_dir.to_string_lossy().to_string(),
        None => worktree_abs_path.to_string_lossy().to_string(),
    }
}

/// Whether a project is a source project a Dev Container can be started from.
///
/// Local, SSH, and WSL projects qualify. Docker and Podman projects do not: they
/// are already the result of such a transition, so they are not offered one of
/// their own. This deliberately does not test whether the worktree is local —
/// SSH and WSL worktrees are remote and are still valid sources.
fn is_supported_suggestion_source(
    project_is_local: bool,
    connection: Option<&RemoteConnectionOptions>,
) -> bool {
    project_is_local || connection.is_some_and(is_supported_dev_container_source_connection)
}

pub fn suggest_on_worktree_updated(
    workspace: &mut Workspace,
    worktree_id: WorktreeId,
    updated_entries: &UpdatedEntriesSet,
    project: &gpui::Entity<Project>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let cli_auto_open = workspace.open_in_dev_container();

    let devcontainer_updated = updated_entries.iter().any(|(path, _, _)| {
        path.as_ref() == paths::local_dev_container_folder_path()
            || path.as_ref() == devcontainer_json_path()
    });

    if !devcontainer_updated && !cli_auto_open {
        return;
    }

    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
        return;
    };

    let worktree = worktree.read(cx);

    let is_supported_source = {
        let project = project.read(cx);
        is_supported_suggestion_source(
            project.is_local(),
            project.remote_connection_options(cx).as_ref(),
        )
    };

    if !is_supported_source {
        return;
    }

    let has_configs = !find_configs_in_snapshot(worktree).is_empty();

    if cli_auto_open {
        workspace.set_open_in_dev_container(false);
        let task = cx.spawn_in(window, async move |workspace, cx| {
            let scans_complete =
                workspace.update(cx, |workspace, cx| workspace.worktree_scans_complete(cx))?;
            scans_complete.await;

            workspace.update_in(cx, |workspace, window, cx| {
                let has_configs = workspace
                    .project()
                    .read(cx)
                    .worktrees(cx)
                    .any(|wt| !find_configs_in_snapshot(wt.read(cx)).is_empty());
                if has_configs {
                    cx.on_next_frame(window, move |_workspace, window, cx| {
                        window.dispatch_action(Box::new(zed_actions::OpenDevContainer), cx);
                    });
                } else {
                    log::warn!("--dev-container: no devcontainer configuration found in project");
                }
            })
        });
        workspace.set_dev_container_task(task);
        return;
    }

    if !has_configs {
        return;
    }

    let abs_path = worktree.abs_path();
    let project_path = abs_path.to_string_lossy().to_string();
    let worktree_name = worktree.root_name_str().to_string();
    let dismiss_path = dismiss_path_for_worktree(project, abs_path.as_ref(), cx);
    let key_for_dismiss = project_devcontainer_key(&dismiss_path);

    let already_dismissed = KeyValueStore::global(cx)
        .read_kvp(&key_for_dismiss)
        .ok()
        .flatten()
        .is_some();

    if already_dismissed {
        return;
    }

    cx.on_next_frame(window, move |workspace, _window, cx| {
        struct DevContainerSuggestionNotification;

        let notification_id = NotificationId::composite::<DevContainerSuggestionNotification>(
            SharedString::from(project_path.clone()),
        );

        workspace.show_notification(notification_id, cx, |cx| {
            cx.new(move |cx| {
                let message: SharedString = format!(
                    "{worktree_name} contains a Dev Container configuration file. Would you like to re-open it in a container?"
                )
                .into();
                let tooltip_text: SharedString = project_path.clone().into();
                MessageNotification::new_from_builder(cx, move |_window, _cx| {
                    div()
                        .id("dev-container-suggest-message")
                        .child(Label::new(message.clone()))
                        .tooltip(Tooltip::text(tooltip_text.clone()))
                        .into_any_element()
                })
                .primary_message("Yes, Open in Container")
                .primary_icon(IconName::Check)
                .primary_icon_color(Color::Success)
                .primary_on_click({
                    move |window, cx| {
                        window.dispatch_action(Box::new(zed_actions::OpenDevContainer), cx);
                    }
                })
                .secondary_message("Don't Show Again")
                .secondary_icon(IconName::Close)
                .secondary_icon_color(Color::Error)
                .secondary_on_click({
                    move |_window, cx| {
                        let key = key_for_dismiss.clone();
                        let kvp = KeyValueStore::global(cx);
                        cx.background_spawn(async move {
                            kvp.write_kvp(key, "dismissed".to_string())
                                .await
                                .log_err();
                        })
                        .detach();
                    }
                })
            })
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote::{
        DockerConnectionOptions, HostDockerConnectionOptions, HostPathBuf,
        ProjectHostConnectionOptions, SshConnectionOptions, WslConnectionOptions,
    };
    use util::paths::PathStyle;

    #[test]
    fn local_ssh_and_wsl_projects_are_offered_dev_containers() {
        assert!(is_supported_suggestion_source(true, None));
        assert!(is_supported_suggestion_source(
            false,
            Some(&RemoteConnectionOptions::Ssh(
                SshConnectionOptions::default()
            ))
        ));
        assert!(is_supported_suggestion_source(
            false,
            Some(&RemoteConnectionOptions::Wsl(WslConnectionOptions {
                distro_name: "Ubuntu".to_string(),
                user: None,
            }))
        ));
    }

    #[test]
    fn container_projects_are_not_offered_dev_containers() {
        assert!(!is_supported_suggestion_source(
            false,
            Some(&RemoteConnectionOptions::Docker(
                DockerConnectionOptions::default()
            ))
        ));
        assert!(!is_supported_suggestion_source(
            false,
            Some(&RemoteConnectionOptions::HostDocker(
                HostDockerConnectionOptions {
                    project_host: ProjectHostConnectionOptions::Ssh(SshConnectionOptions::default()),
                    project_root: HostPathBuf::new("/project", PathStyle::Unix),
                    devcontainer_config: HostPathBuf::new(
                        "/project/.devcontainer/devcontainer.json",
                        PathStyle::Unix,
                    ),
                    container: DockerConnectionOptions::default(),
                }
            ))
        ));
    }
}
