use crate::{Interactive, RemoteConnection, RemoteConnectionOptions, RemoteOs};
use anyhow::{Context as _, Result, bail, ensure};
use collections::HashMap;
use futures::AsyncWriteExt as _;
use gpui::{App, AppContext as _, Task};
use std::{
    ffi::OsStr,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
};
use util::{
    command::{Child, Command, Stdio},
    normalize_path,
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

/// The operating system family of a project host.
///
/// Reported by the host itself so that host-shaped decisions are made from the
/// machine that owns the project rather than from the desktop's build target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectHostPlatform {
    Linux,
    MacOs,
    Windows,
}

impl ProjectHostPlatform {
    /// The platform of the desktop running this process. It is the project
    /// host's platform only when the project host is the desktop itself.
    fn desktop() -> Self {
        match std::env::consts::OS {
            "windows" => Self::Windows,
            "macos" => Self::MacOs,
            _ => Self::Linux,
        }
    }

    pub fn is_windows(self) -> bool {
        self == Self::Windows
    }
}

impl From<RemoteOs> for ProjectHostPlatform {
    fn from(operating_system: RemoteOs) -> Self {
        match operating_system {
            RemoteOs::Linux => Self::Linux,
            RemoteOs::MacOs => Self::MacOs,
            RemoteOs::Windows => Self::Windows,
        }
    }
}

/// A path expressed in the path rules of the machine that owns it.
///
/// [`Path`] answers absoluteness, root, prefix, and separator questions with the
/// rules of the platform Zed was built for, which are the wrong rules for a
/// project living on another machine: on a Windows desktop `/home/user/project`
/// is neither absolute nor rooted, and joining onto it produces backslashes the
/// host cannot resolve. A `HostPathBuf` carries the host's [`PathStyle`] with
/// the path text and answers every such question through it.
///
/// Values are normalized in the host's rules on construction, so two paths that
/// name the same host location compare and hash equal.
#[derive(Clone, Debug)]
pub struct HostPathBuf {
    path: String,
    style: PathStyle,
    native_path: Option<PathBuf>,
}

impl HostPathBuf {
    pub fn new(path: impl AsRef<str>, style: PathStyle) -> Self {
        Self {
            path: style.normalize(path.as_ref()),
            style,
            native_path: None,
        }
    }

    /// Reinterprets a [`Path`] value that already holds host path text.
    ///
    /// The text is preserved verbatim; only its interpretation changes. Use this
    /// where a host path arrives as a `Path` — a project root taken from a
    /// worktree, say — never to move a genuinely desktop-local path onto a host.
    pub fn from_path(path: impl AsRef<Path>, style: PathStyle) -> Self {
        let path = path.as_ref();
        let mut host_path = Self::new(path.to_string_lossy(), style);
        if style == PathStyle::local() {
            host_path.native_path = Some(normalize_path(path));
        }
        host_path
    }

    pub fn path_style(&self) -> PathStyle {
        self.style
    }

    pub fn as_str(&self) -> &str {
        &self.path
    }

    pub fn is_absolute(&self) -> bool {
        self.style.is_absolute(&self.path)
    }

    /// The root is `/` on a Unix host, and a drive (`C:\`), a UNC share
    /// (`\\server\share\`), or a bare root (`\`) on a Windows host.
    pub fn root(&self) -> Option<Self> {
        if !self.is_absolute() {
            return None;
        }
        let root = match self.style {
            PathStyle::Unix => "/".to_string(),
            PathStyle::Windows => {
                if let Some(share) = self.path.strip_prefix("\\\\") {
                    let mut components = share.split('\\');
                    match (components.next(), components.next()) {
                        (Some(server), Some(share)) => format!("\\\\{server}\\{share}\\"),
                        _ => self.path.clone(),
                    }
                } else if self.path.as_bytes().get(1) == Some(&b':') {
                    format!("{}\\", &self.path[..2])
                } else {
                    "\\".to_string()
                }
            }
        };
        Some(Self::new(root, self.style))
    }

    pub fn is_root(&self) -> bool {
        self.root().as_ref() == Some(self)
    }

