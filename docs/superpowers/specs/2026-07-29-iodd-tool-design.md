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

**Evidence strength: one file, directly observed.** This `.rmd` was mounted
through the device during the investigation and appeared as `/dev/sda`, so the
mount is a direct observation rather than an inference. The two `.ima` files
elsewhere on the volume (`tools/sedutil/*.ima`) are unrelated sedutil rescue
images that happen to be stored there; they were never mounted through the
device and are not evidence about firmware behaviour.

So the claim is narrow but clean: a file with no valid VHD footer mounted.
Whether the footer matters *before* a guest has overwritten it is untested and
is tracked as issue #1.

### F3. The FIEMAP query must be bounded to the file size

**Corrected 2026-07-29.** An earlier version of this finding claimed ntfs3
returns two records for a contiguous file and that counting
`fm_mapped_extents` would therefore reject working files. That was wrong, and
prototyping the ioctl caught it. The corrected finding is narrower but still
an implementation requirement.

`filefrag -v -b512` on the working `.rmd` shows two records:

```
ext: logical_offset:        physical_offset:        length:  flags:
  0:      0..83886080:  396594480..480480560:  83886081:  merged,eof
  1: 83886081..83886087: 480480561..480480567:         7:  last,unwritten,merged,eof
```

Record 0 is 83886081 sectors = 42949673472 bytes: the entire file. Record 1
starts at sector 83886081, which is **past end-of-file**, and covers 7 sectors
= 3584 bytes. That is exactly the slack in the final 4096-byte cluster
(4096 - 512 = 3584). It is cluster tail, not file data.

Issuing the ioctl directly with `fm_length` bounded to the file size returns
**one** record for this file:

```
logical=0  physical=203056373760  length=42949673472  flags=last,merged
```

So the requirement is: **bound `fm_length` to the file size.** Query past EOF,
as `filefrag` does, and the cluster slack appears as an extra `unwritten`
record that would inflate the extent count.

Merging physically adjacent records into runs remains the right
implementation, but defensively rather than because this file demands it: a
genuinely fragmented file returns multiple records, and NTFS run lists can
split a physically contiguous allocation across records for reasons unrelated
to fragmentation. Adjacent records must count as one run.

`SPEC.md` line 158 is therefore incomplete rather than wrong. It omits the
bound on `fm_length`, and it omits the merge.

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
- ntfs3 emits `FIEMAP_EXTENT_UNWRITTEN`, though the only instance observed so
  far is on past-EOF cluster slack (F3), not on a `fallocate`d data region.
  Whether ntfs3 flags fresh allocations `UNWRITTEN` is a Phase 0 question, not
  an established fact.

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
| D2 | Footer problems are `WARN`, never `WILL-NOT-MOUNT` | F1, F2. Hard-gating reports a known-working file as broken. |
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

1. **Extent counting** must bound `fm_length` to the file size and merge
   physically adjacent records (F3), and must treat `FIEMAP_EXTENT_UNKNOWN`,
   `_DELALLOC`, `_ENCODED`, or `_DATA_INLINE` as untrusted (exit 8) rather
   than as success.
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
runs. The merge then becomes unit-testable over synthetic records rather than
needing hardware attached.

A working Python prototype of both halves lives in `spike/fiemap.py`. Writing
it is what caught the original error in F3, which is an argument for
prototyping the ioctl before porting it.

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
- Extent merging over synthetic records: adjacent records fold into one run,
  a physical gap splits runs, and a logical gap splits runs even when the
  physical offsets happen to abut.
- The F3 regression: a query bounded to the file size must not pick up
  past-EOF cluster slack as an extra extent.

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

## Hardware acceptance

The final phase, and the only one that tests the actual contract. Every other
test is a proxy for "the device mounts it"; this phase asks the device.

Test bed as measured: the IODD volume is 256058060800 bytes with 35520352256
free (87% used), mounted read-only by default. A volume that full almost
certainly has fragmented free space, which makes it a natural test of both the
success and the refusal path.

### A. `doctor` against the real corpus, read-only

Runs first, needs no write access, and is the single best regression fixture
available. Expected grades:

| File | Expected |
|---|---|
| `VHDs/Windows_Server_2012R2.rmd` | `MOUNTABLE`, footer `WARN` (GPT header, no `conectix`) |
| `_ISO/pop-os_24.04_amd64_nvidia_22.iso` | `MOUNTABLE` (3 extents, under the ceiling) |
| all other measured `.iso` | `MOUNTABLE` (1 extent) |

Assert no writes occurred: mtimes and `st_blocks` unchanged after the run.

### B. Create on the device, then mount through the device

Requires a read-write remount.

1. `create` a small VHD in a dedicated scratch folder.
2. Confirm our `verify` agrees with `filefrag` and `stat`: one merged run,
   non-sparse.
3. Unmount the NTFS volume cleanly, select the file on the IODD, mount it.
4. Assert the presented block device size equals the file size exactly. This
   re-confirms F1 against a file we generated rather than one we found.
5. Partition, format, and write a known file through the device; read it back.
6. Re-attach the NTFS volume and re-run `filefrag`: still one run. Guest writes
   arriving through the IODD must not fragment the backing file.
7. Record whether the footer survived, which feeds issue #1.

### C. Issue #1, resolvable without Windows

Issue #1 currently proposes creating a file with VHD Tool++ on Windows. That is
unnecessary: our own tool generates footers, so the experiment is to create two
files, one with a valid footer and one with the footer zeroed, copy both to the
device without booting either, and see which the device lists and mounts.

That settles whether the footer gates listing, using only this tool and this
hardware.

### D. The refusal path

At 87% used, a sufficiently large request should fail to find a contiguous run.
Verify the tool reports the extent map and free-space diagnostics, exits 4, and
removes the partial file — and that `--keep-on-fail` leaves it in place.

### E. Windows-side check for issue #2 (optional)

The CHS choice in D4 exists for Windows-side tooling, which is the only
consumer that reads the field. Mounting a generated `.vhd` in Windows Disk
Management confirms it opens and reports the expected size. This is the only
part of the phase that needs a Windows install.

### Safety rules

The device holds real data: backups, ISOs, and driver bundles. Therefore:

- Read-only tests (A) run before any read-write remount.
- All writes go to a dedicated scratch folder.
- Never pass `--force` against a pre-existing file on the device.
- Cap test file sizes well below the 33 GiB free.
- Remount read-write only for the duration, and restore read-only afterward.

## Deferred

Packaging via cargo-dist lands after the engine is proven. No existing repo of
ours uses cargo-dist, so it establishes a convention rather than conforming to
one; that is not work worth doing before there is a binary worth shipping.
