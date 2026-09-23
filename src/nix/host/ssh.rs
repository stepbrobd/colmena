use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use shell_escape::unix::escape;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

use super::{CopyDirection, CopyOptions, Host, MAIN_PROFILE_SCRIPT, RebootOptions, key_uploader};
use crate::error::{ColmenaError, ColmenaResult, UnitExit};
use crate::job::JobHandle;
use crate::nix::{
    CURRENT_PROFILE, Goal, Key, NixCommand, NixFlags, Profile, StorePath, SystemType,
};
use crate::util::{CommandExecution, CommandExt, capture_stream};

/// Starts and follows a transient activation unit, see its header for the
/// records it prints.
const ACTIVATION_WATCH_SCRIPT: &str = include_str!("./activation_watch.sh");

/// How long the host may stay unreachable while an activation unit runs.
///
/// A network restart during activation drops the connection for a few
/// seconds. The window starts at the last state the watch reported, and a
/// host that stays down fails the deployment when it closes.
const ACTIVATION_RECONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the watch may stay silent before its connection counts as lost.
///
/// The watch reports every 2 seconds. A network restart can drop an
/// established connection without a reset, and ssh would then wait forever
/// for the next line.
const ACTIVATION_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait before reconnecting to the watch.
const ACTIVATION_RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// A remote machine connected over SSH.
#[derive(Debug)]
pub struct Ssh {
    /// The username to use to connect.
    user: Option<String>,

    /// The hostname or IP address to connect to.
    host: String,

    /// The port to connect to.
    port: Option<u16>,

    /// Local path to a ssh_config file.
    ssh_config: Option<PathBuf>,

    /// Command to elevate privileges with.
    privilege_escalation_command: Vec<String>,

    /// extra SSH options
    extra_ssh_options: Vec<String>,

    /// Whether to use the experimental `nix copy` command.
    use_nix3_copy: bool,

    /// Flags to pass to Nix invocations, local and remote.
    nix_flags: NixFlags,

    /// The type of system running on the host.
    system_type: SystemType,

    /// Whether NixOS switch and test activations run in a transient unit.
    detached_activation: bool,

    job: Option<JobHandle>,
}

/// An opaque boot ID.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BootId(String);

/// The state of a transient activation unit.
#[derive(Debug, PartialEq, Eq)]
enum ActivationState {
    Running,
    Succeeded,
    NotFound,
    Failed {
        result: String,
        exit: Option<UnitExit>,
    },
}

impl ActivationState {
    /// Parses the `Key=Value` properties of a status record.
    ///
    /// Only an exit with status 0 counts as a success. systemd also reports
    /// `Result=success` for a process killed by SIGTERM, SIGHUP, SIGINT or
    /// SIGPIPE.
    fn parse(properties: &str) -> Option<Self> {
        let properties: HashMap<&str, &str> = properties
            .split_whitespace()
            .filter_map(|property| property.split_once('='))
            .collect();
        let get = |key| properties.get(key).copied();

        let (load, active, result) = (get("LoadState")?, get("ActiveState")?, get("Result")?);
        let status = get("ExecMainStatus")?.parse().ok()?;

        // ExecMainCode is CLD_EXITED (1), CLD_KILLED (2) or CLD_DUMPED (3) once the process ended
        let exit = match get("ExecMainCode")?.parse().ok()? {
            0 => None,
            1 => Some(UnitExit::Status(status)),
            _ => Some(UnitExit::Signal(status)),
        };

        Some(match exit {
            _ if load == "not-found" => Self::NotFound,
            Some(UnitExit::Status(0)) if result == "success" => Self::Succeeded,
            None if active != "failed" => Self::Running,
            exit => Self::Failed {
                result: result.to_string(),
                exit,
            },
        })
    }
}

/// A line printed by the activation watch.
#[derive(Debug, PartialEq, Eq)]
enum Record {
    Log(String),
    Cursor(String),
    JournalFailed(String),
    Unreadable(String),
    Status(ActivationState),
}

