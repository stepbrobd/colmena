//! Integration-ish tests

use super::*;

use crate::error::ColmenaError;
use crate::nix::SystemType;
use crate::nix::deployment::{Deployment, EvaluationNodeLimit, EvaluatorType, Goal, Options};
use std::collections::HashSet;
use std::fs;
use std::hash::Hash;
use std::io::Write;
use std::iter::{FromIterator, Iterator};
use std::ops::Deref;
use std::path::PathBuf;

use tempfile::{Builder as TempFileBuilder, NamedTempFile};
use tokio_test::block_on;

macro_rules! node {
    ($n:expr) => {
        NodeName::new($n.to_string()).unwrap()
    };
}

fn set_eq<T>(a: &[T], b: &[T]) -> bool
where
    T: Eq + Hash,
{
    let a: HashSet<_> = HashSet::from_iter(a);
    let b: HashSet<_> = HashSet::from_iter(b);

    a == b
}

/// An ad-hoc Hive configuration.
struct TempHive {
    hive: Hive,
    _temp_file: NamedTempFile,
}

impl TempHive {
    pub fn new(text: &str) -> Self {
        Self::with_flags(text, NixFlags::default())
    }

    pub fn with_flags(text: &str, flags: NixFlags) -> Self {
        let mut temp_file = NamedTempFile::new().unwrap();
        temp_file.write_all(text.as_bytes()).unwrap();

        let hive_path = block_on(HivePath::from_path(temp_file.path(), &flags)).unwrap();
        let hive = block_on(Hive::new(hive_path, flags)).unwrap();

        Self {
            hive,
            _temp_file: temp_file,
        }
    }

    /// Asserts that the configuration is valid.
    ///
    /// Note that this _does not_ attempt to evaluate `config.toplevel`.
    pub fn valid(text: &str) {
        let mut flags = NixFlags::default();
        flags.set_show_trace(true);

        let hive = Self::with_flags(text, flags);
        assert!(block_on(hive.deployment_info()).is_ok());
    }

    /// Asserts that the configuration is invalid.
    ///
    /// Note that this _does not_ attempt to evaluate `config.toplevel`.
    pub fn invalid(text: &str) {
        let hive = Self::new(text);
        assert!(block_on(hive.deployment_info()).is_err());
    }

    /// Asserts that the specified nodes can be fully evaluated.
    pub fn eval_success(text: &str, nodes: Vec<NodeName>) {
        let hive = Self::new(text);
        let profiles = block_on(hive.eval_selected(&nodes, None));
        assert!(profiles.is_ok());
    }

    /// Asserts that the specified nodes will fail to evaluate.
    pub fn eval_failure(text: &str, nodes: Vec<NodeName>) {
        let hive = Self::new(text);
        let profiles = block_on(hive.eval_selected(&nodes, None));
        assert!(profiles.is_err());
    }
}

impl Deref for TempHive {
    type Target = Hive;

    fn deref(&self) -> &Hive {
        &self.hive
    }
}

// eval.nix tests

#[test]
fn test_parse_simple() {
    let hive = TempHive::new(
        r#"
      {
        defaults = { pkgs, ... }: {
          environment.systemPackages = with pkgs; [
            vim wget curl
          ];
          boot.loader.grub.device = "/dev/sda";
          fileSystems."/" = {
            device = "/dev/sda1";
            fsType = "ext4";
          };

          deployment.tags = [ "common-tag" ];
        };

        host-a = { name, nodes, ... }: {
          networking.hostName = name;
          time.timeZone = nodes.host-b.config.time.timeZone;

          deployment.tags = [ "a-tag" ];
        };

        host-b = {
          deployment = {
            targetHost = "somehost.tld";
            targetPort = 1234;
            targetUser = "luser";
          };
          time.timeZone = "America/Los_Angeles";
        };
      }
    "#,
    );
    let nodes = block_on(hive.deployment_info()).unwrap();

    assert!(set_eq(
        &["host-a", "host-b"],
        &nodes.keys().map(NodeName::as_str).collect::<Vec<&str>>(),
    ));

    // host-a
    let host_a = &nodes[&node!("host-a")];
    assert!(set_eq(
        &["common-tag", "a-tag"],
        &host_a
            .tags
            .iter()
            .map(String::as_str)
            .collect::<Vec<&str>>(),
    ));
    assert_eq!(Some("host-a"), host_a.target_host.as_deref());
    assert_eq!(None, host_a.target_port);
    assert_eq!(Some("root"), host_a.target_user.as_deref());

    // host-b
    let host_b = &nodes[&node!("host-b")];
    assert!(set_eq(
        &["common-tag"],
        &host_b
            .tags
            .iter()
            .map(String::as_str)
            .collect::<Vec<&str>>(),
    ));
    assert_eq!(Some("somehost.tld"), host_b.target_host.as_deref());
    assert_eq!(Some(1234), host_b.target_port);
    assert_eq!(Some("luser"), host_b.target_user.as_deref());
}

