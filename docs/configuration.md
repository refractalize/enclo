# Container Configuration Reference

Container configs live at `~/.config/enclo/containers/<NAME>.toml`. Pass an explicit path with `--config <PATH>` to override the name-based lookup.

## Workspace-local configuration

After installing an updated binary, restart an already-running daemon with `systemctl --user restart enclo.service` before using this feature. This stops containers managed by that daemon; they can be started again afterwards. Installing the binary and running `daemon-reload` do not replace the running process.

Without a target selector, Enclo searches the current directory and its parents for the nearest `.enclo.toml`. Only that file is used; parent configurations are not merged. If none exists, the central `default` profile is used. The implicit choice is printed to stderr. A discovered invalid, unreadable or untrusted file causes an error without falling back.

Use `-d/--directory PATH` to search from a specific directory with no fallback if no workspace config exists. `-C/--container NAME` selects a standalone central profile by filename stem, and `-c/--config PATH` selects an explicit file. These selectors are mutually exclusive, may appear before or after commands, and replace positional target names.

```toml
[container]
from = "default"
workspace_network = true # optional: private bridge per instance
directory = "/workspace" # optional
```

`from` selects a profile from `~/.config/enclo/containers/` (respecting `XDG_CONFIG_HOME`). It accepts letters, numbers, hyphens and underscores. The local file may also provide `container.home.init.command_after` when the central profile enables managed homes (see below). `container.workspace_network` may also be set locally. No other settings are allowed locally. All other configuration, including secrets and proxy settings, comes from the central profile.

The directory containing `.enclo.toml` is mounted read/write at `directory`, in addition to the central mounts. The destination defaults to the central profile's `container.directory`, then `/workspace`. Destinations must be absolute and cannot overlap central mounts, module mounts or protected system paths. Host workspace paths containing `:`, `%` or newlines are unsupported.

First use asks for approval in an interactive terminal, showing the selected profile and mount. Approval is bound to the canonical workspace directory and SHA-256 of the local file; editing the file or copying it into another directory requires approval again. Noninteractive commands fail for unapproved files. Approvals live under `$XDG_STATE_HOME/enclo/trust`, defaulting to `~/.local/state/enclo/trust`. Run `enclo untrust` from a workspace to revoke its approval. Central profile edits do not require renewed approval.

Each workspace has a stable `enclo-workspace-<directory-hash>` container identity and separate runtime services, including proxy and secrets processes. Repeated invocations reuse that instance. Changed local or central configuration requires `enclo restart` before entering a running instance. Persistent volumes from the central profile remain shared unless a managed home is enabled as described below. Workspace home overlays are not implemented.

Unnamed `run`, `start`, `enter`, `exec`, `stop`, `restart`, `reload` and `setup-volume` commands use workspace discovery. `status` without a selector still lists all instances. `-C/--container` and `-c/--config` bypass discovery. `untrust` accepts `-d/--directory` but no other target selector; `serve` accepts none. `enter` and `exec` preserve the selected directory's subdirectory under the workspace mount (`--directory` when supplied, otherwise the host current directory); `--workdir` overrides this.

## Persistent instance homes and initialization

Enable a private persistent home in the **central** profile:

```toml
[container.home]
enabled = true

[container.home.init]
timeout_secs = 600 # optional; per script, must be positive
command = '''
set -eu
cd "$HOME"

if ! yadm rev-parse --git-dir >/dev/null 2>&1; then
    yadm clone --no-bootstrap https://github.com/YOUR_USER/dotfiles.git
fi

yadm bootstrap
'''
```

The image must include Git, yadm and any tools used by the scripts. TOML multiline literal strings (`'''`) preserve shell quotes and backslashes.

With `enabled = true`, Enclo mounts an `enclo-home-<instance-hash>` named volume at the host user's home path inside the container (`%h`). New homes start empty, without copying files from the image, and are owned by the normal container user. The volume survives container stops, recreation and daemon restarts. Each workspace directory gets its own volume; named central containers get one per container identity. Changing a workspace's `from` does not change its home identity.

Remove any existing volume mapping at `%h` (or an ancestor such as `/home`) before enabling this feature; Enclo rejects conflicting mappings. Existing shared homes are not copied or deleted. Explicit nested mounts remain in effect, and their contents may still be shared. Managed homes require `userns = "keep-id"` (the default, optionally with mapping options).

A local `.enclo.toml` may append a workspace-specific script:

