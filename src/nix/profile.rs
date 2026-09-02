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
    /// Fails for a goal without an activation step on the system type,
    /// such as `boot` on nix-darwin, which has no boot profile.
    pub fn activation_command(
        &self,
        goal: Goal,
        system_type: SystemType,
    ) -> ColmenaResult<Vec<String>> {
        let command = match system_type {
            SystemType::NixOS => self.activation_command_nixos(goal),
            SystemType::Darwin => self.activation_command_darwin(goal),
        };

        command.ok_or(ColmenaError::UnsupportedGoal { goal, system_type })
    }

    fn activation_command_nixos(&self, goal: Goal) -> Option<Vec<String>> {
        let goal = goal.as_str()?;

        Some(vec![
            self.entry("bin/switch-to-configuration"),
            goal.to_string(),
        ])
    }

    /// `darwin-rebuild activate` activates the profile its binary lives in.
    /// A dry activation runs the profile's activation script with
    /// `checkActivation=1`, which exits after the checks. `darwin-rebuild
    /// check` is unusable here because it rebuilds the target's own
    /// configuration.
    fn activation_command_darwin(&self, goal: Goal) -> Option<Vec<String>> {
        match goal {
            Goal::Switch | Goal::Test => Some(vec![
                self.entry("sw/bin/darwin-rebuild"),
                "activate".to_string(),
            ]),
            Goal::DryActivate => Some(vec![
                "env".to_string(),
                "checkActivation=1".to_string(),
                self.entry("activate"),
            ]),
            _ => None,
        }
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
    fn test_darwin_activation_commands() {
        let profile = profile();

        assert_eq!(
            profile
                .activation_command(Goal::Switch, SystemType::Darwin)
                .unwrap(),
            [
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x/sw/bin/darwin-rebuild",
                "activate"
            ]
        );
        assert_eq!(
            profile
                .activation_command(Goal::DryActivate, SystemType::Darwin)
                .unwrap(),
            [
                "env",
                "checkActivation=1",
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x/activate"
            ]
        );
        assert!(matches!(
            profile.activation_command(Goal::Boot, SystemType::Darwin),
            Err(ColmenaError::UnsupportedGoal {
                goal: Goal::Boot,
                system_type: SystemType::Darwin
            })
        ));
    }
}