#[test]
fn test_parse_makehive_flake() {
    // make a copy of the flake so we can edit the colmena input
    let src_dir = PathBuf::from("./src/nix/hive/tests/makehive-flake");
    let flake_dir = TempFileBuilder::new()
        .prefix("makehive-flake-")
        .tempdir()
        .unwrap();

    for entry in fs::read_dir(src_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), flake_dir.as_ref().join(entry.file_name())).unwrap();
        }
    }

    let flake_nix = flake_dir.as_ref().join("flake.nix");
    let patched_flake = fs::read_to_string(&flake_nix)
        .unwrap()
        .replace("@repoPath@", env!("CARGO_MANIFEST_DIR"));

    fs::write(flake_nix, patched_flake).unwrap();

    // run the test
    let flake = block_on(Flake::from_dir(flake_dir.as_ref(), &NixFlags::default())).unwrap();

    let mut flags = NixFlags::default();
    flags.set_show_trace(true);

    let hive_path = HivePath::Flake(flake);
    let mut hive = block_on(Hive::new(hive_path, flags)).unwrap();

    let nodes = block_on(hive.deployment_info()).unwrap();
    assert!(set_eq(
        &["host-a", "host-b"],
        &nodes.keys().map(NodeName::as_str).collect::<Vec<&str>>(),
    ));

    // nix-eval-jobs --flake <installable> --select <fn>
    {
        let selected = hive.eval_selected_expr(&[node!("host-a")]).unwrap();
        assert!(selected.installable().unwrap().ends_with("#colmenaHive"));

        let expr = selected.expression();
        assert!(expr.starts_with("with builtins; hive:"));
        assert!(expr.contains("host-a"));
    }

    // nix-eval-jobs --expr <expr>
    {
        hive.set_evaluation_method(EvaluationMethod::NixInstantiate);
        assert!(
            hive.eval_selected_expr(&[node!("host-a")])
                .unwrap()
                .installable()
                .is_none()
        );
    }

    drop(flake_dir);
}

#[test]
fn test_parse_node_references() {
    TempHive::valid(
        r#"
      with builtins;
      {
        host-a = { name, nodes, ... }:
          assert name == "host-a";
          assert length (attrNames nodes) == 2;
        {
          time.timeZone = "America/Los_Angeles";
        };
        host-b = { name, nodes, ... }:
          assert name == "host-b";
          assert length (attrNames nodes) == 2;
          assert nodes.host-a.config.time.timeZone == "America/Los_Angeles";
        {};
      }
    "#,
    );
}

#[test]
fn test_parse_unknown_option() {
    TempHive::invalid(
        r#"
      {
        bad = {
          deployment.noSuchOption = "not kidding";
        };
      }
    "#,
    );
}

#[test]
fn test_config_list() {
    TempHive::valid(
        r#"
      with builtins;
      {
        host-a = [
          {
            time.timeZone = "America/Los_Angeles";
          }
          {
            deployment.tags = [ "some-tag" ];
          }
        ];
        host-b = { name, nodes, ... }:
          assert length (attrNames nodes) == 2;
          assert nodes.host-a.config.time.timeZone == "America/Los_Angeles";
          assert elem "some-tag" nodes.host-a.config.deployment.tags;
        {};
      }
    "#,
    );
}

#[test]
fn test_parse_key_text() {
    TempHive::valid(
        r#"
      {
        test = {
          deployment.keys.topSecret = {
            text = "be sure to drink your ovaltine";
          };
        };
      }
    "#,
    );
}

