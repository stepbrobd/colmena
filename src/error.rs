//! Custom error types.

use std::fmt;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;

use snafu::{Backtrace, Snafu};
use validator::ValidationErrors;

use crate::nix::{Goal, Profile, StorePath, SystemType, key};

pub type ColmenaResult<T> = Result<T, ColmenaError>;

#[non_exhaustive]
#[derive(Debug, Snafu)]
pub enum ColmenaError {
    #[snafu(display("I/O Error: {}", error))]
    IoError { error: std::io::Error },

    #[snafu(display("Nix returned invalid response: {}", output))]
    BadOutput { output: String },

    #[snafu(display("Child process exited with error code: {}", exit_code))]
    ChildFailure {
        exit_code: i32,
        backtrace: Backtrace,
    },

    #[snafu(display("Child process was killed by signal {}", signal))]
    ChildKilled { signal: i32, backtrace: Backtrace },

    #[snafu(display("This operation is not supported"))]
    Unsupported,

    #[snafu(display("The {} goal is not supported on {} nodes", goal, system_type))]
    UnsupportedGoal { goal: Goal, system_type: SystemType },

    #[snafu(display("Invalid Nix store path"))]
    InvalidStorePath,

    #[snafu(display("Validation error"))]
    ValidationError { errors: ValidationErrors },

    #[snafu(display("Some attributes failed to evaluate"))]
    AttributeEvaluationError,

    #[snafu(display("Error processing key \"{}\": {}", name, error))]
    KeyError { name: String, error: key::KeyError },

    #[snafu(display("Store path {:?} is not a derivation", store_path))]
    NotADerivation { store_path: StorePath },

    #[snafu(display("Unknown active profile: {:?}", profile))]
    ActiveProfileUnknown { profile: Profile },

    #[snafu(display("Unexpected active profile: {:?}", profile))]
    ActiveProfileUnexpected { profile: Profile },

    #[snafu(display("Could not determine current profile"))]
    FailedToGetCurrentProfile,

    #[snafu(display(
        "Lost contact with host {} for {} seconds, activation unit {} is in an unknown state: {}",
        hostname,
        timeout.as_secs(),
        unit,
        reason
    ))]
    ActivationUnreachable {
        hostname: String,
        unit: String,
        timeout: Duration,
        reason: String,
    },

    #[snafu(display(
        "Could not start activation unit {} on host {} ({}), set deployment.detachedActivation = false to activate in the SSH session",
        unit,
        hostname,
        source
    ))]
    ActivationStartFailed {
        hostname: String,
        unit: String,
        source: Box<ColmenaError>,
    },

    #[snafu(display(
        "Activation unit {} on host {} does not exist, the SSH session may have dropped before systemd-run ran",
        unit,
        hostname
    ))]
    ActivationUnitNotFound { hostname: String, unit: String },

    #[snafu(display(
        "Activation unit {} on host {} disappeared before it finished, it was stopped or the host rebooted",
        unit,
        hostname
    ))]
    ActivationUnitVanished { hostname: String, unit: String },

    #[snafu(display(
        "Activation unit {} on host {} failed with result {}{}",
        unit,
        hostname,
        result,
        exit.map(|exit| format!(", {exit}")).unwrap_or_default()
    ))]
    ActivationFailed {
        hostname: String,
        unit: String,
        result: String,
        exit: Option<UnitExit>,
    },

    #[snafu(display("No answer from the activation watch"))]
    ActivationStepTimeout,

    #[snafu(display("The activation watch ended before the unit finished"))]
    ActivationWatchEnded,

    #[snafu(display("Unexpected line from the activation watch: {}", line))]
    ActivationBadOutput { line: String },

    #[snafu(display("Don't know how to connect to the node"))]
    NoTargetHost,

    #[snafu(display("Node name cannot be empty"))]
    EmptyNodeName,

    #[snafu(display("Filter rule cannot be empty"))]
    EmptyFilterRule,

    #[snafu(display("Unknown error: {}", message))]
    Unknown { message: String },

    #[snafu(display("Exec failed on {} hosts", n_hosts))]
    ExecError { n_hosts: usize },
}

/// How the main process of a finished systemd unit ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitExit {
    Status(i32),
    Signal(i32),
}

impl fmt::Display for UnitExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(status) => write!(f, "exit status {status}"),
            Self::Signal(signal) => write!(f, "killed by signal {signal}"),
        }
    }
}

impl From<std::io::Error> for ColmenaError {
    fn from(error: std::io::Error) -> Self {
        Self::IoError { error }
    }
}

impl From<ValidationErrors> for ColmenaError {
    fn from(errors: ValidationErrors) -> Self {
        Self::ValidationError { errors }
    }
}

impl From<ExitStatus> for ColmenaError {
    fn from(status: ExitStatus) -> Self {
        match status.code() {
            Some(exit_code) => ChildFailureSnafu { exit_code }.build(),
            None => ChildKilledSnafu {
                signal: status.signal().unwrap(),
            }
            .build(),
        }
    }
}

impl ColmenaError {
    pub fn unknown(error: Box<dyn std::error::Error>) -> Self {
        let message = error.to_string();
        Self::Unknown { message }
    }
}
