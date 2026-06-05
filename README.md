# enclo

A modular rootless container launcher and secrets management system. It runs isolated development containers via [Podman](https://podman.io/) and gives them transparent access to host secrets, SSH/GPG agents, GPU devices, display servers, and other services — without granting root access.

**Supported platforms:** Linux (systemd + D-Bus), macOS (LaunchAgent + Keychain)

---

## Prerequisites

### All platforms
- [Rust toolchain](https://rustup.rs/) (stable, for building from source)
- [Podman](https://podman.io/getting-started/installation) configured for rootless use

### Linux only
- `xdg-dbus-proxy` — required for the D-Bus proxy module (usually in your distro's `xdg-desktop-portal` or `bubblewrap` package)
- A running systemd user session (`loginctl enable-linger $USER` if needed)

### macOS only
- Swift toolchain (ships with Xcode or the Xcode Command Line Tools)
- Podman Desktop or `podman machine` set up and running

---

## Installation

Clone the repo and run the install script:

```sh
git clone https://github.com/refractalize/enclo.git
cd enclo
bash install.sh
```

The script:
1. Builds all workspace crates in release mode (`cargo build --release`)
2. Installs binaries to `~/.local/bin/`
3. **Linux:** registers a systemd user service and D-Bus service activation file
4. **macOS:** builds the Swift approval dialog, installs a LaunchAgent, and cross-compiles static Linux binaries for use inside containers

Make sure `~/.local/bin` is on your `PATH`.

---

## Quick start

### 1. Write a container config

Container configs live in `~/.config/enclo/containers/<NAME>.toml`. Create one for your default container:

```toml
# ~/.config/enclo/containers/dev.toml

[container]
name   = "dev"
image  = "docker.io/library/ubuntu:24.04"

[modules]
```

### 2. Start the daemon

**Linux:**
```sh
systemctl --user start enclo
# or start it once manually:
enclo serve
```

**macOS** — the LaunchAgent starts automatically at login. To start it immediately after install:
```sh
launchctl start org.enclo
```

### 3. Run your container

```sh
enclo start          # start the container named "dev" in the background
enclo enter          # open a shell inside it (starts it first if needed)
enclo stop           # stop the container
```

---

## Container configuration reference

```toml
[container]
name   = "dev"           # also used as the Podman container name
image  = "my-image:tag"

# Optional fields and their defaults:
network               = "host"    # Podman network mode
userns                = "keep-id" # user namespace mapping
exec_shell            = "bash"    # shell used by `enclo enter`
exec_workdir          = ""        # working dir for `exec`; defaults to $PWD on host
keep_groups           = false     # pass host supplementary groups to the container
security_label_disable = false    # disable SELinux/AppArmor labels (useful for bind mounts)
command               = ["sh", "-c", "trap 'exit 0' TERM INT; while true; do sleep 1 & wait $!; done"]

# Extra volume mounts: "host:container[:options]"
# %h = $HOME, %t = $XDG_RUNTIME_DIR
volumes = [
    "%h/projects:/home/user/projects",
    "my-named-volume:/data",
]
```

### Available modules

All modules are **disabled by default**. Enable only what you need.

#### GPG agent

Forwards the host GPG agent socket into the container.

```toml
[modules.gpg_agent]
enabled = true
```

#### SSH agent

```toml
[modules.ssh_agent]
enabled = true
backend = "gpg_agent"   # or "standalone" for a regular ssh-agent
# socket_path = ""      # override host socket path
```

#### D-Bus proxy

Exposes a filtered D-Bus session bus inside the container (uses `xdg-dbus-proxy`).

```toml
[modules.dbus_proxy]
enabled = true
# Portal access is allowed by default:
allowed_interfaces = ["org.freedesktop.portal.Desktop"]
# Fine-grained rules (optional):
# call_rules      = ["org.example.Name=method@/path"]
# broadcast_rules = ["org.example.Name=signal@/path"]
```

#### Docker / Podman socket

```toml
[modules.docker_socket]
enabled     = true
socket_path = "/var/run/docker.sock"   # default

[modules.podman_socket]
enabled = true
# socket_path = ""   # defaults to $XDG_RUNTIME_DIR/podman/podman.sock
```

#### Secrets

See [Secrets service](#secrets-service) below for how to configure `enclo-secrets`.

```toml
[modules.secrets]
enabled = true
config  = "~/.config/enclo-secrets/config.toml"
mode    = "explicit"   # "explicit" (sets $ENCLO_PROXY) or "implicit" (sets $HTTP_PROXY/$HTTPS_PROXY)
```

#### AWS credentials

Injects temporary AWS credentials into the container environment via the secrets service.

```toml
[modules.aws_credentials]
enabled = true
```

#### Wayland

```toml
[modules.wayland]
enabled = true
# socket_path = "/tmp/wayland-0"   # default path inside container
```

#### DRM (GPU device access without NVIDIA)

```toml
[modules.drm]
enabled = true
```

#### NVIDIA GPU

```toml
[modules.nvidia]
enabled = true
```

#### XDG open

Forwards `xdg-open` calls from inside the container to the host.

```toml
[modules.xdg_open]
enabled = true
```

---

## Secrets service

`enclo-secrets` is a standalone daemon that aggregates secrets from multiple backends and optionally acts as an authenticating HTTP/S proxy.

### Config file

```toml
# ~/.config/enclo-secrets/config.toml

# ── Secret store ─────────────────────────────────────────────────────────────

[store.cache]
idle_ttl = "5m"
max_ttl  = "1h"
mlock    = false   # lock secret pages in RAM (requires CAP_IPC_LOCK on Linux)

[store.confirm]
enabled     = true   # prompt before releasing secrets
debounce_ms = 50

# Route secret name patterns to backends
[[store.routes]]
patterns = ["aws/*"]
backend  = "aws-vault"

[[store.routes]]
patterns = ["*"]
backend  = "pass"

# Backend definitions
[[store.backends]]
name = "pass"
# args and env are passed to the enclo-secret-store-pass process

[[store.backends]]
name = "aws-vault"
args = ["--profile", "default"]

# ── Secrets socket (for enclo-container-client) ───────────────────────────────

[secrets.socket]
enabled = true
# path = ""   # defaults to a well-known location under $XDG_RUNTIME_DIR

# ── Authenticating HTTP/S proxy (optional) ────────────────────────────────────

[proxy]
enabled = false
listen  = "127.0.0.1:8080"

[proxy.tls]
ca_cert = "ca.crt"
ca_key  = "ca.key"

[proxy.auth]
username      = "enclo"
password_hash = ""   # generate with: enclo-secrets generate-basic-auth <username>

# Inject credentials into matched domains
[[proxy.rules]]
domain = "*.example.com"
auth   = { type = "bearer", secret = "my-secret-name" }

[[proxy.rules]]
domain = "api.another.com"
auth   = { type = "basic", username = "admin", secret = "another-secret-name" }
```

### Running the secrets service

```sh
enclo-secrets serve --config ~/.config/enclo-secrets/config.toml
```

### Fetching secrets inside a container

`enclo-container-client` is injected into the container automatically when the `secrets` module is enabled:

```sh
# Fetch a secret by name
enclo-container-client get my/secret

# Run a command with secrets injected as environment variables
enclo-container-client run -- aws s3 ls
```

---

## CLI reference

### `enclo`

| Command | Description |
|---|---|
| `enclo serve` | Start the daemon (D-Bus on Linux, Unix socket on macOS) |
| `enclo run [NAME]` | Run a container directly in the foreground (no daemon required) |
| `enclo start [NAME]` | Ask the daemon to start a container |
| `enclo stop [NAME]` | Ask the daemon to stop a container |
| `enclo restart [NAME]` | Ask the daemon to restart a container |
| `enclo enter [NAME]` | Open a shell in the container (starts it if necessary) |
| `enclo exec [NAME]` | Exec into an already-running container (no daemon contact) |
| `enclo reload [NAME]` | Reload module config for a running container (sends SIGHUP) |
| `enclo status [NAME]` | Show container status; omit NAME to list all |
| `enclo setup-volume [NAME]` | Create missing home-volume directories with correct ownership |

`NAME` defaults to `dev`. Pass `--config PATH` to use an explicit config file.

### `enclo-secrets`

| Command | Description |
|---|---|
| `enclo-secrets serve` | Start the secrets service (and proxy if configured) |
| `enclo-secrets generate-basic-auth <USERNAME>` | Generate an Argon2 password hash for the proxy auth config |

---

## Logging

Set `RUST_LOG` to control log verbosity:

```sh
RUST_LOG=enclo=debug enclo serve
```

---

## Architecture overview

```
┌─────────────────────────────────────────────────────────────────┐
│  Host                                                           │
│                                                                 │
│  enclo daemon ──── D-Bus / Unix socket ──── enclo CLI          │
│       │                                                         │
│       │  Podman (rootless)                                      │
│       ▼                                                         │
│  ┌──────────────────────────────────────┐                       │
│  │  Container                           │                       │
│  │                                      │                       │
│  │  enclo-container-bridge  ◄──────────────── enclo-secrets    │
│  │  enclo-container-client               │    (host daemon)     │
│  └──────────────────────────────────────┘                       │
└─────────────────────────────────────────────────────────────────┘
```

- **enclo** — daemon + CLI that orchestrates Podman containers via pluggable modules
- **enclo-secrets** — secrets aggregator with optional authenticating HTTPS proxy; runs on the host
- **enclo-container-bridge** — lightweight static binary injected into containers; proxies secret requests back to the host
- **enclo-container-client** — container-side CLI for fetching secrets or running commands with injected credentials
- **enclo-secret-store-{pass,aws-vault,keychain}** — backend processes that retrieve secrets from each source

---

## Building from source (without installing)

```sh
cargo build --release
# Binaries land in target/release/
```

To build only specific crates:

```sh
cargo build --release -p enclo
cargo build --release -p enclo-secrets
```
