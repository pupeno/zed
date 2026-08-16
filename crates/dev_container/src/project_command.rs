//! Project-local and project-remote command construction.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use remote::{Interactive, RemoteConnection, RemoteConnectionOptions};
use util::command::Command;

/// Builds the local process Zed launches to run a command in a project.
///
/// The builder selects the project location when it is constructed. Local
/// projects launch the requested program directly; remote projects delegate
/// quoting, authentication, and process invocation to their connection.
///
/// Its interface deliberately follows `RemoteConnection::build_command` so
/// local and remote projects use the same command description.
pub(crate) struct ProjectCommandBuilder {
    connection: Option<Arc<dyn RemoteConnection>>,
}

impl ProjectCommandBuilder {
    /// Builds commands for a local project.
    pub(crate) fn local() -> Self {
        Self { connection: None }
    }

    /// Builds commands for a project reached through `connection`.
    pub(crate) fn for_remote_connection(connection: Arc<dyn RemoteConnection>) -> Self {
        Self {
            connection: Some(connection),
        }
    }

    /// Returns the selected remote connection's options, when the project is remote.
    pub(crate) fn connection_options(&self) -> Option<RemoteConnectionOptions> {
        self.connection
            .as_ref()
            .map(|connection| connection.connection_options())
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.connection.is_some()
    }

    /// Creates a temporary directory where project commands can use it.
    pub(crate) async fn project_temporary_directory(&self) -> Result<PathBuf> {
        let Some(connection) = &self.connection else {
            return Ok(std::env::temp_dir());
        };

        let command = if connection.path_style().is_posix() {
            let mut command = self.command("mktemp");
            command.arg("-d");
            command
        } else {
            let mut command = self.command("powershell");
            command.args([
                "-NoProfile",
                "-Command",
                "New-Item -ItemType Directory -Path (Join-Path $env:TEMP ('devcontainer-zed-' + [guid]::NewGuid())) | Select-Object -ExpandProperty FullName",
            ]);
            command
        };
        let output = command
            .build()
            .context("building a command to create a project temporary directory")?
            .output()
            .await
            .context("creating a project temporary directory")?;

        if !output.status.success() {
            bail!(
                "creating a project temporary directory failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let temporary_directory = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if temporary_directory.is_empty() {
            bail!("creating a project temporary directory returned no path");
        }

        Ok(PathBuf::from(temporary_directory))
    }

    /// Starts describing a command to run in the project.
    pub(crate) fn command(&self, program: impl AsRef<str>) -> ProjectCommand<'_> {
        ProjectCommand {
            builder: self,
            program: program.as_ref().to_string(),
            args: Vec::new(),
            environment: HashMap::default(),
        }
    }

    /// Builds the executable command Zed launches for a project command.
    ///
    /// The arguments intentionally match `RemoteConnection::build_command`.
    /// Local commands use the program, arguments, environment, and working
    /// directory directly. Remote connections apply their own transport
    /// semantics and return the local launcher Zed must execute.
    pub(crate) fn build_command(
        &self,
        program: Option<String>,
        args: &[String],
        environment: &HashMap<String, String>,
        working_dir: Option<String>,
        port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<Command> {
        match &self.connection {
            None => {
                let program =
                    program.ok_or_else(|| anyhow!("a local command requires a program"))?;
                let mut command = Command::new(program);
                command.args(args).envs(environment);
                if let Some(working_dir) = working_dir {
                    command.current_dir(working_dir);
                }
                Ok(command)
            }
            Some(connection) => {
                let template = connection.build_command(
                    program,
                    args,
                    environment,
                    working_dir,
                    port_forward,
                    interactive,
                )?;
                let mut command = Command::new(template.program);
                command.args(template.args).envs(template.env);
                Ok(command)
            }
        }
    }
}

/// A command described independently of the process used to launch it.
///
/// The description owns its program, arguments, and environment so the
/// project command builder can either launch it locally or delegate it to the
/// project's remote connection.
pub(crate) struct ProjectCommand<'a> {
    builder: &'a ProjectCommandBuilder,
    program: String,
    args: Vec<String>,
    environment: HashMap<String, String>,
}

impl ProjectCommand<'_> {
    pub(crate) fn arg(&mut self, argument: impl AsRef<str>) -> &mut Self {
        self.args.push(argument.as_ref().to_string());
        self
    }

    pub(crate) fn args<I, S>(&mut self, arguments: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.args.extend(
            arguments
                .into_iter()
                .map(|argument| argument.as_ref().to_string()),
        );
        self
    }

    pub(crate) fn env(&mut self, key: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
        self.environment
            .insert(key.as_ref().to_string(), value.as_ref().to_string());
        self
    }

    /// Builds the executable process command for this project command.
    pub(crate) fn build(&self) -> Result<Command> {
        self.builder.build_command(
            Some(self.program.clone()),
            &self.args,
            &self.environment,
            None,
            None,
            Interactive::No,
        )
    }
}
