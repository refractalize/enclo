# Host CLI Reference (`enclo`)

`enclo` is the host-side command. It manages container lifecycle either directly or via a background daemon.

---

## Global options

| Flag | Description |
|---|---|
| `-c, --config <PATH>` | Explicit path to a container config file. When given, overrides the default name-based lookup at `~/.config/enclo/containers/<NAME>.toml`. |

---

## `enclo serve`

Start the enclo daemon.

```
enclo serve
```

On **Linux**, runs a D-Bus session bus service at `org.enclo.Manager`. On **macOS**, listens on a Unix socket at `$XDG_RUNTIME_DIR/enclo/daemon.sock`.

If a daemon is already running it is replaced. Typically managed via the systemd user service (`systemctl --user start enclo.service`).

---

## `enclo run [NAME]`

Start a container directly, in the foreground, without a daemon.

```
enclo run [NAME]
enclo --config /path/to/config.toml run
```

| Argument | Default | Description |
|---|---|---|
| `NAME` | `dev` | Container name; resolves `~/.config/enclo/containers/<NAME>.toml` |

Orchestrates all enabled modules, starts the container, and blocks until the container exits. Useful when you want to observe startup logs directly or when the daemon is unavailable.

---

## `enclo start [NAME]`

Ask the daemon to start a container.

```
enclo start [NAME]
```

| Argument | Default | Description |
|---|---|---|
| `NAME` | `dev` | Container name |

Sends a start request to the running daemon. The daemon reads the config, sets up modules, and starts the container. Returns once the daemon has acknowledged the request.

---

## `enclo stop [NAME] [--force]`

Ask the daemon to stop a container.

```
enclo stop [NAME]
enclo stop [NAME] --force
```

| Argument / Flag | Default | Description |
|---|---|---|
| `NAME` | `dev` | Container name |
| `--force` | `false` | Stop even if there are active `podman exec` sessions (e.g. open shells via `enclo enter`) |

Without `--force`, the command fails if any exec sessions are attached to the container.

---

## `enclo restart [NAME] [--force]`

Ask the daemon to stop then start a container.

```
enclo restart [NAME]
enclo restart [NAME] --force
```

| Argument / Flag | Default | Description |
|---|---|---|
| `NAME` | `dev` | Container name |
| `--force` | `false` | Restart even if there are active exec sessions |

Equivalent to `enclo stop [NAME]` followed by `enclo start [NAME]`, except it is atomic at the daemon level.

---

## `enclo enter [NAME] [-- CMD...]`

Enter a running container, starting it via the daemon first if necessary.

```
enclo enter [NAME]
enclo enter [NAME] -- zsh
enclo enter [NAME] -- bash -c "echo hello"
```

| Argument | Default | Description |
|---|---|---|
| `NAME` | `dev` | Container name |
| `CMD` | *(exec_shell from config)* | Command to run inside the container |

If the container is not running, `enclo enter` asks the daemon to start it first (waits for the container to be ready). Then runs `podman exec` with the exec environment written by the orchestrator.

The working directory inside the container defaults to the current directory on the host (mapped to the same path), or `exec_workdir` from the config if set.

---

## `enclo exec [NAME] [-- CMD...]`

Exec into an already-running container without contacting the daemon.

```
enclo exec [NAME]
enclo exec [NAME] -- zsh
```

| Argument | Default | Description |
|---|---|---|
| `NAME` | `dev` | Container name |
| `CMD` | *(exec_shell from config)* | Command to run inside the container |

Unlike `enclo enter`, this does not start the container if it is not running — it errors instead. Useful when the daemon is unavailable or when you know the container is already up.

---

## `enclo reload [NAME]`

Send SIGHUP to the modules of a running container that support hot-reload.

```
enclo reload [NAME]
```

| Argument | Default | Description |
|---|---|---|
| `NAME` | `dev` | Container name |

Currently reloads: **secrets** (re-reads the store config and proxy rules). The container itself is not restarted.

---

## `enclo status [NAME]`

Show container status.

```
enclo status
enclo status [NAME]
```

| Argument | Default | Description |
|---|---|---|
| `NAME` | *(omit to list all)* | Container name |

With no name, lists all known containers and their states. With a name, shows detailed status including module health.

---

## `enclo setup-volume [NAME] [--dry-run]`

Create missing directories inside the home named volume with the correct ownership.

```
enclo setup-volume [NAME]
enclo setup-volume [NAME] --dry-run
```

| Argument / Flag | Default | Description |
|---|---|---|
| `NAME` | `dev` | Container name |
| `--dry-run` | `false` | Print what would be created without making any changes |

Use this once after creating a container image to ensure the home volume has the expected directory structure (e.g. `.config`, `.local/share`). Ownership is set to match the container user.

---

## Files

| Path | Description |
|---|---|
| `~/.config/enclo/containers/<NAME>.toml` | Container config file (Linux) |
| `~/Library/Application Support/enclo/containers/<NAME>.toml` | Container config file (macOS) |
| `$XDG_RUNTIME_DIR/enclo/<name>/` | Per-container runtime state (sockets, exec-env) |
| `$XDG_RUNTIME_DIR/enclo/daemon.sock` | Daemon socket (macOS) |

---

## Examples

```bash
# Start the daemon, then manage a container
enclo serve &
enclo start dev
enclo enter dev
enclo stop dev

# Run a container directly (no daemon, blocking)
enclo run dev

# Enter with a specific shell
enclo enter dev -- zsh

# Run a one-off command in the container
enclo exec dev -- git status

# Reload secrets/proxy config without restarting
enclo reload dev

# Check what's running
enclo status

# Restart (e.g. after a config change)
enclo restart dev

# Use an explicit config file
enclo --config ~/my-container.toml run
```

## See also

- [Configuration reference](configuration.md)
- [In-container CLI reference](container-commands.md)
- `man enclo`
