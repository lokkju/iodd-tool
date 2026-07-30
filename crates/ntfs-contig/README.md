# ntfs-contig

Allocate and verify **contiguous, fully-allocated** files on Linux.

Some consumers of a file do not read it through the filesystem. They map its
extents once and then stream sectors straight off the block device — IODD
hardware enclosures, certain hypervisor fast paths, some database and swap
configurations. For those, two properties matter and neither is visible through
ordinary file APIs:

- the file is **fully allocated** — not sparse, not merely reserved
- the file occupies a **single contiguous extent**

```
ntfs-contig verify /path/to/image.img
ntfs-contig verify --json /mnt/vol/*.img
ntfs-contig verify --report-only /mnt/vol/big.vhd
```

```
image.img
  size          1073741824
  allocated     1073741824
  sparse        false
  raw records   1
  merged runs   1   contiguous=true
  any unwritten false
  filesystem    0x7366746e (ntfs3)
```

Exit codes match `iodd`'s for the properties both share: `4` fragmented,
`5` sparse or uninitialized, `6` compressed target, `8` contiguity
unverifiable.

## What it cannot do

**It cannot make a file contiguous.** Measurement on ntfs3 showed the allocator
places blocks identically whether the request arrives as `fallocate(2)` or as
sequential writes, so userspace has no lever on placement. Where a volume's free
space permits a single run you get one; where it does not, no approach does.
Consolidating an already-fragmented file needs filesystem-specific relocation
that Linux does not expose for NTFS.

## Two things measured the hard way

**`fallocate(2)` does not zero.** Allocated-but-unwritten clusters keep whatever
was on them. Reads through the filesystem return zeros because the filesystem
synthesizes them past the valid data length; anything reading the raw device
sees the old bytes. `any_unwritten` detects this in one ioctl.

**FIEMAP must be bounded to the file size.** Querying past end-of-file, as
`filefrag` does, returns an extra `UNWRITTEN` record covering slack in the final
cluster. That record is cluster tail, not file data — and left unbounded it
makes `any_unwritten` true forever.

## Library

```toml
ntfs-contig = { version = "0.1", default-features = false }
```

`default-features = false` drops the CLI and its clap/serde dependencies.

```rust
let facts = ntfs_contig::FileFacts::open(path)?;
if facts.is_sparse() || !facts.is_contiguous() { /* ... */ }
```

## License

[PolyForm Shield 1.0.0](../../LICENSE.md)
