//! Wall-clock time as the cache files store it.

/// Seconds since the Unix epoch — the unit of the coarse timestamps seagents
/// writes (`started` in a `.meta`, `since` in a `.state`, `captured` in the
/// snapshot).
pub(crate) fn unix_now() -> u64 {
    unix_now_ms() / 1000
}

/// Milliseconds since the Unix epoch — the unit of the turn ledger in
/// `.agent`, where a prompt landing in the same second as the previous `Stop`
/// is the common case, not an edge.
pub(crate) fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
