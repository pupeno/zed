use crate::{Interactive, RemoteConnection, RemoteConnectionOptions};
use anyhow::{Context as _, Result, bail, ensure};
use collections::HashMap;
use futures::AsyncWriteExt as _;
use gpui::{App, AppContext as _, Task};
use std::{
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
};
use util::{
    command::{Child, Command, Stdio},
    paths::{PathStyle, RemotePathBuf},
};

/// The machine that owns a project and runs project-scoped tooling.
///
/// A project host is either the desktop itself or the SSH/WSL environment that
/// contains the project. It is deliberately distinct from a development
/// container: commands that prepare or manage a container must run on the
/// project host, using that host's filesystem and path conventions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectHostKind {
    /// The desktop on which Zed is running.
    Local,
    /// An environment reached through SSH.
    Ssh,
    /// A Windows Subsystem for Linux distribution.
    Wsl,
}

/// Filesystem details needed to construct paths for a [`ProjectHost`].
///
/// The host may use a path syntax different from the desktop's. Build paths
/// through this type when they will be sent to the host, rather than relying on
/// the local platform's [`std::path::PathBuf`] joining behavior.
#[derive(Clone, Debug)]
pub struct ProjectHostFilesystem {
    project_root: Arc<Path>,
    path_style: PathStyle,
}

impl ProjectHostFilesystem {
    /// Returns the root directory of the project on the host.
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Returns the path syntax used by the host.
    pub fn path_style(&self) -> PathStyle {
        self.path_style
    }

    /// Returns `path` rooted at the host project directory.
    ///
    /// Absolute paths are returned unchanged; relative paths are joined using
    /// the host's path syntax.
    pub fn project_path(&self, path: impl AsRef<Path>) -> RemotePathBuf {
        host_path(self.project_root(), path.as_ref(), self.path_style)
    }

    /// Returns `path` rooted at `temporary_root` using the host's path syntax.
    ///
    /// Absolute paths are returned unchanged.
    pub fn temporary_path(
        &self,
        temporary_root: impl AsRef<Path>,
        path: impl AsRef<Path>,
    ) -> RemotePathBuf {
        host_path(temporary_root.as_ref(), path.as_ref(), self.path_style)
    }
}

/// A command to run on a [`ProjectHost`].
///
/// The request owns its program, arguments, environment, and working
/// directory so it can be passed to either a local process or a remote command
/// template. The working directory must be expressed in the host's path style.
#[derive(Clone, Debug)]
pub struct HostProcessRequest {
    program: String,
    arguments: Vec<String>,
    environment: HashMap<String, String>,
    working_directory: PathBuf,
}

impl HostProcessRequest {
    /// Creates a request with no arguments or environment overrides.
    pub fn new(program: impl Into<String>, working_directory: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: HashMap::default(),
            working_directory: working_directory.into(),
        }
    }

    /// Sets the complete argument list for the command.
    pub fn arguments(mut self, arguments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the complete set of environment variables for the command.
    pub fn environment(
        mut self,
        environment: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Self {
        self.environment = environment
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        self
    }

    /// Returns the requested working directory on the host.
    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }
}

/// The exit status and captured output of a completed host process.
#[derive(Debug)]
pub struct HostProcessOutcome {
    /// The process's final exit status. A non-successful status is reported
    /// here rather than converted into an error.
    pub status: ExitStatus,
    /// All bytes written to standard output.
    pub stdout: Vec<u8>,
    /// All bytes written to standard error.
    pub stderr: Vec<u8>,
}

/// A running process on a [`ProjectHost`].
///
/// Standard input, output, and error are piped. Dropping this handle cancels
/// the child process; use [`Self::collect_output`] to wait for it and retain its
/// output.
pub struct HostProcess {
    child: Child,
}

