# SSH development environments

This is the migration design and guide to the SSH machine backend. Top-level
compatibility commands and the installed daemon retain their original behavior.
`host start/status/list/stop/restart/remove/recreate/home-init` use local Podman directly;
`services` and `session` use OpenSSH. None of these new paths require host
D-Bus, host systemd, or an installed enclo daemon for orchestration. Optional
desktop services use a host compositor/session bus, and Podman uses its configured
container/cgroup runtime. See [the agent migration guide](migrate-ssh-profiles.md)
for manual profile translation; there is no automatic profile migrator.

## Named profiles and the migration entry points

Central configuration is selected by name:

```text
$XDG_CONFIG_HOME/enclo/
    hosts/dev.toml
    connections/work.toml
    secrets/development.toml
```

Configuration-root precedence is `--config-dir`, nonempty `ENCLO_CONFIG_DIR`,
`$XDG_CONFIG_HOME/enclo`, then `~/.config/enclo`. Enclo-specific overrides are
already application roots: `--config-dir ./config` reads
`./config/connections/work.toml` and `./config/hosts/dev.toml`, with no extra
`enclo` component. The flag is global and may appear before or after subcommands.
It also applies to workspace central-profile resolution and a newly launched
container daemon's configuration lookup. It does not relocate or isolate the
existing daemon's IPC endpoint. Explicit `--profile`/`--config` files still select
their supplied paths. Connection names use the same
safe filename rules as runtime names; `work` resolves to `connections/work.toml`.
An explicit `--profile /path/to/file.toml` remains supported, but cannot be combined
with a positional connection name. Neither selection discovers repository-local
configuration or grants a workspace permission to select new secrets.

```sh
# Independent local SSH machines
./target/debug/enclo host start dev
./target/debug/enclo host status dev
./target/debug/enclo host list
./target/debug/enclo host stop dev
./target/debug/enclo host restart dev
# Legacy runner and daemon reload remain available separately
./target/debug/enclo run --config ./container.toml
./target/debug/enclo reload -C legacy

# Named connection profiles
./target/debug/enclo services start work  # background; waits for readiness
# Or: ./target/debug/enclo services run work  # foreground
./target/debug/enclo services status work
./target/debug/enclo services reload work
./target/debug/enclo services stop work
./target/debug/enclo services list
./target/debug/enclo session work -- git status
./target/debug/enclo sessions list work
./target/debug/enclo sessions list
./target/debug/enclo sessions stop SESSION_ID
./target/debug/enclo sessions stop --connection work --all
```

`services list` discovers live supervisors in the runtime root, including unhealthy
ones. Global `sessions list` discovers live session owners across connections, even
when services are stopped or profile files are absent. Global results include a
`connection` field. An unqualified `sessions stop ID` resolves that live owner by
ID; bulk stop requires a connection name or explicit profile. These commands never
search for arbitrary SSH processes.

When upgrading an existing config root, move its profile TOMLs from `containers/` to `hosts/` and rename `container` to `host` under `[target]` in connection profiles. Keep the `[container]` table in each host profile.

The new machine commands read `hosts/NAME.toml` using the small schema below.
For profiles using the legacy `[modules]` schema, use the top-level lifecycle commands such as `enclo run` and `enclo reload`.
`host remove` discards a stopped machine; `host recreate` replaces it
with the current configuration and local image. Both preserve named volumes and
the host SSH identity; running machines require `--force`.

A connection's `[target]` accepts exactly one of `ssh = "host-alias"` or
`host = "dev"`. `services run` starts/provisions a managed machine if needed.
`session` ensures services are ready before opening its independent SSH connection.
If no supervisor is running it calls the background-start path, which also starts
or provisions a managed container if needed. Concurrent sessions serialize startup;
an existing unhealthy supervisor is reconnected. A changed loaded connection profile
still requires explicit `services reload`. Startup failures prevent the remote
command from executing. Session exit leaves services and the machine running.
External machines are contacted over SSH; their lifecycle is not managed.

## Local machine setup

A minimal working configuration is included in `examples/ssh-machine`. Build its
image explicitly (ordinary Cargo tests do not build images):

```sh
podman build --build-arg DEV_UID="$(id -u)" \
  -t localhost/enclo-ssh-test:latest -f examples/ssh-machine/Containerfile examples/ssh-machine
cargo build -p enclo --offline
export ENCLO_CONFIG_DIR="$PWD/examples/ssh-machine"
export ENCLO_RUNTIME_DIR="$PWD/.enclo-runtime"
export ENCLO_STATE_DIR="$PWD/.enclo-state"
./target/debug/enclo host start dev
./target/debug/enclo services run work      # foreground; leave this running
# In another terminal with the same environment:
./target/debug/enclo session work -- uname -a
./target/debug/enclo services list
./target/debug/enclo sessions list
./target/debug/enclo services stop work
./target/debug/enclo host stop dev
```

Container config:

```toml
[container]
image = "localhost/enclo-ssh-test:latest"
# volumes = ["/absolute/host/path:/workspace"]
# env = ["LANG=C.UTF-8"]
# devices = ["/dev/dri"]
# keep_groups = true

[ssh]
user = "developer"
```

Without a `[systemd]` section, the container uses Podman's `--init` with foreground
`sshd -D -e`. The bootstrap generates host keys, creates `/run/sshd`, then replaces
itself with sshd. The image needs `/bin/sh`, `ssh-keygen`, `/usr/sbin/sshd`, `mkdir`,
`touch`, `test`, and an existing unlocked account with the host user's UID. No
systemd commands run in this mode. Podman's init handles signals and orphaned
processes; it does not restart sshd if it exits.

For systemd service management, opt in explicitly:

```toml
[systemd]
enabled = true
mask = ["systemd-oomd.service", "systemd-oomd.socket"]
```

