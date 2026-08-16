use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use remote::{Interactive, RemoteConnection, RemoteConnectionOptions};
use util::command::Command;
use util::paths::PathStyle;

/// A command to run against the container engine, described as plain data.
///
/// Remote command transports wrap the complete invocation, including its
/// program, arguments, and environment.
#[derive(Debug, Clone)]
pub(crate) struct CommandSpec {
    program: String,
    args: Vec<String>,
    /// Ordered so that a wrapped invocation is stable across runs, which keeps
    /// logs and tests readable.
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

/// Where the container engine runs.
pub(crate) trait CommandHost: Debug + Send + Sync {
    fn command(&self, spec: CommandSpec) -> Result<Command>;

    /// Whether Zed must verify that this host uses the local container engine.
    fn requires_local_engine_match_verification(&self) -> bool;

    /// Maps a path understood by the container engine to a path that the Zed
    /// client can use with its local filesystem implementation.
    fn filesystem_path(&self, path: &Path) -> PathBuf;

    /// Whether the host has posix user and path semantics.
    fn is_posix(&self) -> bool;
}

/// The engine runs on the same machine as Zed.
#[derive(Debug)]
pub(crate) struct LocalCommandHost;

impl CommandHost for LocalCommandHost {
    fn requires_local_engine_match_verification(&self) -> bool {
        // Local commands always select Zed's engine.
        false
    }

    fn is_posix(&self) -> bool {
        cfg!(not(target_os = "windows"))
    }

    fn command(&self, spec: CommandSpec) -> Result<Command> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        Ok(command)
    }

    fn filesystem_path(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}

/// The engine is reached from inside a WSL distribution.
///
/// The WSL invocation preserves the project's native POSIX bind-mount source.
/// A Windows-side invocation supplies a `\\wsl.localhost` or `/mnt/c` path,
/// which the engine may reject or access through a filesystem bridge.
#[derive(Debug)]
pub(crate) struct WslCommandHost {
    distro_name: String,
    user: Option<String>,
}

impl CommandHost for WslCommandHost {
    fn requires_local_engine_match_verification(&self) -> bool {
        true
    }

    fn is_posix(&self) -> bool {
        true
    }

    fn command(&self, spec: CommandSpec) -> Result<Command> {
        let mut command = Command::new("wsl.exe");
        command.arg("-d").arg(&self.distro_name);
        if let Some(user) = &self.user {
            command.arg("-u").arg(user);
        }
        command.arg("--");

        // `env` carries the requested variables and doubles as a program to
        // exec, so no shell is involved and arguments pass through as argv.
        // That matters because dev container arguments routinely contain
        // spaces, quotes and `$` — anything shell-mediated would need quoting
        // rules for two shells at once.
        if !spec.env.is_empty() {
            command.arg("env");
            for (key, value) in &spec.env {
                command.arg(format!("{key}={value}"));
            }
        }

        command.arg(&spec.program);
        command.args(&spec.args);
        Ok(command)
    }

    fn filesystem_path(&self, path: &Path) -> PathBuf {
        // A remote WSL worktree reports native Linux paths, while the Zed
        // client owns the Fs implementation on Windows. UNC is the supported
        // Windows view of the distro filesystem; passing `/home/...` directly
        // makes the Windows Fs look for a non-existent local path instead.
        #[cfg(windows)]
        {
            let relative = path
                .to_string_lossy()
                .trim_start_matches(['/', '\\'])
                .replace('/', "\\");
            PathBuf::from(format!(r"\\wsl.localhost\{}\{relative}", self.distro_name))
        }

        #[cfg(not(windows))]
        {
            path.to_path_buf()
        }
    }
}

/// Runs commands through an established remote connection.
pub(crate) struct RemoteCommandHost {
    connection: Arc<dyn RemoteConnection>,
    path_style: PathStyle,
}

impl Debug for RemoteCommandHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteCommandHost")
            .field("connection", &self.connection.connection_options())
            .finish()
    }
}

