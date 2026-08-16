use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use gpui::AsyncApp;
use remote::{HostProcessRequest, ProjectHost};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostCommand {
    program: String,
    args: Vec<String>,
    environment: HashMap<String, String>,
    working_directory: Option<PathBuf>,
}

impl HostCommand {
    pub(crate) fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Default::default()
        }
    }

    pub(crate) fn arg(&mut self, arg: impl Into<String>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    pub(crate) fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub(crate) fn env(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    /// Overrides the working directory. Left unset, the command runs at the
    /// project host's active source root.
    pub(crate) fn current_dir(&mut self, directory: impl Into<PathBuf>) -> &mut Self {
        self.working_directory = Some(directory.into());
        self
    }

    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    pub(crate) fn get_args(&self) -> &[String] {
        &self.args
    }

    pub(crate) fn environment(&self) -> &HashMap<String, String> {
        &self.environment
    }

    pub(crate) fn working_directory(&self) -> Option<&Path> {
        self.working_directory.as_deref()
    }

    /// The command as a single shell word list, used when the command has to be
    /// handed to a shell inside the created container rather than run on the host.
    pub(crate) fn to_shell_words(&self) -> Vec<String> {
        let mut words = vec![self.program.clone()];
        words.extend(self.args.iter().cloned());
        words
    }
}

/// The trait is not `Send` because staging needs an app context; Dev Container
/// startup runs as a foreground task, so nothing crosses a thread boundary.
#[async_trait(?Send)]
pub(crate) trait ProjectHostCapability {
    /// The active source root. Host processes run here unless a command asks for
    /// a different working directory.
    fn source_root(&self) -> &Path;

    /// The temporary directory to put generated host artifacts under.
    async fn temporary_root(&self) -> Result<PathBuf>;

    /// Runs a non-interactive host process to completion and collects its output.
    async fn run(&self, command: &HostCommand) -> Result<Output, std::io::Error>;

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>>;

    async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()>;

    async fn create_dir_all(&self, path: &Path) -> Result<()>;

    async fn is_file(&self, path: &Path) -> Result<bool>;

    async fn is_dir(&self, path: &Path) -> Result<bool>;

    /// Copies a directory that already lives on the project host.
    async fn copy_dir(&self, source: &Path, destination: &Path) -> Result<()>;

    /// Transfers a desktop-local asset directory onto the project host. One-way,
    /// and only for assets the desktop obtained itself (downloaded feature
    /// tarballs); host-side inputs are read from the host instead.
    async fn stage_assets(&self, desktop_source: &Path, host_destination: &Path) -> Result<()>;

    async fn read_to_string(&self, path: &Path) -> Result<String> {
        let contents = self.read_file(path).await?;
        String::from_utf8(contents)
            .with_context(|| format!("project host file {} was not valid UTF-8", path.display()))
    }
}

pub(crate) struct RemoteProjectHost {
    project_host: Arc<ProjectHost>,
    app: AsyncApp,
}

impl RemoteProjectHost {
    pub(crate) fn new(project_host: Arc<ProjectHost>, app: AsyncApp) -> Self {
        Self { project_host, app }
    }

    fn request(&self, command: &HostCommand) -> HostProcessRequest {
        let working_directory = command
            .working_directory()
            .unwrap_or_else(|| <Self as ProjectHostCapability>::source_root(self));
        HostProcessRequest::new(command.program(), working_directory)
            .arguments(command.get_args().iter().cloned())
            .environment(
                command
                    .environment()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            )
    }
}

#[async_trait(?Send)]
impl ProjectHostCapability for RemoteProjectHost {
    fn source_root(&self) -> &Path {
        self.project_host.project_root()
    }

    async fn temporary_root(&self) -> Result<PathBuf> {
        self.project_host.temporary_root().await
    }

    async fn run(&self, command: &HostCommand) -> Result<Output, std::io::Error> {
        let process = self
            .project_host
            .start_process(self.request(command))
            .map_err(into_io_error)?;
        let outcome = process.collect_output().await.map_err(into_io_error)?;
        Ok(Output {
            status: outcome.status,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
        })
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        self.project_host.read_file(path).await
    }

    async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()> {
        self.project_host.write_file(path, contents).await
    }

    async fn create_dir_all(&self, path: &Path) -> Result<()> {
        self.project_host.create_dir_all(path).await
    }

    async fn is_file(&self, path: &Path) -> Result<bool> {
        self.project_host.is_file(path).await
    }

    async fn is_dir(&self, path: &Path) -> Result<bool> {
        self.project_host.is_dir(path).await
    }

    async fn copy_dir(&self, source: &Path, destination: &Path) -> Result<()> {
        self.project_host.copy_dir(source, destination).await
    }

    async fn stage_assets(&self, desktop_source: &Path, host_destination: &Path) -> Result<()> {
        let project_host = self.project_host.clone();
        let desktop_source = desktop_source.to_path_buf();
        let host_destination = host_destination.to_path_buf();
        self.app
            .update(|cx| project_host.stage_assets(desktop_source, host_destination, cx))
            .await
    }
}

fn into_io_error(error: anyhow::Error) -> std::io::Error {
    std::io::Error::other(format!("{error:#}"))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    use fs::{FakeFs, Fs};

    use super::*;

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        match mutex.lock() {
            Ok(value) => value,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// A single host process as the project host received it.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct RecordedHostCommand {
        pub(crate) program: String,
        pub(crate) args: Vec<String>,
        pub(crate) environment: HashMap<String, String>,
        /// Where the process ran. Resolved the way a project host resolves it:
        /// the command's own working directory, or the active source root.
        pub(crate) working_directory: PathBuf,
    }

    impl RecordedHostCommand {
        pub(crate) fn arguments(&self) -> Vec<&str> {
            self.args.iter().map(String::as_str).collect()
        }
    }

    /// A project host that records everything routed through it and resolves
    /// filesystem operations against a [`FakeFs`] standing in for the host.
    pub(crate) struct RecordingProjectHost {
        source_root: PathBuf,
        temporary_root: PathBuf,
        fs: Arc<FakeFs>,
        commands: Mutex<Vec<RecordedHostCommand>>,
        staged: Mutex<Vec<(PathBuf, PathBuf)>>,
        copied: Mutex<Vec<(PathBuf, PathBuf)>>,
        outcomes: Mutex<HashMap<String, Output>>,
        unstartable: Mutex<Vec<String>>,
        unreadable: Mutex<Vec<PathBuf>>,
        unqueryable: Mutex<Vec<PathBuf>>,
    }

    impl RecordingProjectHost {
        pub(crate) fn new(
            source_root: impl Into<PathBuf>,
            temporary_root: impl Into<PathBuf>,
            fs: Arc<FakeFs>,
        ) -> Self {
            Self {
                source_root: source_root.into(),
                temporary_root: temporary_root.into(),
                fs,
                commands: Mutex::new(Vec::new()),
                staged: Mutex::new(Vec::new()),
                copied: Mutex::new(Vec::new()),
                outcomes: Mutex::new(HashMap::new()),
                unstartable: Mutex::new(Vec::new()),
                unreadable: Mutex::new(Vec::new()),
                unqueryable: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn commands(&self) -> Vec<RecordedHostCommand> {
            lock(&self.commands).clone()
        }

        pub(crate) fn commands_by_program(&self, program: &str) -> Vec<RecordedHostCommand> {
            self.commands()
                .into_iter()
                .filter(|command| command.program == program)
                .collect()
        }

        pub(crate) fn staged_assets(&self) -> Vec<(PathBuf, PathBuf)> {
            lock(&self.staged).clone()
        }

        pub(crate) fn copied_directories(&self) -> Vec<(PathBuf, PathBuf)> {
            lock(&self.copied).clone()
        }

        /// Makes the named program exit non-zero, as a failing host command does.
        pub(crate) fn fail_program(&self, program: &str) {
            lock(&self.outcomes).insert(
                program.to_string(),
                Output {
                    status: failed_status(),
                    stdout: Vec::new(),
                    stderr: b"host command failed".to_vec(),
                },
            );
        }

        /// Makes the named program unstartable, as a transport failure does.
        pub(crate) fn fail_to_start(&self, program: &str) {
            lock(&self.unstartable).push(program.to_string());
        }

        /// Makes a path that exists on the host fail to read, as a permission or
        /// transport failure does. Distinct from the path simply being absent.
        pub(crate) fn fail_read(&self, path: impl Into<PathBuf>) {
            lock(&self.unreadable).push(path.into());
        }

        pub(crate) fn fail_path_query(&self, path: impl Into<PathBuf>) {
            lock(&self.unqueryable).push(path.into());
        }

        pub(crate) fn respond_with(&self, program: &str, stdout: &str) {
            lock(&self.outcomes).insert(
                program.to_string(),
                Output {
                    status: std::process::ExitStatus::default(),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: Vec::new(),
                },
            );
        }

        async fn copy_within_host(&self, source: &Path, destination: &Path) -> Result<()> {
            let items = fs::read_dir_items(&*self.fs, source).await?;
            self.fs.create_dir(destination).await?;
            for (item, is_dir) in items {
                let relative = item.strip_prefix(source)?;
                let target = destination.join(relative);
                if is_dir {
                    self.fs.create_dir(&target).await?;
                } else {
                    let contents = self.fs.load_bytes(&item).await?;
                    self.fs.write(&target, &contents).await?;
                }
            }
            Ok(())
        }
    }

    #[async_trait(?Send)]
    impl ProjectHostCapability for RecordingProjectHost {
        fn source_root(&self) -> &Path {
            &self.source_root
        }

        async fn temporary_root(&self) -> Result<PathBuf> {
            Ok(self.temporary_root.clone())
        }

        async fn run(&self, command: &HostCommand) -> Result<Output, std::io::Error> {
            if lock(&self.unstartable)
                .iter()
                .any(|program| program == command.program())
            {
                return Err(std::io::Error::other(format!(
                    "project host could not start {}",
                    command.program()
                )));
            }

            lock(&self.commands).push(RecordedHostCommand {
                program: command.program().to_string(),
                args: command.get_args().to_vec(),
                environment: command.environment().clone(),
                working_directory: command
                    .working_directory()
                    .unwrap_or(&self.source_root)
                    .to_path_buf(),
            });

            let outcome = lock(&self.outcomes)
                .get(command.program())
                .map(|output| Output {
                    status: output.status,
                    stdout: output.stdout.clone(),
                    stderr: output.stderr.clone(),
                });
            Ok(outcome.unwrap_or_else(|| Output {
                status: std::process::ExitStatus::default(),
                stdout: Vec::new(),
                stderr: Vec::new(),
            }))
        }

        async fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
            if lock(&self.unreadable)
                .iter()
                .any(|unreadable| unreadable == path)
            {
                anyhow::bail!("project host could not read {}", path.display());
            }
            self.fs.load_bytes(path).await
        }

        async fn write_file(&self, path: &Path, contents: &[u8]) -> Result<()> {
            self.fs.write(path, contents).await
        }

        async fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.fs.create_dir(path).await
        }

        async fn is_file(&self, path: &Path) -> Result<bool> {
            if lock(&self.unqueryable)
                .iter()
                .any(|unqueryable| unqueryable == path)
            {
                anyhow::bail!("project host could not query {}", path.display());
            }
            Ok(self.fs.is_file(path).await)
        }

        async fn is_dir(&self, path: &Path) -> Result<bool> {
            if lock(&self.unqueryable)
                .iter()
                .any(|unqueryable| unqueryable == path)
            {
                anyhow::bail!("project host could not query {}", path.display());
            }
            Ok(self.fs.is_dir(path).await)
        }

        async fn copy_dir(&self, source: &Path, destination: &Path) -> Result<()> {
            lock(&self.copied).push((source.to_path_buf(), destination.to_path_buf()));
            self.copy_within_host(source, destination).await
        }

        async fn stage_assets(&self, desktop_source: &Path, host_destination: &Path) -> Result<()> {
            lock(&self.staged).push((desktop_source.to_path_buf(), host_destination.to_path_buf()));
            self.copy_within_host(desktop_source, host_destination)
                .await
        }
    }

    pub(crate) fn failed_status() -> std::process::ExitStatus {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(1)
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(256)
        }
    }
}