impl Record {
    fn parse(line: &str) -> ColmenaResult<Self> {
        let bad_output = || ColmenaError::ActivationBadOutput {
            line: line.to_string(),
        };
        let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));

        Ok(match tag {
            "L" => Self::Log(rest.to_string()),
            "C" => Self::Cursor(rest.to_string()),
            "J" => Self::JournalFailed(rest.to_string()),
            "W" => Self::Unreadable(rest.to_string()),
            "S" => Self::Status(ActivationState::parse(rest).ok_or_else(bad_output)?),
            _ => return Err(bad_output()),
        })
    }
}

/// What the deployer knows about a detached activation across watch sessions.
#[derive(Debug)]
struct Watch {
    unit: String,

    /// The journal position to resume streaming after.
    cursor: Option<String>,

    /// Journal lines of this session that wait for the cursor after them.
    pending: Vec<String>,

    /// Whether a record arrived in this session.
    answered: bool,

    /// Whether a status record ever showed the unit.
    seen: bool,

    /// When the last status record arrived.
    contact: Instant,

    /// Whether a session was lost since the last one that answered.
    lost: bool,

    /// Why systemctl last failed to read the unit in this session.
    unreadable: Option<String>,

    /// Whether a journal failure was reported.
    journal_failed: bool,
}

impl Watch {
    fn new(unit: String) -> Self {
        Self {
            unit,
            cursor: None,
            pending: Vec::new(),
            answered: false,
            seen: false,
            contact: Instant::now(),
            lost: false,
            unreadable: None,
            journal_failed: false,
        }
    }

    /// Returns the remote command of a session.
    ///
    /// sh reads the script from stdin, so the login shell never parses it.
    fn argv(&self, start: Option<&[String]>) -> Vec<String> {
        let mut argv = vec![
            "sh".to_string(),
            "-s".to_string(),
            self.unit.clone(),
            if start.is_some() { "1" } else { "0" }.to_string(),
            self.cursor.clone().unwrap_or_default(),
        ];
        argv.extend(start.unwrap_or_default().iter().cloned());
        argv
    }

    /// Returns the error of a session that exited before the unit finished,
    /// given whether the session started the unit.
    fn ended(&self, exit: ExitStatus, start: bool, hostname: &str) -> ColmenaError {
        if exit.success() {
            return ColmenaError::ActivationWatchEnded;
        }

        let error = ColmenaError::from(exit);

        // before the first record, only a lost connection can have started the unit
        let lost = matches!(error, ColmenaError::ChildFailure { exit_code: 255, .. });
        if start && !self.answered && !lost {
            return ColmenaError::ActivationStartFailed {
                hostname: hostname.to_string(),
                unit: self.unit.clone(),
                source: Box::new(error),
            };
        }

        error
    }

    /// Takes in a status record and returns the outcome of the activation,
    /// or `None` while it runs.
    fn observe(&mut self, state: ActivationState, hostname: &str) -> Option<ColmenaResult<()>> {
        self.seen |= state != ActivationState::NotFound;
        self.contact = Instant::now();

        let hostname = hostname.to_string();
        let unit = self.unit.clone();

        Some(match state {
            ActivationState::Running => return None,
            ActivationState::Succeeded => Ok(()),
            ActivationState::NotFound if self.seen => {
                Err(ColmenaError::ActivationUnitVanished { hostname, unit })
            }
            ActivationState::NotFound => {
                Err(ColmenaError::ActivationUnitNotFound { hostname, unit })
            }
            ActivationState::Failed { result, exit } => Err(ColmenaError::ActivationFailed {
                hostname,
                unit,
                result,
                exit,
            }),
        })
    }
}

#[async_trait]
impl Host for Ssh {
    async fn copy_closure(
        &mut self,
        closure: &StorePath,
        direction: CopyDirection,
        options: CopyOptions,
    ) -> ColmenaResult<()> {
        let command = self.nix_copy_closure(closure, direction, options);
        self.run_command(command).await
    }

    async fn realize_remote(&mut self, derivation: &StorePath) -> ColmenaResult<Vec<StorePath>> {
        let argv = derivation
            .realise_command(&self.nix_flags)
            .bin_dir(self.system_type.nix_bin_dir())
            .into_argv();
        let command = self.ssh_argv(argv);

        let mut execution = CommandExecution::new(command);
        execution.set_job(self.job.clone());

        let paths = execution.capture_output().await?;

        paths.lines().map(|p| p.to_string().try_into()).collect()
    }

