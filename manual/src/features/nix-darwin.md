# macOS with nix-darwin

Colmena can deploy macOS machines managed by [nix-darwin](https://github.com/nix-darwin/nix-darwin).
A darwin node is evaluated with `darwinSystem` from the nix-darwin flake in `meta.nix-darwin`, and activated with the `activate` script of its new system.

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nix-darwin.url = "github:nix-darwin/nix-darwin";
    nix-darwin.inputs.nixpkgs.follows = "nixpkgs";
    colmena.url = "github:nix-community/colmena";
  };

  outputs = { nixpkgs, nix-darwin, colmena, ... }: {
    colmenaHive = colmena.lib.makeHive {
      meta = {
        nixpkgs = import nixpkgs { system = "x86_64-linux"; };
        nodeNixpkgs.macbook = import nixpkgs { system = "aarch64-darwin"; };
        nix-darwin = nix-darwin;
      };

      macbook = {
        deployment.systemType = "darwin";
        system.stateVersion = 6;
      };
    };
  };
}
```

## Requirements

- `meta.nix-darwin` must be the nix-darwin flake input, from nix-darwin 25.05 or later.
- A darwin node must set `deployment.systemType = "darwin"`.
  Nodes without it are NixOS nodes.
- With `meta.nix-darwin` set, Colmena reads `deployment.systemType` of every node before it evaluates the node.
  The option must therefore be a plain value in the node or in `defaults`, and which `deployment` attributes a node defines must not depend on its configuration or on `nodes`.
  `lib.mkIf` inside `lib.mkMerge` is fine, while `lib.optionalAttrs config.<option>` is not.
  Colmena reads it with the NixOS `modulesPath`, and a darwin module imported through `modulesPath` fails there.
- The node's Nixpkgs must be for a darwin system.
  `meta.nixpkgs` is usually for Linux, so give darwin nodes their own through `meta.nodeNixpkgs`, or set `nixpkgs.hostPlatform` in the node.
  Evaluation fails with an assertion otherwise.
- Like NixOS nodes, darwin nodes build from the Nixpkgs of `meta.nixpkgs` or `meta.nodeNixpkgs`, not from the Nixpkgs input of nix-darwin.
  The module system and the release check of nix-darwin use the same Nixpkgs.
  A node can still pin its own with `nixpkgs.source`.
  The flake registry and `NIX_PATH` of the node keep the Nixpkgs input of nix-darwin, which the example makes follow `nixpkgs`.

## Defaults

The hive attribute `defaults` applies to every node.
`nixosDefaults` and `darwinDefaults` apply in addition to it, each to the nodes of one system type.
Options that exist only on NixOS, such as `boot` or `services.openssh.settings`, belong in `nixosDefaults`, because a darwin node fails to evaluate an option nix-darwin does not have.

## Goals

- `switch` activates the new system and makes it the system profile.
- `test` is not supported, since a reboot would not undo it.
  nix-darwin copies launchd plists into `/Library/LaunchDaemons`, where they outlive a reboot, and a later activation removes only the plists of the system it replaces.
- `boot` is not supported, since nix-darwin cannot activate at the next boot only.
- `dry-activate` is not supported.
  The activation script of nix-darwin clears its environment and runs steps such as `preActivation` before its checks, which leaves no part of it safe to run as a dry run.
- A `test`, `boot` or `dry-activate` deployment that selects a darwin node fails before Colmena builds anything, and its NixOS nodes are not deployed either.
  Select the NixOS nodes with `--on` to run these goals in a mixed hive.
- `colmena apply switch --reboot` is supported, and Colmena waits for the boot session of macOS to change and for nix-darwin to link `/run/current-system` again.
  `--reboot` without a goal means `boot`.
  With FileVault turned on, key logins fail until someone unlocks the disk, at the login screen or, from macOS 26, with a password over SSH, and Colmena keeps waiting until then.

## Secrets

[Secrets](./keys.md) work on darwin nodes, with these differences:

- The group of a key defaults to `wheel`, since macOS has no `root` group.
- Keys uploaded before activation get their owner at the end of the activation, after nix-darwin has created users and groups.
  If the user or group of such a key does not exist, the activation stops before it points `/run/current-system` at the new system, and the deployment fails.
- Darwin nodes do not get the `<name>-key` systemd services of NixOS nodes.
- `/run` is a link to `/private/var/run`, which is on disk.
  macOS empties it at boot, and keys in `/run/keys` do not survive a reboot, as on NixOS.
- The uploader creates `/run/keys` with `mkdir -p`, owned by root with mode 0755, so every user can list the key names.
  There is no `keys` group, and the `permissions` of each key alone protect its content.

## Local deployment

`colmena apply-local` works on macOS for a darwin node whose attribute name matches the hostname of the machine.
The node's system type must match the machine it runs on.

## Binary paths

Root's `PATH` on macOS does not include Nix.
On darwin nodes, Colmena runs remote and privileged Nix commands from `/run/current-system/sw/bin`, and copies closures with `nix copy` over `ssh-ng`, which starts the `nix-daemon` of that directory.
nix-darwin must therefore manage Nix (`nix.enable = true`, the default).
Nix installed another way, with `nix.enable = false` as for Determinate Nix, is not supported.
NixOS nodes find Nix through `PATH`.