#[test]
fn test_parse_key_command_good() {
    TempHive::valid(
        r#"
      {
        test = {
          deployment.keys.elohim = {
            keyCommand = [ "eternalize" ];
          };
        };
      }
    "#,
    );
}

#[test]
fn test_parse_key_command_bad() {
    TempHive::invalid(
        r#"
      {
        test = {
          deployment.keys.elohim = {
            keyCommand = "transcend";
          };
        };
      }
    "#,
    );
}

#[test]
fn test_parse_key_file() {
    TempHive::valid(
        r#"
      {
        test = {
          deployment.keys.l337hax0rwow = {
            keyFile = "/etc/passwd";
          };
        };
      }
    "#,
    );
}

#[test]
fn test_key_assertion_message() {
    let hive = TempHive::new(
        r#"
      {
        test = {
          boot.isContainer = true;
          nixpkgs.system = "x86_64-linux";
          deployment.keys.both = {
            text = "secret";
            keyFile = "/etc/passwd";
          };
        };
      }
    "#,
    );

    let expr = r#"
      { nodes, ... }:
        map (a: a.message) (builtins.filter (a: !a.assertion) nodes.test.config.assertions)
    "#
    .to_string();

    let messages = block_on(hive.introspect(expr, false)).unwrap();

    assert!(messages.contains("`test.deployment.keys.both.text`"));
}

#[test]
fn test_eval_non_existent_pkg() {
    // Sanity check
    TempHive::eval_failure(
        r#"
      {
        test = { pkgs, ... }: {
          boot.isContainer = true;
          nixpkgs.system = "x86_64-linux";
          environment.systemPackages = with pkgs; [ thisPackageDoesNotExist ];
        };
      }
    "#,
        vec![node!("test")],
    );
}

// Nixpkgs config tests

#[test]
fn test_nixpkgs_system() {
    TempHive::valid(
        r#"
      {
        meta = {
          nixpkgs = import <nixpkgs> {
            system = "armv5tel-linux";
          };
        };
        test = { pkgs, ... }: {
          boot.isContainer = assert pkgs.system == "armv5tel-linux"; true;
        };
      }
    "#,
    );

    TempHive::valid(
        r#"
      {
        meta = {
          nixpkgs = import <nixpkgs> {
            system = "x86_64-linux";
          };
        };
        test = { pkgs, ... }: {
          nixpkgs.system = "armv5tel-linux";
          boot.isContainer = assert pkgs.system == "armv5tel-linux"; true;
        };
      }
    "#,
    );
}

#[test]
fn test_nixpkgs_path_like() {
    TempHive::valid(
        r#"
      {
        meta = {
          nixpkgs = {
            outPath = <nixpkgs>;
          };
        };
        test = { pkgs, ... }: {
          boot.isContainer = true;
        };
      }
    "#,
    );
}

#[test]
fn test_nixpkgs_overlay_meta_nixpkgs() {
    // Only set overlays in meta.nixpkgs
    TempHive::eval_success(
        r#"
      {
        meta = {
          nixpkgs = import <nixpkgs> {
            system = "x86_64-linux";
            overlays = [
              (self: super: { my-coreutils = super.coreutils; })
            ];
          };
        };
        test = { pkgs, ... }: {
          boot.isContainer = true;
          environment.systemPackages = with pkgs; [ my-coreutils ];
        };
      }
    "#,
        vec![node!("test")],
    );
}

#[test]
fn test_nixpkgs_overlay_node_config() {
    // Only set overlays in node config
    TempHive::eval_success(
        r#"
      {
        test = { pkgs, ... }: {
          boot.isContainer = true;
          nixpkgs.system = "x86_64-linux";
          nixpkgs.overlays = [
            (self: super: { my-coreutils = super.coreutils; })
          ];
          environment.systemPackages = with pkgs; [ my-coreutils ];
        };
      }
    "#,
        vec![node!("test")],
    );
}

