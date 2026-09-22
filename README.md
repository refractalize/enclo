# enclo

A modular rootless container launcher and security toolchain. `enclo` starts and manages [Podman](https://podman.io/) containers with a declarative set of pluggable modules that handle secrets, GPG/SSH agent forwarding, D-Bus proxying, GPU access, Wayland display, and more — without requiring root.

## Motivation

Modern development involves running a lot of untrusted code. Installing packages from NPM, PyPI, or other registries means executing arbitrary install scripts and build tooling — any of which could silently exfiltrate SSH keys, API tokens, AWS credentials, or anything else reachable from your home directory.

enclo addresses this by making the isolation the default, not an afterthought:

- **Your container starts with no secrets.** SSH keys, GPG keys, API tokens, and cloud credentials are not mounted into the container. A compromised package or build script cannot read them.
- **Supply chain attacks are contained.** Malicious code running inside the container cannot reach your host environment. It cannot read `~/.ssh`, `~/.aws`, `~/.gnupg`, or your password manager.
- **Your development workflow still works.** Modules give the container access to SSH, GPG, Wayland, D-Bus portals, GPU, and other essential services — forwarded via controlled channels rather than broad bind mounts.
- **Access requires your explicit approval.** The secrets service gates every secret fetch behind a confirmation prompt. You see what is being requested and approve it before it is released. A HTTPS proxy injects credentials only for the specific domains and tools you configure.

## How it works

Rather than a flat `podman run` invocation, you write a TOML config describing your container and which modules to enable. `enclo` orchestrates the modules in dependency order before starting the container, injecting the right environment variables, volume mounts, and podman arguments.

A built-in secrets service (`enclo-secrets`) provides an in-container Unix socket API for fetching secrets from password managers (`pass`, `aws-vault`, macOS Keychain). An optional HTTPS MitM proxy can inject `Authorization` headers automatically so tools that don't know about secrets managers just work.

## Features

- **Secrets service** — in-container socket for explicit secret fetching; optional auto-injecting HTTP/HTTPS proxy
- **GPG agent forwarding** — mounts the host agent's extra socket; prepopulates public keys in a fresh GNUPGHOME
- **SSH agent forwarding** — forwards either the GPG agent's SSH socket or a standalone `ssh-agent`
- **D-Bus proxy** — runs `xdg-dbus-proxy` to give filtered session bus access (e.g. desktop portals)
- **Wayland** — uses `wp_security_context_manager_v1` to give the container a scoped compositor socket. No access to screencasting for e.g. just like flatpak.
- **GPU access** — NVIDIA (`--device=nvidia.com/gpu=all`) and DRM render nodes
- **Podman/Docker socket** — rootless Podman and Docker-in-container support
- **Daemon mode** — Linux D-Bus daemon (`org.enclo.Manager`) or macOS Unix socket daemon for start/stop/status
- **Hot reload** — `enclo reload` sends SIGHUP to modules that support it (secrets, proxy)

## Build and installation

Use mise to build in your existing Linux development container and install on the
Linux host, with the checkout shared between them:

```bash
# Inside the container:
mise run build

# On the host, from the same checkout:
mise run install
```

`mise run build` builds the workspace in release mode into `target/release`.
`mise run install` only installs those artifacts and the user service files;
it never builds or invokes Cargo. It checks all required binaries exist before
changing installed files. Rust is supplied by the build environment, with no
project-wide mise tool declaration that would require it on the host.

The build container must use the host's CPU architecture and compatible system
libraries (using the same distro release is the simplest option). If the checkout
is not shared, copy the release artifacts into the host checkout's `target/release`
directory first.

Other development commands:

```bash
mise run test        # cargo test --workspace
mise run check       # cargo check --workspace
mise tasks           # list available tasks
```

The scripts also work without mise: `bash scripts/build.sh` and
`bash scripts/install.sh`. The root `./install.sh` is also an install-only entrypoint.
Build separately with `mise run build` before installing.

On macOS, the build script also compiles the Swift approval helper and uses Podman
to build Linux container helpers, staging them in `target/release/linux`. The
installer copies these prebuilt artifacts and loads the LaunchAgent. Native macOS
artifacts must be built in a macOS build environment.

On Linux, installation reloads systemd and enables the user service. Start it with:

```bash
systemctl --user start enclo.service
```

Installing updated binaries does not restart an already running Linux daemon.
Restart it explicitly when you are ready for it to pick up the new version.

## Quick start

1. Create a container config at `~/.config/enclo/containers/dev.toml`:

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
```

2. Start the container via the daemon:

```bash
enclo start dev
enclo enter dev        # opens a shell
```

Or run it directly (blocking):

```bash
enclo run dev
```

3. Inside the container, fetch a secret:

```bash
enclo get GITHUB_TOKEN
enclo run GITHUB_TOKEN -- npm publish
```

## Documentation

| Document | Contents |
|---|---|
| [docs/configuration.md](docs/configuration.md) | All container and module config options |
| [docs/secrets-configuration.md](docs/secrets-configuration.md) | `enclo-secrets` config: store, access control, proxy |
| [docs/commands.md](docs/commands.md) | Host-side `enclo` CLI reference |
| [docs/container-commands.md](docs/container-commands.md) | In-container `enclo` CLI reference |
| [docs/architecture.md](docs/architecture.md) | Architecture diagrams |

Man pages: `man enclo` and `man enclo-container` after copying from `man/` to a directory on `$MANPATH`.

## Repository layout

```
enclo/                        main orchestrator
enclo-secrets/                secrets service (socket API + HTTPS proxy)
enclo-container-client/       in-container enclo CLI
enclo-backend-protocol/       shared codec for backend subprocesses
enclo-secret-store-pass/      backend: pass
enclo-secret-store-aws-vault/ backend: aws-vault
enclo-secret-store-github/    backend: GitHub CLI auth token
enclo-secret-store-keychain/  backend: macOS Keychain
enclo-aws-credentials/        in-container AWS credentials helper
enclo-container-bridge/       macOS: tunnels secrets socket via podman exec
enclo-secret-approve-macos/   macOS: native secret approval dialog
systemd/                      user service units
examples/                     annotated config examples
man/                          man pages
```