This requires `/sbin/init`, `systemctl`, and the other systemd image dependencies.
Masks are applied inside the container with `systemctl --root=/ mask` **before**
executing systemd on every boot, including a Podman restart. Enclo always masks
`sshd.service`, `sshd.socket`, `ssh.service`, and `ssh.socket` so an image's default
SSH listener cannot take port 22 away from `enclo-sshd.service`. The additional
mask list defaults to empty; specifying masks with systemd disabled is rejected. Unit names must be
literal names, not paths/options/patterns. Masking `enclo-sshd.service` is rejected.
Masks baked into an image remain in effect too.

`Containerfile.systemd` and `hosts/systemd.toml` provide a separate example.
Its profile masks systemd-resolved and systemd-oomd and their sockets: Podman
supplies DNS configuration and the host handles memory pressure. The image's
ordinary SSH service is disabled so port 22 belongs to enclo's SSH unit.

```sh
podman build --build-arg DEV_UID="$(id -u)" \
  -t localhost/enclo-systemd-test:latest \
  -f examples/ssh-machine/Containerfile.systemd examples/ssh-machine
./target/debug/enclo host start systemd
./target/debug/enclo services run work-systemd
# In another terminal with the same config/runtime environment:
./target/debug/enclo session work-systemd -- sudo systemctl status enclo-sshd
```

Both images use a non-login password hash with password authentication disabled
and passwordless sudo inside the container. Changing startup mode or masks
requires recreating the machine; enclo does not mutate the lifecycle configuration
of an existing instance. Containers from the earlier always-systemd backend also
need recreation; installed legacy enclo containers are unaffected.

Mount strings use Podman syntax directly; there is no `%h` or relative-path
expansion in this backend. Managed homes, nested Podman, NVIDIA/DRM and optional
local desktop integrations are described below. Legacy `[modules]` configuration
is not accepted by this backend.

Podman runs locally (`--remote=false`), rootless, detached, with its small init
(or opt-in systemd) as PID 1, keep-id user mapping, a private bridge, and
`127.0.0.1:<host-port>:22` (allocated unless `[ssh] host_port` is set). SELinux container labeling remains disabled
for this initial local backend, as in
the existing systemd module. Outbound internet access remains available.
Container names and ownership labels include a hash of the canonical config
root, keeping test configurations separate from the installed environment.
Set `[container] hostname = "forbes"` to choose the hostname visible inside the
container. Enclo passes it to Podman at creation; no privileged mode is needed.
Omitting it keeps Podman's default. Hostnames accept ASCII letters, digits,
internal hyphens and dot-separated labels, up to 64 characters in total (63 per
label). This does not change the connection name, Podman name, or exported SSH
alias. Changing it requires recreation.
Existing containers are inspected, never silently recreated. `start` rejects a
changed machine config; use `recreate` to apply it. `restart` retains the existing
writable filesystem and does not apply changed configuration.

```sh
# Preserve writable-layer data while restarting:
./target/debug/enclo host stop dev
./target/debug/enclo host start dev

# Apply a changed image/config, discarding the old writable layer:
./target/debug/enclo host stop dev
./target/debug/enclo host recreate dev
# Or explicitly disconnect a running machine and replace it:
./target/debug/enclo host recreate dev --force

# Remove a stopped container; repeating remove is harmless:
./target/debug/enclo host stop dev
./target/debug/enclo host remove dev
```

`remove` and `recreate` discard the container's writable layer. Put data that must
survive recreation in named volumes or host bind mounts; enclo never passes
Podman's `--volumes` removal option. A running container is refused unless
`--force` is supplied, regardless of whether its sessions are tracked by enclo.
Recreate validates configuration and requires the image to be available locally
before removing the old container. This protects against invalid config and a
missing image, but is not a rollback mechanism: if the replacement fails later
(e.g. invalid device/mount or failed SSH provisioning), the old writable layer has
already been discarded. The replacement is left available for inspection.

Services supervisors remain separately controlled. Stop them before recreation,
or reload an unhealthy supervisor afterward to connect to the new instance.

### Persistent state and disposable runtime

State-root precedence is `--state-dir`, nonempty `ENCLO_STATE_DIR`,
`$XDG_STATE_HOME/enclo`, then `~/.local/state/enclo`. Like the config/runtime
overrides, the explicit flag and enclo environment variable name an application
root, with no extra `enclo` component. The flag is global and applies to local
machine access from `host`, `services run/reload`, and `session`.
A running supervisor retains the state root with which it was launched.

State is scoped by the same canonical-config-root hash used in Podman names,
so independent configurations cannot accidentally share a `dev` identity.
`host status` reports the resolved `state_directory`.

```text
$ENCLO_STATE_DIR/containers/<config-root-hash>/dev/
    lifecycle.lock        # shared even across different runtime roots
    client_ed25519[.pub]   # private client key stays on the host
    known_hosts           # current logical-machine host key used by SSH exports
    bootstrap/            # read-only bind mount; no private keys
        authorized_keys
        sshd_config
        boot.sh
    instances/<container-id>/
        known_hosts       # pin for that specific container instance
        user              # provisioned SSH account

$ENCLO_RUNTIME_DIR/containers/dev/
    ssh_config-<container-id>-<port>  # regenerated from live Podman + state
    endpoint.json                   # diagnostic cache only
```

State directories are owner-only (0700); private keys and pins are private files.
Start obtains the host public key through Podman and pins it on first provisioning
of a container ID. A different key for the **same ID** is rejected, never silently
accepted. Recreating the machine creates a new ID and a new trust record, while
retaining the client identity. Remove also retains the identity and old pins.
There is no automatic identity purge.

Only disposable data lives in the runtime root. After stopping services and
closing tracked sessions, that root can be removed while the container keeps
running: its bootstrap bind mount now comes from persistent state. The next
start/services run rebuilds runtime files using the existing identity and pins.
Session endpoint resolution can also regenerate its SSH config from persistent
state. Do not delete live connection control sockets while supervisors or sessions
are running; use their stop commands first.

Keep the persistent state directory at the same location while a container exists.
New containers record that location in a Podman ownership label; selecting another
state root cannot silently take over the same machine. Copying or moving a config
root changes its namespace as before.

