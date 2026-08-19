use db::kvp::KeyValueStore;
use dev_container::{find_configs_in_snapshot, is_supported_dev_container_source_connection};
use gpui::{App, SharedString, Window};
use project::{Project, WorktreeId};
use remote::RemoteConnectionOptions;
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;
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

/// The worktrees a workspace has already been offered a Dev Container for.
///
/// Two triggers ask the same question — the project's state when the workspace
/// opens, and later worktree changes — and they overlap whenever a configuration
/// is present before the workspace exists and is touched again afterwards. Both
/// share this set so the offer is still made at most once per worktree per
/// session.
pub type OfferedWorktrees = Rc<RefCell<HashSet<WorktreeId>>>;

struct DevContainerSuggestionNotification;

fn suggestion_notification_id(project_path: &str) -> NotificationId {
    NotificationId::composite::<DevContainerSuggestionNotification>(SharedString::from(
        project_path.to_string(),
    ))
}

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
    project_is_local
        || connection.is_some_and(|connection| {
            is_supported_dev_container_source_connection(connection)
                || is_mock_project_host(connection)
        })
}

/// Whether a connection is the mock transport tests use in place of a project
/// host. Reaching a remote worktree in a test requires it, and every such
/// worktree stands in for an SSH or WSL one.
#[cfg(any(test, feature = "test-support"))]
fn is_mock_project_host(connection: &RemoteConnectionOptions) -> bool {
    matches!(connection, RemoteConnectionOptions::Mock(_))
}

#[cfg(not(any(test, feature = "test-support")))]
fn is_mock_project_host(_connection: &RemoteConnectionOptions) -> bool {
    false
}

/// Offers a Dev Container for the configurations the project already contains.
///
/// This is the state-based trigger, and it exists because a worktree's entries
/// routinely arrive before the workspace that would observe them: a remote
/// worktree is populated as soon as it is created, which is before
/// `Workspace::new` runs, and a small local project's scan finishes in the same
/// window. Asking the project what it holds, rather than only reacting to it
/// changing, is what makes the offer independent of that race.
pub fn suggest_for_project_state(
    workspace: &mut Workspace,
    offered: &OfferedWorktrees,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let worktree_ids = workspace
        .project()
        .read(cx)
        .worktrees(cx)
        .map(|worktree| worktree.read(cx).id())
        .collect::<Vec<_>>();

    for worktree_id in worktree_ids {
        suggest_for_worktree(workspace, worktree_id, offered, window, cx);
    }
}

/// Asks the same question again when a worktree changes, so a configuration
/// added to an already-open project is offered too.
pub fn suggest_on_worktree_updated(
    workspace: &mut Workspace,
    worktree_id: WorktreeId,
    updated_entries: &UpdatedEntriesSet,
    offered: &OfferedWorktrees,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let devcontainer_updated = updated_entries.iter().any(|(path, _, _)| {
        path.as_ref() == paths::local_dev_container_folder_path()
            || path.as_ref() == devcontainer_json_path()
    });

    if !devcontainer_updated && !workspace.open_in_dev_container() {
        return;
    }

    suggest_for_worktree(workspace, worktree_id, offered, window, cx);
}

