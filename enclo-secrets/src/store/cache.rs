use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify};
use zeroize::Zeroizing;

struct SecretString(Zeroizing<String>);

impl SecretString {
    fn new(value: String, mlock: bool) -> Self {
        let z = Zeroizing::new(value);
        if mlock && !z.is_empty() {
            let rc = unsafe { libc::mlock(z.as_ptr() as *const libc::c_void, z.len()) };
            if rc != 0 {
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    "mlock failed for cached secret; memory may be swappable. \
                     Consider raising RLIMIT_MEMLOCK."
                );
            }
        }
        Self(z)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            unsafe {
                libc::munlock(self.0.as_ptr() as *const libc::c_void, self.0.len());
            }
        }
    }
}

/// Enough headroom for requests in flight and small clock differences.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

pub struct CachedSecret {
    pub value: String,
    pub expires_at: Option<time::OffsetDateTime>,
}

struct Entry {
    value: SecretString,
    inserted_at: Instant,
    last_accessed: Instant,
    expires_at: Option<time::OffsetDateTime>,
    expiry_deadline: Option<Instant>,
}

pub struct Cache {
    ttls: Arc<(Duration, Duration)>,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    mlock: bool,
}

impl Cache {
    pub fn new(idle_ttl: Duration, max_ttl: Duration, mlock: bool, shutdown: Arc<Notify>) -> Self {
        let ttls = Arc::new((idle_ttl, max_ttl));
        let entries: Arc<Mutex<HashMap<String, Entry>>> = Arc::new(Mutex::new(HashMap::new()));

        spawn_eviction(Arc::downgrade(&ttls), Arc::downgrade(&entries), shutdown);

        Self {
            ttls,
            entries,
            mlock,
        }
    }

    pub async fn get(&self, name: &str) -> Option<CachedSecret> {
        let (idle_ttl, max_ttl) = *self.ttls;
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(name) {
            if now.duration_since(entry.last_accessed) < idle_ttl
                && now.duration_since(entry.inserted_at) < max_ttl
                && entry.before_expiry(now, time::OffsetDateTime::now_utc())
            {
                entry.last_accessed = now;
                return Some(CachedSecret {
                    value: entry.value.as_str().to_owned(),
                    expires_at: entry.expires_at,
                });
            }
            entries.remove(name);
        }
        None
    }

    pub async fn insert(
        &self,
        name: &str,
        value: String,
        expires_at: Option<time::OffsetDateTime>,
    ) {
        let now = Instant::now();
        let expiry_deadline = match expires_at {
            Some(expiry) => {
                let remaining = Duration::try_from(expiry - time::OffsetDateTime::now_utc())
                    .ok()
                    .and_then(|remaining| remaining.checked_sub(EXPIRY_MARGIN));
                match remaining.and_then(|remaining| now.checked_add(remaining)) {
                    Some(deadline) if deadline > now => Some(deadline),
                    _ => {
                        // Never retain an old entry when a replacement is already near expiry.
                        self.entries.lock().await.remove(name);
                        return;
                    }
                }
            }
            None => None,
        };
        self.entries.lock().await.insert(
            name.to_string(),
            Entry {
                value: SecretString::new(value, self.mlock),
                inserted_at: now,
                last_accessed: now,
                expires_at,
                expiry_deadline,
            },
        );
    }
}

fn spawn_eviction(
    ttls: Weak<(Duration, Duration)>,
    entries: Weak<Mutex<HashMap<String, Entry>>>,
    shutdown: Arc<Notify>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                _ = shutdown.notified() => break,
            }

            let (Some(ttls), Some(entries)) = (ttls.upgrade(), entries.upgrade()) else {
                break;
            };

            let (idle_ttl, max_ttl) = *ttls;
            let now = Instant::now();
            entries.lock().await.retain(|_, entry| {
                now.duration_since(entry.last_accessed) < idle_ttl
                    && now.duration_since(entry.inserted_at) < max_ttl
                    && entry.before_expiry(now, time::OffsetDateTime::now_utc())
            });
        }
    });
}

impl Entry {
    fn before_expiry(&self, now: Instant, wall_now: time::OffsetDateTime) -> bool {
        self.expiry_deadline.is_none_or(|deadline| now < deadline)
            && self.expires_at.is_none_or(|expiry| {
                expiry - wall_now > time::Duration::seconds(EXPIRY_MARGIN.as_secs() as i64)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cache() -> Cache {
        Cache::new(
            Duration::from_secs(3600),
            Duration::from_secs(7200),
            false,
            Arc::new(Notify::new()),
        )
    }
    #[tokio::test]
    async fn expiry_metadata_survives_cache_hits() {
        let cache = cache();
        let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        cache.insert("key", "dummy".into(), Some(expiry)).await;
        let value = cache.get("key").await.unwrap();
        assert_eq!(value.value, "dummy");
        assert_eq!(value.expires_at, Some(expiry));
    }
    #[tokio::test]
    async fn expired_or_near_expiry_replacement_is_not_cached() {
        for delta in [-1, 0, 30, 60] {
            let cache = cache();
            cache.insert("key", "old".into(), None).await;
            cache
                .insert(
                    "key",
                    "new".into(),
                    Some(time::OffsetDateTime::now_utc() + time::Duration::seconds(delta)),
                )
                .await;
            assert!(cache.get("key").await.is_none());
            assert!(cache.entries.lock().await.is_empty());
        }
    }
    #[tokio::test]
    async fn reads_do_not_extend_secret_expiry_and_expired_entries_are_removed() {
        let cache = cache();
        let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        cache.insert("key", "dummy".into(), Some(expiry)).await;
        let deadline = cache.entries.lock().await["key"].expiry_deadline;
        assert!(cache.get("key").await.is_some());
        assert_eq!(cache.entries.lock().await["key"].expiry_deadline, deadline);
        cache
            .entries
            .lock()
            .await
            .get_mut("key")
            .unwrap()
            .expiry_deadline = Some(Instant::now() - Duration::from_secs(1));
        assert!(cache.get("key").await.is_none());
        assert!(cache.entries.lock().await.is_empty());
    }
    #[tokio::test]
    async fn configured_idle_and_maximum_ttls_still_apply_with_and_without_metadata() {
        for metadata in [
            None,
            Some(time::OffsetDateTime::now_utc() + time::Duration::days(1)),
        ] {
            for idle in [true, false] {
                let cache = cache();
                cache.insert("key", "dummy".into(), metadata).await;
                {
                    let mut entries = cache.entries.lock().await;
                    let entry = entries.get_mut("key").unwrap();
                    if idle {
                        entry.last_accessed = Instant::now() - Duration::from_secs(3601);
                    } else {
                        entry.inserted_at = Instant::now() - Duration::from_secs(7201);
                    }
                }
                assert!(cache.get("key").await.is_none());
            }
        }
    }
    #[tokio::test]
    async fn wall_clock_jump_forward_also_expires_a_secret() {
        let cache = cache();
        let expiry = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
        cache.insert("key", "dummy".into(), Some(expiry)).await;
        let entries = cache.entries.lock().await;
        let entry = &entries["key"];
        assert!(!entry.before_expiry(Instant::now(), expiry));
        assert!(entry.before_expiry(Instant::now(), expiry - time::Duration::seconds(61)));
    }
}
