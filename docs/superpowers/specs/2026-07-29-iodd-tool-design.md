# iodd: design

Design document for the `iodd` tool described in `SPEC.md`. This document
records the empirical findings that revise the spec, the decisions taken, and
the implementation architecture.

Status: approved 2026-07-29. Supersedes `SPEC.md` where the two disagree.

## Empirical findings

All findings below were measured against a physical IODD device (External HDD
enclosure, NTFS volume labelled `iodd`, mounted read-only via `ntfs3`) and
against `qemu-img` 8.2.2 on the development host. They are recorded here
because several of them contradict `SPEC.md`.

### F1. The device exposes the entire file as the virtual disk

`VHDs/Windows_Server_2012R2.rmd` is 42949673472 bytes. Mounted through the
IODD, it presents as `/dev/sda` of **42949673472 bytes** — byte-identical to
the file size.

The 512-byte footer region is therefore *inside* the guest-visible disk, not
excluded from it. `SPEC.md` lines 21-24 model the guest-visible region as
`virtual_size` with the footer outside it. That is wrong.

Consequence: a guest that partitions the disk with GPT writes its backup GPT
header into the last sector, which is the footer. The footer does not survive
first use.

### F2. A valid VHD footer is not required to mount

The last 512 bytes of that same working `.rmd` are a GPT backup header, not a
VHD footer:

```
EFI PART  rev 1.0  hdrsize 92
MyLBA          = 83886080      (declares itself the last sector)
AlternateLBA   = 1
FirstUsableLBA = 34
LastUsableLBA  = 83886047
PartEntryLBA   = 83886048
```

83886080 + 1 = 83886081 sectors = 42949673472 bytes, confirming F1
independently: the guest that wrote this table saw the whole file.

Both `.ima` files on the device (`tools/sedutil/RESCUE64-tarball.ima` at
78643200 bytes = 75 MiB exactly, `tools/sedutil/UEFI64-tarball.ima` at
33554432 bytes = 32 MiB exactly) are raw images with no footer and no `+512`,
and they also carry `EFI PART` in their last sectors.

Three files, none with a valid VHD footer, all mounted by the device.

### F3. Naive FIEMAP extent counting rejects working files

`filefrag -v` on the working `.rmd`:

```
ext: logical_offset:      physical_offset: length:    flags:
  0:      0..10485760:  49574310..60060070: 10485760: merged,eof
  1: 10485760..10485760: 60060070..60060070:       0: last,unwritten,merged,eof
: 1 extent found
```

Two FIEMAP records. They are physically adjacent: 49574310 + 10485760 =
60060070 exactly. `filefrag` reports **1 extent** because it merges adjacent
records; the trailing record is a zero-length EOF marker for the partial final
cluster.

`SPEC.md` line 158 says to read `fm_mapped_extents` and treat 1 as contiguous.
That yields **2** for this file and would report a demonstrably working file as
fragmented.

The engine must merge physically adjacent records into runs and count runs.

### F4. The round size the user requests is `virtual_size`

40 GiB is 42949672960. The file is 42949673472. The difference is exactly 512.

Had VHD Tool++ treated the requested size as the *file* size, the file would be
42949672960 and `virtual_size` would be 42949672448. It does not. The round
number becomes `virtual_size` and 512 is added on top.

### F5. Fragmented ISOs work in practice

`_ISO/pop-os_24.04_amd64_nvidia_22.iso` has 3 extents on a working device. All
other ISOs measured have 1. This supports the spec's ISO fragment tolerance,
though it does not establish where the ceiling actually is.

### F6. Platform constants

- ntfs3 `statfs` `f_type` is `0x7366746e`.
- FIEMAP works on a read-only `ntfs3` mount without elevated privileges.
- `st_blocks` for the working `.rmd` is 83886088, i.e. 42949677056 bytes
  allocated against a 42949673472-byte file. Non-sparse, rounded up to the
  4096-byte cluster.
- ntfs3 surfaces `FIEMAP_EXTENT_UNWRITTEN`, giving a direct detector for
  uninitialized preallocated ranges.

### F7. qemu-img cannot be used as a footer oracle

Measured directly:

- With `force_size=on`, qemu writes CHS `65535/16/255` for **every** size,
  including 100 MiB. It never runs the geometry algorithm.
- Without `force_size`, qemu computes real CHS but rounds the virtual size
  *up* to `cylinders * heads * spt * 512` (100 MiB becomes 104865792 bytes).
- Creator tag is `qem2`, creator version 5.3. Timestamp and UUID differ per
  invocation.
