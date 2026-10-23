# Secrets Service Configuration Reference

`enclo-secrets` is configured via a single TOML file referenced from the container config as `modules.secrets.config`. The file has three top-level sections: `[store]`, `[secrets]`, and `[proxy]`.

---

## Overview

```toml
[store]
# Secret backend stack: route → route cache → confirm → backend

[secrets]
# Unix socket API: access control

[proxy]
# HTTPS MitM proxy: auth injection rules
```

At least one of `secrets.socket` or `proxy` must be enabled.

---

## `[store]`

Configures the layered secret fetching pipeline. Requests first resolve a route, then flow through that route's cache → global confirmation → backend.

### `[store.cache]`

Defines the default cache settings inherited by all routes. Optional; routes can also define both TTLs directly to enable caching without defaults.

```toml
[store.cache]
idle_ttl = "10m"
max_ttl  = "1h"
mlock    = false
```

| Field | Type | Description |
|---|---|---|
| `idle_ttl` | duration string | Time-to-live reset on each access. A secret not accessed for this duration is evicted. Examples: `"5m"`, `"1h"`, `"30s"`. |
| `max_ttl` | duration string | Hard ceiling. The secret is evicted after this duration regardless of access. |
| `mlock` | bool | When `true`, calls `mlock()` on the cache memory pages to prevent them from being swapped to disk. Requires sufficient `RLIMIT_MEMLOCK`. Default: `false`. |

Each route gets an independent cache, even when multiple routes inherit identical settings. Route-level fields override these defaults individually.

### `[store.confirm]`

Asks for user confirmation before releasing a secret. Optional; omit to allow all requests without prompting.

On Linux, uses `pinentry` (must be installed and configured). On macOS, shows a native dialog via `enclo-secret-approve-macos`.

```toml
[store.confirm]
enabled     = true
debounce_ms = 50
```

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | — | Whether confirmation is required |
| `debounce_ms` | integer | `50` | Requests arriving within this many milliseconds are batched into a single prompt |

After a dialog opens, another request for a secret already displayed in that dialog shares its result. Requests for different secrets never inherit that approval: they accumulate in a new debounce batch and are shown in a separate dialog after the active one closes.

### `[[store.routes]]`

Routes secret names to backend processes using wildcard patterns. Routes are evaluated in order; the first match wins.

```toml
[[store.routes]]
patterns = ["GITHUB_*", "NPM_TOKEN"]
backend  = "pass"
idle_ttl = "30m"
max_ttl  = "8h"

[[store.routes]]
patterns = ["AWS_CREDENTIALS"]
backend  = "aws-vault"
cache    = false
```

| Field | Type | Description |
|---|---|---|
| `patterns` | array of strings | Wildcard patterns matching secret names and, when present in the pattern, query parameters. `*` matches any sequence of characters. |
| `backend` | string | Name of the backend to use. Must match a `[[store.backends]]` entry. |
| `cache` | bool | Set to `false` to disable caching for this route even when `[store.cache]` exists. Set to `true` to explicitly enable it. |
| `idle_ttl` | duration string | Overrides the default idle TTL for this route. |
| `max_ttl` | duration string | Overrides the default maximum TTL for this route. |
| `mlock` | bool | Overrides the default memory-locking setting for this route. |

If caching is enabled, the effective configuration must contain both TTLs. A route may inherit either field from `[store.cache]`. Without global defaults, it must define both itself. Cache hits bypass confirmation; set `cache = false` for secrets that must always be confirmed and fetched.

Route patterns without query parameters match every parameter variant of that secret. Add query constraints to select specific parameter values:

```toml
[[store.routes]]
patterns = ["AWS_CREDENTIALS?profile=production-*"]
backend  = "aws-vault"

[[store.routes]]
patterns = ["AWS_CREDENTIALS"] # fallback for every other profile
backend  = "aws-vault"
```

Parameter names match exactly and values support wildcard patterns. All parameters in a pattern must match, while their order and any extra request parameters do not matter. Names and values are URL-decoded before matching. Routes remain first-match-wins, so parameter-specific routes must appear before broader fallbacks.

### `[[store.backends]]`

