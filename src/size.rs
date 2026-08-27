//! Parses the byte sizes people type on the command line.
//!
//! Both binaries take `--full-download-limit`, and nobody wants to write
//! `4294967296`. Suffixes are the usual binary ones (`K`, `M`, `G`, `T`, and the
//! `KiB`/`MiB` spellings); a bare number is bytes.

/// Turn a size like `4G`, `512MiB`, or `1048576` into a byte count.
///
/// Case-insensitive. `0` is valid and means "no limit" to every caller here.
pub fn parse_size(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("expected a size like 4G, 512M, or a number of bytes".to_string());
    }

    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (number, suffix) = trimmed.split_at(digits_end);
    if number.is_empty() {
        return Err(format!(
            "'{trimmed}' doesn't start with a number; try something like 4G or 512M"
        ));
    }

    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" | "byte" | "bytes" => 1u64,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024u64 * 1024 * 1024 * 1024,
        other => {
            return Err(format!(
            "'{other}' isn't a size suffix I know; use K, M, G, or T (or leave it off for bytes)"
        ))
        }
    };

    let value: u64 = number
        .parse()
        .map_err(|_| format!("'{number}' is too big to be a size in bytes"))?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("'{trimmed}' is larger than a 64-bit byte count can hold"))
}

/// Render a byte count the way [`parse_size`] would accept it back.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1024u64 * 1024 * 1024 * 1024, "TiB"),
        (1024 * 1024 * 1024, "GiB"),
        (1024 * 1024, "MiB"),
        (1024, "KiB"),
    ];
    for (scale, unit) in UNITS {
        if bytes >= scale && bytes % scale == 0 {
            return format!("{} {unit}", bytes / scale);
        }
    }
    format!("{bytes} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_numbers_are_bytes() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("1048576"), Ok(1024 * 1024));
        assert_eq!(parse_size("  4096  "), Ok(4096));
    }

    #[test]
    fn suffixes_are_binary_and_case_insensitive() {
        assert_eq!(parse_size("1K"), Ok(1024));
        assert_eq!(parse_size("1kib"), Ok(1024));
        assert_eq!(parse_size("2M"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_size("4G"), Ok(4 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("4 gb"), Ok(4 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1T"), Ok(1024u64 * 1024 * 1024 * 1024));
    }

    #[test]
    fn nonsense_says_what_to_type_instead() {
        assert!(parse_size("").unwrap_err().contains("4G"));
        assert!(parse_size("big")
            .unwrap_err()
            .contains("doesn't start with"));
        assert!(parse_size("4X").unwrap_err().contains("suffix"));
    }

    #[test]
    fn a_size_that_cannot_fit_is_rejected_rather_than_wrapped() {
        assert!(parse_size("99999999999999999999")
            .unwrap_err()
            .contains("too big"));
        assert!(parse_size("18446744073709551615T")
            .unwrap_err()
            .contains("64-bit"));
    }

    #[test]
    fn formatting_round_trips_through_the_parser() {
        for bytes in [0, 512, 4096, 4 * 1024 * 1024 * 1024] {
            let rendered = format_size(bytes);
            assert_eq!(parse_size(&rendered), Ok(bytes), "{rendered}");
        }
        assert_eq!(format_size(4 * 1024 * 1024 * 1024), "4 GiB");
        assert_eq!(format_size(1500), "1500 bytes");
    }
}
