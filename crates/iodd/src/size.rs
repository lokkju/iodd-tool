//! `SIZE` parsing and alignment rules. Pure; no I/O.
//!
//! # Suffix semantics
//!
//! Neither `SPEC.md` nor the design settles whether `G` means 2^30 or 10^9, so
//! the plan fixed it (OPEN-2):
//!
//! | Form | Meaning |
//! |---|---|
//! | `20` | bytes |
//! | `20K` `20M` `20G` `20T` | binary, 1024^n |
//! | `20KiB` `20MiB` `20GiB` `20TiB` | binary, 1024^n |
//! | `20KB` `20MB` `20GB` `20TB` | decimal, 1000^n |
//!
//! Matching is case-insensitive, but `GB` and `GiB` remain distinct: the `i`
//! is what selects binary, not the case. Bare `G` is binary because that is
//! what `20G` means to everyone who has ever made a disk image, and because it
//! makes `--size 40G` produce the 42949672960-byte `virtual_size` measured on
//! the physical device (FINDINGS F4).

use crate::error::{Error, Result};

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;

/// Parse a `SIZE` argument into an exact byte count.
pub fn parse(input: &str) -> Result<u64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(Error::Usage("size is empty".into()));
    }

    let digits_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (digits, unit) = trimmed.split_at(digits_end);

    if digits.is_empty() {
        return Err(Error::Usage(format!(
            "size {input:?} does not start with a number"
        )));
    }

    let value: u64 = digits
        .parse()
        .map_err(|_| Error::Usage(format!("size {input:?} is out of range")))?;

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => KIB,
        "m" | "mib" => MIB,
        "g" | "gib" => 1024 * MIB,
        "t" | "tib" => 1024 * 1024 * MIB,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        other => {
            return Err(Error::Usage(format!(
                "size {input:?} has an unrecognized unit {other:?} \
                 (use K/M/G/T for binary, KB/MB/GB/TB for decimal)"
            )));
        }
    };

    value
        .checked_mul(multiplier)
        .ok_or_else(|| Error::Usage(format!("size {input:?} overflows a 64-bit byte count")))
}

/// A fixed VHD is a whole number of 512-byte sectors. Anything else is
/// rejected rather than rounded, because silently changing the size the
/// operator asked for is worse than making them retype it.
pub fn require_512_aligned(bytes: u64) -> Result<()> {
    if bytes == 0 {
        return Err(Error::Usage("size must be greater than zero".into()));
    }
    if bytes % 512 != 0 {
        return Err(Error::Usage(format!(
            "size {bytes} is not a multiple of 512 (off by {})",
            bytes % 512
        )));
    }
    Ok(())
}

/// MB-aligned sizes are what Windows, Hyper-V, and IODD expect. Misaligned
/// ones work but invite geometry surprises, so `SPEC.md` line 79 asks for a
/// warning rather than a refusal.
#[must_use]
pub fn mib_alignment_warning(bytes: u64) -> Option<String> {
    if bytes % MIB == 0 {
        return None;
    }
    Some(format!(
        "size {bytes} is not a multiple of 1 MiB; this works but is unusual \
         and may produce surprising guest geometry"
    ))
}

/// Render a byte count for reports. Binary units, since that is what the
/// footer and the device deal in.
#[must_use]
pub fn humanize(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1024 * 1024 * MIB),
        ("GiB", 1024 * MIB),
        ("MiB", MIB),
        ("KiB", KIB),
    ];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            let whole = bytes / scale;
            let frac = (bytes % scale) * 100 / scale;
            return if frac == 0 {
                format!("{whole} {unit}")
            } else {
                format!("{whole}.{frac:02} {unit}")
            };
        }
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_digits_are_bytes() {
        assert_eq!(parse("512").expect("parse"), 512);
        assert_eq!(parse("1048576").expect("parse"), 1_048_576);
        assert_eq!(parse("0").expect("parse"), 0);
    }

    #[test]
    fn bare_suffixes_are_binary() {
        assert_eq!(parse("1K").expect("parse"), 1024);
        assert_eq!(parse("1M").expect("parse"), 1_048_576);
        assert_eq!(parse("1G").expect("parse"), 1_073_741_824);
        assert_eq!(parse("1T").expect("parse"), 1_099_511_627_776);
    }

    #[test]
    fn ibi_suffixes_are_binary() {
        assert_eq!(parse("1KiB").expect("parse"), 1024);
        assert_eq!(parse("20GiB").expect("parse"), 21_474_836_480);
    }

    #[test]
    fn b_suffixes_are_decimal() {
        assert_eq!(parse("1KB").expect("parse"), 1_000);
        assert_eq!(parse("1MB").expect("parse"), 1_000_000);
        assert_eq!(parse("1GB").expect("parse"), 1_000_000_000);
    }

    #[test]
    fn units_are_case_insensitive() {
        assert_eq!(parse("20g").expect("parse"), parse("20G").expect("parse"));
        assert_eq!(
            parse("20gib").expect("parse"),
            parse("20GiB").expect("parse")
        );
        assert_eq!(parse("20gb").expect("parse"), parse("20GB").expect("parse"));
    }

    /// The `i` selects binary, not the case. These must not collapse.
    #[test]
    fn gb_and_gib_stay_distinct() {
        assert_ne!(
            parse("20GB").expect("parse"),
            parse("20GiB").expect("parse")
        );
        assert_eq!(parse("20GB").expect("parse"), 20_000_000_000);
        assert_eq!(parse("20GiB").expect("parse"), 21_474_836_480);
    }

    /// The size measured on the physical device. `--size 40G` must produce the
    /// virtual_size that VHD Tool++ produced. FINDINGS F4.
    #[test]
    fn forty_g_matches_the_device_measurement() {
        assert_eq!(parse("40G").expect("parse"), 42_949_672_960);
        // The file on the device is virtual_size + 512.
        assert_eq!(42_949_672_960_u64 + 512, 42_949_673_472);
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(parse("  20G  ").expect("parse"), 21_474_836_480);
    }

    #[test]
    fn junk_is_rejected() {
        for bad in ["", "  ", "G", "abc", "20X", "20 GG", "1.5G", "-1", "20G20"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn overflow_is_rejected_not_wrapped() {
        assert!(parse("999999999999T").is_err());
    }

    #[test]
    fn alignment_rejects_odd_sizes() {
        assert!(require_512_aligned(512).is_ok());
        assert!(require_512_aligned(1_048_576).is_ok());
        assert!(require_512_aligned(513).is_err());
        assert!(require_512_aligned(1).is_err());
        assert!(require_512_aligned(0).is_err(), "zero is not a disk");
    }

    #[test]
    fn mib_warning_fires_only_when_misaligned() {
        assert!(mib_alignment_warning(1_048_576).is_none());
        assert!(mib_alignment_warning(21_474_836_480).is_none());
        assert!(mib_alignment_warning(1_048_576 + 512).is_some());
        // Decimal sizes are the common way to trip this.
        assert!(mib_alignment_warning(20_000_000_000).is_some());
    }

    #[test]
    fn humanize_reads_sensibly() {
        assert_eq!(humanize(512), "512 B");
        assert_eq!(humanize(1024), "1 KiB");
        assert_eq!(humanize(21_474_836_480), "20 GiB");
        assert_eq!(humanize(42_949_673_472), "40 GiB");
    }
}