impl CommandHost for RemoteCommandHost {
    fn requires_local_engine_match_verification(&self) -> bool {
        true
    }

    fn is_posix(&self) -> bool {
        self.path_style == PathStyle::Unix
    }

    fn command(&self, spec: CommandSpec) -> Result<Command> {
        let args = spec.args;
        let environment = spec.env.into_iter().collect();
        let template = self.connection.build_command(
            Some(spec.program),
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

    fn filesystem_path(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}

/// Returns the host to drive the container engine through for an established
/// remote connection. WSL keeps a specialized filesystem mapping so that the
/// Windows client can read the distro's files; every other connection uses the
/// transport's generic command builder.
pub(crate) fn host_for_remote_connection(
    connection: Arc<dyn RemoteConnection>,
) -> Arc<dyn CommandHost> {
    match connection.connection_options() {
        RemoteConnectionOptions::Wsl(options) => Arc::new(WslCommandHost {
            distro_name: options.distro_name.clone(),
            user: options.user.clone(),
        }),
        _ => Arc::new(RemoteCommandHost {
            path_style: connection.path_style(),
            connection,
        }),
    }
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
    fn local_host_runs_the_program_directly() {
        let mut spec = CommandSpec::new("docker");
        spec.args(["ps", "-a"]);

        assert_eq!(
            rendered(&LocalCommandHost.command(spec).expect("command builds")),
            ["docker", "ps", "-a"]
        );
    }

    #[test]
    fn only_local_hosts_skip_local_engine_match_verification() {
        let wsl_host = WslCommandHost {
            distro_name: "Ubuntu".into(),
            user: None,
        };

        assert!(!LocalCommandHost.requires_local_engine_match_verification());
        assert!(wsl_host.requires_local_engine_match_verification());
    }

    #[test]
    fn wsl_host_wraps_the_program() {
        let host = WslCommandHost {
            distro_name: "Ubuntu".into(),
            user: None,
        };
        let mut spec = CommandSpec::new("docker");
        spec.args(["ps", "-a"]);

        assert_eq!(
            rendered(&host.command(spec).expect("command builds")),
            ["wsl.exe", "-d", "Ubuntu", "--", "docker", "ps", "-a"]
        );
    }

    #[test]
    fn wsl_host_passes_environment_through_env() {
        // Variables set on the Windows-side process would not survive the hop
        // into the distribution, so they have to travel as arguments.
        let host = WslCommandHost {
            distro_name: "Ubuntu".into(),
            user: Some("test-user".into()),
        };
        let mut spec = CommandSpec::new("docker");
        spec.env("DOCKER_BUILDKIT", "0");
        spec.args(["compose", "build"]);

        assert_eq!(
            rendered(&host.command(spec).expect("command builds")),
            [
                "wsl.exe",
                "-d",
                "Ubuntu",
                "-u",
                "test-user",
                "--",
                "env",
                "DOCKER_BUILDKIT=0",
                "docker",
                "compose",
                "build"
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn wsl_host_maps_linux_paths_to_the_distro_unc_share() {
        let host = WslCommandHost {
            distro_name: "Ubuntu".into(),
            user: None,
        };

        assert_eq!(
            host.filesystem_path(Path::new(
                "/home/test-user/project/.devcontainer/devcontainer.json"
            )),
            PathBuf::from(
                r"\\wsl.localhost\Ubuntu\home\test-user\project\.devcontainer\devcontainer.json"
            )
        );
    }

    #[test]
    fn arguments_needing_quoting_survive_verbatim() {
        let host = WslCommandHost {
            distro_name: "Ubuntu".into(),
            user: None,
        };
        let mut spec = CommandSpec::new("docker");
        spec.args([
            "run",
            "--label",
            "devcontainer.local_folder=/home/test-user/example project",
        ]);

        assert_eq!(
            rendered(&host.command(spec).expect("command builds"))
                .last()
                .expect("command has arguments"),
            "devcontainer.local_folder=/home/test-user/example project"
        );
    }
}
