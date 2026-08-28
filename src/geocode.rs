// Best-effort reverse geocoding of EXIF GPS coordinates to a human-readable
// place name, via OpenStreetMap's public Nominatim API. Returns None on any
// error (offline, rate-limited, no result) -- this is an enrichment, never
// a hard requirement, same philosophy as the vision-description step.
//
// Nominatim's usage policy (operations.osmfoundation.org/policies/nominatim)
// requires a descriptive User-Agent and at most ~1 request/second from a
// single client. Both are enforced HERE (not by the caller): a process-wide
// cache (rounded to ~1km) avoids repeat lookups for photos taken minutes
// apart in the same place, and a global mutex serializes every actual
// network call with a minimum spacing -- this holds even when main.rs
// processes photos on multiple threads, since all of them funnel through
// this same lock.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const USER_AGENT: &str = "cs-imageindex/0.2 (+https://github.com/guenther-alka/cs-imageindex)";
const MIN_INTERVAL: Duration = Duration::from_millis(1100);

struct GeoState {
    cache: HashMap<(i64, i64), Option<String>>,
    last_call: Option<Instant>,
}

fn state() -> &'static Mutex<GeoState> {
    static STATE: OnceLock<Mutex<GeoState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(GeoState { cache: HashMap::new(), last_call: None }))
}

/// Round to ~1km resolution (2 decimal degrees) for cache-key purposes --
/// good enough to dedupe "many photos from the same afternoon in the same
/// place" without pretending GPS-derived place names need more precision.
fn cache_key(lat: f64, lon: f64) -> (i64, i64) {
    ((lat * 100.0).round() as i64, (lon * 100.0).round() as i64)
}

pub fn reverse_geocode(lat: f64, lon: f64) -> Option<String> {
    let key = cache_key(lat, lon);

    // Fast path: cache hit -- no network call, no rate-limit wait, no lock
    // held any longer than a HashMap lookup.
    {
        let st = state().lock().unwrap();
        if let Some(cached) = st.cache.get(&key) {
            return cached.clone();
        }
    }

    // Slow path: hold the lock across the actual HTTP call so concurrent
    // callers (from other threads) serialize on Nominatim's rate limit
    // instead of racing it.
    let mut st = state().lock().unwrap();
    if let Some(cached) = st.cache.get(&key) {
        return cached.clone(); // filled by another thread while we waited
    }
    if let Some(last) = st.last_call {
        let elapsed = last.elapsed();
        if elapsed < MIN_INTERVAL {
            std::thread::sleep(MIN_INTERVAL - elapsed);
        }
    }
    let result = fetch(lat, lon);
    st.last_call = Some(Instant::now());
    st.cache.insert(key, result.clone());
    result
}

fn fetch(lat: f64, lon: f64) -> Option<String> {
    let url = format!(
        "https://nominatim.openstreetmap.org/reverse?format=json&lat={lat}&lon={lon}&zoom=10&addressdetails=1"
    );
    let resp = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(10))
        .call()
        .ok()?;
    let data: Value = resp.into_json().ok()?;
    let addr = &data["address"];
    let locality = addr["city"]
        .as_str()
        .or_else(|| addr["town"].as_str())
        .or_else(|| addr["village"].as_str())
        .or_else(|| addr["municipality"].as_str())
        .or_else(|| addr["county"].as_str());
    let country = addr["country"].as_str();
    match (locality, country) {
        (Some(l), Some(c)) => Some(format!("{l}, {c}")),
        (Some(l), None) => Some(l.to_string()),
        (None, Some(c)) => Some(c.to_string()),
        (None, None) => data["display_name"].as_str().map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_rounds_to_two_decimals() {
        // Both within the same 0.01-degree bucket (48.13xx) -- should
        // collapse to the same cache key.
        assert_eq!(cache_key(48.1301, 11.5820), cache_key(48.1304, 11.5822));
        // Clearly different buckets -- should NOT collapse.
        assert_ne!(cache_key(48.1301, 11.5820), cache_key(48.20, 11.5820));
    }
}
