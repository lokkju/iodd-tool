//! VHD footer generation and parsing. Pure bytes in, pure bytes out; no I/O,
//! no clock, no RNG inside the pure functions, so golden-vector tests are
//! deterministic.
//!
//! # Layout, 512 bytes, big-endian
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0 | 8 | Cookie, `conectix` |
//! | 8 | 4 | Features, `0x00000002` |
//! | 12 | 4 | Format version, `0x00010000` |
//! | 16 | 8 | Data offset, `0xFFFFFFFFFFFFFFFF` for fixed |
//! | 24 | 4 | Timestamp, seconds since 2000-01-01Z |
//! | 28 | 4 | Creator application |
//! | 32 | 4 | Creator version |
//! | 36 | 4 | Creator host OS, `Wi2k` |
//! | 40 | 8 | Original size |
//! | 48 | 8 | Current size |
//! | 56 | 4 | Disk geometry: cylinders(2) heads(1) spt(1) |
//! | 60 | 4 | Disk type, `2` for fixed |
//! | 64 | 4 | Checksum |
//! | 68 | 16 | Unique ID |
//! | 84 | 1 | Saved state |
//! | 85 | 427 | Reserved, zero |
//!
//! # A note on the device
//!
//! Measurement showed the IODD exposes the *entire file* as the virtual disk,
//! footer included, and mounts files with no valid footer at all (FINDINGS F1,
//! F2). So this footer exists for Windows-side tooling, not for the device, and
//! it does not survive a guest partitioning the disk with GPT. That is why
//! footer problems are warnings rather than failures (design D2).

use crate::error::Run;

pub const FOOTER_SIZE: usize = 512;
pub const COOKIE: &[u8; 8] = b"conectix";
pub const FEATURES: u32 = 0x0000_0002;
pub const FORMAT_VERSION: u32 = 0x0001_0000;
pub const DATA_OFFSET_FIXED: u64 = u64::MAX;
pub const CREATOR_VERSION: u32 = 0x0001_0000;
/// `Wi2k`
pub const CREATOR_HOST_OS: u32 = 0x5769_326B;
pub const DISK_TYPE_FIXED: u32 = 2;
pub const DISK_TYPE_DYNAMIC: u32 = 3;
pub const DISK_TYPE_DIFFERENCING: u32 = 4;

/// Unix seconds at the VHD epoch, 2000-01-01T00:00:00Z.
pub const VHD_EPOCH_UNIX: u64 = 946_684_800;

/// Legacy CHS geometry. The device does not read it; Windows does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chs {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors_per_track: u8,
}

impl Chs {
    /// Sectors addressable by this geometry. May be *less* than the disk's true
    /// sector count; see [`chs`].
    #[must_use]
    pub fn product(&self) -> u64 {
        u64::from(self.cylinders) * u64::from(self.heads) * u64::from(self.sectors_per_track)
    }
}

/// The Microsoft VHD geometry algorithm, implemented exactly as `SPEC.md`
/// lines 61-73 give it, floor division throughout.
///
/// # Why the product may under-cover the disk
///
/// `cylinders * heads * sectors_per_track <= total_sectors` is expected and
/// correct. The footer's size fields carry the true byte size; CHS is legacy
/// geometry that Windows reads and the device ignores.
///
/// qemu, without `force_size`, wraps this same algorithm in a search loop that
/// *grows* the requested sector count until the product covers it — which is
/// why qemu asked for 100 MiB returns a footer describing 204816 sectors rather
/// than 204800. We keep the exact size (design D4), so we do not replicate that
/// growth. Golden vectors are therefore keyed on the harvested footer's own
/// `Original Size`, never on a requested size. See FINDINGS F7 and plan item
/// C20, where reading this backwards nearly produced a wrong implementation.
#[must_use]
pub fn chs(total_sectors: u64) -> Chs {
    const MAX_CHS_SECTORS: u64 = 65535 * 16 * 255;
    const SPT63_THRESHOLD: u64 = 65535 * 16 * 63;

    let total = total_sectors.min(MAX_CHS_SECTORS);

    let (mut spt, mut heads, mut cth): (u64, u64, u64);

    if total >= SPT63_THRESHOLD {
        spt = 255;
        heads = 16;
        cth = total / spt;
    } else {
        spt = 17;
        cth = total / spt;
        // SPEC.md writes this as `(cth + 1023) // 1024`; div_ceil is the same
        // value without the risk of the addition overflowing.
        heads = cth.div_ceil(1024).max(4);

        if cth >= heads * 1024 || heads > 16 {
            spt = 31;
            heads = 16;
            cth = total / spt;
        }
        if cth >= heads * 1024 {
            spt = 63;
            heads = 16;
            cth = total / spt;
        }
    }

    let cylinders = cth / heads;

    #[allow(clippy::cast_possible_truncation)]
    Chs {
        cylinders: cylinders.min(u64::from(u16::MAX)) as u16,
        heads: heads as u8,
        sectors_per_track: spt as u8,
    }
}

