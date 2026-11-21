# Host CLI Reference (`enclo`)

`enclo` is the host-side command. It manages container lifecycle either directly or via a background daemon.

For named SSH environments, use `enclo host start NAME`, `enclo host status NAME`, or `enclo host list`. These commands use local Podman and read named profiles from `hosts/`. [SSH environment guide](ssh-environments.md)

Without a selector, top-level lifecycle commands discover the nearest `.enclo.toml` in the current directory or a parent, falling back to the central `default` profile if none exists. The implicit choice is printed to stderr. A discovered invalid, unreadable or untrusted file causes an error, never fallback. `enclo status` without a selector lists all instances.

`enclo untrust [-d PATH]` revokes approval for the nearest workspace configuration from that directory (current directory by default). It does not stop a running container and does not accept `--container` or `--config`. `enclo serve` and `enclo auth` accept no target selectors.

---

## Global options

| Flag | Description |
|---|---|
| `-d, --directory <PATH>` | Search PATH and its parents for a workspace configuration. Error if none exists; no central-default fallback. |
| `-C, --container <NAME>` | Select standalone central profile `~/.config/enclo/hosts/<NAME>.toml`, bypassing workspace discovery. NAME is the profile filename stem, not its Podman `container.name`. |
| `-c, --config <PATH>` | Select an explicit configuration file, bypassing discovery. |

These selectors are mutually exclusive and may appear before or after the command, including under `home init`. `[TARGET]` below means any one of them, or omission for implicit discovery/default selection. Positional target names are no longer accepted.

For example, `enclo enter -d .` strictly selects the current workspace, while `enclo enter -C default` selects the standalone default instance. `-d` chooses the search directory without changing the host process's working directory. For workspace `enter` and `exec`, its path maps to the corresponding directory under the workspace mount; `--workdir` overrides this.

---

## `enclo serve`

Start the enclo daemon.

```
enclo serve
```

On **Linux**, runs a D-Bus session bus service at `org.enclo.Manager`. On **macOS**, listens on a Unix socket at `$XDG_RUNTIME_DIR/enclo/daemon.sock`.

If a daemon is already running it is replaced. Typically managed via the systemd user service (`systemctl --user start enclo.service`).

---

## `enclo run [TARGET]`

Start a container directly, in the foreground, without a daemon.

```
enclo run [TARGET]
enclo --config /path/to/config.toml run
```

Orchestrates all enabled modules, starts the container, and blocks until the container exits. Useful when you want to observe startup logs directly or when the daemon is unavailable.

---

## `enclo start [TARGET]`

Ask the daemon to start a container.

```
enclo start [TARGET]
```

Sends a start request to the running daemon. The daemon reads the config, sets up modules, and starts the container. Returns once the daemon has acknowledged the request.

---

## `enclo stop [TARGET] [--force]`

Ask the daemon to stop a container.

```
enclo stop [TARGET]
enclo stop [TARGET] --force
```

| Argument / Flag | Default | Description |
|---|---|---|
| `--force` | `false` | Stop even if there are active `podman exec` sessions (e.g. open shells via `enclo enter`) |

Without `--force`, the command fails if any exec sessions are attached to the container.

---

## `enclo restart [TARGET] [--force]`

Ask the daemon to stop then start a container.

```
enclo restart [TARGET]
enclo restart [TARGET] --force
```

| Argument / Flag | Default | Description |
|---|---|---|
| `--force` | `false` | Restart even if there are active exec sessions |

Equivalent to `enclo stop [TARGET]` followed by `enclo start [TARGET]`, except it is atomic at the daemon level.

---

## `enclo enter [TARGET] [-- CMD...]`

Enter a running container, starting it via the daemon first if necessary.

```
enclo enter [TARGET]
enclo enter [TARGET] -- zsh
enclo enter [TARGET] -- bash -c "echo hello"
```

| Argument | Default | Description |
|---|---|---|
| `CMD` | *(exec_shell from config)* | Command to run inside the container |

If the container is not running, `enclo enter` asks the daemon to start it first (waits for the container to be ready). Then runs `podman exec` with the exec environment written by the orchestrator.

For workspaces, the working directory follows the selected directory (or host current directory for implicit selection), mapped under the workspace mount. For standalone profiles it uses `exec_workdir` from the config, then the host current directory. `--workdir` overrides both.

---

## `enclo exec [TARGET] [-- CMD...]`

Exec into an already-running container without contacting the daemon.

```
enclo exec [TARGET]
enclo exec [TARGET] -- zsh
```

| Argument | Default | Description |
|---|---|---|
| `CMD` | *(exec_shell from config)* | Command to run inside the container |

