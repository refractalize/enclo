use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify, RwLock};
use zeroize::Zeroizing;

// Wraps a secret string with two guarantees:
//   1. Memory is mlock'd so the OS will not page it to swap.
//   2. Memory is zeroed before it is freed (via Zeroizing).
//
// mlock operates at page granularity (typically 4 KB). A failed mlock is
// logged as a warning but does not prevent the secret from being cached —
// zeroing still applies regardless.
struct SecretString(Zeroizing<String>);

impl SecretString {
    fn new(value: String, mlock: bool) -> Self {
        let z = Zeroizing::new(value);
        if mlock && !z.is_empty() {
            let rc = unsafe {
                libc::mlock(z.as_ptr() as *const libc::c_void, z.len())
            };
            if rc != 0 {
                eprintln!(
                    "insectjar-proxy: mlock failed for cached secret ({}); \
                     memory may be swappable. Consider raising RLIMIT_MEMLOCK.",
                    std::io::Error::last_os_error()
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
            // munlock before Zeroizing zeroes and frees the allocation.
            unsafe {
                libc::munlock(self.0.as_ptr() as *const libc::c_void, self.0.len());
            }
        }
        // Zeroizing zeroes the bytes when it drops after this.
    }
}

struct Entry {
    value: SecretString,
    inserted_at: Instant,
    last_accessed: Instant,
}

pub struct Cache {
    ttls: Arc<RwLock<(Duration, Duration)>>,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    mlock: bool,
}

impl Cache {
    pub fn new(idle_ttl: Duration, max_ttl: Duration, mlock: bool, shutdown: Arc<Notify>) -> Self {
        let ttls = Arc::new(RwLock::new((idle_ttl, max_ttl)));
        let entries: Arc<Mutex<HashMap<String, Entry>>> = Arc::new(Mutex::new(HashMap::new()));

        spawn_eviction(Arc::clone(&ttls), Arc::clone(&entries), shutdown);

        Self { ttls, entries, mlock }
    }

    pub async fn reload(&self, idle_ttl: Duration, max_ttl: Duration) {
        *self.ttls.write().await = (idle_ttl, max_ttl);
    }

    pub async fn get(&self, name: &str) -> Option<String> {
        let (idle_ttl, max_ttl) = *self.ttls.read().await;
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(name) {
            if now.duration_since(entry.last_accessed) < idle_ttl
                && now.duration_since(entry.inserted_at) < max_ttl
            {
                entry.last_accessed = now;
                return Some(entry.value.as_str().to_owned());
            }
            entries.remove(name); // Drop triggers munlock + zeroize.
        }
        None
    }

    pub async fn insert(&self, name: &str, value: String) {
        let now = Instant::now();
        self.entries.lock().await.insert(
            name.to_string(),
            Entry {
                value: SecretString::new(value, self.mlock),
                inserted_at: now,
                last_accessed: now,
            },
        );
    }
}

fn spawn_eviction(
    ttls: Arc<RwLock<(Duration, Duration)>>,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    shutdown: Arc<Notify>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                _ = shutdown.notified() => break,
            }

            let (idle_ttl, max_ttl) = *ttls.read().await;
            let now = Instant::now();
            // retain() drops evicted entries in place, triggering munlock + zeroize.
            entries.lock().await.retain(|_, entry| {
                now.duration_since(entry.last_accessed) < idle_ttl
                    && now.duration_since(entry.inserted_at) < max_ttl
            });
        }
    });
}