    fn set_job(&mut self, job: Option<JobHandle>) {
        self.job = job;
    }

    async fn upload_keys(
        &mut self,
        keys: &HashMap<String, Key>,
        require_ownership: bool,
    ) -> ColmenaResult<()> {
        for (name, key) in keys {
            self.upload_key(name, key, require_ownership).await?;
        }

        Ok(())
    }

    async fn activate(&mut self, profile: &Profile, goal: Goal) -> ColmenaResult<()> {
        if !goal.requires_activation() {
            return Err(ColmenaError::Unsupported);
        }

        let activation_command = profile.activation_command(goal, self.system_type)?;

        if goal.should_switch_profile() {
            let argv = profile
                .switch_profile_command(&self.nix_flags)
                .bin_dir(self.system_type.nix_bin_dir())
                .into_argv();
            let set_profile = self.ssh_argv(argv);
            self.run_command(set_profile).await?;
        }

        // switch and test may restart the network and drop the session
        // nix-darwin has no systemd to detach into
        if self.detached_activation
            && self.system_type == SystemType::NixOS
            && matches!(goal, Goal::Switch | Goal::Test)
        {
            return self.activate_detached(&activation_command).await;
        }

        let command = self.ssh(&activation_command);
        self.run_command(command).await
    }

    async fn get_current_system_profile(&mut self) -> ColmenaResult<Profile> {
        let paths = self
            .ssh(&["readlink", "-f", CURRENT_PROFILE])
            .capture_output()
            .await?;

        let path = paths
            .lines()
            .next()
            .ok_or(ColmenaError::FailedToGetCurrentProfile)?
            .to_string()
            .try_into()?;

        Ok(Profile::from_store_path_unchecked(path))
    }

    async fn get_main_system_profile(&mut self) -> ColmenaResult<Profile> {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            MAIN_PROFILE_SCRIPT.to_string(),
        ];
        let paths = self.ssh_argv(argv).capture_output().await?;

        let path = paths
            .lines()
            .next()
            .ok_or(ColmenaError::FailedToGetCurrentProfile)?
            .to_string()
            .try_into()?;

        Ok(Profile::from_store_path_unchecked(path))
    }

    async fn run_command(&mut self, command: &[&str]) -> ColmenaResult<()> {
        let command = self.ssh(command);
        self.run_command(command).await
    }

    async fn reboot(&mut self, options: RebootOptions) -> ColmenaResult<()> {
        if !options.wait_for_boot {
            return self.initate_reboot().await;
        }

        let old_id = self.get_boot_id().await?;

        self.initate_reboot().await?;

        if let Some(job) = &self.job {
            job.message("Waiting for reboot".to_string())?;
        }

        // Wait for node to come back up
        loop {
            // Ignore errors while waiting
            // nix-darwin links /run/current-system from a launchd job that sshd does not wait for
            if let Ok(new_id) = self.get_boot_id().await
                && new_id != old_id
                && (self.system_type == SystemType::NixOS
                    || self.get_current_system_profile().await.is_ok())
            {
                break;
            }

            sleep(Duration::from_secs(2)).await;
        }

        // Ensure node has correct system profile
        if let Some(new_profile) = options.new_profile {
            let profile = self.get_current_system_profile().await?;

            if new_profile != profile {
                return Err(ColmenaError::ActiveProfileUnexpected { profile });
            }
        }

        Ok(())
    }
}

impl Ssh {
    pub fn new(user: Option<String>, host: String, nix_flags: NixFlags) -> Self {
        Self {
            user,
            host,
            port: None,
            ssh_config: None,
            privilege_escalation_command: Vec::new(),
            extra_ssh_options: Vec::new(),
            use_nix3_copy: false,
            nix_flags,
            system_type: SystemType::default(),
            detached_activation: true,
            job: None,
        }
    }

    pub fn set_port(&mut self, port: u16) {
        self.port = Some(port);
    }

    pub fn set_ssh_config(&mut self, ssh_config: PathBuf) {
        self.ssh_config = Some(ssh_config);
    }

    pub fn set_privilege_escalation_command(&mut self, command: Vec<String>) {
        self.privilege_escalation_command = command;
    }