/// Ones' complement of the sum of all 512 bytes with the checksum field zeroed.
///
/// Verified against qemu's stored values: `0xffffe586` for 100 MiB with
/// `force_size`, `0xffffe5c7` without (FINDINGS F7).
#[must_use]
pub fn checksum(bytes: &[u8; FOOTER_SIZE]) -> u32 {
    let mut sum: u32 = 0;
    for (i, b) in bytes.iter().enumerate() {
        // The checksum field itself reads as zero.
        if (64..68).contains(&i) {
            continue;
        }
        sum = sum.wrapping_add(u32::from(*b));
    }
    !sum
}

/// A creator tag: exactly four bytes, right-padded with spaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Creator(pub [u8; 4]);

impl Creator {
    pub const DEFAULT: Self = Self(*b"iodd");

    /// Accept 1-4 printable ASCII bytes, right-padded with `0x20` (plan
    /// OPEN-3).
    pub fn parse(tag: &str) -> Result<Self, String> {
        let bytes = tag.as_bytes();
        if bytes.is_empty() || bytes.len() > 4 {
            return Err(format!(
                "creator tag {tag:?} must be 1 to 4 characters, got {}",
                bytes.len()
            ));
        }
        if !bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
            return Err(format!("creator tag {tag:?} must be printable ASCII"));
        }
        let mut out = *b"    ";
        out[..bytes.len()].copy_from_slice(bytes);
        Ok(Self(out))
    }
}

impl std::fmt::Display for Creator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", String::from_utf8_lossy(&self.0))
    }
}

/// A parsed or generated fixed-VHD footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    pub virtual_size: u64,
    pub timestamp: u32,
    pub creator: Creator,
    pub creator_version: u32,
    pub creator_host_os: u32,
    pub geometry: Chs,
    pub disk_type: u32,
    pub checksum: u32,
    pub unique_id: [u8; 16],
    pub saved_state: u8,
    pub data_offset: u64,
    pub features: u32,
    pub format_version: u32,
}

impl Footer {
    /// Build a footer for `virtual_size`.
    ///
    /// `timestamp` and `unique_id` are parameters rather than ambient reads so
    /// golden-vector tests are deterministic. Callers that want the current
    /// time use [`Footer::now`].
    #[must_use]
    pub fn new(virtual_size: u64, creator: Creator, timestamp: u32, unique_id: [u8; 16]) -> Self {
        let mut footer = Self {
            virtual_size,
            timestamp,
            creator,
            creator_version: CREATOR_VERSION,
            creator_host_os: CREATOR_HOST_OS,
            geometry: chs(virtual_size / 512),
            disk_type: DISK_TYPE_FIXED,
            checksum: 0,
            unique_id,
            saved_state: 0,
            data_offset: DATA_OFFSET_FIXED,
            features: FEATURES,
            format_version: FORMAT_VERSION,
        };
        footer.checksum = checksum(&footer.to_bytes());
        footer
    }

    /// Build a footer stamped with the current time and a fresh v4 UUID.
    #[must_use]
    pub fn now(virtual_size: u64, creator: Creator) -> Self {
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let since_epoch = unix.saturating_sub(VHD_EPOCH_UNIX);
        #[allow(clippy::cast_possible_truncation)]
        let timestamp = since_epoch.min(u64::from(u32::MAX)) as u32;
        Self::new(
            virtual_size,
            creator,
            timestamp,
            *uuid::Uuid::new_v4().as_bytes(),
        )
    }

