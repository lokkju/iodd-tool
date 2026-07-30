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
    /// Bytes are the disk image. Copied directly, no external tool.
    Raw,
    /// A container `qemu-img` must unpack. Carries the name to report.
    Container(&'static str),
}

impl SourceKind {
    #[must_use]
    pub fn needs_qemu(&self) -> bool {
        matches!(self, Self::Container(_))
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A *fixed* VHD carries `conectix` only in its trailing footer, so a raw
    /// image with a fixed-VHD footer must still sniff as raw — that is the
    /// commonest input to `convert`.
    #[test]
    fn a_fixed_vhd_footer_at_the_end_does_not_make_it_a_container() {
        let mut file = vec![0u8; 2048];
        file[1536..1544].copy_from_slice(b"conectix");
        assert_eq!(sniff(&file[..512]), SourceKind::Raw);
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