For containers created by the earlier runtime-backed implementation, `start`
reports the incompatible bootstrap mount. Use `host recreate` to migrate;
if the old runtime directory still contains its private client key, enclo copies
it into persistent state before replacement. The old file must be a regular,
owner-only file belonging to the current user. Keep the old runtime files until
migration succeeds. Existing installed legacy enclo containers are unaffected.

The ignored `mise.local.toml` now sets `ENCLO_STATE_DIR` to `.enclo-state` alongside
the local config/runtime roots; `.enclo-state` is gitignored. Tests always select
separate temporary state, and never write identities into the normal home state.

Service shutdown leaves the machine and independent sessions running. Machine
shutdown refuses tracked active sessions in the selected runtime unless `--force`
is given. It cannot account for SSH sessions launched outside enclo. On machine
shutdown a running services supervisor detects transport loss and remains
available with an unhealthy status; stop it or reload after restarting the machine.
Automatic dependent-supervisor shutdown is not implemented yet.

## Managed secrets and proxy services

The connection supervisor now owns the host `enclo-secrets` process and its
credential backends. Enable it in a trusted connection profile:

```toml
[target]
host = "dev" # or ssh = "my-server"

[services.secrets]
config = "../secrets/development.toml"
remote_proxy_port = 18080
proxy_mode = "explicit" # default; use "implicit" for HTTP(S)_PROXY variables
# Optional: upload a client executable built for the target's architecture/libc.
# Relative paths are resolved against this connection profile.
client_binary = "/absolute/path/to/target/debug/enclo-container-client"
```

The secrets configuration uses the existing schema and keeps the same backend
routes, confirmation, caching, socket access policy, and proxy injection rules.
No raw/unrestricted store socket is exposed. `secrets.socket.enabled` and
`proxy.enabled` independently control which transports are created. The service
uses a private copy of the loaded policy; edits take effect through `services
reload`, not behind the supervisor's back.

Build the host server and optional target-compatible client alongside enclo:

```sh
cargo build -p enclo -p enclo-secrets -p enclo-container-client --offline
./target/debug/enclo services run work
# Another terminal:
./target/debug/enclo session work -- enclo get ALLOWED_SECRET
./target/debug/enclo session work -- enclo proxy -- curl https://example.com
./target/debug/enclo services status work
./target/debug/enclo services reload work
./target/debug/enclo services stop work
```

The host `enclo-secrets` binary is resolved beside the running enclo binary, then
on PATH. Credential backends must be available on PATH; their working directory
is the secrets policy directory. If `client_binary` is
omitted, use an already installed container client supporting
`ENCLO_SECRETS_SOCKET`, or pass that variable with `--socket` to an older client.
Enclo does not infer that a host binary is compatible with an arbitrary SSH target.

Startup validates the policy and CA, starts the host service, and establishes SSH
remote forwards. The host HTTP listener binds an allocated `127.0.0.1` port,
overriding `proxy.listen`; the socket path also overrides the policy's socket
path. The target listener uses `127.0.0.1:remote_proxy_port` (default 18080). Choose
another target port for simultaneous connections to the same machine. Each
service generation gets fresh proxy authentication credentials and a private
remote directory created with `mktemp`, mode 0700.

Sessions source that generation's private environment file. It supplies:

- `ENCLO_SECRETS_SOCKET` for the forwarded policy-filtered Unix socket.
- `ENCLO_PROXY` when the proxy is enabled. Explicit mode uses `enclo proxy -- ...`;
  implicit mode also sets upper/lowercase `HTTP_PROXY` and `HTTPS_PROXY`.
- A session-local CA bundle, preserving normal OS roots and adding the proxy CA,
  through `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`, `GIT_SSL_CAINFO`,
  `AWS_CA_BUNDLE`, and `NODE_EXTRA_CA_CERTS`.
- `$HOME/.local/bin` on PATH and `ENCLO_CONNECTION` when the client is uploaded.

With `client_binary` configured, service startup installs the target-compatible
client at `~/.local/bin/enclo` for both containers and remote machines. Private
settings are published at `~/.local/state/enclo/connections/NAME/active.json`.
Ordinary SSH sessions can invoke `~/.local/bin/enclo get SECRET` directly; no
shell startup files are modified. Add `~/.local/bin` to your shell PATH if needed.
The client selects the only active connection automatically. For multiple
connections to the same remote user, use `--connection NAME` or
`ENCLO_CONNECTION`; the command-line option takes precedence. Connection names
and proxy listener ports must be distinct on that target.

Stopping services withdraws their settings but leaves the installed client.
Updates are atomic, and installation refuses to overwrite an unmanaged existing
`~/.local/bin/enclo`. Generation-specific sockets and CA bundles remain in private
temporary directories; the stable settings file points to the current generation.

For AWS credentials, the installed client also provides
`enclo aws-credentials PROFILE`. It requests `AWS_CREDENTIALS?profile=PROFILE`
(with the profile URL-encoded) through the selected connection and prints the
provider's credential JSON directly. The host secrets policy must allow that
request and configure its AWS credentials provider.

For example, in the target user's `~/.aws/config`:

```ini
[profile development]
credential_process = /home/tim/.local/bin/enclo --connection work aws-credentials myprofile
region = us-east-1
```

Replace the executable path with that user's absolute home path and `myprofile`
with the host provider profile. `--connection` can be omitted when only one
connection is active, or supplied through `ENCLO_CONNECTION`. Services must be
running; the helper does not start host services. No separate helper binary or
image rebuild is needed. `--socket PATH` overrides the selected secrets socket.

Only the public CA certificate is transferred. The CA private key and backend
credentials remain on the host. Proxy credentials are sent over SSH stdin into a
mode-0600 environment file; they are not included in SSH command arguments or
status responses. No system-wide trust changes, privileged remote preparation,
network-namespace tricks, or secret socket bind mounts are needed. Applications
that ignore these CA environment variables need their own explicit trust setup.

