//! Small formatting helpers for the surviving commands.

/// Bytes -> a short human string (`1.2 GiB`).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn counts_below_a_kibibyte_stay_bytes() {
        // The branch that prints the raw count rather than a scaled value.
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
    }

    #[test]
    fn larger_counts_climb_the_units_and_stop_at_gib() {
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
        // The unit list ends at GiB, so a TiB-scale count grows the number, not the unit.
        assert_eq!(human_bytes(1024u64.pow(4)), "1024.0 GiB");
    }
}