#[test]
fn test_nixpkgs_overlay_both() {
    // Set overlays both in meta.nixpkgs and in node config
    TempHive::eval_success(
        r#"
      {
        meta = {
          nixpkgs = import <nixpkgs> {
            system = "x86_64-linux";
            overlays = [
              (self: super: { meta-coreutils = super.coreutils; })
            ];
          };
        };
        test = { pkgs, ... }: {
          boot.isContainer = true;
          nixpkgs.overlays = [
            (self: super: { node-busybox = super.busybox; })
          ];
          environment.systemPackages = with pkgs; [ meta-coreutils node-busybox ];
        };
      }
    "#,
        vec![node!("test")],
    );
}

#[test]
fn test_nixpkgs_config_meta_nixpkgs() {
    // Set config in meta.nixpkgs
    TempHive::eval_success(
        r#"
      {
        meta = {
          nixpkgs = import <nixpkgs> {
            system = "x86_64-linux";
            config = {
              allowUnfree = true;
            };
          };
        };
        test = { pkgs, ... }: {
          nixpkgs.config = {
            allowAliases = false;
          };
          boot.isContainer = assert pkgs.config.allowUnfree; true;
        };
      }
    "#,
        vec![node!("test")],
    );
}

#[test]
fn test_nixpkgs_config_node_config() {
    // Set config in node config
    TempHive::eval_success(
        r#"
      {
        test = { pkgs, ... }: {
          nixpkgs.system = "x86_64-linux";
          nixpkgs.config = {
            allowUnfree = true;
          };
          boot.isContainer = assert pkgs.config.allowUnfree; true;
        };
      }
    "#,
        vec![node!("test")],
    );
}

#[test]
fn test_nixpkgs_config_override() {
    // Set same config both in meta.nixpkgs and in node config
    let template = r#"
      {
        meta = {
          nixpkgs = import <nixpkgs> {
            system = "x86_64-linux";
            config = {
              allowUnfree = META_VAL;
            };
          };
        };
        test = { pkgs, ... }: {
          nixpkgs.config = {
            allowUnfree = NODE_VAL;
          };
          boot.isContainer = assert pkgs.config.allowUnfree == EXPECTED_VAL; true;
        };
      }
    "#;

    TempHive::eval_success(
        &template
            .replace("META_VAL", "true")
            .replace("NODE_VAL", "false")
            .replace("EXPECTED_VAL", "false"),
        vec![node!("test")],
    );

    TempHive::eval_success(
        &template
            .replace("META_VAL", "false")
            .replace("NODE_VAL", "true")
            .replace("EXPECTED_VAL", "true"),
        vec![node!("test")],
    );
}

#[test]
fn test_meta_special_args() {
    TempHive::valid(
        r#"
      {
        meta.specialArgs = {
          undine = "assimilated";
        };

        borg = { undine, ... }:
          assert undine == "assimilated";
        {
          boot.isContainer = true;
        };
      }
    "#,
    );
}

#[test]
fn test_meta_node_special_args() {
    TempHive::valid(
        r#"
      {
        meta.specialArgs = {
          someArg = "global";
        };

        meta.nodeSpecialArgs.node-a = {
          someArg = "node-specific";
        };

        node-a = { someArg, ... }:
          assert someArg == "node-specific";
        {
          boot.isContainer = true;
        };

        node-b = { someArg, ... }:
          assert someArg == "global";
        {
          boot.isContainer = true;
        };
      }
    "#,
    );
}

#[test]
fn test_hive_autocall() {
    TempHive::valid(
        r#"
      {
        argument ? "with default value"
      }: {
        borg = { ... }: {
          boot.isContainer = true;
        };
      }
    "#,
    );

    TempHive::valid(
        r#"
      {
        some = "value";
        __functor = self: { argument ? "with default value" }: {
          borg = { ... }: {
            boot.isContainer = assert self.some == "value"; true;
          };
        };
      }
    "#,
    );

    TempHive::invalid(
        r#"
      {
        thisWontWork
      }: {
        borg = { ... }: {
          boot.isContainer = true;
        };
      }
    "#,
    );
}

#[test]
fn test_hive_introspect() {
    let hive = TempHive::new(
        r#"
      {
        test = { ... }: {
          boot.isContainer = true;
        };
      }
    "#,
    );

    let expr = r#"
      { pkgs, lib, nodes }:
        assert pkgs ? hello;
        assert lib ? versionAtLeast;
        nodes.test.config.boot.isContainer
    "#
    .to_string();

    let eval = block_on(hive.introspect(expr, false)).unwrap();

    assert_eq!("true", eval);
}

