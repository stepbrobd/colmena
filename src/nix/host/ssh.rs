use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use shell_escape::unix::escape;
use tokio::process::Command;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use super::{CopyDirection, CopyOptions, Host, MAIN_PROFILE_SCRIPT, RebootOptions, key_uploader};
use crate::error::{ColmenaError, ColmenaResult};
use crate::job::JobHandle;
use crate::nix::{
    CURRENT_PROFILE, Goal, Key, NIX_BIN_PATH, NixCommand, NixFlags, Profile, StorePath, SystemType,
};
use crate::util::{CommandExecution, CommandExt};

const ACTIVATION_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long the host may stay unreachable while an activation unit runs.
///
/// A network restart during activation drops the SSH session for a few
/// seconds. Without a bound, a host that went down during activation
/// would be polled forever.
const ACTIVATION_RECONNECT_TIMEOUT: Duration = Duration::from_secs(60);

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

    job: Option<JobHandle>,
}

/// An opaque boot ID.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BootId(String);

/// The outcome of polling a transient activation unit.
#[derive(Debug, PartialEq, Eq)]
enum ActivationState {
    Running,
    Succeeded,
    NotFound,
    Failed {
        result: String,
        exit_status: Option<i32>,
    },
}

/// The properties of a transient activation unit from `systemctl show`.
#[derive(Debug, PartialEq, Eq)]
struct ActivationStatus {
    load_state: String,
    active_state: String,
    sub_state: String,
    result: String,
    exec_main_status: Option<i32>,
}

impl ActivationStatus {
    fn from_systemctl_show(output: &str) -> ColmenaResult<Self> {
        let bad_output = || ColmenaError::BadOutput {
            output: output.to_string(),
        };

        let mut properties = HashMap::new();
        for line in output.lines().filter(|line| !line.is_empty()) {
            let (key, value) = line.split_once('=').ok_or_else(bad_output)?;
            properties.insert(key, value);
        }

        let property = |key: &str| {
            properties
                .get(key)
                .map(|value| value.to_string())
                .ok_or_else(bad_output)
        };

        let exec_main_status = match properties.get("ExecMainStatus") {
            None | Some(&"") => None,
            Some(value) => Some(value.parse().map_err(|_| bad_output())?),
        };

        Ok(Self {
            load_state: property("LoadState")?,
            active_state: property("ActiveState")?,
            sub_state: property("SubState")?,
            result: property("Result")?,
            exec_main_status,
        })
    }

    /// Maps the unit properties to an outcome.
    ///
    /// A unit stopped by an operator reports success like a completed one.
    fn state(&self) -> ActivationState {
        if self.load_state == "not-found" {
            return ActivationState::NotFound;
        }

        // RemainAfterExit=yes keeps the unit active after the process exits
        // SubState=exited tells that apart from a running process
        let running = matches!(self.active_state.as_str(), "activating" | "deactivating")
            || (self.active_state == "active" && self.sub_state != "exited");

        if running {
            ActivationState::Running
        } else if self.result == "success" {
            ActivationState::Succeeded
        } else {
            ActivationState::Failed {
                result: self.result.clone(),
                exit_status: self.exec_main_status,
            }
        }
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
            .bin_dir(NIX_BIN_PATH)
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
                .bin_dir(NIX_BIN_PATH)
                .into_argv();
            let set_profile = self.ssh_argv(argv);
            self.run_command(set_profile).await?;
        }