- `qemu-img create -f vpc` rejects sizes above `65535 * 16 * 255 * 512`
  (~127.9 GiB). 2T fails with "image size is too large for file format 'vpc'".

So "real CHS + exact size" is a third combination qemu cannot emit, and the
byte-identity test at `SPEC.md` line 199 is impossible. What *was* verified:
the checksum algorithm reproduces qemu's stored value exactly (`ffffe586` for
100 MiB with `force_size`, `ffffe5c7` without), and the CHS pseudocode matches
qemu's non-`force_size` output (100 MiB gives 1004/12/17, product 204816
sectors).

Also confirmed: `qemu-img create -f vpc -o subformat=fixed` leaves the data
region sparse. A 104858112-byte file occupied 16 blocks. The spec's problem
statement is correct.

## Decisions

| # | Decision | Rationale |
|---|---|---|
| D1 | `--size` sets `virtual_size`; file is `virtual_size + 512` | F4. Matches VHD Tool++ and the one proven-working file. |
| D2 | Footer problems are `WARN`, never `WILL-NOT-MOUNT` | F1, F2. Hard-gating reports three working files as broken. |
| D3 | Hard gates are extent count and sparseness only | These are the two properties the firmware actually enforces. |
| D4 | Real CHS per the MS algorithm, exact byte size | Windows-side tooling is the only consumer that reads the field. |
| D5 | Zeroing policy determined by measurement, not assumption | Phase 0 spike. See below. |
| D6 | Contiguity engine built and proven first | Highest-risk component; everything else depends on it. |

D2 and D4 are provisional and tracked as issues. D2 should be revisited once a
never-booted VHD Tool++ file can be tested; D4 once a file with an intact
footer is available to compare against.

## Spec revisions

Beyond the findings above, the following defects in `SPEC.md` are corrected
here:

1. **Extent counting** must merge physically adjacent records (F3), and must
   treat `FIEMAP_EXTENT_UNKNOWN`, `_DELALLOC`, or `_ENCODED` as untrusted
   (exit 8) rather than as success.
2. **`-S 0` is mandatory** on `qemu-img convert -n`, not "consider". Without
   it qemu punches holes for zero runs, which is exactly the sparseness failure
   mode the tool exists to prevent.
3. **Temp file plus rename.** `SPEC.md` writes directly to the target, so a
   failed allocation under `--force` destroys a good existing file before we
   know we can replace it. Allocate a temp file in the same directory,
   populate, verify, then `rename(2)`. Rename preserves extents.
4. **`--keep-on-fail`** is referenced at lines 162 and 179 but appears in no
   CLI synopsis. It belongs on `create` and `convert`.
5. **`--size` smaller than the source is an error.** Line 123 says
   `max(source, --size)`, which silently accepts an undersized value; line 98
   says it must be `>=`. The latter wins.
6. **Compression is checked after creation**, not only on the directory
   beforehand, because a new file inherits the attribute. Mechanism:
   `FS_IOC_GETFLAGS` testing `FS_COMPR_FL`, gated on `statfs` f_type matching
   ntfs3 (F6).
7. **`verify` asserts `Data Offset == 0xFFFFFFFFFFFFFFFF`.** The spec relies on
   this to detect dynamic VHDs under `doctor` but omits it from the `verify`
   checklist.
8. **`doctor` is recursive by default.** The spec notes IODD reads nested
   folders, then defaults to top-level only. The device's own layout (`_ISO/`,
   `VHDs/`, `tools/sedutil/`) confirms nesting is normal.
9. **The `.ima` extension set is explicit**: `.ima` only. The parenthetical at
   line 146 about `.img`/`.bif`/`.vfd` describes files renamed *to* `.ima`, and
   does not mean those extensions are scanned.
10. **Largest contiguous free run is best-effort and probably unavailable.**
    ntfs3 hides `$Bitmap` without the `showmeta` mount option. Report it when
    obtainable, omit it otherwise; never block on it.
11. **`doctor --strict` exits 4** when promoting `WARN` to failure, reusing the
    existing contiguity code rather than inventing a new one.
12. **No size ceiling at 127.9 GiB.** The tool must not inherit qemu's vpc
    limit (F7); IODD virtual disks routinely exceed it.
13. **Testing compares invariant footer fields only** — cookie, features,
    format version, data offset, size fields, disk type, checksum — never whole
    bytes, and never geometry (F7).

## Architecture

Single binary crate `iodd`. Not a workspace; there is one deliverable.