    /// Resolves `path` against this one using the host's rules.
    ///
    /// A `path` the host reads as absolute replaces this one; otherwise it is
    /// appended with the host's separator and the result normalized. This is the
    /// only correct way to root a configuration-relative path at a project root,
    /// because both operands are read with the host's rules rather than the
    /// desktop's.
    pub fn join(&self, path: impl AsRef<str>) -> Self {
        let path = path.as_ref();
        if let Some(native_path) = &self.native_path {
            return Self::from_path(native_path.join(path), self.style);
        }
        if self.style.is_absolute(path) {
            return Self::new(path, self.style);
        }
        match self.style.join(&self.path, path) {
            Some(joined) => Self::new(joined, self.style),
            None => Self::new(path, self.style),
        }
    }

    /// Relative worktree paths arrive in the desktop's separators even when
    /// their destination is a differently styled project host.
    pub fn join_relative_path(&self, path: &Path, source_style: PathStyle) -> Self {
        let normalized = source_style.normalize(&path.to_string_lossy());
        let translated = normalized
            .split(source_style.separators_ch())
            .collect::<Vec<_>>()
            .join(self.style.primary_separator());
        self.join(translated)
    }

    /// A root has no parent, and neither does a single-component relative path.
    pub fn parent(&self) -> Option<Self> {
        if let Some(native_path) = &self.native_path {
            return native_path
                .parent()
                .map(|parent| Self::from_path(parent, self.style));
        }
        let root = self.root();
        if root.as_ref() == Some(self) {
            return None;
        }
        let (head, _) = self.path.rsplit_once(self.style.separators_ch())?;
        let parent = Self::new(head, self.style);
        match root {
            // Splitting can cut into the root itself: the head of `/foo` is ``
            // and the head of `C:\foo` is `C:`, neither of which names the
            // directory that contains it. Both are the root.
            Some(root) if parent.path.len() < root.path.len() => Some(root),
            _ => Some(parent),
        }
    }

    pub fn file_name(&self) -> Option<&OsStr> {
        if let Some(native_path) = &self.native_path {
            return native_path.file_name();
        }
        if self.is_root() {
            return None;
        }
        let name = match self.path.rsplit_once(self.style.separators_ch()) {
            Some((_, name)) => name,
            None => self.path.as_str(),
        };
        (!name.is_empty()).then_some(OsStr::new(name))
    }

    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.style == prefix.style
            && self
                .style
                .strip_prefix(Path::new(&self.path), Path::new(&prefix.path))
                .is_some()
    }

    /// Returns the path as a desktop [`PathBuf`].
    ///
    /// Meaningful only for a local project host, where the desktop's rules are
    /// the host's rules, or to hand the text to an API that insists on a `Path`
    /// without interpreting it.
    pub fn to_path_buf(&self) -> PathBuf {
        self.native_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.path))
    }

    pub fn to_remote_path(&self) -> RemotePathBuf {
        RemotePathBuf::new(self.path.clone(), self.style)
    }
}

impl PartialEq for HostPathBuf {
    fn eq(&self, other: &Self) -> bool {
        self.style == other.style
            && if self.style == PathStyle::local() {
                self.to_path_buf() == other.to_path_buf()
            } else {
                self.path == other.path
            }
    }
}

impl Eq for HostPathBuf {}

impl Hash for HostPathBuf {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.style.hash(state);
        if self.style == PathStyle::local() {
            self.to_path_buf().hash(state);
        } else {
            self.path.hash(state);
        }
    }
}

impl std::fmt::Display for HostPathBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.path)
    }
}

/// The serialized form records the host's path rules alongside the text, so a
/// persisted host path reads back the same way on any desktop.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SerializedPathStyle {
    Unix,
    Windows,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SerializedHostPath {
    path: String,
    style: SerializedPathStyle,
}

impl serde::Serialize for HostPathBuf {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SerializedHostPath {
            path: self.path.clone(),
            style: match self.style {
                PathStyle::Unix => SerializedPathStyle::Unix,
                PathStyle::Windows => SerializedPathStyle::Windows,
            },
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for HostPathBuf {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let serialized = SerializedHostPath::deserialize(deserializer)?;
        Ok(Self::new(
            serialized.path,
            match serialized.style {
                SerializedPathStyle::Unix => PathStyle::Unix,
                SerializedPathStyle::Windows => PathStyle::Windows,
            },
        ))
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
    working_directory: HostPathBuf,
}

impl HostProcessRequest {
    /// Creates a request with no arguments or environment overrides.
    pub fn new(program: impl Into<String>, working_directory: HostPathBuf) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: HashMap::default(),
            working_directory,
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
    pub fn working_directory(&self) -> &HostPathBuf {
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
    platform: ProjectHostPlatform,
    project_root: HostPathBuf,
    connection: Option<Arc<dyn RemoteConnection>>,
}

impl ProjectHost {
    /// Creates a host for a project stored on the local desktop.
    pub fn local(project_root: impl AsRef<Path>) -> Self {
        Self {
            kind: ProjectHostKind::Local,
            platform: ProjectHostPlatform::desktop(),
            project_root: HostPathBuf::from_path(project_root, PathStyle::local()),
            connection: None,
        }
    }

