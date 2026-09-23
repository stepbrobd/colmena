# Detached Activation

For the `switch` and `test` goals, Colmena runs `switch-to-configuration` on NixOS nodes in a transient systemd unit instead of as a child of the SSH session.
Activation often restarts the network or the firewall, which can drop the SSH connection that Colmena deploys over.
A detached activation keeps running when that happens, and Colmena reconnects to follow it.

Colmena names the unit `colmena-activate-<uuid>` and prints the name when the activation starts.
One SSH session starts the unit, reports its state every 2 seconds and streams its journal until the unit finishes, however long that takes.
When the session drops, or stays silent for 30 seconds because a network restart dropped the connection without a reset, Colmena reconnects and resumes the journal where it stopped.
If the node stays unreachable for 60 seconds after its last report, the deployment fails with an error that names the unit, and the activation may still finish on the node.

The output comes from the journal of the unit.
With `Storage=none` or a strict `MaxLevelStore` in the journald configuration of the node, Colmena shows none of it.

A unit that succeeds is stopped, which lets systemd remove it.
A unit that fails is kept for inspection:

```console
$ systemctl status colmena-activate-<uuid>
$ journalctl -u colmena-activate-<uuid>
```

A failed unit keeps `systemctl is-system-running` at `degraded`, and trips any monitoring of failed units, until the next `switch` or `test` of the node resets it, the node reboots, or you remove it:

```console
$ systemctl reset-failed 'colmena-activate-*'
```

A deployment that was interrupted, or that lost contact with the node, can leave its unit behind.
Once `systemctl list-units 'colmena-activate-*'` shows the unit as `exited`, `systemctl stop` removes it:

```console
$ systemctl stop colmena-activate-<uuid>
```

Stopping a unit that still runs interrupts its activation.

## Requirements

The deploy user runs `sh`, as it already does to upload keys, and `systemctl stop` through `deployment.privilegeEscalationCommand`.
Through `sh`, Colmena calls `systemd-run`, `systemctl` and `journalctl`.

Set `deployment.detachedActivation = false` to activate a node in the SSH session, for example when its sudo rules allow only `switch-to-configuration`.

The `boot` and `dry-activate` goals, [nix-darwin](./nix-darwin.md) nodes and `colmena apply-local` activate directly, without a unit.
