use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Session-scoped password cache. Keyed by purpose (`"sudo"`, `"ssh:<host>"`).
/// Entries expire after `ttl`; expired entries are evicted on read. The whole
/// map is zeroized on `clear` and on drop (Zeroizing values).
pub struct PasswordCache {
    ttl: Duration,
    inner: Mutex<HashMap<String, (Zeroizing<String>, Instant)>>,
}

impl PasswordCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, key: &str, now: Instant) -> Option<Zeroizing<String>> {
        let mut m = self.inner.lock().unwrap();
        match m.get(key) {
            Some((pw, at)) if now.duration_since(*at) <= self.ttl => Some(pw.clone()),
            Some(_) => {
                m.remove(key); // expired → evict (drops Zeroizing → memory wiped)
                None
            }
            None => None,
        }
    }

    pub fn put(&self, key: &str, pw: Zeroizing<String>, now: Instant) {
        self.inner
            .lock()
            .unwrap()
            .insert(key.to_string(), (pw, now));
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use zeroize::Zeroizing;

    #[test]
    fn hit_within_ttl_then_expires() {
        let c = PasswordCache::new(Duration::from_secs(300));
        let t0 = Instant::now();
        c.put("sudo", Zeroizing::new("pw".to_string()), t0);
        assert_eq!(
            c.get("sudo", t0 + Duration::from_secs(10))
                .as_deref()
                .map(|s| s.as_str()),
            Some("pw")
        );
        // Past TTL → miss (and entry dropped).
        assert!(c.get("sudo", t0 + Duration::from_secs(301)).is_none());
        assert!(
            c.get("sudo", t0 + Duration::from_secs(10)).is_none(),
            "expired entry must be evicted"
        );
    }

    #[test]
    fn clear_zeroizes_all() {
        let c = PasswordCache::new(Duration::from_secs(300));
        let t0 = Instant::now();
        c.put("ssh:host", Zeroizing::new("p".to_string()), t0);
        c.clear();
        assert!(c.get("ssh:host", t0).is_none());
    }
}