    pub fn set_extra_ssh_options(&mut self, options: Vec<String>) {
        self.extra_ssh_options = options;
    }

    pub fn set_use_nix3_copy(&mut self, enable: bool) {
        self.use_nix3_copy = enable;
    }

    pub fn set_system_type(&mut self, system_type: SystemType) {
        self.system_type = system_type;
    }

    pub fn set_detached_activation(&mut self, enable: bool) {
        self.detached_activation = enable;
    }

    pub fn upcast(self) -> Box<dyn Host> {
        Box::new(self)
    }

    /// Returns a Tokio Command to run a generated command on the host.
    ///
    /// ssh(1) concatenates its arguments with spaces and lets the remote
    /// shell re-split them, so each argument is escaped first. This
    /// matters for Nix flag values that may contain spaces (e.g.,
    /// `--option substituters "https://a https://b"`).
    fn ssh_argv(&self, argv: Vec<String>) -> Command {
        let escaped: Vec<String> = argv
            .into_iter()
            .map(|arg| escape(arg.into()).into_owned())
            .collect();

        self.ssh(&escaped)
    }

    /// Returns a Tokio Command to run an arbitrary command on the host.
    pub fn ssh<S: AsRef<OsStr>>(&self, command: &[S]) -> Command {
        let options = self.ssh_options();
        let options_str = options.join(" ");
        let privilege_escalation_command = if self.user.as_deref() != Some("root") {
            self.privilege_escalation_command.as_slice()
        } else {
            &[]
        };

        let mut cmd = Command::new("ssh");

        cmd.arg(self.ssh_target())
            .args(&options)
            .arg("--")
            .args(privilege_escalation_command)
            .args(command)
            .env("NIX_SSHOPTS", options_str);

        cmd
    }

    async fn run_command(&mut self, command: Command) -> ColmenaResult<()> {
        let mut execution = CommandExecution::new(command);
        execution.set_job(self.job.clone());

        execution.run().await
    }

    fn ssh_target(&self) -> String {
        match &self.user {
            Some(n) => format!("{}@{}", n, self.host),
            None => self.host.clone(),
        }
    }

    fn nix_copy_closure(
        &self,
        path: &StorePath,
        direction: CopyDirection,
        options: CopyOptions,
    ) -> Command {
        let ssh_options = self.ssh_options();
        let ssh_options_str = ssh_options.join(" ");
        let bin_dir = self.system_type.nix_bin_dir();

        // nix-copy-closure finds the remote nix-store through PATH
        // root's PATH on macOS lacks it
        // nix copy can name the remote nix-daemon instead
        let use_nix3_copy = self.use_nix3_copy || bin_dir.is_some();

        let mut command = if use_nix3_copy {
            // experimental `nix copy` command with ssh-ng://
            let mut command =
                NixCommand::nix(self.nix_flags.clone()).args(["copy", "--no-check-sigs"]);

            if options.use_substitutes {
                command = command.args([
                    "--substitute-on-destination",
                    // needed due to UX bug in ssh-ng://
                    "--builders-use-substitutes",
                ]);
            }

            if let Some("drv") = path.extension().and_then(OsStr::to_str) {
                command = command.arg("--derivation");
            }

            command = match direction {
                CopyDirection::ToRemote => command.arg("--to"),
                CopyDirection::FromRemote => command.arg("--from"),
            };

            let mut params = Vec::new();
            if let Some(dir) = bin_dir {
                params.push(format!("remote-program={dir}/nix-daemon"));
            }
            if options.gzip {
                params.push("compress=true".to_string());
            }

            let mut store_uri = format!("ssh-ng://{}", self.ssh_target());
            if !params.is_empty() {
                store_uri = format!("{store_uri}?{}", params.join("&"));
            }

            command.arg(store_uri).arg(path.as_path()).build()
        } else {
            // nix-copy-closure (ssh://)
            let mut command = NixCommand::nix_copy_closure(self.nix_flags.clone());

            command = match direction {
                CopyDirection::ToRemote => command.arg("--to"),
                CopyDirection::FromRemote => command.arg("--from"),
            };

            // FIXME: Host-agnostic abstraction
            if options.include_outputs {
                command = command.arg("--include-outputs");
            }
            if options.use_substitutes {
                command = command.arg("--use-substitutes");
            }
            if options.gzip {
                command = command.arg("--gzip");
            }

            command.arg(self.ssh_target()).arg(path.as_path()).build()
        };

        command.env("NIX_SSHOPTS", ssh_options_str);

        command
    }