Status includes host-service health, PID, policy revision, endpoint locations,
and the last error. A failed host service tears down its tunnels and makes the
connection unready while leaving the supervisor available for reload. Validation
errors during reload preserve the running generation. Startup failures during a
validated reload restore the previous profile and policy snapshot when possible.
Rollback cannot restore externally edited CA keys or backend executables.

Stopping services closes forwarding and terminates the host service's process
group, including credential backends. Existing shells stay alive. Reload creates
fresh service credentials and remote paths, so open a new session to pick up the
new environment; existing shells are not modified. Private remote files are
removed on normal shutdown when the machine is reachable. If it is unreachable,
that generation's `/tmp/enclo-svc.*` directory can remain, but its tunnels and host
processes are stopped. SIGKILL/crash cleanup and automatic reconnection remain
future work.

Host runtime files live under `connections/work/`:

```text
host-services.log         # owner-only diagnostic log
hs-<generation>/          # owner-only; removed when that generation stops
    config.toml           # loaded policy snapshot with resolved TLS paths
    sock                  # filtered secrets socket, when enabled
    ready                 # host-service startup marker
services-env              # remote environment filename only, not its contents
```

Existing `[[forwards]]` still work alongside managed secrets for development
servers and other already-running services. Arbitrary host-process definitions
and SSH-agent service management are not added by this slice.

## Three lifetimes

1. **Machine:** an enclo-managed container, or an existing SSH machine whose
   lifecycle belongs to someone else.
2. **Services:** host secret services, credential-injecting proxies, agent
   forwarding, and tunnels in either direction (including development servers).
   A persistent SSH transport belongs to this lifetime.
3. **Session:** an individual SSH shell connection. Multiple sessions share an
   established set of services; exiting the last shell leaves those services up.

Starting services first ensures machine availability. Opening a shell first
ensures services are ready. A managed container starts if needed; an existing
machine is checked for accessibility, never provisioned or started by enclo.
Ordinary outbound network access does not require the proxy.

Stopping services leaves both existing shells and the machine running. Starting a
new shell ensures services are ready, starting or reconnecting them if necessary.
Stopping a managed machine closes its SSH connections; its supervisor reports
transport loss. Requests to stop an external machine are rejected
without touching its services. Machine shutdown refuses tracked active shell
sessions unless forced. A failed shutdown preserves remaining resources for retry.

The library serializes explicit lifecycle operations. Its drivers implement
readiness checks and retryable cleanup. The foreground SSH supervisor detects
transport exit. A separate local IPC registry tracks shell processes independently
of the service transport. Automatic reconnection and daemon integration remain
future work. Losing a service connection never triggers a shell or machine stop.

## Configuration boundary

Container profiles own image selection, systemd and sshd bootstrapping, mounts,
persistent homes, NVIDIA/DRM, nested Podman, network isolation, resource limits,
and optional local Wayland/filtered D-Bus integration. These integrations may
retain local socket mounts; SSH is not a replacement for descriptor-passing
desktop protocols.

Connection profiles select either a managed container profile or an existing
SSH destination. They own secrets policy, proxy selection, agent forwarding,
development-server forwards, remote working directory, and session environment.
Selecting an SSH destination should preserve normal user SSH configuration.
Machine initialization happens before services; development initialization that
needs credentials happens after services are ready.

Host and remote paths, user identities, and runtime directories must be separate.
Per-connection remote runtime directories prevent socket collisions. Multiple
shells share a service instance; separate connections must not overwrite each
other's endpoints, environment, or trust settings. Repository-local configuration
must retain the current restriction against granting itself additional secrets
or forwarding permissions.

## Implementation sequence

1. Establish independent lifecycle interfaces and ordering tests (implemented).
2. Add connection-profile parsing and an OpenSSH driver for an existing target,
   exercised through foreground commands (implemented; automatic reconnects pending).
3. Host secrets/proxy processes are owned by the services supervisor, with SSH
   forwarding, policy-preserving reload, and session-scoped client/CA preparation
   (implemented).
4. Optional client upload and session-scoped proxy CA preparation are implemented
   without changes to system-wide trust. Agent forwarding remains pending.
5. Local container provider, systemd/sshd image example, authentication, host-key
   pinning, endpoint discovery, persistent SSH state, explicit remove/recreate,
   managed homes, nested Podman, NVIDIA/DRM and local desktop services are implemented.
6. The new CLI has independent machine, services and session lifetimes with
   supervisor IPC and session ownership. Legacy commands/daemon remain available
   for existing installations; removing them is a later compatibility decision.
   Profiles are migrated manually using the agent guide, not by application code.

## Using the foreground SSH commands

Create a trusted connection profile outside the repository's automatic workspace
configuration. This new command only loads the explicit `--profile` file; it
does not discover `.enclo.toml` or reuse container selectors.

```toml
[target]
ssh = "my-development-server" # alias from ~/.ssh/config
# config = "ssh_config"       # optional; relative to this profile file

[[forwards]]
direction = "remote"
spec = "127.0.0.1:8080:127.0.0.1:18080" # host HTTP service exposed on target

[[forwards]]
direction = "local"
spec = "127.0.0.1:3000:127.0.0.1:3000" # target dev server exposed on host

[session]
workdir = "/home/developer/project"

[session.env]
HTTP_PROXY = "http://127.0.0.1:8080"
HTTPS_PROXY = "http://127.0.0.1:8080"
```

Build without installing, then keep the first command running in its terminal:

```sh
cargo build -p enclo --offline
./target/debug/enclo services run --profile /path/to/connection.toml
```

`services run` runs the supervisor in the foreground. `services start` launches
that same supervisor in a new background session, redirects its output to
`$ENCLO_RUNTIME_DIR/connections/NAME/supervisor.log` (mode 0600), and waits for a
private startup acknowledgement before returning status JSON. Readiness includes
machine provisioning, configured services and all SSH forwards. A stale ready
file cannot make startup succeed. Duplicate starts fail without replacing the
existing supervisor; use `services status` or `services reload` instead.

```sh
./target/debug/enclo services start work
./target/debug/enclo services start --profile /path/to/work.toml --timeout-secs 900
./target/debug/enclo services status work
./target/debug/enclo services stop work
```

