//! Identifying what a source image actually is.
//!
//! `convert` handles raw images natively and delegates everything else to
//! `qemu-img`, so it has to tell them apart. Neither `SPEC.md` nor the design
//! says how; the plan settled it as magic sniffing (OPEN-5).
//!
//! Sniffing rather than trusting the extension, because the whole point of
//! `doctor` is that IODD filenames lie: a `.vmdk` on an IODD is normally
//! fixed-VHD bytes, and a `.vhd` may be a VHDX. An extension is a claim; a
//! magic number is evidence.

/// What a source file turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Bytes are the disk image, all of them. Copied directly, no external
    /// tool.
    Raw,
    /// A fixed VHD: the bytes are the disk image *except* the trailing
    /// 512-byte footer.
    ///
    /// Distinguishing this from [`Self::Raw`] is the whole reason
    /// [`classify`] exists. Treating a fixed VHD as raw takes its footer to be
    /// guest data and appends a second one, growing the virtual disk by 512
    /// bytes — silently, because the new footer is internally consistent and
    /// `verify` reports it valid.
    FixedVhd {
        /// File length minus the footer. What the guest actually sees.
        virtual_size: u64,
        /// What the footer claims. Equal to `virtual_size` in a well-formed
        /// file; a mismatch means the file was truncated or extended without
        /// the footer being updated.
        declared_size: u64,
    },
    /// A container `qemu-img` must unpack. Carries the name to report.
    Container(&'static str),
}

impl SourceKind {
    #[must_use]
    pub fn needs_qemu(&self) -> bool {
        matches!(self, Self::Container(_))
    }

    /// Bytes of the source that are guest data rather than metadata.
    #[must_use]
    pub fn data_len(&self, file_len: u64) -> u64 {
        match self {
            Self::FixedVhd { virtual_size, .. } => *virtual_size,
            _ => file_len,
        }
    }
}

/// Container magics, longest first so a prefix cannot shadow a longer match.
const MAGICS: &[(&[u8], &str)] = &[
    (b"# Disk DescriptorFile", "VMDK descriptor"),
    (b"KDMV", "VMDK sparse extent"),
    (b"QFI\xfb", "qcow2"),
    (b"vhdxfile", "VHDX"),
    (b"<<< Oracle VM VirtualBox Disk Image >>>", "VDI"),
    (b"conectix", "VHD"),
    (b"cxsparse", "VHD (sparse header)"),
];

/// Classify from the first bytes of a file.
///
/// A fixed VHD keeps `conectix` only at the *end*, so a match at offset 0 means
/// a dynamic or differencing VHD, which is a container. That asymmetry is the
/// reason this looks only at the head.
#[must_use]
pub fn sniff(head: &[u8]) -> SourceKind {
    for (magic, name) in MAGICS {
        if head.starts_with(magic) {
            return SourceKind::Container(name);
        }
    }
    SourceKind::Raw
}