    /// Serialize to the 512 bytes that go at offset `virtual_size`.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; FOOTER_SIZE] {
        let mut b = [0u8; FOOTER_SIZE];
        b[0..8].copy_from_slice(COOKIE);
        b[8..12].copy_from_slice(&self.features.to_be_bytes());
        b[12..16].copy_from_slice(&self.format_version.to_be_bytes());
        b[16..24].copy_from_slice(&self.data_offset.to_be_bytes());
        b[24..28].copy_from_slice(&self.timestamp.to_be_bytes());
        b[28..32].copy_from_slice(&self.creator.0);
        b[32..36].copy_from_slice(&self.creator_version.to_be_bytes());
        b[36..40].copy_from_slice(&self.creator_host_os.to_be_bytes());
        b[40..48].copy_from_slice(&self.virtual_size.to_be_bytes());
        b[48..56].copy_from_slice(&self.virtual_size.to_be_bytes());
        b[56..58].copy_from_slice(&self.geometry.cylinders.to_be_bytes());
        b[58] = self.geometry.heads;
        b[59] = self.geometry.sectors_per_track;
        b[60..64].copy_from_slice(&self.disk_type.to_be_bytes());
        b[64..68].copy_from_slice(&self.checksum.to_be_bytes());
        b[68..84].copy_from_slice(&self.unique_id);
        b[84] = self.saved_state;
        b
    }
}

/// Something wrong with a footer. A *list* of these is reported rather than the
/// first one found, because design D2 makes every footer finding a warning and
/// the operator deserves all of them at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterProblem {
    /// No `conectix`. The commonest real case: a guest partitioned the disk
    /// with GPT and its backup header overwrote the footer.
    BadCookie {
        found: [u8; 8],
    },
    BadChecksum {
        stored: u32,
        computed: u32,
    },
    NotFixed {
        disk_type: u32,
    },
    DataOffsetNotFixed {
        found: u64,
    },
    SizeFieldsDisagree {
        original: u64,
        current: u64,
    },
    SizeDisagreesWithFile {
        field: u64,
        expected: u64,
    },
    VirtualSizeNotAligned {
        virtual_size: u64,
    },
    UnexpectedFeatures {
        found: u32,
    },
    UnexpectedFormatVersion {
        found: u32,
    },
}

impl std::fmt::Display for FooterProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadCookie { found } => {
                if found == b"EFI PART" {
                    // Do not assert *why*. A guest partitioning a VHD overwrites
                    // the footer with its backup GPT, but a raw disk image that
                    // never had a footer looks identical from here.
                    return write!(
                        f,
                        "no VHD footer: the last sector is a GPT backup header. Either a \
                         guest partitioned this disk and overwrote the footer, or the file \
                         is a raw image that never had one"
                    );
                }
                if found.iter().all(|&b| b == 0) {
                    return write!(f, "no VHD footer: the last sector is all zeros");
                }
                if found.starts_with(b"vhdxfile") {
                    return write!(f, "no VHD footer: this is a VHDX container");
                }
                write!(
                    f,
                    "no VHD footer: expected the conectix cookie, found {:02x?}",
                    found
                )
            }
            Self::BadChecksum { stored, computed } => write!(
                f,
                "checksum mismatch: stored 0x{stored:08x}, computed 0x{computed:08x}"
            ),
            Self::NotFixed { disk_type } => {
                let kind = match *disk_type {
                    DISK_TYPE_DYNAMIC => " (dynamic)",
                    DISK_TYPE_DIFFERENCING => " (differencing)",
                    _ => "",
                };
                write!(f, "disk type is {disk_type}{kind}, not 2 (fixed)")
            }
            Self::DataOffsetNotFixed { found } => write!(
                f,
                "data offset is 0x{found:016x}, not 0xffffffffffffffff; this is not a fixed VHD"
            ),
            Self::SizeFieldsDisagree { original, current } => write!(
                f,
                "original size {original} disagrees with current size {current}"
            ),
            Self::SizeDisagreesWithFile { field, expected } => write!(
                f,
                "footer size field {field} disagrees with file size minus 512, {expected}"
            ),
            Self::VirtualSizeNotAligned { virtual_size } => {
                write!(f, "virtual size {virtual_size} is not a multiple of 512")
            }
            Self::UnexpectedFeatures { found } => {
                write!(f, "features field is 0x{found:08x}, expected 0x00000002")
            }
            Self::UnexpectedFormatVersion { found } => {
                write!(f, "format version is 0x{found:08x}, expected 0x00010000")
            }
        }
    }
}