    /// Creates a host backed by an SSH or WSL connection.
    ///
    /// Other remote connection kinds cannot serve as project hosts and return
    /// an error.
    pub fn from_remote_connection(
        project_root: HostPathBuf,
        connection: Arc<dyn RemoteConnection>,
    ) -> Result<Self> {
        let kind = project_host_kind(&connection.connection_options())?;
        let platform: ProjectHostPlatform = connection.remote_platform().os.into();
        ensure!(
            project_root.path_style() == connection.path_style(),
            "project root path style does not match the project host connection"
        );
        ensure!(
            platform.is_windows() == connection.path_style().is_windows(),
            "project host platform and path style disagree"
        );
        Ok(Self {
            kind,
            platform,
            project_root,
            connection: Some(connection),
        })
    }

    /// Creates a host from the connection currently held by `client`.
    ///
    /// `project_root` holds the project's path on the host; it is reinterpreted
    /// with that host's path rules rather than the desktop's.
    ///
    /// Returns an error when the client is not connected or its connection
    /// kind cannot host a project.
    pub fn from_remote_client(
        project_root: impl AsRef<Path>,
        client: &crate::RemoteClient,
    ) -> Result<Self> {
        let connection = client
            .connection()
            .context("remote project connection is not available")?;
        let project_root = HostPathBuf::from_path(project_root, connection.path_style());
        Self::from_remote_connection(project_root, connection)
    }

    /// Returns how this project host is reached.
    pub fn kind(&self) -> ProjectHostKind {
        self.kind
    }

    pub fn platform(&self) -> ProjectHostPlatform {
        self.platform
    }

    pub fn path_style(&self) -> PathStyle {
        self.project_root.path_style()
    }

