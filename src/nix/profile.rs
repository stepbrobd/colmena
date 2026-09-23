use std::convert::TryFrom;
use std::path::Path;
use std::process::Stdio;

use super::{
    BuildResult, ColmenaError, ColmenaResult, Goal, NixCommand, NixFlags, SYSTEM_PROFILE,
    StoreDerivation, StorePath, SystemType,
};

pub type ProfileDerivation = StoreDerivation<Profile>;

/// A NixOS or nix-darwin system profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile(StorePath);

impl Profile {
    /// Returns the `nix-env --set` command that makes this the system profile.
    pub fn switch_profile_command(&self, flags: &NixFlags) -> NixCommand {
        NixCommand::nix_env(flags.clone())
            .args(["--profile", SYSTEM_PROFILE, "--set"])
            .arg(self.as_path())
    }

    /// Returns the command to activate this profile.
    ///
    /// Fails for a goal that does not activate, and for one the system type
    /// does not support, see [`SystemType::supports`].
    pub fn activation_command(
        &self,
        goal: Goal,
        system_type: SystemType,
    ) -> ColmenaResult<Vec<String>> {
        let action = goal
            .as_str()
            .filter(|_| goal.requires_activation() && system_type.supports(goal))
            .ok_or(ColmenaError::UnsupportedGoal { goal, system_type })?;

        Ok(match system_type {
            SystemType::NixOS => vec![
                self.entry("bin/switch-to-configuration"),
                action.to_string(),
            ],
            SystemType::Darwin => vec![self.entry("activate")],
        })
    }

    /// Returns the path of an entry in the profile.
    fn entry(&self, name: &str) -> String {
        self.as_path()
            .join(name)
            .to_str()
            .expect("The store path should be UTF-8 valid")
            .to_string()
    }

    /// Returns the store path.
    pub fn as_store_path(&self) -> &StorePath {
        &self.0
    }

    /// Returns the raw store path.
    pub fn as_path(&self) -> &Path {
        self.0.as_path()
    }

    /// Create a GC root for this profile.
    pub async fn create_gc_root(&self, path: &Path, flags: &NixFlags) -> ColmenaResult<()> {
        let mut command = NixCommand::nix_store(flags.clone())
            .args(["--no-build-output", "--indirect", "--add-root"])
            .arg(path)
            .arg("--realise")
            .arg(self.as_path())
            .build();
        command.stdout(Stdio::null());

        let status = command.status().await?;
        if !status.success() {
            return Err(status.into());
        }

        Ok(())
    }

    pub(super) fn from_store_path_unchecked(path: StorePath) -> Self {
        Self(path)
    }
}

impl TryFrom<BuildResult<Profile>> for Profile {
    type Error = ColmenaError;

    fn try_from(result: BuildResult<Self>) -> ColmenaResult<Self> {
        let paths = result.paths();

        if paths.is_empty() {
            return Err(ColmenaError::BadOutput {
                output: String::from("There is no store path"),
            });
        }

        if paths.len() > 1 {
            return Err(ColmenaError::BadOutput {
                output: String::from("Build resulted in more than 1 store path"),
            });
        }

        let path = paths.iter().next().unwrap().to_owned();

        Ok(Self::from_store_path_unchecked(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        let path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x".to_string();
        Profile::from_store_path_unchecked(path.try_into().unwrap())
    }

    #[test]
    fn test_darwin_activates_only_switch() {
        for goal in [
            Goal::Build,
            Goal::Push,
            Goal::Boot,
            Goal::Test,
            Goal::DryActivate,
            Goal::UploadKeys,
        ] {
            assert!(
                profile()
                    .activation_command(goal, SystemType::Darwin)
                    .is_err()
            );
        }

        assert!(
            profile()
                .activation_command(Goal::Switch, SystemType::Darwin)
                .is_ok()
        );
    }
}