/// Full classification: head for containers, tail for a fixed VHD's footer.
///
/// The two ends answer different questions. `conectix` at offset 0 means a
/// dynamic or differencing VHD — a container qemu must unpack. The same cookie
/// in the last sector means a fixed VHD, whose bytes are already the disk.
///
/// The cookie alone is the gate, matching `doctor`'s D2 stance: a footer with
/// a bad checksum is still plainly a footer, and treating it as guest data
/// because it failed validation would corrupt the very file the operator is
/// trying to rescue.
#[must_use]
pub fn classify(head: &[u8], tail: &[u8; 512], file_len: u64) -> SourceKind {
    if let SourceKind::Container(name) = sniff(head) {
        return SourceKind::Container(name);
    }
    if file_len >= 512 {
        let (footer, problems) = crate::footer::parse(tail, None);
        let no_cookie = problems
            .iter()
            .any(|p| matches!(p, crate::footer::FooterProblem::BadCookie { .. }));
        if !no_cookie {
            return SourceKind::FixedVhd {
                virtual_size: file_len - 512,
                declared_size: footer.virtual_size,
            };
        }
    }
    SourceKind::Raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footer::{Creator, Footer};

    fn head_of(bytes: &[u8]) -> Vec<u8> {
        let mut v = bytes.to_vec();
        v.resize(512, 0);
        v
    }

    #[test]
    fn plain_bytes_are_raw() {
        assert_eq!(sniff(&head_of(b"\0\0\0\0")), SourceKind::Raw);
        assert_eq!(sniff(&head_of(&[0xAB; 64])), SourceKind::Raw);
        assert_eq!(sniff(&[]), SourceKind::Raw);
    }

    #[test]
    fn containers_are_recognized() {
        for (magic, name) in MAGICS {
            let got = sniff(&head_of(magic));
            assert_eq!(
                got,
                SourceKind::Container(name),
                "failed to recognize {name}"
            );
            assert!(got.needs_qemu());
        }
    }

    /// A *fixed* VHD carries `conectix` only in its trailing footer, so head
    /// sniffing alone must not call it a container.
    #[test]
    fn a_fixed_vhd_footer_at_the_end_does_not_make_it_a_container() {
        let mut file = vec![0u8; 2048];
        file[1536..1544].copy_from_slice(b"conectix");
        assert_eq!(sniff(&file[..512]), SourceKind::Raw);
    }

    // ---- classify: the head/tail distinction -------------------------------

    fn footer_bytes(virtual_size: u64) -> [u8; 512] {
        Footer::new(virtual_size, Creator::DEFAULT, 0x3000_0000, [3u8; 16]).to_bytes()
    }

    /// The bug this function exists to prevent: taking a fixed VHD's footer
    /// for guest data, which grows the virtual disk by 512 bytes.
    #[test]
    fn a_fixed_vhd_is_recognized_by_its_tail() {
        let head = [0u8; 512];
        let tail = footer_bytes(16 * 1024 * 1024);
        let len = 16 * 1024 * 1024 + 512;

        assert_eq!(
            classify(&head, &tail, len),
            SourceKind::FixedVhd {
                virtual_size: 16 * 1024 * 1024,
                declared_size: 16 * 1024 * 1024,
            }
        );
    }

    #[test]
    fn a_raw_image_with_no_footer_stays_raw() {
        let head = [0u8; 512];
        let tail = [0u8; 512];
        assert_eq!(classify(&head, &tail, 1_048_576), SourceKind::Raw);
    }

    /// A container at the head wins: a dynamic VHD also carries `conectix` in
    /// its trailing footer, and qemu must still unpack it.
    #[test]
    fn a_container_head_outranks_a_footer_tail() {
        let mut head = [0u8; 512];
        head[..8].copy_from_slice(b"conectix");
        let tail = footer_bytes(1024);
        assert_eq!(
            classify(&head, &tail, 4096),
            SourceKind::Container("VHD"),
            "a dynamic VHD has a footer too, but needs unpacking"
        );
    }

    /// A damaged footer is still a footer. Falling back to raw here would
    /// append a second one and corrupt the file being rescued.
    #[test]
    fn a_footer_with_a_bad_checksum_is_still_a_fixed_vhd() {
        let mut tail = footer_bytes(1_048_576);
        tail[64] ^= 0xFF; // checksum field
        assert!(matches!(
            classify(&[0u8; 512], &tail, 1_048_576 + 512),
            SourceKind::FixedVhd { .. }
        ));
    }

    #[test]
    fn a_footer_declaring_a_different_size_is_reported_not_hidden() {
        let tail = footer_bytes(999 * 512);
        let got = classify(&[0u8; 512], &tail, 1_048_576 + 512);
        assert_eq!(
            got,
            SourceKind::FixedVhd {
                virtual_size: 1_048_576,
                declared_size: 999 * 512,
            },
            "the caller decides what to do about the disagreement"
        );
    }

    #[test]
    fn a_file_shorter_than_a_footer_is_raw() {
        assert_eq!(classify(&[0u8; 512], &[0u8; 512], 256), SourceKind::Raw);
    }

    #[test]
    fn data_len_excludes_only_a_vhd_footer() {
        let vhd = SourceKind::FixedVhd {
            virtual_size: 1000,
            declared_size: 1000,
        };
        assert_eq!(vhd.data_len(1512), 1000);
        assert_eq!(SourceKind::Raw.data_len(1512), 1512);
        assert_eq!(SourceKind::Container("qcow2").data_len(1512), 1512);
    }

    /// But at offset 0 it is a dynamic or differencing VHD, which qemu must
    /// unpack.
    #[test]
    fn a_vhd_footer_copy_at_offset_zero_is_a_container() {
        assert_eq!(sniff(&head_of(b"conectix")), SourceKind::Container("VHD"));
    }

    #[test]
    fn a_short_file_does_not_panic() {
        assert_eq!(sniff(b"KD"), SourceKind::Raw);
        assert_eq!(sniff(b""), SourceKind::Raw);
    }
}