```
src/
  main.rs        CLI dispatch, exit-code mapping
  cli.rs         clap derive definitions
  error.rs       Error enum with exit_code()
  size.rs        SIZE parsing, 512-alignment, 1 MiB warning
  footer.rs      footer generate/parse/checksum/CHS — pure bytes, no I/O
  extents.rs     FIEMAP ioctl + merge into physical runs
  alloc.rs       fallocate, sparseness check, temp-file + rename
  fsinfo.rs      statfs ntfs3 detection, FS_IOC_GETFLAGS compression check
  report.rs      table and JSON rendering for doctor
  cmd/
    create.rs  convert.rs  verify.rs  doctor.rs  retype.rs
```

Each module has one job and a narrow interface. Two boundaries matter most:

**`footer` does no I/O.** It converts a `virtual_size` into 512 bytes and back.
Every footer property — checksum, CHS branch coverage, field packing — is
testable without touching a filesystem.

**`extents` separates the ioctl from the merge.** The unsafe `FS_IOC_FIEMAP`
call returns a `Vec<FiemapRecord>`; a pure function folds that into physical
runs. F3 becomes a unit test seeded with the exact record pair measured off
the device, rather than something reproducible only with hardware attached.

`error.rs` holds one `thiserror` enum with an `exit_code()` method, so the exit
code table lives in one place instead of being scattered across commands.

## Phase 0: the allocation spike

Runs before implementation. Answers D5 by measurement.

The decisive experiment, on a loopback NTFS volume:

1. `mkfs.ntfs` a loop-backed image.
2. Fill the volume with a recognizable byte pattern in a large file.
3. Delete that file.
4. `fallocate` a fresh file of known size.
5. Read FIEMAP to get the physical extent offsets.
6. Read the *backing loop image* at those offsets and look for the pattern.

Pattern present means `fallocate` alone hands the guest stale cluster contents
and zeroing must be the default. All zeros means ntfs3 already zeroes and the
write can be skipped.

This matters because the device streams sectors straight off the extent map,
bypassing the filesystem. NTFS tracks valid data length separately from
allocated length; reads past VDL return zeros only because the filesystem
synthesizes them. IODD never asks the filesystem.

Secondary questions the spike answers:

- Does `fallocate(2)` mode 0 succeed on ntfs3, and at what sizes?
- Does it produce a single merged run at 8 GiB and above?
- Are new allocations flagged `FIEMAP_EXTENT_UNWRITTEN`? (F6 says ntfs3 can
  surface the flag, so this is a cheap direct detector.)
- Does musl's `posix_fallocate` differ from glibc's? glibc falls back to manual
  zero-writing on `EOPNOTSUPP`; musl returns the error. Since the tool ships as
  a static musl binary, the implementation calls raw `fallocate(2)` and handles
  the fallback itself rather than depending on libc behavior.

Deliverable: a findings document committed to the repo, plus the decision on
default zeroing behavior.

The spike needs `sudo` for `mount`. Integration tests that depend on it are
feature-gated so CI can skip them.

## Testing

**Unit, no filesystem required:**

- Footer field packing and checksum, against golden vectors harvested from
  `qemu-img` (invariant fields only, per revision 13).
- CHS across every branch of the algorithm: below the 17-spt threshold, the
  31- and 63-spt fallbacks, and the cap.
- Size parsing and alignment rules.
- Extent merging over synthetic records, including the device-measured pair
  from F3 as a regression fixture.

**Integration, feature-gated, requires sudo:**

- `create` on a fresh NTFS loopback yields one merged run and non-sparse
  `st_blocks`.
- A deliberately fragmented free-space layout is detected and refused.
- `.vhd` and `.rmd` outputs are byte-identical for the same size, and `retype`
  is a pure rename.

**doctor fixtures:**

A directory holding one file per verdict class per type: a clean fixed VHD, a
fragmented one, a sparse one, a dynamic VHD and a VHDX under `.vhd` names, an
ISO under and over the fragment ceiling, a real VMDK under a `.vmdk` name, and
— per D2 — a footerless raw file under a `.rmd` name that must grade
`MOUNTABLE` with a footer `WARN`. Assert per-file verdicts and aggregate exit
codes in default and `--strict` modes, and assert no writes occurred by
checking mtimes and block counts afterward.

## Deferred

Packaging via cargo-dist lands after the engine is proven. No existing repo of
ours uses cargo-dist, so it establishes a convention rather than conforming to
one; that is not work worth doing before there is a binary worth shipping.