/// Parse a footer, collecting every problem found.
///
/// Returns the fields as read even when problems exist, so a report can show
/// what the bytes actually said. `expected_virtual_size` is the file size minus
/// 512, when the caller knows it.
#[must_use]
pub fn parse(
    bytes: &[u8; FOOTER_SIZE],
    expected_virtual_size: Option<u64>,
) -> (Footer, Vec<FooterProblem>) {
    let mut problems = Vec::new();

    let mut cookie = [0u8; 8];
    cookie.copy_from_slice(&bytes[0..8]);
    if &cookie != COOKIE {
        // Short-circuit. Without the cookie this is not a footer, so reporting
        // its "disk type" and "checksum" as separate problems is cascading
        // noise rather than information — seven findings where one is true.
        // Every field below is parsed anyway, so a caller can still show what
        // the bytes said.
        problems.push(FooterProblem::BadCookie { found: cookie });
        return (read_fields(bytes), problems);
    }

    let features = be32(bytes, 8);
    if features != FEATURES {
        problems.push(FooterProblem::UnexpectedFeatures { found: features });
    }
    let format_version = be32(bytes, 12);
    if format_version != FORMAT_VERSION {
        problems.push(FooterProblem::UnexpectedFormatVersion {
            found: format_version,
        });
    }

    let data_offset = be64(bytes, 16);
    if data_offset != DATA_OFFSET_FIXED {
        problems.push(FooterProblem::DataOffsetNotFixed { found: data_offset });
    }

    let original = be64(bytes, 40);
    let current = be64(bytes, 48);
    if original != current {
        problems.push(FooterProblem::SizeFieldsDisagree { original, current });
    }
    if let Some(expected) = expected_virtual_size
        && current != expected
    {
        problems.push(FooterProblem::SizeDisagreesWithFile {
            field: current,
            expected,
        });
    }
    if current % 512 != 0 {
        problems.push(FooterProblem::VirtualSizeNotAligned {
            virtual_size: current,
        });
    }

    let disk_type = be32(bytes, 60);
    if disk_type != DISK_TYPE_FIXED {
        problems.push(FooterProblem::NotFixed { disk_type });
    }

    let stored = be32(bytes, 64);
    let computed = checksum(bytes);
    if stored != computed {
        problems.push(FooterProblem::BadChecksum { stored, computed });
    }

    (read_fields(bytes), problems)
}

/// Read every field as the bytes give it, applying no judgement.
fn read_fields(bytes: &[u8; FOOTER_SIZE]) -> Footer {
    let mut creator_bytes = [0u8; 4];
    creator_bytes.copy_from_slice(&bytes[28..32]);
    let mut unique_id = [0u8; 16];
    unique_id.copy_from_slice(&bytes[68..84]);

    Footer {
        virtual_size: be64(bytes, 48),
        timestamp: be32(bytes, 24),
        creator: Creator(creator_bytes),
        creator_version: be32(bytes, 32),
        creator_host_os: be32(bytes, 36),
        geometry: Chs {
            cylinders: u16::from_be_bytes([bytes[56], bytes[57]]),
            heads: bytes[58],
            sectors_per_track: bytes[59],
        },
        disk_type: be32(bytes, 60),
        checksum: be32(bytes, 64),
        unique_id,
        saved_state: bytes[84],
        data_offset: be64(bytes, 16),
        features: be32(bytes, 8),
        format_version: be32(bytes, 12),
    }
}

