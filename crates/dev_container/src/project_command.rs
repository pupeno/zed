//! Project-local and project-remote command construction.
//!
//! Its API follows `remote::RemoteConnection::build_command` so local and
//! remote projects accept the same command description.

use std::sync::Arc;

use anyhow::{Result, anyhow};
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