#[test]
fn test_eval_node_limit_none_without_targets() {
    for evaluator in [EvaluatorType::Chunked, EvaluatorType::Streaming] {
        let TempHive { hive, _temp_file } = TempHive::new("{ }");

        let mut deployment = Deployment::new(hive, HashMap::new(), Goal::Build, None);
        let mut options = Options::default();
        options.set_evaluator(evaluator);
        deployment.set_options(options);
        deployment.set_evaluation_node_limit(EvaluationNodeLimit::None);

        block_on(deployment.execute()).unwrap();
    }
}

#[test]
fn test_hive_get_meta() {
    let hive = TempHive::new(
        r#"
      {
        meta.allowApplyAll = false;
        meta.specialArgs = {
          this_is_new = false;
        };
      }
  "#,
    );

    let eval = block_on(hive.get_meta_config()).unwrap();

    eprintln!("{:?}", eval);

    assert!(!eval.allow_apply_all);
}

#[test]
fn test_build_on_target_without_target_host() {
    let TempHive { hive, _temp_file } = TempHive::new(
        r#"
      {
        test = {
          boot.isContainer = true;
          nixpkgs.system = "x86_64-linux";
          deployment = {
            targetHost = null;
            buildOnTarget = true;
          };
        };
      }
    "#,
    );

    let targets = block_on(hive.select_nodes(None, None, false)).unwrap();
    let deployment = Deployment::new(hive, targets, Goal::Build, None);

    assert!(matches!(
        block_on(deployment.execute()),
        Err(ColmenaError::NoTargetHost)
    ));
}

#[test]
fn test_remote_flags_exclude_machines_file() {
    let mut flags = NixFlags::default();
    flags.add_option("cores".to_string(), "4".to_string());

    let hive = TempHive::with_flags(
        r#"
      {
        meta.machinesFile = "/etc/nix/machines";
      }
    "#,
        flags,
    );

    let with_builders = block_on(hive.nix_flags_with_builders()).unwrap();
    let argv = NixCommand::nix_store(with_builders).into_argv();
    assert!(
        argv.windows(3)
            .any(|w| w == ["--option", "builders", "@/etc/nix/machines"])
    );
    assert!(argv.windows(3).any(|w| w == ["--option", "cores", "4"]));

    let remote = NixCommand::nix_store(hive.base_flags()).into_argv();
    assert_eq!(remote, vec!["nix-store", "--option", "cores", "4"]);
}

#[test]
fn test_user_builders_override_machines_file() {
    let mut flags = NixFlags::default();
    flags.add_option("builders".to_string(), "@/custom/machines".to_string());

    let hive = TempHive::with_flags(
        r#"
      {
        meta.machinesFile = "/etc/nix/machines";
      }
    "#,
        flags,
    );

    let with_builders = block_on(hive.nix_flags_with_builders()).unwrap();
    let argv = NixCommand::nix_store(with_builders).into_argv();
    assert!(
        argv.windows(3)
            .any(|w| w == ["--option", "builders", "@/custom/machines"])
    );
    assert!(!argv.contains(&"@/etc/nix/machines".to_string()));
}

/// Builds a hive from `nodes` whose `meta.nix-darwin` stubs `darwinSystem`
/// with the module system for the `system` colmena passes.
///
/// The stub declares the nix-darwin options colmena sets, defaults the
/// nixpkgs source to its own input like `darwinSystem` does, and tags its
/// nodes so tests can tell which evaluator ran.
fn darwin_hive(nodes: &str) -> String {
    format!(
        r#"
      {{
        meta.nix-darwin.lib.darwinSystem = {{ lib, modules, specialArgs, system }}:
          lib.evalModules {{
            modules = modules ++ [
              ({{ lib, ... }}: {{
                options = {{
                  nixpkgs.source = lib.mkOption {{ type = lib.types.unspecified; }};
                  nixpkgs.flake.source = lib.mkOption {{ type = lib.types.unspecified; }};
                  nixpkgs.overlays = lib.mkOption {{ type = lib.types.listOf lib.types.unspecified; default = [ ]; }};
                  nixpkgs.config = lib.mkOption {{ type = lib.types.attrsOf lib.types.unspecified; default = {{ }}; }};
                  assertions = lib.mkOption {{ type = lib.types.listOf lib.types.unspecified; default = [ ]; }};
                  system.activationScripts.postActivation.text = lib.mkOption {{ type = lib.types.lines; default = ""; }};
                }};
                config = {{
                  _module.check = false;
                  _module.args.pkgs.stdenv.hostPlatform = lib.systems.elaborate system;
                  nixpkgs.source = lib.mkDefault "nix-darwin-input";
                  nixpkgs.flake.source = lib.mkDefault "nix-darwin-input";
                  deployment.tags = [ "darwin-stub" ];
                }};
              }})
            ];
            inherit specialArgs;
          }};
        {nodes}
      }}
    "#
    )
}