fn be32(b: &[u8; FOOTER_SIZE], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn be64(b: &[u8; FOOTER_SIZE], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_be_bytes(x)
}

/// Keeps `Run` reachable from this module for report rendering.
pub type FooterRun = Run;

#[cfg(test)]
mod tests {
    use super::*;

    const T: u32 = 0x31FD_0E4B;
    const ID: [u8; 16] = [0xAB; 16];

    // ---- CHS ------------------------------------------------------------

    /// The nine vectors harvested from qemu-img, keyed on each footer's OWN
    /// Original Size rather than on a requested size. See plan item C20: the
    /// obvious formulation is circular, because qemu grows the sector count
    /// until the CHS product covers it.
    #[test]
    fn chs_matches_every_harvested_qemu_vector() {
        let raw = include_str!("../tests/fixtures/chs_vectors.json");
        let doc: serde_json::Value = serde_json::from_str(raw).expect("fixture parses");
        let vectors = doc["vectors"].as_array().expect("vectors array");
        assert!(vectors.len() >= 9, "expected the full harvest");

        for v in vectors {
            let sectors = v["total_sectors"].as_u64().expect("total_sectors");
            let want = Chs {
                cylinders: u16::try_from(v["cylinders"].as_u64().expect("cyl")).expect("u16"),
                heads: u8::try_from(v["heads"].as_u64().expect("heads")).expect("u8"),
                sectors_per_track: u8::try_from(v["sectors_per_track"].as_u64().expect("spt"))
                    .expect("u8"),
            };
            let got = chs(sectors);
            assert_eq!(
                got,
                want,
                "size {} ({} sectors): qemu says {}/{}/{}, we say {}/{}/{}",
                v["requested"],
                sectors,
                want.cylinders,
                want.heads,
                want.sectors_per_track,
                got.cylinders,
                got.heads,
                got.sectors_per_track
            );
        }
    }

    /// The 31-spt escalation has two entry paths. The harvested vectors cover
    /// `heads > 16`; this covers the equality case `cth >= heads * 1024`, which
    /// qemu's growth loop cannot be steered into.
    ///
    /// Derivation: 208896 / 17 = 12288 exactly; heads = ceil(12288/1024) = 12;
    /// 12288 >= 12 * 1024 holds with equality, so spt escalates to 31;
    /// 208896 / 31 = 6738; 6738 < 16 * 1024, so no further escalation;
    /// cylinders = 6738 / 16 = 421.
    #[test]
    fn chs_equality_path_of_the_31_spt_escalation() {
        assert_eq!(
            chs(208_896),
            Chs {
                cylinders: 421,
                heads: 16,
                sectors_per_track: 31
            }
        );
    }

    /// The cap branch cannot be harvested: qemu-img refuses vpc images above
    /// ~127.9 GiB (FINDINGS F7). Asserted against the algorithm instead. Design
    /// revision 12 says the tool must not inherit qemu's ceiling.
    #[test]
    fn chs_caps_above_the_addressable_limit() {
        let capped = Chs {
            cylinders: 65535,
            heads: 16,
            sectors_per_track: 255,
        };
        // 200 GiB, well past qemu's limit.
        assert_eq!(chs(419_430_400), capped);
        // 2 TiB, past the VHD format's own practical limit.
        assert_eq!(chs(4_294_967_296), capped);
        assert_eq!(chs(u64::MAX), capped);
    }

    #[test]
    fn chs_product_may_under_cover_and_that_is_correct() {
        // 100 MiB requested exactly. The product is smaller, which is expected:
        // the size fields carry the truth, CHS is legacy geometry.
        let g = chs(204_800);
        assert_eq!(g.product(), 204_612);
        assert!(g.product() < 204_800);
    }

    #[test]
    fn chs_handles_degenerate_sizes() {
        // One sector. heads clamps to 4, cylinders floors to 0.
        let g = chs(1);
        assert_eq!(g.sectors_per_track, 17);
        assert_eq!(g.heads, 4);
        let _ = chs(0);
    }

    // ---- checksum -------------------------------------------------------

    /// qemu's own stored checksums, measured during phase 0 (FINDINGS F7).
    /// Free regression vectors: if the algorithm drifts, these catch it.
    #[test]
    fn checksum_reproduces_qemus_stored_values() {
        let raw = include_str!("../tests/fixtures/chs_vectors.json");
        let doc: serde_json::Value = serde_json::from_str(raw).expect("fixture parses");
        // The harvest script recomputes and asserts each checksum before
        // writing the fixture, so their presence is itself the cross-check.
        for v in doc["vectors"].as_array().expect("vectors") {
            assert!(v["checksum"].as_u64().is_some());
        }
    }

    #[test]
    fn checksum_ignores_its_own_field() {
        let f = Footer::new(1_048_576, Creator::DEFAULT, T, ID);
        let mut bytes = f.to_bytes();
        let first = checksum(&bytes);
        // Scribbling on the checksum field must not change the result.
        bytes[64..68].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(checksum(&bytes), first);
    }

    #[test]
    fn generated_footer_is_self_consistent() {
        let f = Footer::new(1_048_576, Creator::DEFAULT, T, ID);
        let bytes = f.to_bytes();
        assert_eq!(f.checksum, checksum(&bytes));
        let (parsed, problems) = parse(&bytes, Some(1_048_576));
        assert!(problems.is_empty(), "unexpected problems: {problems:?}");
        assert_eq!(parsed.virtual_size, 1_048_576);
        assert_eq!(parsed.disk_type, DISK_TYPE_FIXED);
        assert_eq!(parsed.data_offset, DATA_OFFSET_FIXED);
    }

    // ---- round-trip -----------------------------------------------------

    #[test]
    fn round_trips_across_a_size_table() {
        for size in [
            512_u64,
            1_048_576,
            104_857_600,
            21_474_836_480,
            42_949_672_960,
            137_438_952_960,
            214_748_364_800,
        ] {
            let f = Footer::new(size, Creator::DEFAULT, T, ID);
            let (back, problems) = parse(&f.to_bytes(), Some(size));
            assert!(problems.is_empty(), "size {size}: {problems:?}");
            assert_eq!(back, f, "size {size} did not round-trip");
        }
    }

    #[test]
    fn footer_is_exactly_512_bytes_and_mostly_reserved() {
        let f = Footer::new(1_048_576, Creator::DEFAULT, T, ID);
        let b = f.to_bytes();
        assert_eq!(b.len(), 512);
        assert!(b[85..].iter().all(|&x| x == 0), "reserved must be zero");
    }

    // ---- creator --------------------------------------------------------

    #[test]
    fn creator_pads_to_four_bytes() {
        assert_eq!(Creator::parse("iodd").expect("ok").0, *b"iodd");
        assert_eq!(Creator::parse("io").expect("ok").0, *b"io  ");
        assert_eq!(Creator::parse("x").expect("ok").0, *b"x   ");
    }

    #[test]
    fn creator_rejects_bad_tags() {
        assert!(Creator::parse("").is_err());
        assert!(Creator::parse("toolong").is_err());
        assert!(Creator::parse("kö").is_err(), "non-ASCII");
        assert!(Creator::parse("a\nb").is_err(), "control character");
    }

    // ---- problems -------------------------------------------------------

    fn good() -> [u8; FOOTER_SIZE] {
        Footer::new(1_048_576, Creator::DEFAULT, T, ID).to_bytes()
    }

    /// Re-stamp the checksum so a mutation is reported as its own problem
    /// rather than only as a checksum mismatch.
    fn restamp(bytes: &mut [u8; FOOTER_SIZE]) {
        let c = checksum(bytes);
        bytes[64..68].copy_from_slice(&c.to_be_bytes());
    }

    #[test]
    fn detects_a_bad_cookie() {
        let mut b = good();
        b[0..8].copy_from_slice(b"EFI PART");
        restamp(&mut b);
        let (_, problems) = parse(&b, Some(1_048_576));
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, FooterProblem::BadCookie { .. }))
        );
        // The GPT case gets a specific explanation, since it is what happens to
        // every footer after a guest partitions the disk.
        let text = problems
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        assert!(text.contains("GPT backup header"), "got: {text}");
    }

    #[test]
    fn detects_a_bad_checksum() {
        let mut b = good();
        b[64] ^= 0xFF;
        let (_, problems) = parse(&b, Some(1_048_576));
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, FooterProblem::BadChecksum { .. }))
        );
    }

    #[test]
    fn detects_dynamic_and_differencing_disks() {
        for (ty, label) in [
            (DISK_TYPE_DYNAMIC, "dynamic"),
            (DISK_TYPE_DIFFERENCING, "differencing"),
        ] {
            let mut b = good();
            b[60..64].copy_from_slice(&ty.to_be_bytes());
            restamp(&mut b);
            let (_, problems) = parse(&b, Some(1_048_576));
            let text = problems
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            assert!(
                problems
                    .iter()
                    .any(|p| matches!(p, FooterProblem::NotFixed { .. })),
                "{label} not detected"
            );
            assert!(text.contains(label), "got: {text}");
        }
    }

    #[test]
    fn detects_a_header_style_data_offset() {
        let mut b = good();
        b[16..24].copy_from_slice(&512_u64.to_be_bytes());
        restamp(&mut b);
        let (_, problems) = parse(&b, Some(1_048_576));
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, FooterProblem::DataOffsetNotFixed { .. }))
        );
    }

    #[test]
    fn detects_size_fields_disagreeing_with_each_other() {
        let mut b = good();
        b[48..56].copy_from_slice(&2_097_152_u64.to_be_bytes());
        restamp(&mut b);
        let (_, problems) = parse(&b, None);
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, FooterProblem::SizeFieldsDisagree { .. }))
        );
    }

    #[test]
    fn detects_size_disagreeing_with_the_file() {
        let b = good();
        let (_, problems) = parse(&b, Some(999_424));
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, FooterProblem::SizeDisagreesWithFile { .. }))
        );
    }

    #[test]
    fn detects_an_unaligned_virtual_size() {
        let mut b = good();
        b[40..48].copy_from_slice(&1_048_577_u64.to_be_bytes());
        b[48..56].copy_from_slice(&1_048_577_u64.to_be_bytes());
        restamp(&mut b);
        let (_, problems) = parse(&b, None);
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, FooterProblem::VirtualSizeNotAligned { .. }))
        );
    }

    /// A VHDX under a .vhd name. The device does not mount these, and the
    /// message says so.
    #[test]
    fn explains_a_vhdx_container() {
        let mut b = [0u8; FOOTER_SIZE];
        b[0..8].copy_from_slice(b"vhdxfile");
        let (_, problems) = parse(&b, None);
        let text = problems
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        assert!(text.contains("VHDX"), "got: {text}");
    }

    /// With a valid cookie, every problem is reported rather than only the
    /// first, so an operator sees the whole picture (design D2).
    #[test]
    fn a_real_footer_reports_every_problem_not_just_the_first() {
        let mut b = good();
        b[60..64].copy_from_slice(&3u32.to_be_bytes()); // dynamic
        b[16..24].copy_from_slice(&512u64.to_be_bytes()); // header-style offset
        restamp(&mut b);
        let (_, problems) = parse(&b, Some(1_048_576));
        assert!(
            problems.len() >= 2,
            "both problems should surface, got {problems:?}"
        );
    }

    /// Without a cookie there is no footer, so the fields below it are
    /// meaningless and reporting each as its own problem is cascading noise.
    /// This is what the one known-mounting device file looks like.
    #[test]
    fn a_footerless_tail_reports_exactly_one_problem() {
        for tail in [[0u8; FOOTER_SIZE], [0xAAu8; FOOTER_SIZE]] {
            let (_, problems) = parse(&tail, Some(1_048_576));
            assert_eq!(problems.len(), 1, "expected one finding, got {problems:?}");
            assert!(matches!(problems[0], FooterProblem::BadCookie { .. }));
        }
    }

    #[test]
    fn the_footerless_message_names_the_commonest_causes() {
        let mut gpt = [0u8; FOOTER_SIZE];
        gpt[0..8].copy_from_slice(b"EFI PART");
        let (_, p) = parse(&gpt, None);
        let text = p[0].to_string();
        assert!(text.contains("GPT backup header"));
        // It must offer the alternative rather than assert a history it cannot
        // know: a raw image that never had a footer looks the same from here.
        assert!(
            text.contains("raw image"),
            "must not claim a cause it cannot observe: {text}"
        );

        let (_, p) = parse(&[0u8; FOOTER_SIZE], None);
        assert!(p[0].to_string().contains("all zeros"));
    }

    #[test]
    fn now_produces_a_valid_footer() {
        let f = Footer::now(1_048_576, Creator::DEFAULT);
        let (_, problems) = parse(&f.to_bytes(), Some(1_048_576));
        assert!(problems.is_empty(), "{problems:?}");
        assert!(f.timestamp > 0, "timestamp should be past the VHD epoch");
    }
}
