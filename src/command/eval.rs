use std::path::PathBuf;

use clap::Args;

use crate::error::{ColmenaError, ColmenaResult};
use crate::nix::Hive;

/// Evaluate an expression using the complete configuration
///
/// Your expression should take an attribute set with keys `pkgs`, `lib` and `nodes` (like a NixOS
/// module) and return a JSON-serializable value. For example, to retrieve the configuration of one
/// node, you may write something like:
///
///    { nodes, ... }: nodes.node-a.config.networking.hostName
#[derive(Debug, Args)]
#[command(name = "eval", alias = "introspect")]
pub struct Opts {
    /// The Nix expression
    #[arg(short = 'E', value_name = "EXPRESSION")]
    expression: Option<String>,

    /// Actually instantiate the expression
    #[arg(long)]
    instantiate: bool,

    /// The .nix file containing the expression
    #[arg(value_name = "FILE", conflicts_with("expression"))]
    expression_file: Option<PathBuf>,
}

pub async fn run(
    hive: Hive,
    Opts {
        expression,
        instantiate,
        expression_file,
    }: Opts,
) -> Result<(), ColmenaError> {
    let Some(expression) = get_expression(
        expression,
        expression_file,
        hive.is_flake(),
        hive.base_flags().impure(),
    )?
    else {
        tracing::error!(
            "Provide either an expression (-E) or a .nix file containing an expression."
        );
        quit::with_code(1);
    };

    let result = hive.introspect(expression, instantiate).await?;

    if instantiate {
        print!("{}", result);
    } else {
        println!("{}", result);
    }

    Ok(())
}

/// Returns the expression to evaluate, from the file when one is given.
///
/// A `hive.nix` hive imports the file, which resolves the relative paths in
/// it against its directory and keeps the file positions in errors. A flake
/// hive evaluates purely, where importing a path outside the store is
/// forbidden, and reads the file instead, unless `--impure` allows the import.
fn get_expression(
    expression: Option<String>,
    expression_file: Option<PathBuf>,
    flake: bool,
    impure: bool,
) -> ColmenaResult<Option<String>> {
    let Some(path) = expression_file else {
        return Ok(expression);
    };

    let expression = if flake && !impure {
        std::fs::read_to_string(&path)
    } else {
        path.canonicalize()
            .map(|path| format!("import {}", path.display()))
    }
    .map_err(|error| ColmenaError::ExpressionFileError { path, error })?;

    Ok(Some(expression))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    use tempfile::NamedTempFile;

    #[test]
    fn test_get_expression() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"{ nodes, ... }: nodes\n").unwrap();

        let from_file = get_expression(None, Some(file.path().to_owned()), true, false).unwrap();
        assert_eq!(Some("{ nodes, ... }: nodes\n".to_string()), from_file);

        let import = Some(format!(
            "import {}",
            file.path().canonicalize().unwrap().display()
        ));

        let imported = get_expression(None, Some(file.path().to_owned()), false, false).unwrap();
        assert_eq!(import, imported);

        // an impure flake hive imports the file too
        let imported = get_expression(None, Some(file.path().to_owned()), true, true).unwrap();
        assert_eq!(import, imported);

        let inline = get_expression(Some("1".to_string()), None, true, false).unwrap();
        assert_eq!(Some("1".to_string()), inline);

        assert_eq!(None, get_expression(None, None, true, false).unwrap());

        let missing = get_expression(None, Some(file.path().join("missing.nix")), true, false);
        assert!(matches!(
            missing,
            Err(ColmenaError::ExpressionFileError { .. })
        ));
    }
}