impl HostProcess {
    /// Waits for the process and returns its exit status and captured output.
    ///
    /// A non-zero exit status is preserved in the outcome; only failures to
    /// wait for or collect from the process return an error.
    pub async fn collect_output(self) -> Result<HostProcessOutcome> {
        let output = self
            .child
            .output()
            .await
            .context("waiting for host process")?;
        Ok(HostProcessOutcome {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    /// Requests termination of the running process.
    pub async fn cancel(&mut self) -> Result<()> {
        self.child.kill().context("cancelling host process")
    }

    pub(crate) fn into_child(self) -> Child {
        self.child
    }
}

#[derive(Clone, Copy)]
enum PathKind {
    File,
    Directory,
}

impl PathKind {
    fn powershell_path_type(self) -> &'static str {
        match self {
            PathKind::File => "Leaf",
            PathKind::Directory => "Container",
        }
    }
}

/// Provides host-aware filesystem operations, command execution, and asset staging.
///
/// This is the boundary between desktop-local work and work that must happen
/// where the project lives. Its filesystem methods accept paths on the project
/// host, while [`Self::stage_assets`] accepts a desktop-local source and moves
/// it to a host destination.
pub struct ProjectHost {
    kind: ProjectHostKind,
    filesystem: ProjectHostFilesystem,
    connection: Option<Arc<dyn RemoteConnection>>,
}

impl ProjectHost {
    /// Creates a host for a project stored on the local desktop.
    pub fn local(project_root: impl Into<Arc<Path>>) -> Self {
        let project_root = project_root.into();
        Self {
            kind: ProjectHostKind::Local,
            filesystem: ProjectHostFilesystem {
                project_root,
                path_style: PathStyle::local(),
            },
            connection: None,
        }
    }

    /// Creates a host backed by an SSH or WSL connection.
    ///
    /// Other remote connection kinds cannot serve as project hosts and return
    /// an error.
    pub fn from_remote_connection(
        project_root: impl Into<Arc<Path>>,
        connection: Arc<dyn RemoteConnection>,
    ) -> Result<Self> {
        let kind = project_host_kind(&connection.connection_options())?;
        let project_root = project_root.into();
        let path_style = connection.path_style();
        Ok(Self {
            kind,
            filesystem: ProjectHostFilesystem {
                project_root,
                path_style,
            },
            connection: Some(connection),
        })
    }

    /// Creates a host from the connection currently held by `client`.
    ///
    /// Returns an error when the client is not connected or its connection
    /// kind cannot host a project.
    pub fn from_remote_client(
        project_root: impl Into<Arc<Path>>,
        client: &crate::RemoteClient,
    ) -> Result<Self> {
        let connection = client
            .connection()
            .context("remote project connection is not available")?;
        Self::from_remote_connection(project_root, connection)
    }

    /// Returns how this project host is reached.
    pub fn kind(&self) -> ProjectHostKind {
        self.kind
    }

    /// Returns filesystem details for building paths understood by this host.
    pub fn filesystem(&self) -> &ProjectHostFilesystem {
        &self.filesystem
    }

    /// Returns the project's root directory on this host.
    pub fn project_root(&self) -> &Path {
        self.filesystem.project_root()
    }

    /// Returns the remote connection options when this host is remote.
    pub fn remote_connection_options(&self) -> Option<RemoteConnectionOptions> {
        self.connection
            .as_ref()
            .map(|connection| connection.connection_options())
    }

    /// Determines the temporary directory root on the project host.
    ///
    /// For a local host this is the desktop process's temporary directory. For
    /// a remote host it is queried from that host's shell environment.
    pub async fn temporary_root(&self) -> Result<PathBuf> {
        if self.connection.is_none() {
            return Ok(std::env::temp_dir());
        }

        let request = match self.filesystem.path_style() {
            PathStyle::Unix => HostProcessRequest::new("sh", self.project_root())
                .arguments(["-c", "printf %s \"${TMPDIR:-/tmp}\""]),
            PathStyle::Windows => HostProcessRequest::new("cmd.exe", self.project_root())
                .arguments(["/D", "/S", "/C", "echo %TEMP%"]),
        };
        let outcome = self.start_process(request)?.collect_output().await?;
        ensure!(
            outcome.status.success(),
            "determining the project host temporary root failed: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        let temporary_root = String::from_utf8(outcome.stdout)
            .context("project host temporary root was not valid UTF-8")?;
        let temporary_root = temporary_root.trim();
        ensure!(
            !temporary_root.is_empty(),
            "project host did not report a temporary root"
        );
        Ok(PathBuf::from(temporary_root))
    }

    /// Reads a file that exists on the project host.
    ///
    /// `path` must use the project's host path convention.
    pub async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        if self.connection.is_none() {
            return std::fs::read(path)
                .with_context(|| format!("reading project host file {}", path.display()));
        }

        let request = match self.filesystem.path_style() {
            PathStyle::Unix => HostProcessRequest::new("cat", self.project_root())
                .arguments([path.display().to_string()]),
            PathStyle::Windows => HostProcessRequest::new("powershell.exe", self.project_root())
                .arguments([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$bytes = [IO.File]::ReadAllBytes($args[0]); [Console]::OpenStandardOutput().Write($bytes, 0, $bytes.Length)",
                    path.display().to_string().as_str(),
                ]),
        };
        let outcome = self.start_process(request)?.collect_output().await?;
        ensure!(
            outcome.status.success(),
            "reading project host file {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&outcome.stderr)
        );
        Ok(outcome.stdout)
    }

    /// Returns whether `path` is a regular file on the project host.
    pub async fn is_file(&self, path: &Path) -> Result<bool> {
        self.path_exists(path, PathKind::File).await
    }

    /// Returns whether `path` is a directory on the project host.
    pub async fn is_dir(&self, path: &Path) -> Result<bool> {
        self.path_exists(path, PathKind::Directory).await
    }

    async fn path_exists(&self, path: &Path, kind: PathKind) -> Result<bool> {
        if self.connection.is_none() {
            return Ok(match kind {
                PathKind::File => path.is_file(),
                PathKind::Directory => path.is_dir(),
            });
        }

        let path = path.display().to_string();
        let request = match self.filesystem.path_style() {
            PathStyle::Unix => {
                let test = match kind {
                    PathKind::File => "-f",
                    PathKind::Directory => "-d",
                };
                HostProcessRequest::new("test", self.project_root()).arguments([test, path.as_str()])
            }
            PathStyle::Windows => HostProcessRequest::new("powershell.exe", self.project_root())
                .arguments([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "if (Test-Path -LiteralPath $args[0] -PathType $args[1]) { exit 0 } else { exit 1 }",
                    path.as_str(),
                    kind.powershell_path_type(),
                ]),
        };
        let outcome = self.start_process(request)?.collect_output().await?;
        Ok(outcome.status.success())
    }

    /// Copies the contents of a host directory into another host directory.
    ///
    /// This is a host-to-host filesystem operation. To move assets from the
    /// desktop onto the host, use [`Self::stage_assets`] instead.
    pub async fn copy_dir(&self, source: &Path, destination: &Path) -> Result<()> {
        if self.connection.is_none() {
            return copy_directory(source, destination).with_context(|| {
                format!(
                    "copying project host directory {} to {}",
                    source.display(),
                    destination.display()
                )
            });
        }

        let source = source.display().to_string();
        let destination = destination.display().to_string();
        let request = match self.filesystem.path_style() {
            PathStyle::Unix => HostProcessRequest::new("sh", self.project_root()).arguments([
                "-c",
                "mkdir -p \"$2\" && cp -R \"$1/.\" \"$2\"",
                "project-host",
                source.as_str(),
                destination.as_str(),
            ]),
            PathStyle::Windows => HostProcessRequest::new("powershell.exe", self.project_root())
                .arguments([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[IO.Directory]::CreateDirectory($args[1]) | Out-Null; Copy-Item -LiteralPath (Join-Path $args[0] '*') -Destination $args[1] -Recurse -Force",
                    source.as_str(),
                    destination.as_str(),
                ]),
        };
        let outcome = self.start_process(request)?.collect_output().await?;
        ensure!(
            outcome.status.success(),
            "copying project host directory {source} to {destination} failed: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        Ok(())
    }

    /// Creates a directory and any missing parent directories on the project host.
    pub async fn create_dir_all(&self, path: &Path) -> Result<()> {
        if self.connection.is_none() {
            return std::fs::create_dir_all(path)
                .with_context(|| format!("creating project host directory {}", path.display()));
        }

        let request = match self.filesystem.path_style() {
            PathStyle::Unix => HostProcessRequest::new("mkdir", self.project_root())
                .arguments(["-p", path.display().to_string().as_str()]),
            PathStyle::Windows => HostProcessRequest::new("powershell.exe", self.project_root())
                .arguments([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "[IO.Directory]::CreateDirectory($args[0]) | Out-Null",
                    path.display().to_string().as_str(),
                ]),
        };
        let outcome = self.start_process(request)?.collect_output().await?;
        ensure!(
            outcome.status.success(),
            "creating project host directory {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&outcome.stderr)
        );
        Ok(())
    }

    /// Writes `content` to a file on the project host, replacing it if present.
    pub async fn write_file(&self, path: &Path, content: &[u8]) -> Result<()> {
        if self.connection.is_none() {
            return std::fs::write(path, content)
                .with_context(|| format!("writing project host file {}", path.display()));
        }

        let path = path.display().to_string();
        let request = match self.filesystem.path_style() {
            PathStyle::Unix => HostProcessRequest::new("sh", self.project_root()).arguments([
                "-c",
                "cat > \"$1\"",
                "project-host",
                path.as_str(),
            ]),
            PathStyle::Windows => HostProcessRequest::new("powershell.exe", self.project_root())
                .arguments([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$input = [Console]::OpenStandardInput(); $output = [IO.File]::Open($args[0], [IO.FileMode]::Create); $input.CopyTo($output); $output.Dispose()",
                    path.as_str(),
                ]),
        };
        let mut child = self.start_process(request)?.into_child();
        {
            let stdin = child
                .stdin
                .as_mut()
                .context("host process did not provide standard input")?;
            stdin
                .write_all(content)
                .await
                .context("writing project host file contents")?;
            stdin
                .flush()
                .await
                .context("flushing project host file contents")?;
        }
        child.stdin.take();
        let output = child
            .output()
            .await
            .context("waiting for project host file write")?;
        ensure!(
            output.status.success(),
            "writing project host file {path} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    /// Starts a command on the project host with piped standard streams.
    ///
    /// The process is killed if the returned [`HostProcess`] is dropped before
    /// completion. Use [`HostProcess::collect_output`] to wait for it, or
    /// [`HostProcess::into_child`] for callers that must stream its pipes.
    pub fn start_process(&self, request: HostProcessRequest) -> Result<HostProcess> {
        let mut command = if self.connection.is_some() {
            let template = self.build_command(request, Interactive::No)?;
            command_from_template(template)
        } else {
            let mut command = Command::new(request.program);
            command
                .args(request.arguments)
                .envs(request.environment)
                .current_dir(request.working_directory);
            command
        };

        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().context("starting host process")?;
        Ok(HostProcess { child })
    }

    /// Builds the remote command template for a host process request.
    ///
    /// This exposes the connection-specific command construction for callers
    /// that need to launch the process themselves. It returns an error for a
    /// local host; use [`Self::start_process`] when a ready-to-run process is
    /// needed instead.
    pub fn build_command(
        &self,
        request: HostProcessRequest,
        interactive: Interactive,
    ) -> Result<crate::CommandTemplate> {
        let connection = self
            .connection
            .as_ref()
            .context("local project hosts do not build remote commands")?;
        connection.build_command(
            Some(request.program),
            &request.arguments,
            &request.environment,
            Some(request.working_directory.display().to_string()),
            None,
            interactive,
        )
    }

    /// Stages desktop-local directory contents at `host_destination`.
    ///
    /// Remote hosts upload through their connection; local hosts copy in a
    /// background task. `desktop_source` is always interpreted on the desktop,
    /// while `host_destination` must use the project's host path convention.
    pub fn stage_assets(
        &self,
        desktop_source: PathBuf,
        host_destination: PathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        if let Some(connection) = &self.connection {
            return connection.upload_directory(
                desktop_source,
                host_staging_destination(host_destination, self.filesystem.path_style()),
                cx,
            );
        }

        cx.background_spawn(async move {
            copy_directory(&desktop_source, &host_destination).with_context(|| {
                format!(
                    "staging desktop assets from {} to {}",
                    desktop_source.display(),
                    host_destination.display()
                )
            })
        })
    }
}

fn command_from_template(template: crate::CommandTemplate) -> Command {
    let mut command = Command::new(template.program);
    command.args(template.args).envs(template.env);
    command
}

fn project_host_kind(connection: &RemoteConnectionOptions) -> Result<ProjectHostKind> {
    match connection {
        RemoteConnectionOptions::Ssh(_) => Ok(ProjectHostKind::Ssh),
        RemoteConnectionOptions::Wsl(_) => Ok(ProjectHostKind::Wsl),
        options => bail!(
            "{} connections cannot be used as project hosts",
            options.connection_type()
        ),
    }
}

fn host_staging_destination(path: PathBuf, path_style: PathStyle) -> RemotePathBuf {
    RemotePathBuf::new(path.display().to_string(), path_style)
}

fn host_path(base: &Path, path: &Path, path_style: PathStyle) -> RemotePathBuf {
    let base = base.display().to_string();
    let path = path.display().to_string();
    if path_style.is_absolute(&path) {
        return RemotePathBuf::new(path, path_style);
    }
    let path = path_style
        .join(base, &path)
        .map(|path| path_style.normalize(&path))
        .unwrap_or(path);
    RemotePathBuf::new(path, path_style)
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    let mut directories = vec![(source.to_path_buf(), destination.to_path_buf())];
    while let Some((source, destination)) = directories.pop() {
        std::fs::create_dir_all(&destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                directories.push((source_path, destination_path));
            } else {
                std::fs::copy(source_path, destination_path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn local_host_uses_the_requested_working_directory() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let request = if cfg!(windows) {
            HostProcessRequest::new("cmd.exe", host.project_root()).arguments(["/C", "echo %CD%"])
        } else {
            HostProcessRequest::new("sh", host.project_root()).arguments(["-c", "pwd"])
        };

        let outcome = smol::block_on(host.start_process(request)?.collect_output())?;

        assert!(outcome.status.success());
        assert_eq!(
            String::from_utf8(outcome.stdout)?.trim(),
            project_directory.path().display().to_string()
        );
        Ok(())
    }

    #[gpui::test]
    async fn local_host_stages_assets_at_the_requested_destination(cx: &mut gpui::TestAppContext) {
        let result = async {
            let desktop_assets = tempdir()?;
            let host_root = tempdir()?;
            fs::write(desktop_assets.path().join("feature.json"), "asset")?;
            let host = ProjectHost::local(host_root.path());

            let stage = cx.update(|cx| {
                host.stage_assets(
                    desktop_assets.path().to_path_buf(),
                    host_root.path().join("staged-assets"),
                    cx,
                )
            });
            stage.await?;

            assert_eq!(
                fs::read_to_string(host_root.path().join("staged-assets/feature.json"))?,
                "asset"
            );
            Result::<()>::Ok(())
        }
        .await;
        assert!(result.is_ok(), "staging assets failed: {result:?}");
    }

    #[test]
    fn local_host_exposes_desktop_temporary_root() -> Result<()> {
        let host = ProjectHost::local(PathBuf::from("/project"));

        assert_eq!(host.kind(), ProjectHostKind::Local);
        assert_eq!(smol::block_on(host.temporary_root())?, std::env::temp_dir());
        Ok(())
    }

    #[test]
    fn local_host_performs_filesystem_operations() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let generated_directory = project_directory.path().join(".devcontainer/generated");
        let generated_file = generated_directory.join("compose.yaml");

        smol::block_on(async {
            host.create_dir_all(&generated_directory).await?;
            host.write_file(&generated_file, b"services: {}\n").await?;
            let contents = host.read_file(&generated_file).await?;
            assert_eq!(contents, b"services: {}\n");
            Result::<()>::Ok(())
        })
    }

    #[test]
    fn local_host_reports_path_kinds_and_copies_host_directories() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let feature_source = project_directory.path().join(".devcontainer/local-feature");
        let feature_destination = project_directory.path().join("staged/local-feature");

        smol::block_on(async {
            host.create_dir_all(&feature_source.join("nested")).await?;
            host.write_file(
                &feature_source.join("install.sh"),
                b"#!/bin/sh
",
            )
            .await?;
            host.write_file(&feature_source.join("nested/extra"), b"extra")
                .await?;

            assert!(host.is_dir(&feature_source).await?);
            assert!(!host.is_file(&feature_source).await?);
            assert!(host.is_file(&feature_source.join("install.sh")).await?);
            assert!(!host.is_dir(&feature_source.join("install.sh")).await?);
            assert!(!host.is_file(&feature_source.join("missing")).await?);

            host.copy_dir(&feature_source, &feature_destination).await?;

            assert_eq!(
                host.read_file(&feature_destination.join("install.sh"))
                    .await?,
                b"#!/bin/sh
"
            );
            assert_eq!(
                host.read_file(&feature_destination.join("nested/extra"))
                    .await?,
                b"extra"
            );
            Result::<()>::Ok(())
        })
    }

    #[test]
    fn local_host_returns_unsuccessful_process_outcomes() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let request = if cfg!(windows) {
            HostProcessRequest::new("cmd.exe", host.project_root()).arguments(["/C", "exit 7"])
        } else {
            HostProcessRequest::new("sh", host.project_root()).arguments(["-c", "exit 7"])
        };

        let outcome = smol::block_on(host.start_process(request)?.collect_output())?;

        assert_eq!(outcome.status.code(), Some(7));
        Ok(())
    }

    #[test]
    fn cancelling_a_host_process_stops_it() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let request = if cfg!(windows) {
            HostProcessRequest::new("cmd.exe", host.project_root())
                .arguments(["/C", "ping -n 30 127.0.0.1 > NUL"])
        } else {
            HostProcessRequest::new("sh", host.project_root()).arguments(["-c", "sleep 30"])
        };

        let mut process = host.start_process(request)?;
        let outcome = smol::block_on(async {
            process.cancel().await?;
            process.collect_output().await
        })?;

        assert!(!outcome.status.success());
        Ok(())
    }

    #[test]
    fn host_paths_use_the_project_host_path_style() {
        let unix_filesystem = ProjectHostFilesystem {
            project_root: PathBuf::from("/project").into(),
            path_style: PathStyle::Unix,
        };
        assert_eq!(
            unix_filesystem
                .project_path(".devcontainer/devcontainer.json")
                .to_string(),
            "/project/.devcontainer/devcontainer.json"
        );

        let windows_filesystem = ProjectHostFilesystem {
            project_root: PathBuf::from(r"C:\project").into(),
            path_style: PathStyle::Windows,
        };
        assert_eq!(
            windows_filesystem
                .temporary_path(r"C:\Temp", "compose.yaml")
                .to_string(),
            r"C:\Temp\compose.yaml"
        );
    }

    #[test]
    fn staging_destination_keeps_the_host_path() {
        assert_eq!(
            host_staging_destination(PathBuf::from("/tmp/assets"), PathStyle::Unix).to_string(),
            "/tmp/assets"
        );
    }

    #[test]
    fn ssh_and_wsl_connections_select_the_matching_project_host_kind() -> Result<()> {
        assert_eq!(
            project_host_kind(&RemoteConnectionOptions::Ssh(Default::default()))?,
            ProjectHostKind::Ssh
        );
        assert_eq!(
            project_host_kind(&RemoteConnectionOptions::Wsl(crate::WslConnectionOptions {
                distro_name: "Ubuntu".to_string(),
                user: None,
            }))?,
            ProjectHostKind::Wsl
        );
        Ok(())
    }

    #[test]
    fn remote_layer_can_access_raw_host_process_pipes() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let request = if cfg!(windows) {
            HostProcessRequest::new("cmd.exe", host.project_root()).arguments(["/C", "more"])
        } else {
            HostProcessRequest::new("cat", host.project_root())
        };
        let mut child = host.start_process(request)?.into_child();
        smol::block_on(async {
            let stdin = child
                .stdin
                .as_mut()
                .context("host process did not provide standard input")?;
            stdin.write_all(b"host pipe").await?;
            child.stdin.take();
            let output = child.output().await?;
            ensure!(output.status.success(), "host process failed");
            Result::<()>::Ok(())
        })
    }
}
