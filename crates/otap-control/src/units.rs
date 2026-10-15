//! Sizes and bounds both ends agree on: the arena (S5-1), the CP timer, and
//! the `10G` / `512M` size syntax of the command line.

use crate::protocol::ArenaKind;

/// The production arena: 8 GiB (S5-1).
pub const DEFAULT_ARENA_BYTES: u64 = 8 << 30;
/// The smallest arena a daemon accepts: 64 MiB (S5-1).
pub const MIN_ARENA_BYTES: u64 = 64 << 20;
/// The arena's page: 2 MiB, so an arena is a whole number of huge pages.
pub const ARENA_PAGE_BYTES: u64 = 2 << 20;
/// The largest arena a daemon accepts: 256 GiB. An absurd size on an
/// overcommitting host would be killed for memory rather than refused.
pub const MAX_ARENA_BYTES: u64 = 256 << 30;
/// The CP timer when nothing is said: 5 seconds (S5-1).
pub const DEFAULT_CP_INTERVAL_SECS: u32 = 5;
/// The shortest CP timer (S5-1).
pub const MIN_CP_INTERVAL_SECS: u32 = 1;
/// The longest CP timer (S5-1).
pub const MAX_CP_INTERVAL_SECS: u32 = 15;
/// The hugetlbfs arena file of the Linux platform profile.
pub const DEFAULT_HUGETLB_PATH: &str = "/run/opentap/arena/dbladed";

impl ArenaKind {
    /// The arena of this host when nothing is said: the hugetlbfs file at
    /// [`DEFAULT_HUGETLB_PATH`] on Linux, heap memory elsewhere.
    #[must_use]
    pub fn default_for_host() -> Self {
        if cfg!(target_os = "linux") {
            Self::Hugetlb {
                path: DEFAULT_HUGETLB_PATH.to_string(),
            }
        } else {
            Self::Heap
        }
    }

    /// `hugetlb:<path>` or `heap`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Hugetlb { path } => format!("hugetlb:{path}"),
            Self::Heap => "heap".to_string(),
        }
    }
}

/// Checks an arena size: at least [`MIN_ARENA_BYTES`], a multiple of the
/// 2 MiB page.
pub fn check_arena_bytes(bytes: u64) -> Result<(), String> {
    if bytes < MIN_ARENA_BYTES {
        return Err(format!(
            "an arena of {} is below the {} minimum",
            format_size(bytes),
            format_size(MIN_ARENA_BYTES)
        ));
    }
    if bytes > MAX_ARENA_BYTES {
        return Err(format!(
            "an arena of {} is above the {} maximum",
            format_size(bytes),
            format_size(MAX_ARENA_BYTES)
        ));
    }
    if !bytes.is_multiple_of(ARENA_PAGE_BYTES) {
        return Err(format!(
            "an arena of {bytes} bytes is not a multiple of the 2 MiB page"
        ));
    }
    Ok(())
}

/// Checks a CP interval: [`MIN_CP_INTERVAL_SECS`] to [`MAX_CP_INTERVAL_SECS`].
pub fn check_cp_interval(secs: u32) -> Result<(), String> {
    if !(MIN_CP_INTERVAL_SECS..=MAX_CP_INTERVAL_SECS).contains(&secs) {
        return Err(format!(
            "a CP interval of {secs} s is outside {MIN_CP_INTERVAL_SECS} to {MAX_CP_INTERVAL_SECS} seconds"
        ));
    }
    Ok(())
}

/// Parses a size: digits with an optional binary suffix (`K`, `M`, `G`,
/// `T`, with or without `iB`, any case), or plain bytes.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let suffixes: [(&str, u64); 8] = [
        ("tib", 1 << 40),
        ("gib", 1 << 30),
        ("mib", 1 << 20),
        ("kib", 1 << 10),
        ("t", 1 << 40),
        ("g", 1 << 30),
        ("m", 1 << 20),
        ("k", 1 << 10),
    ];
    let (digits, factor) = suffixes
        .iter()
        .find_map(|(suffix, factor)| lower.strip_suffix(suffix).map(|d| (d, *factor)))
        .unwrap_or((lower.as_str(), 1));
    let digits = digits.trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "{text:?} is not a size (digits, optionally followed by K, M, G or T)"
        ));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("{text:?} is too large"))?;
    n.checked_mul(factor)
        .ok_or_else(|| format!("{text:?} is too large"))
}

/// A size for people: whole units when exact (`64 MiB`), else two
/// decimals (`1.50 GiB`); bytes below 1 KiB as `N B`.
#[must_use]
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (name, unit) in UNITS {
        if bytes >= unit {
            if bytes.is_multiple_of(unit) {
                return format!("{} {name}", bytes / unit);
            }
            #[allow(clippy::cast_precision_loss)]
            let value = bytes as f64 / unit as f64;
            return format!("{value:.2} {name}");
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("64K").unwrap(), 65_536);
        assert_eq!(parse_size("64KiB").unwrap(), 65_536);
        assert_eq!(parse_size("1g").unwrap(), 1 << 30);
        assert_eq!(parse_size("2 TiB").unwrap(), 2 << 40);
        assert_eq!(parse_size(" 8M ").unwrap(), 8 << 20);
        assert!(parse_size("").is_err());
        assert!(parse_size("ten").is_err());
        assert!(parse_size("-5M").is_err());
        assert!(parse_size("99999999999999999999G").is_err());
        assert!(parse_size("1.5G").is_err(), "fractions are not sizes");
    }

    #[test]
    fn sizes_format() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(64 << 20), "64 MiB");
        assert_eq!(format_size((1 << 30) + (1 << 29)), "1.50 GiB");
        assert_eq!(format_size(3 << 40), "3 TiB");
    }

    #[test]
    fn arena_and_interval_bounds() {
        assert!(check_arena_bytes(MIN_ARENA_BYTES).is_ok());
        assert!(check_arena_bytes(DEFAULT_ARENA_BYTES).is_ok());
        assert!(check_arena_bytes(MIN_ARENA_BYTES - 1).is_err());
        assert!(check_arena_bytes(MIN_ARENA_BYTES + 4096).is_err());
        assert!(check_cp_interval(1).is_ok());
        assert!(check_cp_interval(15).is_ok());
        assert!(check_cp_interval(0).is_err());
        assert!(check_cp_interval(16).is_err());
        let kind = ArenaKind::default_for_host();
        if cfg!(target_os = "linux") {
            assert_eq!(kind.describe(), format!("hugetlb:{DEFAULT_HUGETLB_PATH}"));
        } else {
            assert_eq!(kind.describe(), "heap");
        }
    }
}
