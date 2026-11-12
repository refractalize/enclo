# Agent guide: migrate a legacy enclo profile

Use this guide to manually translate a user's small set of profiles. There is no
migration command or automatic config converter. Read `ssh-environments.md` for
the implemented backend, and use the checkout binary's `--help` for commands.

## Scope and prerequisites

Inspect the requested legacy profile and image recipe, including inherited
workspace configuration. Record enabled modules, mounts, identities, device
requirements, hooks, secret policy, and expected shell environment. Do not print
credential values, private keys, or provider output. Never broaden secret or
D-Bus permissions to make a test pass. Repository-local configuration must not
become authority to grant new secrets or host-service access.

Prepare the translation in a separate directory. Do not overwrite existing
profiles, stop installed services, run installation scripts, remove old volumes,
or replace existing containers as part of drafting the migration. Preserve the
legacy setup for comparison and rollback. A user can explicitly authorize a
later cutover; this guide does not authorize one.

## Translate configuration by lifetime

Create `containers/NAME.toml` for local machine configuration and
`connections/NAME.toml` for connection configuration. These names need not match.
Use a separate secrets policy file referenced by the connection profile. Paths
in `services.secrets.config` and `client_binary` are relative to that connection
file. Container mount strings use literal Podman syntax: resolve legacy `%h`,
`%r`, `%u`, tilde, and relative paths explicitly. Do not evaluate shell text from
configuration to expand paths.

| Legacy setting | SSH backend destination |
| --- | --- |
| Container image, volumes, env, devices, keep_groups | Same keys in `[container]` |
| Workspace user | Existing image account in `[ssh] user`; UID must match host |
| `[modules.systemd]` | `[systemd] enabled=true`, with explicit `mask` list |
| `[container.home]` and central init | `[home]` and `[home.init]` |
| `[modules.nested_podman] enabled=true` | `[nested_podman] enabled=true` plus managed home |
| `[modules.nvidia] enabled=true` | `[gpu] nvidia=["all"]`, or specific CDI selectors |
| `[modules.drm] enabled=true` | `[gpu] drm=true` (adds supplementary-group preservation) |
| `[modules.wayland]` | `[desktop] wayland=true`; fixed target socket path |
| D-Bus `allowed_interfaces`, `call_rules`, `broadcast_rules` | `[desktop.dbus]` `talk`, `call`, `broadcast`; preserve rules exactly |
| Secrets and HTTP proxy | Connection `[services.secrets]`, retaining the policy file |
| Shell working directory and environment | Connection `[session] workdir` and `[session.env]` |
| Development-server tunnels | Connection `[[forwards]]` with explicit direction/spec |
| Existing remote machine | `[target] ssh="ssh-config-alias"`, optional `config`; no container profile |

Do not copy an old profile wholesale: the new schema rejects legacy `[modules]`
and unsupported container fields. Image-building tools and packages belong in
the image recipe. Start/stop, resources, networking, workspaces, agent forwarding,
custom hooks and socket modules are not all direct one-to-one translations.
Identify unsupported behavior explicitly; do not silently drop it or pass
unrestricted Podman arguments. GPG/SSH agent forwarding and host Podman/Docker
socket exposure must be reviewed separately. The new SSH driver currently
forces `ForwardAgent=no`; setting `ForwardAgent` in SSH config does not override
that. Do not promise automatic agent forwarding.

A new managed home is initially empty and selects a volume by config-root scope
and machine name. It does not adopt or copy a legacy home volume. Report the old
and new volume names. If preserving data is requested, plan a deliberate copy
while writers are stopped; preserve ownership and inspect old `.enclo-init`
checkpoints before deciding whether to retain or rerun initialization. Do not
mount host credential directories to populate a new home. Never delete the old
volume during validation.

Split hooks according to their needs. `[home.init] command` runs as the SSH user
once successfully, before connection services exist. It is appropriate for home
setup that does not need secrets. It is not a substitute for arbitrary privileged
post-start hooks or workspace-specific hooks. Put root setup into the image when
possible. Run development setup needing credentials explicitly through
`session ... -- command args` after services are ready. Document any remaining
manual step rather than introducing a new automatic hook or privilege grant.

Nested Podman is a local engine inside the machine, not a forwarded host engine.
It requires image tools from `examples/ssh-machine/Containerfile.nested` and a
managed home. Both init/sshd and systemd modes are supported. It relaxes outer
seccomp/masked-path restrictions and exposes FUSE/TUN devices; preserve this
choice only when the original workflow needs it.

Wayland and D-Bus are local-only descriptor-passing integrations. Container config
owns permissions and the socket mount; `services run` owns the security context
and proxy processes. Do not mount the raw compositor socket or session bus as a
fallback. The compositor must support `wp_security_context_manager_v1`. Only one
connection may own desktop services for a given machine at a time.

## Validate without installing

Use fresh, short config/runtime/state roots so no installed instance is selected:

```sh
export ENCLO_CONFIG_DIR=/tmp/enclo-migration/config
export ENCLO_RUNTIME_DIR=/tmp/enclo-migration/runtime
export ENCLO_STATE_DIR=/tmp/enclo-migration/state
# Populate config/containers/dev.toml and config/connections/work.toml first.
cargo build -p enclo -p enclo-secrets -p enclo-container-client --offline
target/debug/enclo container start dev
target/debug/enclo services run work       # keep this foreground process running
# In another terminal with the same directory overrides:
target/debug/enclo session work -- id
target/debug/enclo container list
target/debug/enclo services list
target/debug/enclo sessions list
```

First validate with synthetic credentials and a private upstream fixture; never
request real secrets just to check plumbing. `scripts/smoke-local-machine.py`
automates isolated fixtures, including `--secrets`, `--managed-home`,
`--nested-podman`, `--desktop-dbus`, and optional `--systemd`. Its D-Bus tests
launch a private bus, not the desktop bus. Hardware and real compositor tests are
separate optional validation and should be reported as untested if unavailable.

Check that a shell survives `services stop work`, a new session starts services again,
and services can restart. Stop services and sessions before testing container
stop/start or recreation. `container recreate` discards the writable layer;
managed home data must survive. Use `container home-init dev --rerun` only when
intentionally reapplying the init script. A failed or timed-out init stops the
container and keeps partial changes for a retry.

Deliver the translated files, a behavior mapping, test results, unsupported
features/manual steps, and exact cutover/rollback commands. Clearly distinguish
checkout testing from installation. Leave the user's existing installation and
profiles intact unless they explicitly requested that final cutover.
