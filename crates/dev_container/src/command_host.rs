use std::collections::BTreeMap;
use std::fmt::Debug;
use std::path::{Path, PathBuf};

use remote::RemoteConnectionOptions;
use util::command::Command;

/// A command to run against the container engine, described as plain data.
///
/// The engine does not always live on the machine running Zed — for a project
/// opened over WSL it lives inside the distribution — so the whole invocation
/// (program, arguments and environment) has to be known before it can be
/// wrapped for its destination. Building a `Command` incrementally would leave
/// nothing left to wrap.
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
    fn command(&self, spec: CommandSpec) -> Command;

    /// Maps a path understood by the container engine to a path that the Zed
    /// client can use with its local filesystem implementation.
    fn filesystem_path(&self, path: &Path) -> PathBuf;

    /// Whether the host has posix user and path semantics.
    ///
    /// Several decisions — remapping the container user's uid, and the shape
    /// of the paths recorded in container labels — depend on the machine the
    /// engine runs on, not on the machine running Zed. Those coincide for a
    /// local project and diverge for a project opened in WSL.
    fn is_posix(&self) -> bool;
}

/// The engine runs on the same machine as Zed.
#[derive(Debug)]
pub(crate) struct LocalCommandHost;

impl CommandHost for LocalCommandHost {
    fn is_posix(&self) -> bool {
        cfg!(not(target_os = "windows"))
    }

    fn command(&self, spec: CommandSpec) -> Command {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        command
    }

    fn filesystem_path(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}

/// The engine is reached from inside a WSL distribution.
///
/// This is what makes a dev container for a project on the distro's ext4 come
/// out right: the daemon receives the project's native posix path as the bind
/// mount source and resolves it as a block device. Invoking the engine from
/// the Windows side instead would mean handing it a `\\wsl.localhost` or
/// `/mnt/c` path, which either fails or silently downgrades the mount to a
/// filesystem bridge.
#[derive(Debug)]
pub(crate) struct WslCommandHost {
    distro_name: String,
    user: Option<String>,
}

impl CommandHost for WslCommandHost {
    fn is_posix(&self) -> bool {
        true
    }

    fn command(&self, spec: CommandSpec) -> Command {
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
        command
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

/// Returns the host to drive the container engine through for a project on
/// `connection`, or `None` for a connection we cannot yet run commands on.
pub(crate) fn host_for_connection(
    connection: Option<&RemoteConnectionOptions>,
) -> Option<std::sync::Arc<dyn CommandHost>> {
    match connection {
        None => Some(std::sync::Arc::new(LocalCommandHost)),
        Some(RemoteConnectionOptions::Wsl(options)) => Some(std::sync::Arc::new(WslCommandHost {
            distro_name: options.distro_name.clone(),
            user: options.user.clone(),
        })),
        Some(_) => None,
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
            rendered(&LocalCommandHost.command(spec)),
            ["docker", "ps", "-a"]
        );
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
            rendered(&host.command(spec)),
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
            rendered(&host.command(spec)),
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
            rendered(&host.command(spec)).last().unwrap(),
            "devcontainer.local_folder=/home/test-user/example project"
        );
    }
}
