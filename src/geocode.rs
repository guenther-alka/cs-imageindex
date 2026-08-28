// Best-effort reverse geocoding of EXIF GPS coordinates to a human-readable
// place name, via OpenStreetMap's public Nominatim API. Returns None on any
// error (offline, rate-limited, no result) -- this is an enrichment, never
// a hard requirement, same philosophy as the vision-description step.
//
// Nominatim's usage policy (operations.osmfoundation.org/policies/nominatim)
// requires a descriptive User-Agent and at most ~1 request/second from a
// single client; both are respected here (fixed User-Agent below, and the
// caller in main.rs sleeps between calls -- only invoked for photos that
// actually have GPS data, which in practice is a small fraction).

use serde_json::Value;

const USER_AGENT: &str = "cs-imageindex/0.1 (+https://github.com/guenther-alka/cs-imageindex)";

pub fn reverse_geocode(lat: f64, lon: f64) -> Option<String> {
    let url = format!(
        "https://nominatim.openstreetmap.org/reverse?format=json&lat={lat}&lon={lon}&zoom=10&addressdetails=1"
    );
    let resp = ureq::get(&url)
        .set("User-Agent", USER_AGENT)
        .timeout(std::time::Duration::from_secs(10))
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