Unlike `enclo enter`, this does not start the container if it is not running — it errors instead. Useful when the daemon is unavailable or when you know the container is already up.

---

## `enclo reload [TARGET]`

Send SIGHUP to the modules of a running container that support hot-reload.

```
enclo reload [TARGET]
```

Currently reloads: **secrets** (re-reads the store config and proxy rules). The container itself is not restarted.

---

## `enclo status [TARGET]`

Show container status.

```
enclo status
enclo status [TARGET]
```

Without a selector, lists all known containers and their states. With a selector, shows detailed status including module health.

---

## `enclo setup-volume [TARGET] [--dry-run]`

Create missing directories inside the home named volume with the correct ownership.

```
enclo setup-volume [TARGET]
enclo setup-volume [TARGET] --dry-run
```

| Argument / Flag | Default | Description |
|---|---|---|
| `--dry-run` | `false` | Print what would be created without making any changes |

Use this once after creating a container image to ensure the home volume has the expected directory structure (e.g. `.config`, `.local/share`). Ownership is set to match the container user.

---

## Files

| Path | Description |
|---|---|
| `~/.config/enclo/hosts/<NAME>.toml` | Container config file (Linux) |
| `~/.config/enclo/hosts/<NAME>.toml` | Container config file (macOS, also respects `XDG_CONFIG_HOME`) |
| `$XDG_RUNTIME_DIR/enclo/<name>/` | Per-container runtime state (sockets, exec-env) |
| `$XDG_RUNTIME_DIR/enclo/daemon.sock` | Daemon socket (macOS) |

---

## Examples

```bash
# Start the daemon, then manage a container
enclo serve &
enclo start -C dev
enclo enter -C dev
enclo stop -C dev

# Run a container directly (no daemon, blocking)
enclo run -C dev

# Enter with a specific shell
enclo enter -C dev -- zsh

# Run a one-off command in the container
enclo exec -C dev -- git status

# Reload secrets/proxy config without restarting
enclo reload -C dev

# Check what's running
enclo status

# Restart (e.g. after a config change)
enclo restart -C dev

# Use an explicit config file
enclo --config ~/my-container.toml run
```

## See also

- [Configuration reference](configuration.md)
- [In-container CLI reference](container-commands.md)
- `man enclo`

## `enclo home init [TARGET] [--rerun]`

Run pending managed-home initialization in an already-running instance. With `--rerun`, clear both completion records and explicitly rerun the central script, then the local script. Without a selector, discovers the workspace configuration or uses central `default`. All target selectors are supported. Configuration changes to workspace instances require a restart first. See [persistent instance homes](configuration.md#persistent-instance-homes-and-initialization).

## Private workspace networking

A local `.enclo.toml` can request an automatically created bridge network:

```toml
[container]
from = "forbes"
workspace_network = true
```

The network is specific to the workspace identity and is prepared before secrets
setup. It overrides the central profile's `network`, remains after stops, and is
reused on restart. After installing the updated binary, restart the daemon with
`systemctl --user restart enclo.service` before using it. This stops its existing
containers. Workspace identities now use `enclo-workspace-<hash>`; existing home
volumes under the old identity remain intact but are not automatically migrated.

## `enclo auth login codex`

On the host, create a separate Codex login for Enclo containers:

```sh
enclo auth login codex
```

Requires the Codex CLI on `PATH`. Enclo launches its device-authentication flow
with a dedicated `CODEX_HOME` at `$XDG_DATA_HOME/enclo/codex`, defaulting to
`~/.local/share/enclo/codex`. Relative `XDG_DATA_HOME` values are ignored. It forces
`cli_auth_credentials_store="file"`, so successful credentials are saved in that
profile's `auth.json`. An inherited `CODEX_HOME` does not change this location.
The command runs from the dedicated profile, independently of the current
workspace and its Codex configuration. It does not contact the Enclo daemon or
start a container, and it rejects all target selectors.

Codex displays the verification URL and device code in the terminal; complete
sign-in in your browser. Device authentication must be enabled for your account
or workspace. Enclo waits for Codex and reports a failed login as an error. The
profile directory has mode 0700 and `auth.json` has mode 0600. Symlinked or
foreign-owned profile directories and credential files are rejected. The normal
host Codex profile is left unchanged. Rerunning the command signs in again using
the dedicated profile.

The `codex` secret backend reads and refreshes this dedicated host login. With
the Codex proxy rule configured, run `enclo auth setup codex` once, then `enclo proxy -- codex` inside a container to use it
without copying credentials into the container. See the [Codex authentication documentation](https://learn.chatgpt.com/docs/auth).