fn assert_darwin_evaluated(nodes: &str) {
    let hive = TempHive::new(&darwin_hive(nodes));
    let nodes = block_on(hive.deployment_info()).unwrap();
    let test = &nodes[&node!("test")];

    assert_eq!(SystemType::Darwin, test.system_type());
    assert_eq!(["darwin-stub".to_string()], test.tags());
}

#[test]
fn test_system_type_darwin() {
    assert_darwin_evaluated(r#"test = { deployment.systemType = "darwin"; };"#);
}

#[test]
fn test_system_type_darwin_in_defaults() {
    assert_darwin_evaluated(r#"defaults.deployment.systemType = "darwin"; test = { };"#);
}

#[test]
fn test_system_type_darwin_requires_nix_darwin() {
    // without meta.nix-darwin the node would evaluate as NixOS
    // that must fail instead of producing a NixOS profile
    TempHive::invalid(
        r#"
      {
        test = {
          deployment.systemType = "darwin";
        };
      }
    "#,
    );
}

#[test]
fn test_system_type_probe_accepts_modules_path() {
    // generated hardware-configuration.nix files import through modulesPath
    TempHive::valid(&darwin_hive(
        r#"test = { modulesPath, ... }: {
          imports = [ (modulesPath + "/profiles/minimal.nix") ];
          boot.isContainer = true;
        };"#,
    ));
}

#[test]
fn test_system_type_probe_passes_pkgs() {
    // the probe reads every deployment definition of a node
    TempHive::valid(&darwin_hive(
        r#"test = { lib, pkgs, ... }: {
          boot.isContainer = true;
          deployment = { } // lib.optionalAttrs pkgs.stdenv.isLinux { tags = [ "linux" ]; };
        };"#,
    ));
}

#[test]
fn test_nixos_hive_skips_system_type_probe() {
    // the probe declares only the deployment options
    TempHive::valid(
        r#"
      {
        test = { config, lib, ... }: {
          boot.isContainer = true;
          deployment = { } // lib.optionalAttrs config.boot.isContainer { tags = [ "container" ]; };
        };
      }
    "#,
    );
}

#[test]
fn test_nodes_argument_skips_system_type_check() {
    // deployment reads a node through nodes, which must not force its deployment
    TempHive::valid(
        r#"
      {
        defaults = { nodes, lib, ... }: {
          boot.isContainer = true;
          deployment = lib.optionalAttrs nodes.bastion.config.services.openssh.enable {
            tags = [ "behind-bastion" ];
          };
        };
        bastion = { services.openssh.enable = true; };
      }
    "#,
    );
}

#[test]
fn test_darwin_rejects_boot_before_building() {
    let TempHive { hive, _temp_file } = TempHive::new(&darwin_hive(
        r#"test = { deployment.systemType = "darwin"; };"#,
    ));

    let targets = block_on(hive.select_nodes(None, None, false)).unwrap();
    let deployment = Deployment::new(hive, targets, Goal::Boot, None);

    assert!(matches!(
        block_on(deployment.execute()),
        Err(ColmenaError::UnsupportedGoal { .. })
    ));
}