The startup timeout defaults to 600 seconds. Startup errors are returned to the
caller with a log path. Timeout or interruption terminates the new supervisor
and cleans up its service transport; it does not stop the machine or unrelated
sessions. Detached services are not a login/reboot service and do not require a
global daemon or host systemd. Config/runtime/state overrides and the launching
terminal's environment are inherited. Logs append across starts.

`session` replaces the earlier `ssh` command; `connect` is no longer accepted.

After it reports `Services ready`, use other terminals for independent sessions:

```sh
./target/debug/enclo session --profile /path/to/connection.toml
./target/debug/enclo session --profile /path/to/connection.toml -- command arg1 arg2
./target/debug/enclo session --profile /path/to/connection.toml -- git status
```

The profile uses your SSH host, user, identity, and jump-host settings. Authentication
is noninteractive (keys or an available agent) and requires an already trusted
host key. Enclo does not accept unknown keys or install credentials. Existing SSH
configuration's forwards and connection-sharing settings are overridden so this
transport owns only its declared forwards. Agent and X11 forwarding are disabled
in this initial implementation. Command arguments are individually shell-quoted;
session environment and working-directory setup require a POSIX-compatible remote
login shell. Interactive sessions use the remote default shell, not a host path.

For manually declared forwards, the destination service must already be running;
forward readiness confirms listener setup, not application health. Configured
`[services.secrets]` instead starts the host secrets/proxy service and prepares
session-scoped CA trust as described above.

Ctrl-C or SIGTERM in the foreground supervisor disconnects forwards but leaves
existing shell connections and the target machine running. Commands depending on
those forwards can fail until services are restored. Restarting on the same
endpoints permits new requests from existing shells; interrupted requests are not
replayed. Shell exits alone never disconnect services. Command exit codes are
preserved.

If the transport fails after startup, the supervisor stays alive and reports an
unhealthy transport. Use `services reload` to reconnect. SSH keepalives detect
otherwise silent connection loss. Existing
shells have their own SSH connections and do not need to migrate when services
restart. Loss of the machine itself can still end those shell connections.

### Listing and stopping sessions

These commands work whether or not `services run` is running:

```sh
./target/debug/enclo sessions --profile /path/to/connection.toml list
./target/debug/enclo sessions --profile /path/to/connection.toml stop SESSION_ID
./target/debug/enclo sessions --profile /path/to/connection.toml stop --all
```

Listing returns JSON with each live session's random ID, SSH target, owner PID
(informational only), and start time (Unix seconds). Stopping sends a request to
the owning `enclo session` process, which closes and reaps its SSH child before
acknowledging. Explicitly stopped sessions exit with code 143. Services, other
sessions, and the machine are unaffected. `--all` stops a snapshot of sessions;
it does not prevent concurrent new sessions from opening. This terminates the
local SSH connection, not necessarily every background process it started remotely.

Each session owner provides a control socket under a private local registry
directory. The CLI discovers owners through these sockets and verifies liveness
with IPC; there is no new daemon to install or keep running. Normal exit removes
the socket. Dead socket files after crashes are ignored, never used to signal a
stored PID. Unresponsive owners produce an error rather than being reported dead.
Registry identity depends on the connection name, not profile contents, so
editing a profile or restarting services does not hide existing sessions.

### Services supervisor and named runtime directory

The connection name is the profile filename without its extension: `dev.toml`
selects `dev`. Names accept 1–32 ASCII letters, digits, hyphens, or underscores.
Within a runtime root, profiles with the same filename address the same connection;
use distinct names for distinct connections.

```sh
./target/debug/enclo services --profile /path/to/dev.toml status
./target/debug/enclo services --profile /path/to/dev.toml reload
./target/debug/enclo services --profile /path/to/dev.toml stop
```

`services run` runs in the foreground. There is no global daemon or implicit background
process. A second supervisor for the same name is rejected. `stop` closes services
and exits the supervisor; shells remain running. A stopped supervisor cannot
receive requests, so start it again with the CLI. `status` reports a stopped
supervisor as JSON, even if the runtime directory does not yet exist.

Status distinguishes `supervisor_running` from `transport_ready` and includes the
loaded profile path, semantic configuration revision, target, configured forwards,
and last error. Readiness covers transport/listener establishment, not backend
HTTP health. On transport failure the control API remains available for status,
reload, or stop. Automatic reconnection is still pending.

Reload reads the profile path recorded at startup, validates it, and replaces the
services transport to apply added, removed, or changed forwards and SSH settings.
This first implementation reconnects the whole transport rather than changing
forwards in place; existing requests through those forwards may be interrupted.
Shell connections are independent and stay open. Session environment changes
apply only to new shells. Malformed configuration leaves the existing transport
untouched. If applying valid configuration fails, the supervisor tries to restore
the previous loaded profile and reports the failure. If restoration also fails,
it remains available with `transport_ready: false`. Changes to external SSH config
or key files cannot themselves be rolled back. Reload/stop operations are serialized.

Shell startup rejects a profile that differs from the loaded service configuration;
reload first. Status and stop do not parse profile contents, so malformed edits do
not make the supervisor inaccessible. The loaded profile path in status identifies
which file the supervisor will reload.

The directory layout is now stable:

```text
$XDG_RUNTIME_DIR/enclo/connections/dev/
    supervisor.sock       # enclo status/stop/reload API
    ssh-control           # OpenSSH's internal connection-control socket
    supervisor-lock       # exclusive supervisor lifetime lock
    lock                  # transport lifetime lock
    ready                 # loaded semantic profile revision
    sessions/
        <session-id>      # session-owner IPC sockets
```

