use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use remote::{Interactive, RemoteConnection, RemoteConnectionOptions};
use util::command::Command;
use util::paths::PathStyle;

/// A container-engine command, described as plain data.
#[derive(Debug, Clone)]
pub(crate) struct CommandSpec {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
}

impl CommandSpec {
    pub(crate) fn new(program: impl AsRef<str>) -> Self {
        Self {
            program: program.as_ref().to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    pub(crate) fn arg(&mut self, arg: impl AsRef<str>) -> &mut Self {
        self.args.push(arg.as_ref().to_string());
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_string()));
        self
    }

    pub(crate) fn env(&mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
        self.env
            .insert(key.as_ref().to_string(), value.as_ref().to_string());
        self
    }
}

/// The context used to run commands against a container engine.
///
/// Remote connections build their own commands so that each transport controls
/// quoting, authentication, and process invocation.
pub(crate) struct ContainerEngine {
    connection: Option<Arc<dyn RemoteConnection>>,
    path_style: PathStyle,
    wsl_distro_name: Option<String>,
}

impl std::fmt::Debug for ContainerEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContainerEngine")
            .field(
                "connection",
                &self
                    .connection
                    .as_ref()
                    .map(|connection| connection.connection_options()),
            )
            .finish()
    }
}

impl ContainerEngine {
    pub(crate) fn local() -> Self {
        Self {
            connection: None,
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
            connection: Some(connection),
            wsl_distro_name,
        }
    }

    pub(crate) fn command(&self, spec: CommandSpec) -> Result<Command> {
        let CommandSpec { program, args, env } = spec;
        let Some(connection) = &self.connection else {
            let mut command = Command::new(program);
            command.args(args);
            for (key, value) in env {
                command.env(key, value);
            }
            return Ok(command);
        };
        let environment = env.into_iter().collect();
        let template = connection.build_command(
            Some(program),
            &args,
            &environment,
            None,
            None,
            Interactive::No,
        )?;
        let mut command = Command::new(template.program);
        command.args(template.args);
        for (key, value) in template.env {
            command.env(key, value);
        }
        Ok(command)
    }

    /// Remote engine selections must be compared with Zed's local selection.
    pub(crate) fn requires_local_engine_match_verification(&self) -> bool {
        self.connection.is_some()
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

    fn rendered(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn local_engine_runs_the_program_directly() {
        let mut spec = CommandSpec::new("docker");
        spec.args(["ps", "-a"]);

        assert_eq!(
            rendered(
                &ContainerEngine::local()
                    .command(spec)
                    .expect("command builds")
            ),
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