```toml
[container]
from = "default"
directory = "/workspace"

[container.home.init]
command_after = '''
set -eu
cd /workspace
mise install
'''
```

Local files cannot enable managed homes or replace the central script. The central profile must enable the feature. The approval prompt displays the local script, and the file hash covers it; approval authorizes executing it with the container's configured capabilities and secrets.

Both scripts run with `sh -eu` as the normal mapped user, starting in `$HOME`, after module setup and post-start hooks. They have the container's network and module environment, including secrets and proxy access when configured. Enclo reports the instance ready only after initialization succeeds. Output goes to the daemon logs (or the terminal with `enclo run`); failures include the script output in the returned error. Each script has a separate timeout, defaulting to 600 seconds. Startup waiting accounts for initialization time separately from the Podman readiness timeout.

Completion is tracked separately for the central and local scripts in the reserved `.enclo-init` directory in the home volume. Successful scripts are skipped on future starts. A failure stops the container, leaves the failed stage incomplete and retries it on the next launch; completed earlier stages are preserved. Scripts should tolerate partially completed work.

Editing a completed script does not automatically rerun it. After restarting the instance to load configuration changes, explicitly run:

```sh
enclo home init --rerun
# Or select a central profile explicitly:
enclo home init -C default --rerun
```

This clears both completion records and runs the central script followed by the local script. Without `--rerun`, `enclo home init` runs only pending stages. The command requires an already-running, initialized instance. A failed or timed-out script stops that container; enter it again to retry. Removing the private home volume discards its files and completion records, so the next launch initializes a fresh home.

---

## `[container]`

Core container settings and startup behavior.

### `name` *(required)*

```toml
name = "dev"
```

The container name. Used as the Podman container name, the D-Bus service name suffix, and for runtime directory naming (`$XDG_RUNTIME_DIR/enclo/<name>/`).

### `image` *(required)*

```toml
image = "localhost/dev-container:latest"
```

The container image URI passed to `podman run`.

### `startup_timeout_secs`

```toml
startup_timeout_secs = 60   # default; increase for slower startup, e.g. 180
```

Positive integer seconds (1–4294967295) to wait after launching Podman for the
container to reach `running`. Existing configs use 60 seconds when omitted.
This checks the container state, not application health. A container that exits
early still fails immediately; a timeout triggers the existing stop and cleanup.

The daemon allows an additional 90 seconds for module setup and post-start work.
Calls joining an existing startup share its original deadline. Changes apply on
the next container start, not through module reload.

### `network`

```toml
network = "host"   # default
```

Passed as `podman run --network=<value>`. `"host"` shares the host network stack, which is the typical choice for a development container.

### `workspace_network`

```toml
workspace_network = true # default: false
```