    fn ssh_options(&self) -> Vec<String> {
        // TODO: Allow configuration of SSH parameters

        // NOTE: extra_ssh_options needs to come first so that
        // deployment.sshOptions can override our settings.
        //
        // From ssh_config(5):
        // > Unless noted otherwise, for each parameter, the first obtained
        // > value will be used.
        let mut options: Vec<String> = self
            .extra_ssh_options
            .iter()
            .cloned()
            .chain(
                [
                    "-o",
                    "StrictHostKeyChecking=accept-new",
                    "-o",
                    "BatchMode=yes",
                    "-T",
                ]
                .map(String::from),
            )
            .collect();

        if let Some(port) = self.port {
            options.push("-p".to_string());
            options.push(port.to_string());
        }

        if let Some(ssh_config) = self.ssh_config.as_ref() {
            options.push("-F".to_string());
            options.push(ssh_config.to_str().unwrap().to_string());
        }

        options
    }

    /// Uploads a single key.
    async fn upload_key(
        &mut self,
        name: &str,
        key: &Key,
        require_ownership: bool,
    ) -> ColmenaResult<()> {
        if let Some(job) = &self.job {
            job.message(format!("Uploading key {}", name))?;
        }

        let path = key.path();
        let key_script = key_uploader::generate_script(key, path, require_ownership);

        let mut command = self.ssh_argv(vec!["sh".to_string(), "-c".to_string(), key_script]);

        command.stdin(Stdio::piped());
        command.stderr(Stdio::piped());
        command.stdout(Stdio::piped());

        let uploader = command.spawn()?;
        key_uploader::feed_uploader(uploader, key, self.job.clone()).await
    }

    /// Returns the current Boot ID.
    async fn get_boot_id(&mut self) -> ColmenaResult<BootId> {
        let command: &[&str] = match self.system_type {
            SystemType::NixOS => &["cat", "/proc/sys/kernel/random/boot_id"],
            SystemType::Darwin => &["sysctl", "-n", "kern.bootsessionuuid"],
        };

        let boot_id = self.ssh(command).capture_output().await?;

        Ok(BootId(boot_id))
    }

