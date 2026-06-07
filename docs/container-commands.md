# In-Container CLI Reference (`enclo`)

Inside the container, the `enclo` binary is the `enclo-container-client`. It provides commands for fetching secrets and running commands with secrets or proxy settings injected as environment variables.

The binary is installed at `/usr/local/bin/enclo` by the `container_client` module (enabled by default).

---

## `enclo get`

Fetch one or more secrets and print them to stdout.

```
enclo get [OPTIONS] [NAMES...]
```

At least one name or `--group` is required.

### Options

| Flag | Default | Description |
|---|---|---|
| `-s, --socket <PATH>` | `/run/enclo/secrets.sock` | Path to the `enclo-secrets` Unix socket |
| `-c, --config <PATH>` | `$XDG_CONFIG_HOME/enclo/client.toml` | Client config file (for group definitions) |
| `-g, --group <GROUP>` | — | Fetch all secrets in the named group. May be repeated or comma-separated. |
| `--env` | `false` | Print secrets as shell `export` statements instead of JSON |

### Output formats

**Default (JSON):**

```bash
$ enclo get GITHUB_TOKEN NPM_TOKEN
{"GITHUB_TOKEN":"ghp_abc123","NPM_TOKEN":"npm_xyz789"}
```

**Shell exports (`--env`):**

```bash
$ enclo get --env GITHUB_TOKEN
export GITHUB_TOKEN='ghp_abc123'
```

Suitable for use with `eval`:

```bash
eval "$(enclo get --env GITHUB_TOKEN NPM_TOKEN)"
```

### Examples

```bash
# Fetch a single secret as JSON
enclo get GITHUB_TOKEN

# Fetch multiple secrets
enclo get GITHUB_TOKEN NPM_TOKEN

# Fetch as shell exports
eval "$(enclo get --env GITHUB_TOKEN NPM_TOKEN)"

# Fetch a group defined in client.toml
enclo get --group dev_tools

# Combine a group with an explicit name
enclo get --group dev_tools EXTRA_TOKEN

# Fetch with AWS profile parameter
enclo get "AWS_CREDENTIALS?profile=prod"
```

---

## `enclo run`

Fetch secrets and execute a command with them set as environment variables.

```
enclo run [OPTIONS] [NAMES...] -- CMD [ARGS...]
```

The `--` separator is required. The command inherits the current environment with the fetched secrets added.

### Options

| Flag | Default | Description |
|---|---|---|
| `-s, --socket <PATH>` | `/run/enclo/secrets.sock` | Path to the `enclo-secrets` Unix socket |
| `-c, --config <PATH>` | `$XDG_CONFIG_HOME/enclo/client.toml` | Client config file |
| `-g, --group <GROUP>` | — | Fetch all secrets in the named group |

### Examples

```bash
# Run npm with a GitHub token
enclo run GITHUB_TOKEN -- npm publish

# Run with multiple secrets
enclo run GITHUB_TOKEN NPM_TOKEN -- npm publish

# Use a group
enclo run --group dev_tools -- make deploy

# Use an AWS profile secret
enclo run "AWS_CREDENTIALS?profile=prod" -- terraform apply
```

---

## `enclo proxy`

Run a command with `$HTTP_PROXY` and `$HTTPS_PROXY` set from `$ENCLO_PROXY`, or print the proxy env vars.

```
enclo proxy [--env]
enclo proxy -- CMD [ARGS...]
```

`$ENCLO_PROXY` is set by the `secrets` module when `mode = "explicit"` (the default). It contains the full proxy URL including the ephemeral basic-auth credentials generated at container start.

### Options

| Flag | Description |
|---|---|
| `--env` | Print `HTTP_PROXY` and `HTTPS_PROXY` as shell `export` statements. Cannot be combined with a command. |

### Examples

```bash
# Run curl through the injecting proxy
enclo proxy -- curl https://api.github.com/user

# Run an arbitrary command with proxy env vars
enclo proxy -- git clone https://github.com/example/repo

# Print proxy settings for manual use
enclo proxy --env

# Set proxy for the current shell session
eval "$(enclo proxy --env)"

# Make a tool use the proxy without running it via enclo
HTTP_PROXY="$ENCLO_PROXY" HTTPS_PROXY="$ENCLO_PROXY" some-tool
```

---

## Client config (`client.toml`)

Groups of secrets can be defined in the client config file at `$XDG_CONFIG_HOME/enclo/client.toml` (default: `~/.config/enclo/client.toml` inside the container).

```toml
[groups]
dev_tools = ["GITHUB_TOKEN", "NPM_TOKEN"]
aws_prod  = ["AWS_CREDENTIALS?profile=prod"]
```

Reference groups with `-g` / `--group`:

```bash
enclo get --group dev_tools
enclo run --group dev_tools,aws_prod -- make deploy
```

Deduplication is applied: if the same secret name appears in both a positional argument and a group, it is fetched only once.

---

## See also

- [Secrets configuration](secrets-configuration.md)
- [Container configuration — secrets module](configuration.md#modulessecrets)
- `man enclo-container`