    /// Returns the project's root directory on this host.
    pub fn project_root(&self) -> &HostPathBuf {
        &self.project_root
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
    pub async fn temporary_root(&self) -> Result<HostPathBuf> {
        if self.connection.is_none() {
            return Ok(HostPathBuf::from_path(
                std::env::temp_dir(),
                self.path_style(),
            ));
        }

        let request = match self.path_style() {
            PathStyle::Unix => self
                .request_at_root("sh")
                .arguments(["-c", "printf %s \"${TMPDIR:-/tmp}\""]),
            PathStyle::Windows => {
                self.request_at_root("cmd.exe")
                    .arguments(["/D", "/S", "/C", "echo %TEMP%"])
            }
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
        Ok(HostPathBuf::new(temporary_root, self.path_style()))
    }

    /// Reads a file that exists on the project host.
    pub async fn read_file(&self, path: &HostPathBuf) -> Result<Vec<u8>> {
        if self.connection.is_none() {
            return std::fs::read(path.to_path_buf())
                .with_context(|| format!("reading project host file {path}"));
        }

        let request = match self.path_style() {
            PathStyle::Unix => self.request_at_root("cat").arguments([path.as_str()]),
            PathStyle::Windows => self
                .request_at_root("powershell.exe")
                .arguments([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$bytes = [IO.File]::ReadAllBytes($args[0]); [Console]::OpenStandardOutput().Write($bytes, 0, $bytes.Length)",
                    path.as_str(),
                ]),
        };
        let outcome = self.start_process(request)?.collect_output().await?;
        ensure!(
            outcome.status.success(),
            "reading project host file {path} failed: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        Ok(outcome.stdout)
    }

    /// Returns whether `path` is a regular file on the project host.
    pub async fn is_file(&self, path: &HostPathBuf) -> Result<bool> {
        self.path_exists(path, PathKind::File).await
    }

    /// Returns whether `path` is a directory on the project host.
    pub async fn is_dir(&self, path: &HostPathBuf) -> Result<bool> {
        self.path_exists(path, PathKind::Directory).await
    }

    async fn path_exists(&self, path: &HostPathBuf, kind: PathKind) -> Result<bool> {
        if self.connection.is_none() {
            let path = path.to_path_buf();
            return Ok(match kind {
                PathKind::File => path.is_file(),
                PathKind::Directory => path.is_dir(),
            });
        }

        let request = match self.path_style() {
            PathStyle::Unix => {
                let test = match kind {
                    PathKind::File => "-f",
                    PathKind::Directory => "-d",
                };
                self.request_at_root("test").arguments([test, path.as_str()])
            }
            PathStyle::Windows => self
                .request_at_root("powershell.exe")
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
    pub async fn copy_dir(&self, source: &HostPathBuf, destination: &HostPathBuf) -> Result<()> {
        if self.connection.is_none() {
            return copy_directory(&source.to_path_buf(), &destination.to_path_buf()).with_context(
                || format!("copying project host directory {source} to {destination}"),
            );
        }

        let request = match self.path_style() {
            PathStyle::Unix => self.request_at_root("sh").arguments([
                "-c",
                "mkdir -p \"$2\" && cp -R \"$1/.\" \"$2\"",
                "project-host",
                source.as_str(),
                destination.as_str(),
            ]),
            PathStyle::Windows => self
                .request_at_root("powershell.exe")
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
    pub async fn create_dir_all(&self, path: &HostPathBuf) -> Result<()> {
        if self.connection.is_none() {
            return std::fs::create_dir_all(path.to_path_buf())
                .with_context(|| format!("creating project host directory {path}"));
        }

        let request = match self.path_style() {
            PathStyle::Unix => self
                .request_at_root("mkdir")
                .arguments(["-p", path.as_str()]),
            PathStyle::Windows => self.request_at_root("powershell.exe").arguments([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "[IO.Directory]::CreateDirectory($args[0]) | Out-Null",
                path.as_str(),
            ]),
        };
        let outcome = self.start_process(request)?.collect_output().await?;
        ensure!(
            outcome.status.success(),
            "creating project host directory {path} failed: {}",
            String::from_utf8_lossy(&outcome.stderr)
        );
        Ok(())
    }

    /// Writes `content` to a file on the project host, replacing it if present.
    pub async fn write_file(&self, path: &HostPathBuf, content: &[u8]) -> Result<()> {
        if self.connection.is_none() {
            return std::fs::write(path.to_path_buf(), content)
                .with_context(|| format!("writing project host file {path}"));
        }

        let request = match self.path_style() {
            PathStyle::Unix => self.request_at_root("sh").arguments([
                "-c",
                "cat > \"$1\"",
                "project-host",
                path.as_str(),
            ]),
            PathStyle::Windows => self
                .request_at_root("powershell.exe")
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

    fn request_at_root(&self, program: impl Into<String>) -> HostProcessRequest {
        HostProcessRequest::new(program, self.project_root.clone())
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
                .current_dir(request.working_directory.to_path_buf());
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
            Some(request.working_directory.as_str().to_string()),
            None,
            interactive,
        )
    }

    /// Stages desktop-local directory contents at `host_destination`.
    ///
    /// Remote hosts upload through their connection; local hosts copy in a
    /// background task. `desktop_source` is always interpreted on the desktop,
    /// while `host_destination` is read with the project host's path rules.
    pub fn stage_assets(
        &self,
        desktop_source: PathBuf,
        host_destination: HostPathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        if let Some(connection) = &self.connection {
            return connection.upload_directory(
                desktop_source,
                host_destination.to_remote_path(),
                cx,
            );
        }

        cx.background_spawn(async move {
            copy_directory(&desktop_source, &host_destination.to_path_buf()).with_context(|| {
                format!(
                    "staging desktop assets from {} to {host_destination}",
                    desktop_source.display(),
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
    use crate::{
        CommandTemplate, RemoteArch, RemoteClientDelegate, RemotePlatform, WslConnectionOptions,
    };
    use async_trait::async_trait;
    use futures::channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender};
    use gpui::AsyncApp;
    use rpc::proto::Envelope;
    use std::{
        fs,
        sync::{Mutex, MutexGuard},
    };
    use tempfile::tempdir;

    fn unix(path: &str) -> HostPathBuf {
        HostPathBuf::new(path, PathStyle::Unix)
    }

    fn windows(path: &str) -> HostPathBuf {
        HostPathBuf::new(path, PathStyle::Windows)
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        match mutex.lock() {
            Ok(value) => value,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[test]
    fn absoluteness_is_answered_by_the_host_not_the_desktop() {
        // `Path::is_absolute` reports false for this on a Windows desktop.
        assert!(unix("/home/user/project").is_absolute());
        assert!(!unix("build/context").is_absolute());
        assert!(windows(r"C:\project").is_absolute());
        assert!(windows(r"\\server\share\project").is_absolute());
        assert!(!windows(r"build\context").is_absolute());
    }

    #[test]
    fn roots_are_derived_with_the_host_path_rules() {
        assert_eq!(unix("/home/user/project").root(), Some(unix("/")));
        assert!(unix("/").is_root());
        assert_eq!(unix("build/context").root(), None);

        assert_eq!(windows(r"C:\work\project").root(), Some(windows(r"C:\")));
        assert!(windows(r"C:\").is_root());
        assert_eq!(
            windows(r"\\server\share\project").root(),
            Some(windows(r"\\server\share\"))
        );
        assert_eq!(windows(r"build\context").root(), None);
    }

    #[test]
    fn joining_uses_the_host_separator_and_absolute_paths_replace_the_base() {
        assert_eq!(
            unix("/work/project")
                .join(".devcontainer/devcontainer.json")
                .as_str(),
            "/work/project/.devcontainer/devcontainer.json"
        );
        assert_eq!(
            unix("/work/project").join("/etc/hosts").as_str(),
            "/etc/hosts"
        );
        assert_eq!(
            windows(r"C:\work\project").join("build/context").as_str(),
            r"C:\work\project\build\context"
        );
        assert_eq!(
            windows(r"C:\work\project").join(r"D:\other").as_str(),
            r"D:\other"
        );
    }

    #[test]
    fn desktop_relative_paths_are_resolved_with_the_host_separator() {
        assert_eq!(
            unix("/work/project")
                .join_relative_path(
                    Path::new(r".devcontainer\devcontainer.json"),
                    PathStyle::Windows,
                )
                .as_str(),
            "/work/project/.devcontainer/devcontainer.json"
        );
    }

    #[test]
    fn components_are_derived_with_the_host_path_rules() {
        let configuration = unix("/work/project/.devcontainer/devcontainer.json");
        assert_eq!(
            configuration.file_name(),
            Some(OsStr::new("devcontainer.json"))
        );
        assert_eq!(
            configuration.parent(),
            Some(unix("/work/project/.devcontainer"))
        );
        assert_eq!(unix("/work").parent(), Some(unix("/")));
        assert_eq!(unix("/").parent(), None);
        assert_eq!(unix("/").file_name(), None);

        assert_eq!(
            windows(r"C:\work\project").parent(),
            Some(windows(r"C:\work"))
        );
        assert_eq!(windows(r"C:\work").parent(), Some(windows(r"C:\")));
        assert_eq!(windows(r"C:\").parent(), None);

        // A backslash is an ordinary character on a Unix host; a desktop `Path`
        // on Windows would split this into two components.
        assert_eq!(unix(r"/work/a\b").file_name(), Some(OsStr::new(r"a\b")));
    }

    #[test]
    fn prefix_questions_compare_whole_host_components() {
        assert!(unix("/work/project/src").starts_with(&unix("/work/project")));
        assert!(unix("/work/project").starts_with(&unix("/work/project")));
        assert!(!unix("/work/project-two").starts_with(&unix("/work/project")));
        assert!(windows(r"C:\work\project\src").starts_with(&windows(r"c:\work\project")));
        assert!(!windows(r"C:\work\other").starts_with(&windows(r"C:\work\project")));
    }

    #[test]
    fn paths_are_normalized_with_the_host_path_rules() {
        assert_eq!(
            unix("/work//project/./sub/../.devcontainer").as_str(),
            "/work/project/.devcontainer"
        );
        assert_eq!(windows("C:/work/project/").as_str(), r"C:\work\project");
    }

    #[test]
    fn serialized_host_paths_carry_their_path_rules() -> Result<()> {
        let path = unix("/work/project");
        let serialized = serde_json::to_string(&path)?;

        assert_eq!(serde_json::from_str::<HostPathBuf>(&serialized)?, path);
        assert_ne!(
            serde_json::to_string(&HostPathBuf::new("/work/project", PathStyle::Windows))?,
            serialized
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_host_paths_preserve_non_utf8_native_paths() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

        let native_path = PathBuf::from(OsString::from_vec(b"/work/project-\xff".to_vec()));
        let host_path = HostPathBuf::from_path(&native_path, PathStyle::local());

        assert_eq!(host_path.to_path_buf(), native_path);
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedCommand {
        program: Option<String>,
        arguments: Vec<String>,
        working_directory: Option<String>,
    }

    /// A remote connection that records what the project host hands the
    /// transport, with a path style and platform chosen by the test rather than
    /// inherited from the desktop the test runs on.
    ///
    /// Its command templates deliberately name a program that cannot be spawned,
    /// so a test can drive the host's filesystem operations and inspect what
    /// reached the transport without depending on the desktop's tooling.
    struct RecordingConnection {
        options: RemoteConnectionOptions,
        path_style: PathStyle,
        platform: RemotePlatform,
        commands: Mutex<Vec<RecordedCommand>>,
        uploads: Mutex<Vec<(PathBuf, String)>>,
    }

    impl RecordingConnection {
        fn new(
            options: RemoteConnectionOptions,
            path_style: PathStyle,
            operating_system: RemoteOs,
        ) -> Arc<Self> {
            Arc::new(Self {
                options,
                path_style,
                platform: RemotePlatform {
                    os: operating_system,
                    arch: RemoteArch::X86_64,
                },
                commands: Mutex::new(Vec::new()),
                uploads: Mutex::new(Vec::new()),
            })
        }

        fn ssh(path_style: PathStyle, operating_system: RemoteOs) -> Arc<Self> {
            Self::new(
                RemoteConnectionOptions::Ssh(Default::default()),
                path_style,
                operating_system,
            )
        }

        fn wsl() -> Arc<Self> {
            Self::new(
                RemoteConnectionOptions::Wsl(WslConnectionOptions {
                    distro_name: "Ubuntu".to_string(),
                    user: None,
                }),
                PathStyle::Unix,
                RemoteOs::Linux,
            )
        }

        fn commands(&self) -> Vec<RecordedCommand> {
            lock(&self.commands).clone()
        }

        fn upload_destinations(&self) -> Vec<String> {
            lock(&self.uploads)
                .iter()
                .map(|(_, destination)| destination.clone())
                .collect()
        }
    }

    #[async_trait(?Send)]
    impl RemoteConnection for RecordingConnection {
        fn start_proxy(
            &self,
            _unique_identifier: String,
            _reconnect: bool,
            _incoming_tx: UnboundedSender<Envelope>,
            _outgoing_rx: UnboundedReceiver<Envelope>,
            _connection_activity_tx: Sender<()>,
            _delegate: Arc<dyn RemoteClientDelegate>,
            _cx: &mut AsyncApp,
        ) -> Task<Result<i32>> {
            unreachable!("the recording connection does not proxy")
        }

        fn upload_directory(
            &self,
            src_path: PathBuf,
            dest_path: RemotePathBuf,
            _cx: &App,
        ) -> Task<Result<()>> {
            lock(&self.uploads).push((src_path, dest_path.to_string()));
            Task::ready(Ok(()))
        }

        async fn kill(&self) -> Result<()> {
            Ok(())
        }

        fn has_been_killed(&self) -> bool {
            false
        }

        fn build_command(
            &self,
            program: Option<String>,
            args: &[String],
            _env: &HashMap<String, String>,
            working_dir: Option<String>,
            _port_forward: Option<(u16, String, u16)>,
            _interactive: Interactive,
        ) -> Result<CommandTemplate> {
            lock(&self.commands).push(RecordedCommand {
                program,
                arguments: args.to_vec(),
                working_directory: working_dir,
            });
            Ok(CommandTemplate {
                program: "zed-recording-connection-never-runs".to_string(),
                args: Vec::new(),
                env: HashMap::default(),
            })
        }

        fn build_forward_ports_command(
            &self,
            _forwards: Vec<(u16, String, u16)>,
        ) -> Result<CommandTemplate> {
            unreachable!("the recording connection does not forward ports")
        }

        fn connection_options(&self) -> RemoteConnectionOptions {
            self.options.clone()
        }

        fn path_style(&self) -> PathStyle {
            self.path_style
        }

        fn remote_platform(&self) -> RemotePlatform {
            self.platform
        }

        fn remote_os_version(&self) -> Option<String> {
            None
        }

        fn shell(&self) -> String {
            "sh".to_string()
        }

        fn default_system_shell(&self) -> String {
            "sh".to_string()
        }

        fn has_wsl_interop(&self) -> bool {
            false
        }
    }

    #[test]
    fn a_project_host_reports_its_own_platform_and_path_rules() -> Result<()> {
        let linux_host = ProjectHost::from_remote_connection(
            unix("/work/project"),
            RecordingConnection::ssh(PathStyle::Unix, RemoteOs::Linux),
        )?;
        assert_eq!(linux_host.platform(), ProjectHostPlatform::Linux);
        assert_eq!(linux_host.path_style(), PathStyle::Unix);
        assert!(!linux_host.platform().is_windows());

        let windows_host = ProjectHost::from_remote_connection(
            windows(r"C:\work\project"),
            RecordingConnection::ssh(PathStyle::Windows, RemoteOs::Windows),
        )?;
        assert_eq!(windows_host.platform(), ProjectHostPlatform::Windows);
        assert_eq!(windows_host.path_style(), PathStyle::Windows);
        assert!(windows_host.platform().is_windows());

        let wsl_host =
            ProjectHost::from_remote_connection(unix("/work/project"), RecordingConnection::wsl())?;
        assert_eq!(wsl_host.kind(), ProjectHostKind::Wsl);
        assert_eq!(wsl_host.platform(), ProjectHostPlatform::Linux);

        let local_host = ProjectHost::local(PathBuf::from("/project"));
        assert_eq!(local_host.kind(), ProjectHostKind::Local);
        assert_eq!(local_host.path_style(), PathStyle::local());
        Ok(())
    }

    #[test]
    fn a_project_host_rejects_inconsistent_platform_and_path_rules() {
        let result = ProjectHost::from_remote_connection(
            unix("/work/project"),
            RecordingConnection::ssh(PathStyle::Unix, RemoteOs::Windows),
        );

        assert!(result.is_err());
    }

    /// A Windows-like desktop would normalize the Linux root with backslashes;
    /// keeping the original text inside the Unix host contract prevents that.
    #[test]
    fn a_windows_like_desktop_preserves_linux_host_configuration_and_working_directory()
    -> Result<()> {
        assert_eq!(
            PathStyle::Windows.normalize("/work/project"),
            r"\work\project"
        );
        let linux_connection = RecordingConnection::ssh(PathStyle::Unix, RemoteOs::Linux);
        let linux_host = ProjectHost::from_remote_connection(
            HostPathBuf::from_path(PathBuf::from("/work/project"), PathStyle::Unix),
            linux_connection.clone() as Arc<dyn RemoteConnection>,
        )?;
        let configuration = linux_host.project_root().join_relative_path(
            Path::new(r".devcontainer\devcontainer.json"),
            PathStyle::Windows,
        );
        assert!(
            smol::block_on(linux_host.read_file(&configuration)).is_err(),
            "the recording transport records the command instead of running it"
        );

        assert_eq!(
            linux_connection.commands(),
            vec![RecordedCommand {
                program: Some("cat".to_string()),
                arguments: vec!["/work/project/.devcontainer/devcontainer.json".to_string()],
                working_directory: Some("/work/project".to_string()),
            }]
        );

        let windows_connection = RecordingConnection::ssh(PathStyle::Windows, RemoteOs::Windows);
        let windows_host = ProjectHost::from_remote_connection(
            windows(r"C:\work\project"),
            windows_connection.clone() as Arc<dyn RemoteConnection>,
        )?;
        let generated = windows_host.project_root().join(".devcontainer/generated");
        assert!(
            smol::block_on(windows_host.create_dir_all(&generated)).is_err(),
            "the recording transport records the command instead of running it"
        );

        let recorded = windows_connection.commands();
        let [command] = recorded.as_slice() else {
            panic!("expected exactly one recorded command, got {recorded:?}");
        };
        assert_eq!(command.program.as_deref(), Some("powershell.exe"));
        assert!(
            command
                .arguments
                .contains(&r"C:\work\project\.devcontainer\generated".to_string()),
            "recorded {recorded:?}"
        );
        assert_eq!(
            command.working_directory.as_deref(),
            Some(r"C:\work\project")
        );
        Ok(())
    }

    #[test]
    fn a_wsl_project_host_runs_commands_at_its_linux_project_root() -> Result<()> {
        let connection = RecordingConnection::wsl();
        let host = ProjectHost::from_remote_connection(
            unix("/home/zed/project"),
            connection.clone() as Arc<dyn RemoteConnection>,
        )?;

        host.build_command(
            HostProcessRequest::new("docker", host.project_root().clone()).arguments(["ps"]),
            Interactive::No,
        )?;
        host.build_command(
            HostProcessRequest::new("docker", host.project_root().join(".devcontainer"))
                .arguments(["build", "."]),
            Interactive::No,
        )?;

        assert_eq!(
            connection
                .commands()
                .into_iter()
                .map(|command| command.working_directory)
                .collect::<Vec<_>>(),
            vec![
                Some("/home/zed/project".to_string()),
                Some("/home/zed/project/.devcontainer".to_string()),
            ]
        );
        Ok(())
    }

    #[gpui::test]
    async fn staging_sends_the_host_destination_in_the_host_path_style(
        cx: &mut gpui::TestAppContext,
    ) {
        let result = async {
            let connection = RecordingConnection::ssh(PathStyle::Unix, RemoteOs::Linux);
            let host = ProjectHost::from_remote_connection(
                unix("/work/project"),
                connection.clone() as Arc<dyn RemoteConnection>,
            )?;
            let destination = host.project_root().join(".devcontainer/staged-features");
            let desktop_assets = tempdir()?;

            let stage = cx.update(|cx| {
                host.stage_assets(desktop_assets.path().to_path_buf(), destination, cx)
            });
            stage.await?;

            assert_eq!(
                connection.upload_destinations(),
                vec!["/work/project/.devcontainer/staged-features".to_string()]
            );
            Result::<()>::Ok(())
        }
        .await;
        assert!(result.is_ok(), "staging assets failed: {result:?}");
    }

    #[test]
    fn ssh_and_wsl_connections_select_the_matching_project_host_kind() -> Result<()> {
        assert_eq!(
            project_host_kind(&RemoteConnectionOptions::Ssh(Default::default()))?,
            ProjectHostKind::Ssh
        );
        assert_eq!(
            project_host_kind(&RemoteConnectionOptions::Wsl(WslConnectionOptions {
                distro_name: "Ubuntu".to_string(),
                user: None,
            }))?,
            ProjectHostKind::Wsl
        );
        Ok(())
    }

    #[test]
    fn local_host_uses_the_requested_working_directory() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let request = if cfg!(windows) {
            HostProcessRequest::new("cmd.exe", host.project_root().clone())
                .arguments(["/C", "echo %CD%"])
        } else {
            HostProcessRequest::new("sh", host.project_root().clone()).arguments(["-c", "pwd"])
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
                    host.project_root().join("staged-assets"),
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
        assert_eq!(
            smol::block_on(host.temporary_root())?,
            HostPathBuf::from_path(std::env::temp_dir(), PathStyle::local())
        );
        Ok(())
    }

    #[test]
    fn local_host_performs_filesystem_operations() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let generated_directory = host.project_root().join(".devcontainer/generated");
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
        let feature_source = host.project_root().join(".devcontainer/local-feature");
        let feature_destination = host.project_root().join("staged/local-feature");

        smol::block_on(async {
            host.create_dir_all(&feature_source.join("nested")).await?;
            host.write_file(&feature_source.join("install.sh"), b"#!/bin/sh\n")
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
                b"#!/bin/sh\n"
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
            HostProcessRequest::new("cmd.exe", host.project_root().clone())
                .arguments(["/C", "exit 7"])
        } else {
            HostProcessRequest::new("sh", host.project_root().clone()).arguments(["-c", "exit 7"])
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
            HostProcessRequest::new("cmd.exe", host.project_root().clone())
                .arguments(["/C", "ping -n 30 127.0.0.1 > NUL"])
        } else {
            HostProcessRequest::new("sh", host.project_root().clone()).arguments(["-c", "sleep 30"])
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
    fn remote_layer_can_access_raw_host_process_pipes() -> Result<()> {
        let project_directory = tempdir()?;
        let host = ProjectHost::local(project_directory.path());
        let request = if cfg!(windows) {
            HostProcessRequest::new("cmd.exe", host.project_root().clone())
                .arguments(["/C", "more"])
        } else {
            HostProcessRequest::new("cat", host.project_root().clone())
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
