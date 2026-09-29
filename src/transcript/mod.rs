//! An agent's own conversation record, as it writes it to disk — the source
//! for content (and, today, for the one turn fact the hooks cannot give: an
//! interrupt). One parser per transcript format; this is the only per-agent
//! code orchestration has, and the rest of seagents asks through here.
//!
//! Phase 3 (`read`) grows the common record model in this module.

use std::path::Path;

mod claude;

/// Did the agent write an interrupt record — its turn cut short by the user —
/// stamped at or after `since_ms`? `agent` is the adapter id (`claude`, …); a
/// format without a parser here never reports one.
pub(crate) fn interrupted_since(agent: &str, transcript: &Path, since_ms: u64) -> bool {
    match agent {
        "claude" => claude::interrupted_since(transcript, since_ms),
        _ => false,
    }
}

/// An RFC 3339 UTC timestamp as transcripts write it
/// (`2026-09-29T06:01:13.951Z`), in Unix milliseconds. Fractional digits
/// past milliseconds are dropped; offsets other than `Z` are refused rather
/// than misread — every format parsed here writes UTC.
fn parse_utc_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(|p| p.parse::<u64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let ms: u64 = match frac.len() {
        0 => 0,
        _ if !frac.bytes().all(|b| b.is_ascii_digit()) => return None,
        n => {
            let head = &frac[..n.min(3)];
            head.parse::<u64>().ok()? * 10u64.pow(3 - head.len() as u32)
        }
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = u64::try_from(days).ok()? * 86_400 + h * 3600 + mi * 60 + se;
    Some(secs * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_timestamps_parse_to_unix_milliseconds() {
        assert_eq!(parse_utc_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        // `date -u -d 2026-09-29T06:01:13Z +%s` = 1790661673
        assert_eq!(parse_utc_ms("2026-09-29T06:01:13.951Z"), Some(1_790_661_673_951));
        assert_eq!(parse_utc_ms("2024-02-29T23:59:59.123456Z"), Some(1_709_251_199_123));
        for bad in ["", "2026-09-29T06:01:13", "2026-09-29 06:01:13Z", "2026-13-01T00:00:00Z",
                    "2026-09-29T06:01:13+02:00", "2026-09-29T06:01:13.x1Z", "garbage"] {
            assert_eq!(parse_utc_ms(bad), None, "{bad}");
        }
    }
}
