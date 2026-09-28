//! Wall-clock time as the cache files store it.

/// Seconds since the Unix epoch — the unit of every timestamp seagents writes
/// (`started` in a `.meta`, `since` in a `.state`, `captured` in the snapshot).
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