#[test]
fn test_darwin_nixpkgs_overlays_and_config_from_meta() {
    let hive = TempHive::new(&darwin_hive(
        r#"
        meta.nixpkgs = import <nixpkgs> {
          overlays = [ (final: prev: { colmenaMarker = 1; }) ];
          config.allowUnfree = true;
        };
        test = { deployment.systemType = "darwin"; };
        "#,
    ));
    let expr = r#"
      { nodes, lib, ... }:
        let nixpkgs = nodes.test.config.nixpkgs; in
        lib.length nixpkgs.overlays == 1 && nixpkgs.config.allowUnfree
    "#;
    assert_eq!(
        "true",
        block_on(hive.introspect(expr.to_string(), false)).unwrap()
    );
}

#[test]
fn test_darwin_nixpkgs_from_meta() {
    let hive = TempHive::new(&darwin_hive(
        r#"test = { deployment.systemType = "darwin"; };"#,
    ));
    let expr = r#"
      { nodes, pkgs, ... }:
        let config = nodes.test.config; in
        config.nixpkgs.source == pkgs.path && config.nixpkgs.flake.source == "nix-darwin-input"
    "#;
    assert_eq!(
        "true",
        block_on(hive.introspect(expr.to_string(), false)).unwrap()
    );

    // a node's own pin still wins
    let pinned = TempHive::new(&darwin_hive(
        r#"test = {
          deployment.systemType = "darwin";
          nixpkgs.source = "pinned-by-the-node";
        };"#,
    ));
    let expr = r#"{ nodes, ... }: nodes.test.config.nixpkgs.source"#;
    assert_eq!(
        "\"pinned-by-the-node\"",
        block_on(pinned.introspect(expr.to_string(), false)).unwrap()
    );
}

#[test]
fn test_darwin_node_needs_darwin_nixpkgs() {
    let failed = r#"
      { nodes, ... }:
        map (a: a.message) (builtins.filter (a: !a.assertion) nodes.test.config.assertions)
    "#;
    let node = r#"test = { deployment.systemType = "darwin"; };"#;

    let linux = TempHive::new(&darwin_hive(&format!(
        r#"meta.nixpkgs = import <nixpkgs> {{ system = "x86_64-linux"; }}; {node}"#
    )));
    let messages = block_on(linux.introspect(failed.to_string(), false)).unwrap();
    assert!(messages.contains("test is a darwin node, but its nixpkgs is for x86_64-linux"));

    let darwin = TempHive::new(&darwin_hive(&format!(
        r#"meta.nixpkgs = import <nixpkgs> {{ system = "x86_64-linux"; }};
        meta.nodeNixpkgs.test = import <nixpkgs> {{ system = "aarch64-darwin"; }};
        {node}"#
    )));
    assert_eq!(
        "[]",
        block_on(darwin.introspect(failed.to_string(), false)).unwrap()
    );
}

#[test]
fn test_darwin_chowns_pre_activation_keys() {
    // macOS has no root group
    // the default owner is root:wheel
    let hive = TempHive::new(&darwin_hive(
        r#"test = {
          deployment.systemType = "darwin";
          deployment.keys.default.keyCommand = [ "true" ];
          deployment.keys.owned = {
            keyCommand = [ "true" ];
            user = "nobody";
            group = "nogroup";
          };
        };"#,
    ));
    let expr = r#"
      { nodes, lib, ... }:
        let text = nodes.test.config.system.activationScripts.postActivation.text; in
        lib.hasInfix "chown root:wheel" text && lib.hasInfix "chown nobody:nogroup" text
    "#;
    assert_eq!(
        "true",
        block_on(hive.introspect(expr.to_string(), false)).unwrap()
    );
}

#[test]
fn test_nixos_defaults_skip_darwin_nodes() {
    let hive = TempHive::new(&darwin_hive(
        r#"
        defaults.deployment.tags = [ "all" ];
        nixosDefaults.deployment.tags = [ "nixos" ];
        darwinDefaults.deployment.tags = [ "darwin" ];
        linux = { boot.isContainer = true; };
        mac = { deployment.systemType = "darwin"; };
        "#,
    ));
    let nodes = block_on(hive.deployment_info()).unwrap();

    let tags = |name: &str| nodes[&node!(name)].tags().to_vec();
    assert!(set_eq(
        &["all".to_string(), "nixos".to_string()],
        &tags("linux")
    ));
    assert!(set_eq(
        &[
            "all".to_string(),
            "darwin".to_string(),
            "darwin-stub".to_string()
        ],
        &tags("mac")
    ));
}