fn suggest_for_worktree(
    workspace: &mut Workspace,
    worktree_id: WorktreeId,
    offered: &OfferedWorktrees,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();

    let Some(worktree) = project.read(cx).worktree_for_id(worktree_id, cx) else {
        return;
    };

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

    if workspace.open_in_dev_container() {
        open_in_dev_container_from_cli(workspace, &project, offered, window, cx);
        return;
    }

    let (has_configs, abs_path, worktree_name) = {
        let worktree = worktree.read(cx);
        (
            !find_configs_in_snapshot(worktree).is_empty(),
            worktree.abs_path(),
            worktree.root_name_str().to_string(),
        )
    };

    if !has_configs {
        return;
    }

    let project_path = abs_path.to_string_lossy().to_string();
    let dismiss_path = dismiss_path_for_worktree(&project, abs_path.as_ref(), cx);
    let key_for_dismiss = project_devcontainer_key(&dismiss_path);

    let already_dismissed = KeyValueStore::global(cx)
        .read_kvp(&key_for_dismiss)
        .ok()
        .flatten()
        .is_some();

    if already_dismissed {
        return;
    }

    if !offered.borrow_mut().insert(worktree_id) {
        return;
    }

    cx.on_next_frame(window, move |workspace, _window, cx| {
        let notification_id = suggestion_notification_id(&project_path);

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

/// Consumes the `--dev-container` flag and opens the project in a container once
/// it has finished loading.
///
/// The flag is consumed here rather than at the point it is read so that it can
/// only fire once, whichever trigger reaches it first.
fn open_in_dev_container_from_cli(
    workspace: &mut Workspace,
    project: &gpui::Entity<Project>,
    offered: &OfferedWorktrees,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.set_open_in_dev_container(false);

    // The CLI is opening the whole project in a container, so none of its
    // worktrees should also be suggested one.
    offered.borrow_mut().extend(
        project
            .read(cx)
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).id()),
    );

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension::ExtensionHostProxy;
    use fs::{FakeFs, RemoveOptions};
    use gpui::{Entity, TestAppContext};
    use http_client::BlockedHttpClient;
    use node_runtime::NodeRuntime;
    use remote::{
        DockerConnectionOptions, HostDockerConnectionOptions, HostPathBuf,
        ProjectHostConnectionOptions, RemoteClient, SshConnectionOptions, WslConnectionOptions,
    };
    use remote_server::{HeadlessAppState, HeadlessProject};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use util::path;
    use util::paths::PathStyle;
    use workspace::{AppState, MultiWorkspace, OpenOptions};

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

    /// The reporter's project shape: small enough that every entry is delivered
    /// in one batch, which for a remote project lands before the workspace that
    /// would observe it exists.
    fn smoke_test_project() -> serde_json::Value {
        json!({
            ".devcontainer": {
                "devcontainer.json": r#"{ "image": "mcr.microsoft.com/devcontainers/base:ubuntu" }"#,
            },
            "README.md": "# smoke",
        })
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            crate::init(cx);
            editor::init(cx);
            state
        })
    }

    fn active_workspace(cx: &mut TestAppContext) -> Entity<Workspace> {
        let window = cx.update(|cx| cx.windows()[0].downcast::<MultiWorkspace>().unwrap());
        window
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap()
    }

    /// Tests have no platform frame loop, and the suggestion is raised from a
    /// next-frame callback, so deliver the frame by hand.
    fn deliver_next_frame(cx: &mut TestAppContext) {
        let window = cx.update(|cx| *cx.windows().last().unwrap());
        cx.update_window(window, |_, window, cx| {
            window.simulate_next_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    fn suggestion_is_shown(workspace: &Entity<Workspace>, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| {
            let workspace = workspace.read(cx);
            let Some(worktree) = workspace.project().read(cx).worktrees(cx).next() else {
                return false;
            };
            let expected =
                suggestion_notification_id(&worktree.read(cx).abs_path().to_string_lossy());
            workspace.notification_ids().contains(&expected)
        })
    }

    async fn open_local_project(
        path: &str,
        tree: serde_json::Value,
        app_state: &Arc<AppState>,
        cx: &mut TestAppContext,
    ) -> Entity<Workspace> {
        app_state.fs.as_fake().insert_tree(path, tree).await;

        cx.update(|cx| {
            workspace::open_paths(
                &[PathBuf::from(path)],
                app_state.clone(),
                OpenOptions::default(),
                cx,
            )
        })
        .await
        .expect("opening a local project should succeed");

        cx.run_until_parked();
        deliver_next_frame(cx);
        active_workspace(cx)
    }

    /// A remote project whose worktree is fully populated before anything asks
    /// about it. The mock transport stands in for a project host; `Project` sees
    /// exactly what it would see over SSH or WSL.
    ///
    /// Returns the headless project too — it has to outlive the assertions.
    async fn remote_project_with_a_populated_worktree(
        path: &str,
        tree: serde_json::Value,
        app_state: &Arc<AppState>,
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<HeadlessProject>) {
        server_cx.update(|cx| {
            release_channel::init(semver::Version::new(0, 0, 0), cx);
        });

        let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);

        let remote_fs = FakeFs::new(server_cx.executor());
        remote_fs.insert_tree(path, tree).await;

        server_cx.update(HeadlessProject::init);
        let languages = Arc::new(language::LanguageRegistry::new(server_cx.executor()));
        let headless = server_cx.new(|cx| {
            HeadlessProject::new(
                HeadlessAppState {
                    session: server_session,
                    fs: remote_fs.clone(),
                    http_client: Arc::new(BlockedHttpClient),
                    node_runtime: NodeRuntime::unavailable(),
                    languages,
                    extension_host_proxy: Arc::new(ExtensionHostProxy::new()),
                    startup_time: std::time::Instant::now(),
                },
                false,
                cx,
            )
        });

        drop(connect_guard);

        let remote_client = RemoteClient::connect_mock(opts, cx).await;
        // A client of its own: `Workspace::test_new` registers a workspace store
        // against the project's client, and the one `AppState::test` built
        // already has one.
        let project = cx.update(|cx| {
            let client = client::Client::new(
                Arc::new(clock::FakeSystemClock::new()),
                http_client::FakeHttpClient::with_404_response(),
                cx,
            );
            let user_store = cx.new(|cx| client::UserStore::new(client.clone(), cx));
            Project::remote(
                remote_client,
                client,
                NodeRuntime::unavailable(),
                user_store,
                app_state.languages.clone(),
                app_state.fs.clone(),
                false,
                cx,
            )
        });

        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(path, true, cx)
            })
            .await
            .expect("should open the remote worktree");
        cx.run_until_parked();

        (project, headless)
    }

    /// Builds the workspace over a project that is already fully loaded, which
    /// is the ordering the defect turns on: every `WorktreeUpdatedEntries` event
    /// has been emitted before anything is subscribed.
    fn workspace_over(project: Entity<Project>, cx: &mut TestAppContext) -> Entity<Workspace> {
        let window = cx.add_window(|window, cx| Workspace::test_new(project, window, cx));
        cx.run_until_parked();
        deliver_next_frame(cx);
        window.root(cx).unwrap()
    }

    fn worktree_holds_a_config(project: &Entity<Project>, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| {
            project
                .read(cx)
                .worktrees(cx)
                .any(|worktree| !find_configs_in_snapshot(worktree.read(cx)).is_empty())
        })
    }

    /// The regression test for the reported defect. A remote worktree is
    /// populated as soon as it is created, which is before `Workspace::new`
    /// runs, so the change event that used to be the only trigger was emitted to
    /// nobody.
    #[gpui::test]
    async fn a_remote_project_loaded_before_its_workspace_is_offered_a_dev_container(
        cx: &mut TestAppContext,
        server_cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        let (project, _headless) = remote_project_with_a_populated_worktree(
            path!("/remote-smoke"),
            smoke_test_project(),
            &app_state,
            cx,
            server_cx,
        )
        .await;

        assert!(
            worktree_holds_a_config(&project, cx),
            "the remote worktree must be fully delivered before the workspace exists, \
             or this test is not exercising the defect"
        );

        let workspace = workspace_over(project, cx);

        assert!(
            suggestion_is_shown(&workspace, cx),
            "a remote project holding a Dev Container configuration should be offered one"
        );
    }

    /// The same defect on a local project, which is where it was first observed:
    /// a two-entry directory finishes scanning while `Workspace::new_local` is
    /// still awaiting the workspace database.
    #[gpui::test]
    async fn a_local_project_loaded_before_its_workspace_is_offered_a_dev_container(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/local-smoke"), smoke_test_project())
            .await;

        let project =
            Project::test(app_state.fs.clone(), [path!("/local-smoke").as_ref()], cx).await;
        cx.run_until_parked();

        assert!(
            worktree_holds_a_config(&project, cx),
            "the local worktree must be fully scanned before the workspace exists, \
             or this test is not exercising the defect"
        );

        let workspace = workspace_over(project, cx);

        assert!(
            suggestion_is_shown(&workspace, cx),
            "a local project holding a Dev Container configuration should be offered one"
        );
    }

    /// A large project streams its entries over several batches, some of which
    /// land after the workspace exists. Both shapes must reach the same answer.
    #[gpui::test]
    async fn a_large_local_project_is_offered_a_dev_container_too(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let mut tree = serde_json::Map::new();
        tree.insert(
            ".devcontainer".to_string(),
            json!({ "devcontainer.json": "{}" }),
        );
        for index in 0..200 {
            tree.insert(format!("file-{index}.rs"), json!("fn main() {}"));
        }

        let workspace = open_local_project(
            path!("/large-project"),
            serde_json::Value::Object(tree),
            &app_state,
            cx,
        )
        .await;

        assert!(
            suggestion_is_shown(&workspace, cx),
            "project size must not decide whether the suggestion is offered"
        );
    }

    /// The state check is added alongside the change trigger, not in place of
    /// it: a configuration written into an already-open project is still
    /// offered.
    #[gpui::test]
    async fn a_config_added_to_an_open_project_is_offered_a_dev_container(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let workspace = open_local_project(
            path!("/added-later"),
            json!({ "README.md": "# no container yet" }),
            &app_state,
            cx,
        )
        .await;

        assert!(
            !suggestion_is_shown(&workspace, cx),
            "a project with no configuration should not be offered a Dev Container"
        );

        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/added-later/.devcontainer"),
                json!({ "devcontainer.json": "{}" }),
            )
            .await;
        cx.run_until_parked();
        deliver_next_frame(cx);

        assert!(
            suggestion_is_shown(&workspace, cx),
            "a configuration added to an open project should be offered"
        );
    }

    /// Two triggers now ask the same question, so the answer must still be given
    /// only once: dismissing the suggestion and touching the configuration again
    /// must not bring it back.
    #[gpui::test]
    async fn the_suggestion_is_offered_at_most_once_per_session(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let workspace =
            open_local_project(path!("/offered-once"), smoke_test_project(), &app_state, cx).await;

        assert!(suggestion_is_shown(&workspace, cx));

        workspace.update(cx, |workspace, cx| {
            workspace.clear_all_notifications(cx);
        });
        assert!(!suggestion_is_shown(&workspace, cx));

        // Rewrite the configuration directory, which is the strongest form of
        // the change trigger: the entry the trigger watches for is removed and
        // added again.
        let fs = app_state.fs.clone();
        fs.remove_dir(
            path!("/offered-once/.devcontainer").as_ref(),
            RemoveOptions {
                recursive: true,
                ignore_if_not_exists: true,
            },
        )
        .await
        .unwrap();
        cx.run_until_parked();

        fs.as_fake()
            .insert_tree(
                path!("/offered-once/.devcontainer"),
                json!({ "devcontainer.json": "{}" }),
            )
            .await;
        cx.run_until_parked();
        deliver_next_frame(cx);

        assert!(
            !suggestion_is_shown(&workspace, cx),
            "the suggestion should be offered at most once per project per session"
        );
    }

    /// "Don't Show Again" is recorded against the project, and the state check
    /// has to honor it just as the change trigger did.
    #[gpui::test]
    async fn a_dismissed_project_is_not_offered_a_dev_container(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        let key = project_devcontainer_key(path!("/dismissed-project"));
        cx.update(|cx| KeyValueStore::global(cx))
            .write_kvp(key, "dismissed".to_string())
            .await
            .unwrap();

        let workspace = open_local_project(
            path!("/dismissed-project"),
            smoke_test_project(),
            &app_state,
            cx,
        )
        .await;

        assert!(
            !suggestion_is_shown(&workspace, cx),
            "a project the user dismissed should not be offered a Dev Container"
        );
    }
}