Creates a private Podman bridge network for each container instance, including each
workspace using a central profile. Allowed in both central and local `[container]`
configuration; the local value overrides the central value. When enabled, it overrides
`network`, including the default `host` value. No network name or subnet is required.
Enclo derives `enclo-network-<identity-hash>` from the container identity and creates
its configuration before module setup. On Linux with the secrets proxy enabled,
Enclo uses `podman create` and `podman init` to activate the bridge, then starts the
proxy on a listener bound in the appropriate network namespace before `podman start`.
Credential backends stay on the host; the proxy frontend and its upstream traffic
stay in Podman’s namespace for rootless bridges. Host networking
retains its existing startup behavior. See [proxy startup](secrets-configuration.md#container-networking-and-proxy-startup).
Networks are reused across restarts and retained after stops and failed starts.
Unused networks can be removed with `podman network rm <name>`.

Workspace identities now use `enclo-workspace-<directory-hash>` instead of the old
`enclo-project-` prefix. This also changes derived managed-home volume names;
existing home volumes remain intact but are not automatically migrated. Stop old
instances before upgrading; re-entering a workspace starts its new identity.

### Devices, security options, environment and required setup commands

Central profiles may configure `devices` (`--device`), `security_opts`
(`--security-opt`), and `env` (`--env`, in `KEY=VALUE` format). Device paths and
values in `env` support the usual `%h` and `%t` expansion. These settings are not
allowed in local workspace configuration.

```toml
devices = ["/dev/fuse"]
security_opts = ["seccomp=unconfined", "unmask=ALL"]
env = ["XDG_RUNTIME_DIR=%t"]
post_start = [
  { args = ["/usr/local/bin/workspace-setup"], user = "root" },
]
```

`post_start` commands run after the container is running and before module
post-start hooks and home initialization. `user` defaults to `root`. Commands
run in order, with the container startup timeout applied to each command. A
failure stops/removes the container and tears down modules without reporting
readiness. These central-profile commands are separate from module-generated
post-start commands.

For nested rootless Podman, use [`modules.nested_podman`](#modulesnested_podman).
Its required setup runs before central `post_start` hooks and home initialization.

### `userns`

```toml
userns = "keep-id"   # default
```

Passed as `podman run --userns=<value>`. `"keep-id"` maps the host UID/GID into the container so file ownership works correctly with bind mounts.

### `keep_groups`

```toml
keep_groups = false   # default
```

When `true`, passes `--group-add=keep-groups` so the container inherits the host user's supplementary groups (e.g. `render`, `video`).

### `exec_shell`

```toml
exec_shell = "bash"   # default
```

The shell invoked by `enclo enter` and `enclo exec` when no command is given. Set to `"zsh"` or `"fish"` if your image ships a different default shell.

### `security_label_disable`

```toml
security_label_disable = false   # default
```

When `true`, passes `--security-opt=label=disable` to turn off SELinux labelling. Required on SELinux-enforcing hosts when using bind mounts that the policy does not cover.

### `volumes`

```toml
volumes = [
    "%h/workspaces:/home/user/workspaces",
    "%h/.config/git:/home/user/.config/git:ro",
    "myvolume:/data",
]
```

Additional volume mounts in `host:container[:options]` format, added on top of whatever modules inject.

**Path specifiers** (expanded before passing to Podman):

| Specifier | Expands to |
|---|---|
| `%h` | Home directory (`$HOME`) |
| `%t` | XDG runtime directory (`$XDG_RUNTIME_DIR`) |

Relative paths (starting with `.` or containing `/`) are resolved relative to the config file's directory. Plain names with no path separators (e.g. `myvolume`) are treated as named Podman volumes.

The `:O` overlay option is supported — Podman will create a writable overlay over the host path.

### `command`

```toml
command = ["sh", "-c", "trap 'exit 0' TERM INT; while true; do sleep 1 & wait $!; done"]
```

The PID 1 command inside the container. The default keeps the container alive and exits cleanly on SIGTERM/SIGINT. Override this if your image provides its own init process.

### `exec_workdir`

```toml
exec_workdir = "%h/workspaces"   # optional
```

The working directory for `enclo enter` and `enclo exec`. Supports `%h` and `%t` specifiers. Defaults to the current working directory on the host at the time the command is run.

### `tz`

```toml
tz = "local"   # optional
```

The container timezone. Passed as `podman run --tz=<value>`. Accepts:

- Area-based names: `"Europe/London"`, `"America/New_York"`
- GMT offsets: `"GMT+0"`, `"UTC-5"`
- `"local"` — mirrors the host timezone

---

## `[modules]`

All modules are disabled by default unless noted otherwise. Each module section has at least `enabled = true/false`.

Modules are set up in dependency order. If a module's prerequisites are not met (e.g. the GPG agent socket doesn't exist), `enclo` exits with an error before starting the container.

---

### `[modules.container_client]`

Mounts the `enclo-container-client` binary into the container as `/usr/local/bin/enclo`, giving you the in-container `enclo get` / `enclo run` / `enclo proxy` commands.

**Enabled by default.**

```toml
[modules.container_client]
enabled = true   # default
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `true` | Enable or disable the module |

---

### `[modules.secrets]`

Starts `enclo-secrets` as a sidecar process, then mounts its Unix socket into the container. Optionally starts an HTTPS proxy that injects authorization headers.

```toml
[modules.secrets]
enabled = true
config  = "~/.config/enclo/secrets.toml"
proxy_mode = "explicit"
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable the secrets service |
| `config` | string | *(required when enabled)* | Path to the `enclo-secrets` config file. Relative paths are resolved from the container config file's directory. |
| `proxy_mode` | `"explicit"` \| `"implicit"` | `"explicit"` | How the proxy URL is exposed in the container (only relevant when `proxy.enabled = true` in the secrets config) |
| `bridge_socket_path` | string | `"/run/enclo/secrets/secrets.sock"` | macOS only. Container path where the bridge socket is created. |

**Proxy modes:**

- `"explicit"` — sets `$ENCLO_PROXY=http://enclo:<password>@host.containers.internal:<port>`. Use `enclo proxy <cmd>` inside the container to opt traffic through the proxy.
- `"implicit"` — sets `$HTTP_PROXY`, `$HTTPS_PROXY`, `$http_proxy`, `$https_proxy`. All HTTP/HTTPS traffic goes through the proxy automatically.

**What it injects:**

- Volume: `$XDG_RUNTIME_DIR/enclo/<name>/secrets.sock` → `/run/enclo/secrets/secrets.sock`
- Volume (if proxy enabled): CA certificate → `/etc/pki/ca-trust/source/anchors/enclo-secrets-ca.pem` (read-only)
- Post-start command (if proxy enabled): `update-ca-trust` (as root)
- Env var (explicit mode): `ENCLO_PROXY`
- Env vars (implicit mode): `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy`

See [docs/secrets-configuration.md](secrets-configuration.md) for the `enclo-secrets` config file format.

---

### `[modules.gpg_agent]`

Forwards the host GPG agent into the container. The host agent's extra socket is mounted as `~/.gnupg/S.gpg-agent`, so GPG operations inside the container use the host agent. Host public keys are exported and imported into a fresh GNUPGHOME.

```toml
[modules.gpg_agent]
enabled = true
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable GPG agent forwarding |

**Prerequisites:** `gpg-agent` must be running on the host with its extra socket at `$XDG_RUNTIME_DIR/gnupg/S.gpg-agent.extra`.

**What it injects:**

- Volume: `$XDG_RUNTIME_DIR/enclo/<name>/gpg-agent/` → `$HOME/.gnupg` (contains `gpg.conf` with `no-autostart` and imported public keys)
- Volume: `$XDG_RUNTIME_DIR/gnupg/S.gpg-agent.extra` → `$HOME/.gnupg/S.gpg-agent`

---

### `[modules.ssh_agent]`

Forwards an SSH agent socket into the container and sets `$SSH_AUTH_SOCK`.

```toml
[modules.ssh_agent]
enabled = true
backend = "gpg"   # or "ssh"
# socket_path = ""      # optional override
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable SSH agent forwarding |
| `backend` | `"gpg"` \| `"ssh"` | `"gpg"` | Which agent's socket to use |
| `socket_path` | string | *(see below)* | Override the host socket path |

**Backend defaults:**

| Backend | Default socket path |
|---|---|
| `"gpg"` | `$XDG_RUNTIME_DIR/gnupg/S.gpg-agent.ssh` |
| `"ssh"` | Value of `$SSH_AUTH_SOCK` |

**What it injects:**

- Volume: `<host socket>` → `/run/ssh-agent.sock`
- Env var: `SSH_AUTH_SOCK=/run/ssh-agent.sock`

---

### `[modules.dbus_proxy]`

Spawns `xdg-dbus-proxy` to give the container filtered access to the host D-Bus session bus. By default, grants TALK policy on `org.freedesktop.portal.Desktop`.

```toml
[modules.dbus_proxy]
enabled             = true
allowed_interfaces  = ["org.freedesktop.portal.Desktop"]
call_rules          = []
broadcast_rules     = []
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable D-Bus proxy |
| `allowed_interfaces` | array of strings | `["org.freedesktop.portal.Desktop"]` | Bus names granted `--talk` policy. Allows sending method calls and receiving broadcasts from that name. |
| `call_rules` | array of strings | `[]` | Fine-grained call rules passed as `--call=<rule>`. Format: `"org.example.Name=method@/path"` |
| `broadcast_rules` | array of strings | `[]` | Fine-grained broadcast rules passed as `--broadcast=<rule>`. Format: `"org.example.Name=signal@/path"` |

**Prerequisites:** `xdg-dbus-proxy` must be installed on the host.

**What it injects:**

- Volume: `$XDG_RUNTIME_DIR/enclo/<name>/dbus-proxy/bus` → `/run/dbus-proxy/bus`
- Env var: `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/dbus-proxy/bus`

---

### `[modules.wayland]`

Gives the container scoped access to the host Wayland compositor using `wp_security_context_manager_v1`. A dedicated listening socket is registered with the compositor and mounted into the container.

```toml
[modules.wayland]
enabled     = true
# socket_path = ""   # container-side socket path; default: $XDG_RUNTIME_DIR/wayland-0
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable Wayland forwarding |
| `socket_path` | string | `$XDG_RUNTIME_DIR/wayland-0` | Container-side socket path and `$WAYLAND_DISPLAY` value. `$XDG_RUNTIME_DIR` must be set if this is not overridden. |

**Prerequisites:** The compositor must support `wp_security_context_manager_v1` (KWin 6+, sway, Hyprland). `$WAYLAND_DISPLAY` must be set on the host.

**What it injects:**

- Volume: `$XDG_RUNTIME_DIR/enclo/<name>/wayland.sock` → `<socket_path>`
- Env var: `WAYLAND_DISPLAY=<socket_path>`

The security context is revoked automatically when the container stops.

---

### `[modules.nested_podman]`

Enable a real rootless Podman engine inside the workspace container:

```toml
[modules.nested_podman]
enabled = true
```

Disabled by default. Requires a Linux host, a non-root workspace user, the default
`container.userns = "keep-id"`, and host devices `/dev/fuse` and `/dev/net/tun`.
It cannot be combined with `modules.podman_socket` or `modules.docker_socket`.

The module adds both devices, `seccomp=unconfined`, `unmask=ALL`, and
`label=disable`; it runs the workspace as its mapped user (or runs systemd as
PID 1 when `modules.systemd` is enabled) and prepares a private `XDG_RUNTIME_DIR`. These settings relax the outer container's security controls
and are applied only when the module is enabled. Conflicting security options,
including `no-new-privileges`, are rejected before launch.

The image needs a writable root filesystem, full Podman (not only podman-remote),
`newuidmap`, `newgidmap`, `setcap`, and Python 3. Install `fuse-overlayfs` and
networking helpers for the storage and network modes you use. On Fedora the
packages are `podman`, `shadow-utils-subid`, `libcap`, `python3`,
`fuse-overlayfs`, and `slirp4netns`; add `podman-compose` for `podman compose`.
No image-specific setup script or build-time helper permission changes are needed.

At startup the module runs required setup as container root. It removes the
mapping helpers' setuid/setgid bits and grants the limited file capabilities
`cap_setuid=ep` and `cap_setgid=ep` in the container's writable layer. It derives
`/etc/subuid` and `/etc/subgid` from the outer container's actual mappings,
excluding outer root and the workspace user's own IDs. Setup is repeated safely
for each new container, preserves other users' subordinate ID entries, and fails
startup if dependencies or permissions are missing.

The module disables inner cgroup management and defaults inner networking to
`host`. Here `host` means the outer workspace network namespace. Nested
containers therefore share the workspace's network, including its private
workspace bridge when enabled. Explicit inner network settings can override this
and use the supplied TUN device. Remote-engine environment defaults are cleared
so Podman uses the nested local engine. The module does not change registry
search configuration or install packages automatically.

### `[modules.podman_socket]`

Mounts the host rootless Podman socket into the container for Docker-compatible container management from within the container.

```toml
[modules.podman_socket]
enabled     = true
# socket_path = ""   # optional override
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable Podman socket forwarding |
| `socket_path` | string | `$XDG_RUNTIME_DIR/podman/podman.sock` | Override the host socket path |

**What it injects:**

- Volume: `<socket>` → `/run/podman/podman.sock`
- Env var: `CONTAINER_HOST=unix:///run/podman/podman.sock`

---

### `[modules.docker_socket]`

Mounts the host Docker socket into the container.

```toml
[modules.docker_socket]
enabled     = true
socket_path = "/var/run/docker.sock"   # default
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable Docker socket forwarding |
| `socket_path` | string | `"/var/run/docker.sock"` | Host Docker socket path |

**What it injects:**

- Volume: `<socket_path>` → `/var/run/docker.sock`

---

### `[modules.nvidia]`

Passes all NVIDIA GPUs to the container.

```toml
[modules.nvidia]
enabled = true
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable NVIDIA GPU access |

**Prerequisites:** NVIDIA devices must be present on the host (`/dev/nvidia0`).

**What it injects:**

- Podman arg: `--device=nvidia.com/gpu=all`

---

### `[modules.drm]`

Passes DRM render nodes (`/dev/dri/renderD*`) to the container for non-NVIDIA GPU compute or rendering.

```toml
[modules.drm]
enabled = true
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable DRM render node access |

**Prerequisites:** `/dev/dri/renderD*` devices must exist. The host user should be in the `render` group.

**What it injects:**

- Podman arg: `--device=/dev/dri/renderD<N>` for each render node found

---

### `[modules.aws_credentials]`

Mounts the `enclo-aws-credentials` helper binary into the container as `/usr/local/bin/enclo-aws-credentials`. It is designed to be used as an [AWS process credential provider](https://docs.aws.amazon.com/sdkref/latest/guide/feature-process-credentials.html): when invoked by the AWS SDK, it fetches credentials from the enclo secrets service and prints them to stdout in the JSON format the SDK expects.

```toml
[modules.aws_credentials]
enabled = true
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable AWS credentials helper |

**What it injects:**

- Volume: `enclo-aws-credentials` binary → `/usr/local/bin/enclo-aws-credentials` (read-only)

**Usage — `~/.aws/config`:**

Configure a profile inside the container to use `enclo-aws-credentials` as its credential source, passing the profile name as a positional argument:

```ini
[profile myprofile]
credential_process = enclo-aws-credentials myprofile
region = us-east-1
```

When the AWS SDK needs credentials for `myprofile`, it runs `enclo-aws-credentials myprofile`, which fetches the secret `AWS_CREDENTIALS?profile=myprofile` from the secrets socket and prints the result to stdout. The secret value must be JSON in the format the AWS SDK expects:

```json
{
    "Version": 1,
    "AccessKeyId": "AKIA...",
    "SecretAccessKey": "...",
    "SessionToken": "...",
    "Expiration": "2026-01-01T00:00:00Z"
}
```

`SessionToken` and `Expiration` are only required for temporary credentials. When `Expiration` is present the SDK automatically re-invokes the process before the credentials expire; when absent the credentials are treated as long-term and never refreshed.

**Secrets config:** the `AWS_CREDENTIALS` secret must be routed to a backend that returns credentials in the above format (e.g. the `aws-vault` backend):

```toml
[[store.routes]]
patterns = ["AWS_CREDENTIALS"]
backend  = "aws-vault"

[[store.backends]]
name = "aws-vault"

[store.backends.env]
AWS_VAULT_BACKEND = "pass"
```

---

### `[modules.xdg_open]`

Installs a `xdg-open` wrapper inside the container that delegates to the host desktop via the D-Bus `org.freedesktop.portal.OpenURI` portal. Requires `dbus_proxy` to be enabled with `org.freedesktop.portal.Desktop` in `allowed_interfaces`.

```toml
[modules.xdg_open]
enabled = true

[modules.dbus_proxy]
enabled            = true
allowed_interfaces = ["org.freedesktop.portal.Desktop"]
```

| Option | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `false` | Enable xdg-open integration |

**Dependencies:** `dbus_proxy`

**What it injects:**

- Volume: a generated `xdg-open` shell script → `/usr/local/bin/xdg-open` (read-only)

---

## Path specifiers

Several config values support `%`-style specifiers:

| Specifier | Value |
|---|---|
| `%h` | `$HOME` (host home directory) |
| `%t` | `$XDG_RUNTIME_DIR` (host XDG runtime directory) |

---

## Minimal example

```toml
[container]
name    = "dev"
image   = "localhost/dev-container:latest"
network = "host"
userns  = "keep-id"

[modules.ssh_agent]
enabled = true

[modules.gpg_agent]
enabled = true

[modules.dbus_proxy]
enabled = true

[modules.secrets]
enabled = true
config  = "secrets-config.toml"
```

See [default.toml.example](../default.toml.example) for a more complete annotated example.

### `[modules.systemd]`

```toml
[modules.systemd]
enabled = true
```

Runs `/sbin/init` as container root with Podman's systemd mode and a private
cgroup namespace. Requires Linux, `container.userns = "keep-id"`, and an image
with systemd and `systemctl` installed (Fedora: `dnf install systemd`). Omit
`container.command`, or set it to `["/sbin/init"]`; other commands are rejected.
The module disables SELinux container labeling to allow systemd cgroup access.
It does not enable privileged mode or change the configured network.

Startup waits for systemd to report `running` or `degraded`, within
`container.startup_timeout_secs`. A degraded system can have failed units;
inspect them with `systemctl --failed`. Readiness completes before nested Podman
setup, configured startup hooks, and managed home initialization.

`enclo enter` and `enclo exec` still default to the host workspace UID/GID;
use `enclo exec --user root -- systemctl ...` for system administration.
Systemd runs as root inside the container's user namespace; host Podman remains
rootless. This module starts the system manager, not a persistent per-user
`systemctl --user` manager. It works alongside `[modules.nested_podman]`.