        // switch and test may restart the network and drop the session
        // nix-darwin has no systemd to detach into
        if self.system_type == SystemType::NixOS && matches!(goal, Goal::Switch | Goal::Test) {
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
        let script = format!("\"{MAIN_PROFILE_SCRIPT}\"");

        let paths = self
            .ssh(&["sh", "-c", script.as_str()])
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
            if let Ok(new_id) = self.get_boot_id().await
                && new_id != old_id
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

    fn message(&self, message: String) -> ColmenaResult<()> {
        match &self.job {
            Some(job) => job.message(message),
            None => Ok(()),
        }
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

        // root's PATH on macOS lacks nix-store
        // nix-copy-closure has no flag for its path
        // nix copy sets remote-program on the ssh-ng store instead
        let use_nix3_copy = self.use_nix3_copy || self.system_type == SystemType::Darwin;

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

            let mut params = vec![format!("remote-program={NIX_BIN_PATH}/nix-daemon")];
            if options.gzip {
                params.push("compress=true".to_string());
            }
            let store_uri = format!("ssh-ng://{}?{}", self.ssh_target(), params.join("&"));

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

        let mut command = self.ssh(&["sh", "-c", key_script.as_ref()]);

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

    /// Runs the activation in a transient systemd unit and polls it, so
    /// that the activation completes even when it drops the SSH session.
    ///
    /// A failed unit stays on the host for `systemctl status` and
    /// `journalctl -u`. An interrupted poll leaves a finished unit behind
    /// until `systemctl stop`.
    async fn activate_detached(&mut self, activation_command: &[String]) -> ColmenaResult<()> {
        let unit = format!("colmena-activate-{}", Uuid::new_v4().simple());
        self.message(format!("Starting activation in unit {unit}"))?;

        match self.start_activation_unit(&unit, activation_command).await {
            Err(error) if Self::is_connection_loss(&error) => {
                self.message(
                    "SSH session dropped while starting the activation, reconnecting".to_string(),
                )?;
            }
            result => result?,
        }

        let mut deadline: Option<Instant> = None;

        loop {
            let status = match self.get_activation_status(&unit).await {
                Ok(status) => status,
                Err(error) if Self::is_retryable(&error) => {
                    if deadline.is_none() {
                        self.message(format!(
                            "Lost contact with host, retrying for up to {}s",
                            ACTIVATION_RECONNECT_TIMEOUT.as_secs()
                        ))?;
                    }

                    let until = *deadline
                        .get_or_insert_with(|| Instant::now() + ACTIVATION_RECONNECT_TIMEOUT);
                    if Instant::now() > until {
                        return Err(ColmenaError::ActivationUnreachable {
                            hostname: self.host.clone(),
                            unit,
                            timeout: ACTIVATION_RECONNECT_TIMEOUT,
                            source: Box::new(error),
                        });
                    }

                    sleep(ACTIVATION_POLL_INTERVAL).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            deadline = None;

            match status.state() {
                ActivationState::Running => sleep(ACTIVATION_POLL_INTERVAL).await,
                ActivationState::Succeeded => {
                    self.cleanup_activation_unit(&unit).await;
                    return Ok(());
                }
                ActivationState::NotFound => {
                    return Err(ColmenaError::ActivationUnitNotFound {
                        hostname: self.host.clone(),
                        unit,
                    });
                }
                ActivationState::Failed {
                    result,
                    exit_status,
                } => {
                    if let Err(error) = self.emit_activation_logs(&unit).await {
                        self.message(format!(
                            "Could not read the journal of unit {unit}: {error}"
                        ))?;
                    }

                    return Err(ColmenaError::ActivationFailed {
                        hostname: self.host.clone(),
                        unit,
                        result,
                        exit_status,
                    });
                }
            }
        }
    }

    async fn start_activation_unit(
        &mut self,
        unit: &str,
        activation_command: &[String],
    ) -> ColmenaResult<()> {
        let mut command = vec![
            "systemd-run".to_string(),
            format!("--unit={unit}"),
            "--service-type=exec".to_string(),
            // keeps the exit status around for the poll
            "--property=RemainAfterExit=yes".to_string(),
            "--quiet".to_string(),
            "--".to_string(),
        ];
        command.extend_from_slice(activation_command);

        let command = self.ssh_argv(command);
        self.run_command(command).await
    }

    async fn get_activation_status(&mut self, unit: &str) -> ColmenaResult<ActivationStatus> {
        let output = self
            .ssh(&[
                "systemctl",
                "show",
                "--property=LoadState,ActiveState,SubState,Result,ExecMainStatus",
                unit,
            ])
            .capture_output()
            .await?;

        ActivationStatus::from_systemctl_show(&output)
    }

    /// Emits the last journal lines of a failed activation unit.
    async fn emit_activation_logs(&mut self, unit: &str) -> ColmenaResult<()> {
        let output = self
            .ssh(&[
                "journalctl",
                "-u",
                unit,
                "-n",
                "20",
                "--no-pager",
                "-o",
                "cat",
            ])
            .capture_output()
            .await?;

        if let Some(job) = &self.job {
            for line in output.lines() {
                job.stderr(line.to_string())?;
            }
        }

        Ok(())
    }

    /// Stops a finished unit, which releases the RemainAfterExit hold so
    /// that systemd garbage collects it. A failed stop is ignored, since
    /// the unit is only a leftover.
    async fn cleanup_activation_unit(&mut self, unit: &str) {
        let _ = self
            .ssh(&["systemctl", "stop", unit])
            .capture_output()
            .await;
    }

    fn is_connection_loss(error: &ColmenaError) -> bool {
        matches!(error, ColmenaError::ChildFailure { exit_code: 255, .. })
    }

    /// Whether a failed poll may be a lost connection or a restarting
    /// systemd rather than a failed activation.
    fn is_retryable(error: &ColmenaError) -> bool {
        matches!(
            error,
            ColmenaError::ChildFailure { .. } | ColmenaError::ChildKilled { .. }
        )
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn test_nix3_copy_uses_system_profile_nix_daemon() {
        let mut host = Ssh::new(
            Some("root".to_string()),
            "example.com".to_string(),
            NixFlags::default(),
        );
        host.set_use_nix3_copy(true);

        let store_path: StorePath = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x"
            .to_string()
            .try_into()
            .unwrap();

        let command =
            host.nix_copy_closure(&store_path, CopyDirection::ToRemote, CopyOptions::default());

        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert!(args.contains(
            &"ssh-ng://root@example.com?remote-program=/run/current-system/sw/bin/nix-daemon&compress=true"
                .to_string()
        ));
    }

    #[test]
    fn test_darwin_forces_nix3_copy() {
        let mut host = Ssh::new(
            Some("root".to_string()),
            "example.com".to_string(),
            NixFlags::default(),
        );
        host.set_system_type(SystemType::Darwin);

        let store_path: StorePath = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x"
            .to_string()
            .try_into()
            .unwrap();

        let command =
            host.nix_copy_closure(&store_path, CopyDirection::ToRemote, CopyOptions::default());

        assert_eq!(command.as_std().get_program(), "nix");
    }

    #[test]
    fn test_activation_status_parses_successful_unit() {
        // completed and kept active/exited by RemainAfterExit=yes
        let status = ActivationStatus::from_systemctl_show(
            "LoadState=loaded\nActiveState=active\nSubState=exited\nResult=success\nExecMainStatus=0\n",
        )
        .unwrap();

        assert_eq!(ActivationState::Succeeded, status.state());
    }

    #[test]
    fn test_activation_status_parses_inactive_success() {
        let status = ActivationStatus::from_systemctl_show(
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\nExecMainStatus=0\n",
        )
        .unwrap();

        assert_eq!(ActivationState::Succeeded, status.state());
    }

    #[test]
    fn test_activation_status_parses_activating_unit() {
        let status = ActivationStatus::from_systemctl_show(
            "LoadState=loaded\nActiveState=activating\nSubState=start\nResult=success\nExecMainStatus=0\n",
        )
        .unwrap();

        assert_eq!(ActivationState::Running, status.state());
    }

    #[test]
    fn test_activation_status_parses_active_running_unit() {
        let status = ActivationStatus::from_systemctl_show(
            "LoadState=loaded\nActiveState=active\nSubState=running\nResult=success\nExecMainStatus=0\n",
        )
        .unwrap();

        assert_eq!(ActivationState::Running, status.state());
    }

    #[test]
    fn test_activation_status_parses_not_found_unit() {
        let status = ActivationStatus::from_systemctl_show(
            "LoadState=not-found\nActiveState=inactive\nSubState=dead\nResult=success\nExecMainStatus=0\n",
        )
        .unwrap();

        assert_eq!(ActivationState::NotFound, status.state());
    }

    #[test]
    fn test_activation_status_handles_trailing_blank_line() {
        let status = ActivationStatus::from_systemctl_show(
            "LoadState=loaded\nActiveState=active\nSubState=exited\nResult=success\nExecMainStatus=0\n\n",
        )
        .unwrap();

        assert_eq!(ActivationState::Succeeded, status.state());
    }

    #[test]
    fn test_activation_status_parses_failed_unit() {
        let status = ActivationStatus::from_systemctl_show(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainStatus=1\n",
        )
        .unwrap();

        assert_eq!(
            ActivationState::Failed {
                result: "exit-code".to_string(),
                exit_status: Some(1),
            },
            status.state()
        );
    }

    #[test]
    fn test_activation_status_rejects_missing_property() {
        assert!(ActivationStatus::from_systemctl_show("LoadState=loaded\n").is_err());
        assert!(ActivationStatus::from_systemctl_show("garbage").is_err());
    }
}