All runtime directories are user-owned and mode 0700. Runtime selection precedence
is `--runtime-dir`, then nonempty `ENCLO_RUNTIME_DIR`, then `$XDG_RUNTIME_DIR/enclo`, then
the temporary-directory fallback `/tmp/enclo-<uid>/connections/<name>`.
`--runtime-dir /existing/path` or `ENCLO_RUNTIME_DIR=/existing/path`
uses `/existing/path/connections/<name>`; use the same root for services,
shells, and session-management commands. Short paths are required by Unix socket
limits. Lock files/directories remain after shutdown to avoid lock-inode races;
service sockets/readiness files are removed and session sockets remain until their
owners exit. This experimental layout replaces the earlier hash-named directories;
stop old checkout processes before switching versions.

For isolated checkout use, set the variable in each terminal (or inherit it from
your development shell):

```sh
mkdir -p -m 700 .enclo-runtime
mkdir -p .enclo-config/connections .enclo-config/hosts
export ENCLO_RUNTIME_DIR="$PWD/.enclo-runtime"
export ENCLO_CONFIG_DIR="$PWD/.enclo-config"
# Put the work profile in .enclo-config/connections/work.toml.
./target/debug/enclo services run work
# Another terminal with the same variable:
./target/debug/enclo session work
./target/debug/enclo sessions list
```

Alternatively, pass `--runtime-dir "$PWD/.enclo-runtime"` and
`--config-dir "$PWD/.enclo-config"` explicitly; flags override environment values.
The resulting paths are `.enclo-runtime/connections/work/` and
`.enclo-config/connections/work.toml`. Existing runtime roots must be user-owned
and mode 0700; `mkdir -m 700` does not change permissions of an existing directory.

Empty `ENCLO_RUNTIME_DIR` and `ENCLO_CONFIG_DIR` are treated as unset. Use an absolute, short path so
commands from different working directories share the same sockets. This setting
only affects the new services/session runtime; it does not change profile lookup
or redirect the existing container daemon/backend.

## Development and verification

Do not install this branch on the development host or restart the installed
enclo daemon. Tests must not assume host D-Bus or systemd availability.

The initial ordering and failure tests use in-memory drivers:

```sh
cargo test -p enclo --lib --offline
```

The opt-in real SSH integration test starts a temporary user-owned sshd, HTTP
server, keys, configuration, known-hosts file, and runtime directory. It needs
permission to bind local sockets and an OpenSSH server/client installation:

```sh
cargo build -p enclo-secrets -p enclo-container-client --offline
cargo test -p enclo --offline --test ssh_connection -- --ignored --nocapture
```

Set `ENCLO_TEST_SSHD` if sshd is not at `/usr/sbin/sshd`. The fixture checks both
forwarding directions, shell output/exit status, forwarding survival across shell
exits, duplicate-supervisor rejection, session survival across service shutdown
and restart, session discovery after profile edits, individual/bulk termination,
stale socket handling, named supervisor status/stop/reload, malformed reloads,
forward addition/removal, rollback, transport failure recovery, partial-startup
rollback, and unknown-host-key rejection.
It never stops a system SSH service.

The real local-machine smoke test is opt-in and runs the checkout binary against
rootless Podman. Build the example image above, then run:

```sh
cargo build -p enclo --offline
python3 scripts/smoke-local-machine.py --image localhost/enclo-ssh-test:latest
# After building Containerfile.systemd as shown above:
python3 scripts/smoke-local-machine.py --image localhost/enclo-systemd-test:latest --systemd
```

It creates its own temporary config/runtime roots, uniquely named container, and
host HTTP fixture. It checks Podman init as PID 1 by default, or clean systemd
boot, configured masks, and system unit management with `--systemd`. Both check
outbound DNS, strict host-key enforcement, HTTP forwarding, session listing,
shutdown refusal with active sessions, shell survival across services stop/start,
container stop/start with filesystem/key persistence, unhealthy-supervisor reload,
and endpoint/host-key discovery after container recreation. It also corrupts the
diagnostic endpoint cache to ensure discovery uses Podman instead. It deletes
the runtime tree with the machine still running, verifies persistent identity and
trust, applies changed config through `host recreate`, checks writable-layer
data is discarded while named-volume data survives, and verifies `container
remove` preserves the volume and identity.

Add `--secrets` to either smoke-test invocation to exercise the real host service
with a synthetic backend, socket allow/deny checks, and HTTP/HTTPS credential
injection against local upstream fixtures. It generates a temporary CA and checks
both upstream TLS verification and the session's proxy-CA trust. The test checks
that proxy authentication headers are not forwarded upstream. Build
`enclo-secrets` and `enclo-container-client` first; the fixture also uses `openssl`
on the host and `curl` in the target image. No real credential provider is used.

The test removes its containers, dedicated test volume, temporary state and
runtime files, retaining the built image.
It does not install enclo, call the installed daemon, or administer host systemd
units. Podman still uses the host's configured container/cgroup runtime; this test
requires permission to use it and bind local sockets. DNS verification requires
outbound DNS access.

Verified on 2026-11-12 with Fedora 44, rootless Podman 5.8.9, crun 1.30, cgroup v2,
and SELinux enabled on the host (container labeling disabled by this backend).
Systemd mode boots with `running` and no failed units using profile-defined masks.
The default mode runs `podman-init` as PID 1 with foreground sshd. Both modes
exercise restart, recreation, forwarding, and independent session lifetimes.

Build and run binaries from the checkout; do not invoke installation scripts or
`mise run install`.

### Managed homes

Standalone `hosts/NAME.toml` can opt into a persistent home (the example
configs enable this):

```toml
[home]
enabled = true

[home.init]
command = "cp -an /etc/skel/. ."
timeout_secs = 300
```

The image must provide `getent` and an SSH account whose UID matches the host.
Enclo discovers that account's home and GID from the image, creates a named Podman
volume scoped to the config directory and machine name, and mounts it with
`nocopy`. The home starts empty, with mode 0700 and the account's ownership;
existing contents are never recursively chowned. Explicit mounts covering the
home are rejected. Child mounts such as a workspace are allowed. Volume ownership
labels are verified before use. `host status NAME` includes the volume name,
path, UID and GID.

