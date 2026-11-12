use super::*;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};

impl LocalContainer {
    // Export trust follows the logical machine; internal connections retain
    // immutable per-instance pins. Only copy an already-provisioned trusted key.
    pub(super) fn refresh_export_trust(&self, endpoint: &Endpoint) -> Result<()> {
        let instance = self.state.join("instances").join(&endpoint.container_id);
        crate::connection_runtime::validate(&instance)?;
        let pinned = fs::read_to_string(instance.join("known_hosts"))?;
        let fields: Vec<_> = pinned.split_whitespace().collect();
        ensure!(
            fields.len() == 3
                && fields[0] == format!("enclo-{}", endpoint.container_id)
                && fields[1] == "ssh-ed25519",
            "invalid pinned host key for export"
        );
        atomic_write(
            self.state.join("known_hosts"),
            format!("{} {} {}\n", self.podman_name, fields[1], fields[2]),
        )
    }

    pub async fn export_ssh(&self, alias: Option<&str>, add: bool) -> Result<String> {
        let alias = alias
            .map(str::to_owned)
            .unwrap_or_else(|| format!("enclo-{}", self.name));
        ensure!(
            !alias.is_empty()
                && alias.len() <= 253
                && !alias.starts_with('-')
                && alias
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "SSH host alias must contain only letters, numbers, dots, underscores or hyphens"
        );
        let _lock = self.lock()?;
        let endpoint = self
            .status()
            .await?
            .context("container does not exist; start it first")?;
        ensure!(
            endpoint.running,
            "container is stopped; start it before exporting SSH settings"
        );
        self.refresh_export_trust(&endpoint)?;
        let stanza = self.ssh_stanza(&endpoint, &alias, true)?;
        if add {
            let home = std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .context("HOME is not set")?;
            let dir = PathBuf::from(home).join(".ssh");
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.into()),
            }
            let meta = fs::metadata(&dir)?;
            ensure!(
                meta.is_dir()
                    && meta.uid() == unsafe { libc::geteuid() }
                    && meta.mode() & 0o022 == 0,
                "SSH directory must be owned by you and not writable by others"
            );
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(dir.join("enclo-config.lock"))?;
            ensure!(
                unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
                "another SSH config export is in progress"
            );
            let config = dir.join("config");
            // Preserve a dotfile-manager symlink, updating its actual target.
            let target = match fs::symlink_metadata(&config) {
                Ok(m) if m.file_type().is_symlink() => config
                    .canonicalize()
                    .context("resolve SSH config symlink")?,
                Ok(_) => config.clone(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => config.clone(),
                Err(e) => return Err(e.into()),
            };
            let original = match fs::symlink_metadata(&target) {
                Ok(m) => {
                    ensure!(
                        m.is_file()
                            && m.uid() == unsafe { libc::geteuid() }
                            && m.mode() & 0o022 == 0,
                        "SSH config must be a regular file owned by you and not writable by others"
                    );
                    Some(fs::read_to_string(&target)?)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            let owner = format!("{}/{}", self.scope, self.name);
            let updated =
                update_config(original.as_deref().unwrap_or(""), &owner, &alias, &stanza)?;
            ensure!(
                fs::read_to_string(&target).ok() == original,
                "SSH config changed during export; retry"
            );
            atomic_write(&target, updated)?;
            eprintln!("Updated {}: Host {alias}", config.display());
        }
        Ok(stanza)
    }
}

fn update_config(original: &str, owner: &str, alias: &str, stanza: &str) -> Result<String> {
    // One block per machine and alias. Never replace a block owned by another
    // config root or an unmanaged explicit Host entry.
    let begin = format!("# BEGIN enclo {owner} {}", alias.to_ascii_lowercase());
    let end = format!("# END enclo {owner} {}", alias.to_ascii_lowercase());
    let mut rest = String::new();
    let mut inside = false;
    let mut found = false;
    for line in original.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == begin {
            ensure!(!inside && !found, "duplicate enclo SSH config markers");
            inside = true;
            found = true;
        } else if trimmed == end {
            ensure!(inside, "unmatched enclo SSH config marker");
            inside = false;
        } else if !inside {
            rest.push_str(line);
        }
    }
    ensure!(
        !inside,
        "incomplete enclo SSH config block; repair it before exporting"
    );
    for line in rest.lines() {
        let normalized = line.replacen('=', " ", 1);
        let mut words = normalized.split_whitespace();
        if words.next().is_some_and(|w| w.eq_ignore_ascii_case("host")) {
            ensure!(!words.take_while(|w| !w.starts_with('#')).any(|w| w.trim_matches('"').eq_ignore_ascii_case(alias)),
                "SSH alias {alias} already exists outside this container's managed block; choose another --host");
        }
    }
    // Specific options must precede Host * defaults (OpenSSH takes first values).
    // Reset scope so the original file's global options retain their meaning.
    Ok(format!("{begin}\n{stanza}Host *\n{end}\n{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_export_is_idempotent_and_preserves_unrelated_text() {
        let original =
            "# user config\nHost *\n  Port 2222\nHost server\n  HostName server.example\n";
        let first = update_config(
            original,
            "scope/dev",
            "enclo-dev",
            "Host enclo-dev\n Port 42\n",
        )
        .unwrap();
        assert!(first.ends_with(original));
        assert_eq!(
            first,
            update_config(
                &first,
                "scope/dev",
                "enclo-dev",
                "Host enclo-dev\n Port 42\n"
            )
            .unwrap()
        );
        let changed = update_config(
            &first,
            "scope/dev",
            "enclo-dev",
            "Host enclo-dev\n Port 43\n",
        )
        .unwrap();
        assert!(!changed.contains("Port 42"));
        assert!(changed.ends_with(original));
        assert!(update_config(&first, "other/dev", "enclo-dev", "").is_err());
        assert!(update_config("Host ENClO-dev\n", "scope/dev", "enclo-dev", "").is_err());
        assert!(update_config(
            "# BEGIN enclo scope/dev enclo-dev\n",
            "scope/dev",
            "enclo-dev",
            ""
        )
        .is_err());
    }
}