    /// Initiates reboot.
    async fn initate_reboot(&mut self) -> ColmenaResult<()> {
        match self.run_command(self.ssh(&["reboot"])).await {
            Ok(()) => Ok(()),
            Err(e) => {
                if let ColmenaError::ChildFailure { exit_code: 255, .. } = e {
                    // Assume it's "Connection closed by remote host"
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    fn message(&self, message: String) -> ColmenaResult<()> {
        match &self.job {
            Some(job) => job.message(message),
            None => Ok(()),
        }
    }

    /// Runs the activation in a transient systemd unit and follows it, so
    /// that the activation completes even when it drops the SSH session.
    ///
    /// A failed unit stays on the host for `systemctl status` and
    /// `journalctl -u`.
    async fn activate_detached(&mut self, activation_command: &[String]) -> ColmenaResult<()> {
        let unit = format!("colmena-activate-{}", Uuid::new_v4().simple());
        self.message(format!("Starting activation in unit {unit}"))?;

        let mut watch = Watch::new(unit);
        let mut start = Some(activation_command);

        let outcome = loop {
            match self.watch_session(&mut watch, start.take()).await {
                Err(error) if Self::is_retryable(&error) => {
                    let left = ACTIVATION_RECONNECT_TIMEOUT.saturating_sub(watch.contact.elapsed());
                    if !watch.lost && !left.is_zero() {
                        watch.lost = true;
                        self.message(format!(
                            "Lost contact with the host ({error}), reconnecting for up to {} seconds",
                            left.as_secs()
                        ))?;
                    }

                    // a session started with no time left would only report its own timeout
                    sleep(ACTIVATION_RETRY_INTERVAL).await;

                    if watch.contact.elapsed() >= ACTIVATION_RECONNECT_TIMEOUT {
                        return Err(ColmenaError::ActivationUnreachable {
                            hostname: self.host.clone(),
                            unit: watch.unit,
                            timeout: ACTIVATION_RECONNECT_TIMEOUT,
                            reason: watch.unreadable.unwrap_or_else(|| error.to_string()),
                        });
                    }
                }
                outcome => break outcome,
            }
        };

        if outcome.is_ok() {
            self.stop_unit(&watch.unit).await?;
        }

        outcome
    }

    /// Runs the watch script once and passes its records to `watch`.
    ///
    /// With `start`, the script first starts the unit with that command.
    /// Returns the outcome of the activation once the unit finishes, or the
    /// error that ended the session earlier.
    async fn watch_session(
        &mut self,
        watch: &mut Watch,
        start: Option<&[String]>,
    ) -> ColmenaResult<()> {
        let mut command = self.ssh_argv(watch.argv(start));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn()?;

        // a session that ends before it reads the script reports that with its exit
        let mut stdin = child.stdin.take().unwrap();
        let _ = stdin.write_all(ACTIVATION_WATCH_SCRIPT.as_bytes()).await;
        drop(stdin);
        let stderr = BufReader::new(child.stderr.take().unwrap());
        tokio::spawn(capture_stream(stderr, self.job.clone(), true));

        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        if let Some(outcome) = self.follow(watch, &mut stdout).await? {
            return outcome;
        }

        let exit = child.wait().await?;
        Err(watch.ended(exit, start.is_some(), &self.host))
    }

    /// Passes the records of one session to `watch`, and prints the journal
    /// lines once the cursor after them arrives.
    ///
    /// Returns the outcome of the activation once the unit finishes, or
    /// `None` when the session ends first.
    async fn follow(
        &self,
        watch: &mut Watch,
        stdout: &mut (impl AsyncBufRead + Unpin),
    ) -> ColmenaResult<Option<ColmenaResult<()>>> {
        watch.unreadable = None;
        watch.pending.clear();
        watch.answered = false;

        loop {
            let limit = ACTIVATION_STEP_TIMEOUT
                .min(ACTIVATION_RECONNECT_TIMEOUT.saturating_sub(watch.contact.elapsed()));

            let mut line = Vec::new();
            let read = timeout(limit, stdout.read_until(b'\n', &mut line))
                .await
                .map_err(|_| ColmenaError::ActivationStepTimeout)??;
            // a cut last line means the connection dropped mid record
            if read == 0 || !line.ends_with(b"\n") {
                return Ok(None);
            }
            // the first record of a session ends an outage
            if !std::mem::replace(&mut watch.answered, true) && std::mem::take(&mut watch.lost) {
                self.message("Reconnected to the host".to_string())?;
            }

            // journal lines are bytes
            let line = String::from_utf8_lossy(&line);
            match Record::parse(line.trim_end_matches('\n'))? {
                // a drop before the cursor makes the next session send the lines again
                Record::Log(line) => watch.pending.push(line),
                Record::Cursor(cursor) => {
                    watch.cursor = Some(cursor);
                    for line in watch.pending.drain(..) {
                        if let Some(job) = &self.job {
                            job.stderr(line)?;
                        }
                    }
                }
                Record::JournalFailed(status) => {
                    if !std::mem::replace(&mut watch.journal_failed, true) {
                        self.message(format!(
                            "Could not read the journal of unit {}, journalctl exited with {status}",
                            watch.unit
                        ))?;
                    }
                }
                Record::Unreadable(reason) => watch.unreadable = Some(reason),
                // the final status follows every line and cursor of the unit
                Record::Status(state) => {
                    if let Some(outcome) = watch.observe(state, &self.host) {
                        return Ok(Some(outcome));
                    }
                }
            }
        }
    }

    /// Stops a finished unit, which releases the RemainAfterExit hold and
    /// lets systemd remove it.
    async fn stop_unit(&mut self, unit: &str) -> ColmenaResult<()> {
        let mut command = self.ssh(&["systemctl", "stop", unit]);
        command.kill_on_drop(true);

        let stopped = timeout(ACTIVATION_STEP_TIMEOUT, self.run_command(command)).await;
        if !matches!(stopped, Ok(Ok(()))) {
            self.message(format!(
                "Could not stop the finished unit {unit}, it stays until `systemctl stop {unit}`"
            ))?;
        }

        Ok(())
    }

    /// Whether a watch session may have failed for a lost connection or for
    /// access that recovers, rather than for the activation.
    fn is_retryable(error: &ColmenaError) -> bool {
        matches!(
            error,
            ColmenaError::ChildFailure { .. }
                | ColmenaError::ChildKilled { .. }
                | ColmenaError::ActivationStepTimeout
                | ColmenaError::ActivationWatchEnded
        )
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use tokio_test::block_on;

    use super::*;

    #[test]
    fn test_ssh_argv_escapes_for_remote_shell() {
        let mut flags = NixFlags::default();
        flags.add_option(
            "substituters".to_string(),
            "https://a https://b".to_string(),
        );

        let host = Ssh::new(Some("root".to_string()), "example.com".to_string(), flags);

        let argv = NixCommand::nix_store(host.nix_flags.clone())
            .args(["--no-gc-warning", "--realise"])
            .arg("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x")
            .into_argv();
        let command = host.ssh_argv(argv);

        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert!(args.contains(&"'https://a https://b'".to_string()));
        assert!(args.contains(&"--realise".to_string()));
        assert!(args.contains(&"nix-store".to_string()));
    }

    #[test]
    fn test_copy_closure_threads_nix_flags() {
        let mut flags = NixFlags::default();
        flags.add_option("cores".to_string(), "4".to_string());

        let mut host = Ssh::new(Some("root".to_string()), "example.com".to_string(), flags);

        let store_path: StorePath = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x"
            .to_string()
            .try_into()
            .unwrap();

        for use_nix3_copy in [false, true] {
            host.set_use_nix3_copy(use_nix3_copy);

            let command =
                host.nix_copy_closure(&store_path, CopyDirection::ToRemote, CopyOptions::default());

            let args: Vec<String> = command
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();

            assert!(
                args.windows(3).any(|w| w == ["--option", "cores", "4"]),
                "nix_flags not threaded (use_nix3_copy: {}): {:?}",
                use_nix3_copy,
                args
            );
        }
    }

    fn state(properties: &str) -> ActivationState {
        ActivationState::parse(properties).unwrap()
    }

    #[test]
    fn test_activation_state() {
        use ActivationState::*;

        let failed = |result: &str, exit| Failed {
            result: result.to_string(),
            exit,
        };
        let cases = [
            (
                "active Result=success ExecMainCode=1 ExecMainStatus=0",
                Succeeded,
            ),
            (
                "failed Result=exit-code ExecMainCode=1 ExecMainStatus=4",
                failed("exit-code", Some(UnitExit::Status(4))),
            ),
            // a process left in the unit was killed after the main process exited
            (
                "failed Result=oom-kill ExecMainCode=1 ExecMainStatus=0",
                failed("oom-kill", Some(UnitExit::Status(0))),
            ),
            // systemd counts SIGTERM as a clean exit for Type=exec
            (
                "active Result=success ExecMainCode=2 ExecMainStatus=15",
                failed("success", Some(UnitExit::Signal(15))),
            ),
            // failed before the process started
            (
                "failed Result=resources ExecMainCode=0 ExecMainStatus=0",
                failed("resources", None),
            ),
            (
                "active Result=success ExecMainCode=0 ExecMainStatus=0",
                Running,
            ),
            // the start job still waits
            (
                "inactive Result=success ExecMainCode=0 ExecMainStatus=0",
                Running,
            ),
        ];

        for (properties, expected) in cases {
            let properties = format!("LoadState=loaded ActiveState={properties}");
            assert_eq!(expected, state(&properties), "{properties}");
        }

        let gone = "LoadState=not-found ActiveState=inactive Result=success ExecMainCode=0 ExecMainStatus=0";
        assert_eq!(NotFound, state(gone));
    }

    #[test]
    fn test_record_parse() {
        assert_eq!(
            Record::Log("two  words ".to_string()),
            Record::parse("L two  words ").unwrap()
        );
        assert_eq!(Record::Log(String::new()), Record::parse("L ").unwrap());
        assert_eq!(
            Record::Cursor("s=1;i=2".to_string()),
            Record::parse("C s=1;i=2").unwrap()
        );
        assert!(Record::parse("S LoadState=loaded").is_err());
        assert!(Record::parse("-- cursor: s=1").is_err());
    }

    #[test]
    fn test_watch_tells_a_vanished_unit_from_a_missing_one() {
        let running =
            "LoadState=loaded ActiveState=active Result=success ExecMainCode=0 ExecMainStatus=0";
        let gone = "LoadState=not-found ActiveState=inactive Result=success ExecMainCode=0 ExecMainStatus=0";

        let mut missing = Watch::new("unit".to_string());
        assert!(matches!(
            missing.observe(state(gone), "host"),
            Some(Err(ColmenaError::ActivationUnitNotFound { .. }))
        ));

        let mut vanished = Watch::new("unit".to_string());
        vanished.contact -= ACTIVATION_RECONNECT_TIMEOUT;
        assert!(vanished.observe(state(running), "host").is_none());
        assert!(vanished.contact.elapsed() < ACTIVATION_RECONNECT_TIMEOUT);
        assert!(matches!(
            vanished.observe(state(gone), "host"),
            Some(Err(ColmenaError::ActivationUnitVanished { .. }))
        ));
    }

    #[test]
    fn test_watch_argv_needs_no_escapes() {
        // sshd hands the command to the login shell, and nushell misreads the escapes of ' and !
        let mut watch = Watch::new("unit".to_string());
        watch.cursor = Some("s=1;i=2".to_string());
        let start = [
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x/bin/switch-to-configuration".to_string(),
            "switch".to_string(),
        ];

        let argv = watch.argv(Some(&start));
        assert!(
            argv.iter().all(|arg| !arg.contains(['\'', '!'])),
            "{argv:?}"
        );
    }

    fn follow(watch: &mut Watch, records: &[u8]) -> Option<ColmenaResult<()>> {
        let ssh = Ssh::new(None, "host".to_string(), NixFlags::default());
        block_on(ssh.follow(watch, &mut BufReader::new(records))).unwrap()
    }

    #[test]
    fn test_follow_holds_lines_until_their_cursor() {
        let mut watch = Watch::new("unit".to_string());

        assert!(follow(&mut watch, b"L one\nC s=1\nL two\n").is_none());
        assert_eq!(Some("s=1"), watch.cursor.as_deref());
        assert_eq!(["two"], watch.pending.as_slice());
    }

    #[test]
    fn test_follow_starts_each_session_afresh() {
        let mut watch = Watch::new("unit".to_string());
        watch.lost = true;
        watch.unreadable = Some("bus".to_string());

        // the first record of a session ends an outage
        assert!(follow(&mut watch, b"L one\n").is_none());
        assert!(!watch.lost);
        assert_eq!(None, watch.unreadable);

        // a session that dropped before the cursor sends its lines again
        watch.lost = true;
        assert!(follow(&mut watch, b"L one\n").is_none());
        assert!(!watch.lost);
        assert_eq!(["one"], watch.pending.as_slice());
    }

    #[test]
    fn test_follow_ends_at_a_cut_record() {
        // the connection dropped in the middle of the record
        let mut watch = Watch::new("unit".to_string());

        assert!(follow(&mut watch, b"S LoadState=loaded ActiveSt").is_none());
        assert!(!watch.answered);
    }

    #[test]
    fn test_first_session_that_fails_before_a_record() {
        let watch = Watch::new("unit".to_string());
        let ended = |code| watch.ended(ExitStatus::from_raw(code << 8), true, "host");

        // ssh exits 255 when the connection drops, possibly after systemd-run
        assert!(matches!(
            ended(255),
            ColmenaError::ChildFailure { exit_code: 255, .. }
        ));
        assert!(matches!(
            ended(1),
            ColmenaError::ActivationStartFailed { .. }
        ));
        assert!(matches!(ended(0), ColmenaError::ActivationWatchEnded));

        // a later session, or one that answered, did not start the unit
        let mut answered = Watch::new("unit".to_string());
        answered.answered = true;
        for (watch, start) in [(&watch, false), (&answered, true)] {
            assert!(matches!(
                watch.ended(ExitStatus::from_raw(1 << 8), start, "host"),
                ColmenaError::ChildFailure { exit_code: 1, .. }
            ));
        }
    }
}