Initialization runs as the SSH user, with HOME and working directory set to the
managed home, before connection services are started. It has no connection secret
or proxy environment. `.enclo-init/central` records successful completion. Home
contents and this checkpoint survive stop, remove and recreate; removing a
container never removes the managed home volume. Changing the config directory
or machine name selects a different volume.

Editing the init script does not require recreation or automatically rerun it.
Use `enclo host home-init NAME --rerun` on a running machine to apply it
explicitly; without `--rerun`, a completed initialization is skipped. Failed or
timed-out initialization stops the container (including existing sessions for a
manual rerun) to terminate remaining init processes. Partial file changes remain;
no success checkpoint is saved, and the next start retries. Scripts should
therefore tolerate partial completion. The timeout defaults to 300 seconds.

Add `--managed-home` to the local-machine smoke test to check ownership,
permissions, once-only initialization, explicit rerun, and persistence through
recreation alongside SSH services and optional synthetic secrets.

## Per-container initialization

Use `container.init` for machine setup that must be repeated after recreation,
independently of the persistent home's initialization checkpoint:

```toml
[container.init]
user = "root"
command = "loginctl enable-linger tim"
timeout_secs = 60
```

The account must already exist in the image. `user` defaults to `root` and
`timeout_secs` to 60 (positive integer seconds). The command runs through
`sh -eu -c`, with working directory `/`, and HOME/USER/LOGNAME set for that
account. It has the container's environment, without connection secrets/proxy
settings. This example requires systemd mode and an image with `loginctl`.

Startup completes systemd/SSH provisioning, nested-Podman setup, and `home.init`
before running this hook; it verifies SSH readiness afterward. Thus dotfiles and
user units installed by home initialization are present before lingering starts
the user's service manager. The hook also works without a managed home or without
systemd when its commands do not need them.

Successful completion is recorded at `/var/lib/enclo/container-init` in the
container, with the instance ID and a digest of the command and account. Ordinary
stop/start skips it; recreation runs it again. Changes to the hook do not require
recreation and do not silently rerun a completed script. To apply them explicitly:

```sh
enclo host init dev --rerun
```

`host init dev` runs a pending hook on an existing running container, or
skips a completed one. Both forms ensure home initialization has completed first.
Other lifecycle configuration changes still require recreation. Initialization
is serialized with other lifecycle operations.

Failure or timeout stops the container, including its sessions, without recording
success; the next start retries. Partial effects remain, so scripts should be
safe to retry. An explicit rerun clears the previous checkpoint before execution.
Neither hook runs on arbitrary remote SSH hosts.

Add `--container-init` to `scripts/smoke-local-machine.py` to exercise ordering,
restart, explicit rerun and recreation. With `--systemd`, it also tests lingering
and verifies that the user's systemd manager is active.

## Nested Podman, GPUs and local desktop

These optional settings belong in `hosts/NAME.toml`:

```toml
[nested_podman]
enabled = true
# proc_mount = "host" # explicit compatibility option; see isolation tradeoff below

[gpu]
nvidia = ["all"] # CDI device selectors; e.g. ["0"] or a GPU UUID
# drm = true     # expose render nodes and retain supplementary host groups

[desktop]
wayland = true

[desktop.dbus]
enabled = true
# Explicitly allowed service names; empty lists grant no application services.
talk = ["org.freedesktop.portal.Desktop"]
# call = ["org.example.Service=org.example.Interface.Method@/org/example/Object"]
# broadcast = ["org.example.Service=org.example.Interface.Signal@/org/example/Object"]
# address = "unix:path=/path/to/a/private/bus" # defaults to host DBUS_SESSION_BUS_ADDRESS
```

All are disabled by default. Changing them requires `host recreate`, as
with mounts and image selection. They are local Podman features, not features of
arbitrary external SSH machines. GPU selectors are passed to Podman CDI; the host
must already have compatible devices/drivers and CDI configuration. Enclo does
not install drivers or generate host CDI files. DRM adds only `/dev/dri/renderD*` nodes and
`--group-add=keep-groups`; the OCI runtime must support preserving those groups.
Use `drm_nodes = ["renderD128"]` to select specific render nodes; otherwise they
are discovered under host `/dev/dri`. Primary display/card nodes are not exposed.
Podman reports missing devices or unknown CDI selectors during creation.

Nested Podman works with either Podman init/sshd or systemd. It requires a managed
home and image packages for Podman, UID/GID mapping helpers, `setcap`, Python,
FUSE overlay storage and a networking backend. Build the optional image layer:

```sh
podman build -f examples/ssh-machine/Containerfile.nested \
  --build-arg BASE=localhost/enclo-ssh-test:latest \
  -t localhost/enclo-nested-test:latest examples/ssh-machine
# Use the systemd image as BASE for the systemd variant.
```

The outer container exposes `/dev/fuse` and `/dev/net/tun`, disables seccomp and
masked-path restrictions, and retains the existing SELinux labeling-disabled
setting. This is an explicit isolation tradeoff for nested containers; it is not
host-engine socket forwarding or a privileged outer container. Setup derives
subordinate UID/GID ranges from the actual outer mappings, configures mapping
helpers, and verifies that the nested user namespace can create a mount/PID
namespace and mount proc before readiness (requires util-linux `unshare`, `mount`
and `umount` in the image).
It runs again after each start, before home initialization, and stops the machine
on failure. `XDG_RUNTIME_DIR=/run/user/UID` is supplied to enclo sessions; nested
storage lives in the managed home. Nested cgroups are disabled and the default
nested network is the outer machine's network. Nested containers can request
another supported network explicitly. Image/volume storage survives recreation;
running inner processes do not. No host Podman/Docker sockets are exposed.

`nested_podman.proc_mount` defaults to `"private"`. NVIDIA CDI may overlay
`/proc/driver/nvidia/params` to report `ModifyDeviceFiles: 0`, preventing NVIDIA
software from attempting device-file modifications. That overlay can cause Linux
to reject fresh proc mounts in nested user namespaces. Startup now detects this
failure and stops the machine rather than reporting nested Podman ready.
Enclo does not remove the NVIDIA hook or change the host CDI configuration.

