//! FILETIMEs: 100-nanosecond intervals since 1601-01-01 UTC, as NTFS stores every time.

/// FILETIME intervals per second.
pub const TICKS_PER_SECOND: u64 = 10_000_000;
/// Seconds from 1601-01-01 to 1970-01-01.
pub const UNIX_EPOCH_SECONDS: u64 = 11_644_473_600;
/// The FILETIME of 1970-01-01 00:00:00 UTC.
pub const UNIX_EPOCH: u64 = UNIX_EPOCH_SECONDS * TICKS_PER_SECOND;

/// Seconds since the Unix epoch (negative before it).
#[must_use]
pub const fn unix_seconds(filetime: u64) -> i64 {
    // Lossless: a FILETIME's seconds fit an i64 with room to spare.
    #[allow(clippy::cast_possible_wrap)]
    let seconds = (filetime / TICKS_PER_SECOND) as i64;
    seconds - UNIX_EPOCH_SECONDS as i64
}

/// Nanoseconds since the Unix epoch (negative before it).
#[must_use]
pub const fn unix_nanos(filetime: u64) -> i128 {
    (filetime as i128 - UNIX_EPOCH as i128) * 100
}

/// Seconds since the Unix epoch as a `u32`: 0 for a zero FILETIME and anything before 1970,
/// `u32::MAX` past 2106. Compact enough for a disk-usage tree.
#[must_use]
pub const fn unix_seconds_u32(filetime: u64) -> u32 {
    if filetime == 0 {
        return 0;
    }
    let seconds = (filetime / TICKS_PER_SECOND).saturating_sub(UNIX_EPOCH_SECONDS);
    if seconds > u32::MAX as u64 {
        u32::MAX
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let seconds = seconds as u32;
        seconds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_around_the_epoch() {
        assert_eq!(unix_seconds(UNIX_EPOCH), 0);
        assert_eq!(unix_seconds(UNIX_EPOCH + 1000 * TICKS_PER_SECOND), 1000);
        assert_eq!(unix_seconds(0), -11_644_473_600);
        assert_eq!(unix_nanos(UNIX_EPOCH + 1), 100);
        assert_eq!(unix_seconds_u32(0), 0);
        assert_eq!(unix_seconds_u32(UNIX_EPOCH - 1), 0);
        assert_eq!(unix_seconds_u32(UNIX_EPOCH + 1000 * TICKS_PER_SECOND), 1000);
        assert_eq!(unix_seconds_u32(u64::MAX), u32::MAX);
    }
}
