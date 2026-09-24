//! Utilities for using the key uploader script.
//!
//! The key uploader is a simple shell script that reads the contents
//! of the secret file from stdin into a temporary file then atomically
//! replaces the destination file with the temporary file.

use std::path::Path;

use futures::future::join3;
use shell_escape::unix::escape;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Child;

use crate::error::{ColmenaError, ColmenaResult};
use crate::job::JobHandle;
use crate::nix::Key;
use crate::util::capture_stream;

const SCRIPT_TEMPLATE: &str = include_str!("./key_uploader.template.sh");

/// Returns the uploader script, to be run with `sh -c`.
pub fn generate_script(key: &Key, destination: &Path, require_ownership: bool) -> String {
    SCRIPT_TEMPLATE
        .to_string()
        .replace("%DESTINATION%", destination.to_str().unwrap())
        .replace("%USER%", &escape(key.user().into()))
        .replace("%GROUP%", &escape(key.group().into()))
        .replace("%PERMISSIONS%", &escape(key.permissions().into()))
        .replace(
            "%REQUIRE_OWNERSHIP%",
            if require_ownership { "1" } else { "" },
        )
        .trim_end_matches('\n')
        .to_string()
}

pub async fn feed_uploader(
    mut uploader: Child,
    key: &Key,
    job: Option<JobHandle>,
) -> ColmenaResult<()> {
    let mut reader = key.reader().await.map_err(|error| ColmenaError::KeyError {
        name: key.name().to_owned(),
        error,
    })?;
    let mut stdin = uploader.stdin.take().unwrap();

    tokio::io::copy(reader.as_mut(), &mut stdin).await?;
    stdin.flush().await?;
    drop(stdin);

    let stdout = BufReader::new(uploader.stdout.take().unwrap());
    let stderr = BufReader::new(uploader.stderr.take().unwrap());

    let futures = join3(
        capture_stream(stdout, job.clone(), false),
        capture_stream(stderr, job.clone(), true),
        uploader.wait(),
    );
    let (stdout, stderr, exit) = futures.await;
    stdout?;
    stderr?;

    let exit = exit?;

    if exit.success() {
        Ok(())
    } else {
        Err(exit.into())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

    use tokio_test::block_on;

    use super::super::{Host, Local};
    use crate::error::ColmenaResult;
    use crate::nix::{Key, NixFlags};

    fn key(dir: &Path, user: &str, group: &str) -> Key {
        serde_json::from_value(serde_json::json!({
            "name": "secret",
            "path": dir.join("secret"),
            "text": "hunter2",
            "destDir": dir,
            "user": user,
            "group": group,
            "permissions": "0600",
            "uploadAt": "pre-activation",
        }))
        .unwrap()
    }

    fn upload(key: Key, require_ownership: bool) -> ColmenaResult<()> {
        let keys = HashMap::from([("secret".to_string(), key)]);
        block_on(Local::new(NixFlags::default()).upload_keys(&keys, require_ownership))
    }

    fn user() -> String {
        let output = Command::new("id").arg("-un").output().unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn test_local_upload_keeps_bang_in_path() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("a!b");
        // chown to another user needs root
        // the uploader skips chown for an unknown owner
        let unknown = "colmena-no-such-user";

        upload(key(&dest, unknown, unknown), false).unwrap();
        assert_eq!(
            "hunter2",
            std::fs::read_to_string(dest.join("secret")).unwrap()
        );
    }

    #[test]
    fn test_required_owner_with_unknown_group_fails() {
        let dir = tempfile::tempdir().unwrap();
        let key = key(dir.path(), &user(), "colmena-no-such-group");

        assert!(upload(key, true).is_err());
    }

    #[test]
    fn test_template_has_no_quote_or_bang() {
        // ssh_argv escapes both in a way a nushell login shell misreads
        assert!(!super::SCRIPT_TEMPLATE.contains(['\'', '!']));
    }
}