For this combination, explicitly opt into compatibility mode:

```toml
[nested_podman]
enabled = true
proc_mount = "host"
```

This configures nested Podman to bind the **outer container's** `/proc` read-only
by default, preserving NVIDIA's replacement file. It does not mount the physical
host's `/proc`. Nested workloads can see outer-container process information
subject to normal access checks, and their proc view no longer matches their
private PID namespace. Read-only is not a substitute for process isolation.
Use this only for nested workloads trusted with that visibility. It is not an
automatic fallback, and individual Podman settings can override these defaults.
Changing the mode requires container recreation, which discards the writable
layer while preserving the managed home and mounted volumes.

The opt-in smoke test exercises this combination without host installation:

```sh
python3 scripts/smoke-local-machine.py --image localhost/enclo-nested-test:latest \
  --nested-podman --nvidia --nested-proc-mount host
```

It checks GPU discovery and actual nested process execution, including after
recreation. Omitting `--nvidia` and `--nested-proc-mount host` tests the private
proc default. These checks do not validate CUDA workloads or GPU use inside
the nested container.

Desktop sockets require file-descriptor passing and therefore use a local
read-only directory mount, not SSH byte-stream forwarding. Machine startup only
creates this mount. The services supervisor registers a Wayland security context
and/or starts `xdg-dbus-proxy --filter`; it releases them on stop, transport
failure or reload. Shell sessions remain alive but desktop clients can lose
connections. Restart creates new sockets at the same container paths; existing
applications may need restarting. A compositor must implement
`wp_security_context_manager_v1`; there is no raw-display fallback.

The host needs `xdg-dbus-proxy` for D-Bus support. `talk`, `call` and `broadcast`
are forwarded as separate filter arguments. Do not grant broad names unless the
workflow requires them. The source bus defaults to `DBUS_SESSION_BUS_ADDRESS`,
with an optional explicit Unix address for a private bus. This changes only the
proxy's source, not Podman's environment. One connection at a time may own a
machine's desktop services; a second owner fails without disturbing the first.
`services status` reports `desktop_running`; a proxy failure marks services
unhealthy and `services reload` retries them.

Persistent machine state now additionally contains:

```text
$ENCLO_STATE_DIR/containers/<config-root-hash>/NAME/desktop/
    services.lock       # exclusive ownership across connection/runtime roots
    dbus.log            # proxy diagnostics
    sockets/            # only this directory is mounted into the machine
        wayland         # /run/enclo-desktop/wayland
        bus             # /run/enclo-desktop/bus
```

Directories are private; the socket directory inode stays stable across services
restarts. Socket files are recreated, and no proxy process persists after the
supervisor exits. Use a short state root if Unix socket path limits are exceeded.
Resolved sessions receive `WAYLAND_DISPLAY` and/or `DBUS_SESSION_BUS_ADDRESS` for
these target paths; explicit connection session values take precedence. Neither
the raw host bus nor raw compositor socket is mounted.

Additional isolated validation:

```sh
cargo test -p enclo --offline -- --include-ignored
python3 scripts/smoke-local-machine.py --image localhost/enclo-nested-test:latest \
  --nested-podman --desktop-dbus --secrets
# Add --systemd with a nested systemd image.
```

The smoke test builds a scratch image and runs a nested process without pulling
inner images, checks nested storage after outer recreation, and uses a private
D-Bus daemon for proxy checks. Unit/integration fixtures check GPU arguments,
Wayland descriptor passing/revocation and refusal of unsupported compositors,
D-Bus allow/deny policy, exclusive ownership, and restart. These fixtures do not
validate real GPU workloads or rendering against a desktop compositor.

## Exporting SSH access for external clients

```sh
enclo host export dev                     # print Host enclo-dev
enclo host export dev --host enclo-work   # choose another alias
enclo host export dev --add               # add/update ~/.ssh/config
```

The argument names the container, not the connection profile (`work` selects
`dev` in the examples). The container must already be running. Export reads its
live Podman endpoint and uses persistent client identity and pinned host-key files;
it does not start services or change trust. Without `--add`, only a standard SSH
stanza is printed, suitable for redirection into an included config file.

`--add` writes a marked block to `$HOME/.ssh/config`, preserving unrelated content
and updating that machine/alias block on repeated calls. The specific stanza is
placed before existing defaults because OpenSSH uses the first matching values.
An explicit unmanaged alias or another machine's block is never overwritten:
choose another `--host`. Config symlinks are retained and their regular-file target
is updated atomically. The resulting config has mode 0600. Incomplete managed
markers are reported for repair rather than guessing which text to replace.

For a stable host endpoint, configure the container before exporting:

```toml
[ssh]
user = "tim"
host_port = 2222
```

`host_port` accepts integers 1–65535. Omitting it retains automatic allocation.
The bind address is always IPv4 loopback; an unavailable port fails without an
allocated-port fallback. Changing this setting requires recreation. Choose a
different fixed port for each concurrently running machine.

Exports use a stable, config-root-and-machine-specific `HostKeyAlias` and a
persistent `known_hosts` file. After successful SSH provisioning, enclo updates
that file from the new instance's Podman-verified host key. Internal connections
retain immutable per-instance pins; a changed key on the same instance is still
rejected. Strict host-key checking remains enabled.

With a fixed port and unchanged SSH user/state directory, the same exported entry
works after stop/start and recreation: no edit to `~/.ssh/config` is needed.
Automatically allocated ports can change on recreation, requiring another export.
Changing the SSH user, fixed port or state directory also requires re-exporting.
Entries generated by versions using per-instance export pins must be exported
once more with `--add` to adopt this stable layout.
The entry references persistent state, so clearing runtime files alone does not
invalidate it. Direct SSH clients do not use enclo session accounting, automatic
service startup, or session secret/proxy environment setup; start the container
and any required services separately. The installed enclo client discovers its
service settings itself, including from ordinary SSH sessions; this does not set
the environment of the surrounding shell. External application SSH compatibility
must be tested in that application.
