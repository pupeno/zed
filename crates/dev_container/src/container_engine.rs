use std::path::{Path, PathBuf};
use std::sync::Arc;

use remote::{RemoteConnection, RemoteConnectionOptions};
use util::paths::PathStyle;

use crate::project_command::{ProjectCommand, ProjectCommandBuilder};

/// The context used to run commands against a container engine.
///
/// Remote connections build their own commands so that each transport controls
/// quoting, authentication, and process invocation.
pub(crate) struct ContainerEngine {
    pub(crate) command_builder: ProjectCommandBuilder,
    path_style: PathStyle,
    wsl_distro_name: Option<String>,
}

impl std::fmt::Debug for ContainerEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerEngine")
            .field("connection", &self.command_builder.connection_options())
            .finish()
    }
}

impl ContainerEngine {
    pub(crate) fn local() -> Self {
        Self {
            command_builder: ProjectCommandBuilder::local(),
            path_style: if cfg!(windows) {
                PathStyle::Windows
            } else {
                PathStyle::Unix
            },
            wsl_distro_name: None,
        }
    }

    pub(crate) fn for_remote_connection(connection: Arc<dyn RemoteConnection>) -> Self {
        let wsl_distro_name = match connection.connection_options() {
            RemoteConnectionOptions::Wsl(options) => Some(options.distro_name.clone()),
            _ => None,
        };
        Self {
            path_style: connection.path_style(),
            command_builder: ProjectCommandBuilder::for_remote_connection(connection),
            wsl_distro_name,
        }
    }

    /// Remote engine selections must be compared with Zed's local selection.
    pub(crate) fn requires_local_engine_match_verification(&self) -> bool {
        self.command_builder.connection_options().is_some()
    }

    pub(crate) fn command(&self, program: impl AsRef<str>) -> ProjectCommand<'_> {
        self.command_builder.command(program)
    }

    pub(crate) fn join_path(&self, base: &Path, path: impl AsRef<Path>) -> PathBuf {
        let path = path.as_ref();
        if self.path_style.is_posix() {
            let path_string = path.to_string_lossy();
            if self.path_style.is_absolute(&path_string) {
                return PathBuf::from(self.path_style.normalize(&path_string));
            }

            let base_string = base.to_string_lossy();
            let separator = if base_string.ends_with(self.path_style.primary_separator()) {
                ""
            } else {
                self.path_style.primary_separator()
            };
            PathBuf::from(
                self.path_style
                    .normalize(&format!("{base_string}{separator}{path_string}")),
            )
        } else {
            base.join(path)
        }
    }

    pub(crate) fn normalize_path(&self, path: &Path) -> PathBuf {
        if self.path_style.is_posix() {
            PathBuf::from(self.path_style.normalize(&path.to_string_lossy()))
        } else {
            util::normalize_path(path)
        }
    }

    pub(crate) fn temporary_directory(&self) -> PathBuf {
        if self.path_style.is_posix() {
            PathBuf::from("/tmp/devcontainer-zed")
        } else {
            std::env::temp_dir().join("devcontainer-zed")
        }
    }

    pub(crate) fn filesystem_path(&self, path: &Path) -> PathBuf {
        let Some(distro_name) = &self.wsl_distro_name else {
            return path.to_path_buf();
        };

        // A remote WSL worktree reports native Linux paths, while the Zed
        // client owns the Fs implementation on Windows. UNC is the supported
        // Windows view of the distro filesystem; passing `/home/...` directly
        // makes the Windows Fs look for a non-existent local path instead.
        #[cfg(windows)]
        {
            wsl_filesystem_path(path, distro_name)
        }

        #[cfg(not(windows))]
        {
            let _ = distro_name;
            path.to_path_buf()
        }
    }

    pub(crate) fn is_posix(&self) -> bool {
        self.path_style == PathStyle::Unix
    }

    pub(crate) fn path_style(&self) -> PathStyle {
        self.path_style
    }
}

#[cfg(windows)]
fn wsl_filesystem_path(path: &Path, distro_name: &str) -> PathBuf {
    let relative = path
        .to_string_lossy()
        .trim_start_matches(['/', '\\'])
        .replace('/', "\\");
    PathBuf::from(format!(r"\\wsl.localhost\{distro_name}\{relative}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use util::command::Command;

    fn rendered(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn local_engine_runs_the_program_directly() {
        let engine = ContainerEngine::local();
        let mut command = engine.command("docker");
        command.args(["ps", "-a"]);

        assert_eq!(
            rendered(&command.build().expect("command builds")),
            ["docker", "ps", "-a"]
        );
    }

    #[test]
    fn local_engine_skips_local_match_verification() {
        assert!(!ContainerEngine::local().requires_local_engine_match_verification());
    }

    #[cfg(windows)]
    #[test]
    fn wsl_filesystem_paths_use_the_distro_unc_share() {
        assert_eq!(
            wsl_filesystem_path(
                Path::new("/home/test-user/project/.devcontainer/devcontainer.json"),
                "Ubuntu",
            ),
            PathBuf::from(
                r"\\wsl.localhost\Ubuntu\home\test-user\project\.devcontainer\devcontainer.json"
            )
        );
    }
}