Declares backend processes. A backend is an external binary named `enclo-secret-store-<name>` looked up on `$PATH`. Each backend communicates over stdin/stdout using the length-prefixed JSON protocol.

```toml
[[store.backends]]
name = "pass"

[[store.backends]]
name = "aws-vault"

[store.backends.env]
AWS_VAULT_BACKEND = "pass"
```

| Field | Type | Description |
|---|---|---|
| `name` | string | Backend identifier. The binary `enclo-secret-store-<name>` is run on demand. |
| `args` | array of strings | Additional command-line arguments passed to the backend. |
| `env` | table | Extra environment variables set when the backend process is spawned. |

Backends are started lazily and restarted automatically (up to 10 times per 60-second window) if they crash.

**Built-in backends:**

| Name | Binary | Secret format |
|---|---|---|
| `pass` | `enclo-secret-store-pass` | Secret name maps to a `pass` entry path |
| `aws-vault` | `enclo-secret-store-aws-vault` | `AWS_CREDENTIALS?profile=<profile>` |
| `github` | `enclo-secret-store-github` | `GITHUB_TOKEN` or `GH_TOKEN` from `gh auth token` |
| `keychain` | `enclo-secret-store-keychain` | macOS Keychain (service name = secret name) |

**Query parameters:** Secret names may include `?key=value` parameters (e.g. `AWS_CREDENTIALS?profile=prod`). The complete original name is forwarded to the backend. Parameter-less route patterns ignore the query string; route patterns containing query constraints match decoded parameter values as described above.

---

## `[secrets]`

Configures the Unix socket API. The socket path is managed by the enclo orchestrator and overridden via `--socket` when `enclo-secrets` is started; the `path` field here is rarely needed.

### `[secrets.socket]`

```toml
[secrets.socket]
enabled = true   # default
path    = ""     # optional override
```

| Field | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `true` | Enable the Unix socket API |
| `path` | string | *(managed by enclo)* | Override the socket path. Leave empty when using the enclo module. |

### `[secrets.access]`

Controls which secrets can be fetched via the socket API. Patterns are wildcard-matched against the secret name (query parameters are stripped).

```toml
[secrets.access]
mode  = "deny-all"
allow = ["GITHUB_*", "NPM_TOKEN"]
```

```toml
[secrets.access]
mode  = "allow-all"
deny  = ["INTERNAL_*"]
```

| Field | Type | Default | Description |
|---|---|---|---|
| `mode` | `"deny-all"` \| `"allow-all"` | `"deny-all"` | Base policy |
| `allow` | array of strings | `[]` | Patterns explicitly allowed. Only used in `deny-all` mode. |
| `deny` | array of strings | `[]` | Patterns explicitly denied. Only used in `allow-all` mode. |

In `"deny-all"` mode, a secret must match at least one `allow` pattern and must not match any `deny` pattern.

In `"allow-all"` mode, a secret is permitted unless it matches a `deny` pattern. (The `allow` list has no effect in this mode.)

---

## `[proxy]`

An HTTP/HTTPS proxy that MitM-intercepts CONNECT tunnels, terminates TLS, and injects authorization headers into outbound requests. Optional; only needed if you want transparent auth injection.

### Basic configuration

```toml
[proxy]
enabled = true
listen  = "127.0.0.1:8080"

[proxy.tls]
ca_cert = "certs/ca.crt"
ca_key  = "certs/ca.key"
```

| Field | Type | Description |
|---|---|---|
| `enabled` | bool | Enable the proxy. Requires `listen` and `tls`. |
| `listen` | socket address | Address and port the proxy listens on. Example: `"127.0.0.1:8080"`. The port must be fixed (not `0`). `enclo-secrets serve --listen IP:PORT` overrides it. Ignored, and not required, when enclo starts the proxy: enclo passes a pre-bound socket on a free port via `--listen-fd`. |

### `[proxy.tls]`

The proxy generates a per-connection leaf certificate signed by this CA to terminate TLS. The CA certificate is automatically mounted into the container by the `secrets` module so that the container trusts it.

Use `enclo-secrets gen-ca` (or `examples/gen-dev-ca.sh`) to generate a local CA.

```toml
[proxy.tls]
ca_cert = "certs/ca.crt"
ca_key  = "certs/ca.key"
```

| Field | Type | Description |
|---|---|---|
| `ca_cert` | path | CA certificate (PEM). Relative paths are resolved from the config file's directory. |
| `ca_key` | path | CA private key (PEM). |

### `[proxy.auth]`

Optional basic authentication on the proxy itself. When set, clients must present these credentials in the `Proxy-Authorization` header. The `secrets` module generates an ephemeral password and passes it to `enclo-secrets` automatically, so this section is only needed for manual `enclo-secrets` invocations.

```toml
[proxy.auth]
username      = "enclo"
password_hash = "$argon2id$..."
```

| Field | Type | Description |
|---|---|---|
| `username` | string | Proxy username |
| `password_hash` | string | Argon2id hash of the password. Generate with: `enclo-secrets generate-basic-auth <username>` |

### `[[proxy.rules]]`

Rules controlling which authorization header is injected for outbound requests. Rules are matched by domain using wildcard patterns; the first match wins.

```toml
[[proxy.rules]]
domain = "api.github.com"
auth   = { type = "bearer", secret = "GITHUB_TOKEN" }

[[proxy.rules]]
domain = "github.com"
auth   = { type = "basic", username = "git", secret = "GITHUB_TOKEN" }

[[proxy.rules]]
domain = "*.example.com"
auth   = { type = "header", name = "X-Api-Key", secret = "API_KEY" }
```

| Field | Type | Description |
|---|---|---|
| `domain` | string | Wildcard domain pattern. `*` matches any sequence excluding `.`. `*.example.com` matches `foo.example.com` but not `foo.bar.example.com`. |
| `auth` | auth config | The authorization to inject (see below). |

**Auth types:**

`bearer` — injects `Authorization: Bearer <secret_value>`:
```toml
auth = { type = "bearer", secret = "GITHUB_TOKEN" }
```

`basic` — injects `Authorization: Basic base64(<username>:<secret_value>)`:
```toml
auth = { type = "basic", username = "git", secret = "GITHUB_TOKEN" }
```

`header` — injects an arbitrary header:
```toml
auth = { type = "header", name = "X-Api-Key", secret = "API_KEY" }
```

The proxy reloads its rules on `SIGHUP` (triggered by `enclo reload <name>`).

---

## In-container client config

The `enclo` in-container binary can be configured with named secret groups. The config lives at `$XDG_CONFIG_HOME/enclo/client.toml` (default: `~/.config/enclo/client.toml`) inside the container.

```toml
[groups]
dev_tools = ["GITHUB_TOKEN", "NPM_TOKEN"]
aws_prod  = ["AWS_CREDENTIALS?profile=prod"]
```

Groups are referenced with `--group` / `-g`:

```bash
enclo get --group dev_tools
enclo run --group dev_tools -- npm publish
enclo get --group aws_prod,dev_tools   # comma-separated
```

Explicit names are deduplicated against group members: if a name appears in both a positional argument and a group, it is fetched only once.

---

## Complete example

```toml
# secrets-config.toml

[store.cache]
idle_ttl = "10m" # defaults for routes without overrides
max_ttl  = "1h"

[store.confirm]
enabled     = true
debounce_ms = 50

[[store.routes]]
patterns = ["GITHUB_*", "NPM_*"]
backend  = "pass"

[[store.routes]]
patterns = ["AWS_CREDENTIALS"]
backend  = "aws-vault"

[[store.backends]]
name = "pass"

[[store.backends]]
name = "aws-vault"

[store.backends.env]
AWS_VAULT_BACKEND = "pass"

[secrets.access]
mode  = "deny-all"
allow = ["GITHUB_*", "NPM_TOKEN", "AWS_CREDENTIALS"]

[proxy]
enabled = true
listen  = "127.0.0.1:8080"

[proxy.tls]
ca_cert = "certs/ca.crt"
ca_key  = "certs/ca.key"

[[proxy.rules]]
domain = "api.github.com"
auth   = { type = "bearer", secret = "GITHUB_TOKEN" }

[[proxy.rules]]
domain = "github.com"
auth   = { type = "basic", username = "git", secret = "GITHUB_TOKEN" }
```
