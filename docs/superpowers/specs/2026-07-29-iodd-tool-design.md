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

### F1. RETRACTED — the device does not simply expose the whole file

**Original claim, now known to be wrong:** that the IODD presents the entire
file, footer included, as the virtual disk.

That came from measuring exactly one file —
`VHDs/Windows_Server_2012R2.rmd`, 42949673472 bytes, presented as a
42949673472-byte `/dev/sda`. The arithmetic was right; generalising from it
was not. It was the only `.rmd` on the volume and the only file without a
valid footer, so it was the worst possible single sample.

Hardware acceptance, measured 2026-07-30 with files this tool created:

| File | Ext | Footer | File size | Presented |
|---|---|---|---|---|
| `item-b.vhd` | `.vhd` | valid | 1073742336 | **1073741824** = file − 512 |
| `item-c-nofooter.vhd` | `.vhd` | **zeroed** | 67109376 | **67108864** = file − 512 |
| `Windows_Server_2012R2.rmd` | `.rmd` | none | 42949673472 | 42949673472 = whole file |

Confirmed by `/sys/block/sda/size` and `lsblk -b` independently.

**Two hypotheses are dead.** The device does not always present the whole
file, and it does not decide by reading the footer — a `.vhd` with its footer
zeroed is trimmed by 512 just the same as one with a valid footer.

**What is established:**

- A `.vhd` is presented as **file size minus 512**, whatever the footer says or
  whether there is one at all. The device evidently assumes the fixed-VHD
  layout from the extension.
- Footer validity affects neither the presented size nor listing (see F2).
- Therefore `--size 20G` gives a guest a disk of exactly 20 GiB, not the
  odd 20 GiB + 512 previously stated here. D1 is correct and cleaner than
  described.

**Resolved: the extension decides.** `retype` was used to rename
`item-c-footer.vhd` to `.rmd` — same bytes, same inode, nothing else changed —
and the device's presentation moved:

```
item-c-footer.vhd  ->  67108864 presented  (file - 512)
item-c-footer.rmd  ->  67109376 presented  (whole file)
```

So:

| Extension | Presented as |
|---|---|
| `.vhd` | file size − 512, i.e. the last sector is assumed to be a footer and hidden |
| `.rmd` | the whole file, footer or not |

**`SPEC.md` lines 26-28 are wrong.** They state the two carry identical bytes
and that converting between them is a zero-cost rename. The bytes are indeed
identical; the disk the guest sees is not. The difference is 512 bytes.

This also explains the file that began the investigation.
`Windows_Server_2012R2.rmd` is 40 GiB + 512 presented whole, with a GPT backup
header in its final sector — precisely what results from creating a 40 GiB
`.vhd` (file 40 GiB + 512, presenting 40 GiB) and then renaming it to `.rmd`,
after which the guest saw 512 more bytes and partitioned to fit. The original
sample was a record of exactly the operation `retype` performs.

**Two consequences follow.**

`retype` is not safe as documented. Converting a genuine `.rmd` to `.vhd`
hides its final sector from the guest. On the sample file that sector holds the
GPT backup header, so the operation would silently damage the partition table
as the guest sees it.

`create --removable` is questionable. It currently writes `virtual_size + 512`
with a footer, which for a `.rmd` the guest will see in full — an odd
`virtual_size + 512` disk with 512 bytes of footer at the end. To give a
removable guest exactly the requested size, the file should be `virtual_size`
with no footer, which is also what the `.ima` files on the device look like and
what F2 says the device accepts.

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
100 MiB with `force_size`, `ffffe5c7` without).

**Corrected 2026-07-29.** This finding previously added that the CHS pseudocode
"matches qemu's non-`force_size` output (100 MiB gives 1004/12/17)". That
wording is wrong, and it nearly caused a real bug — a draft implementation plan
read it as evidence that `SPEC.md`'s pseudocode was miscalculating and proposed
changing its rounding to match.

The pseudocode matches qemu's `calculate_geometry`. Qemu's *output* reflects a
search loop layered on top that **grows the requested sector count** until the
CHS product covers it:

```
chs(204612) = 1003/12/17, product 204612   covers exactly
chs(204800) = 1003/12/17, product 204612   does NOT cover  <- 100 MiB
chs(204816) = 1004/12/17, product 204816   covers

qemu-img create -f vpc, size 104761344 (204612 sectors)
  -> footer CHS bytes 03eb 0c11 = 1003/12/17
```

Hand qemu a size the algorithm covers exactly and it returns 1003, matching the
pseudocode. **The pseudocode is correct as written**, floor division throughout,
and must be implemented unchanged. `cylinders * heads * spt <= total_sectors` is
expected: the footer's size fields carry the truth and CHS is legacy geometry.
D4 keeps the exact size, so the tool must not replicate qemu's growth.

Consequence for testing: never compare our CHS against qemu's for a size *we*
requested, because qemu may have grown it. Read the harvested footer's
`Original Size`, divide by 512, and assert `chs(that_count)` equals the footer's
CHS bytes.

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
| D5 | ~~Zeroing is mandatory~~ **Superseded by D9** | Phase 0 measured stale cluster contents surviving `fallocate` in 6 of 6 samples. See `spike/FINDINGS.md` S1. |
| D10 | **`.vhd` and `.rmd` are different formats.** `create --removable` writes a raw image of exactly `virtual_size` with no footer; `retype` converts rather than renaming | Measured: the device presents `.vhd` as file − 512 and `.rmd` whole, decided by extension. See F1. |
| D9 | **Zeroing is off by default, with a loud warning; `--zero` opts in** | Analysis of VHD Tool++ showed the official tool does not zero either — it uses `SetFileValidData`, whose whole purpose is to skip the write. Instant creation is the point of the tool, and matching vendor behaviour matters more than being quietly slower and differently-behaved. See `spike/VHD-TOOL-PLUSPLUS.md` V1. |
| D7 | **There is no allocation strategy.** Resolved in the negative | All three strategies produce byte-identical extent layouts. ntfs3's allocator cannot be steered from userspace. See `spike/FINDINGS.md` S7. |
| D8 | The tool verifies contiguity; it does not produce it | Follows from D7. `fallocate` is retained as a cheap early abort, not as an allocation technique. |
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

The spike needs `sudo` for `mount`. Integration tests that depend on it are
feature-gated so CI can skip them.

### Results

Run 2026-07-29. Full write-up in [`spike/FINDINGS.md`](../../../spike/FINDINGS.md).

- **S1. `fallocate` does not zero on ntfs3.** Six of six samples read back the
  pre-deletion pattern from the raw device while the filesystem reported
  zeros. D5 resolved: zeroing is mandatory.
- **S2. `fallocate` does not produce one extent.** A 1 GiB allocation on a
  freshly emptied 4 GiB volume returned two runs separated by a ~1.5 GiB
  physical gap. This is the more consequential finding, and it invalidates the
  `SPEC.md` approach of "call `posix_fallocate`, then verify": on an empty
  volume that approach refuses to create the file. Raised as D7.
- **S3. Fresh allocations carry `FIEMAP_EXTENT_UNWRITTEN`.** This closes the
  F6 question and provides a one-ioctl detector for an uninitialized region,
  usable as post-write verification.
- **S4.** `fallocate(2)` mode 0 is supported (no `EOPNOTSUPP`), and the
  `st_blocks` sparseness check behaves as specified.

All of the above measures the **ntfs3 driver**, which is what this tool calls.
The IODD device has its own NTFS reader in firmware and is not ntfs3. The
inference that a guest would observe the stale bytes is deferred to hardware
acceptance.

### The allocation sequence

Settled by S7. Since the extent layout is identical whatever the strategy,
`fallocate` is kept for a different reason than the spec assumed: it is an
instant, cheap way to fail before doing expensive work.

1. `fallocate(2)` the full `virtual_size + 512`. Instant, and fails fast on
   ENOSPC.
2. FIEMAP bounded to the file size, merged into runs. **More than one run
   aborts here**, before any data is written. On a 20 GiB target this saves
   the entire zeroing pass on the failure path when `--zero` was given.
3. **Only with `--zero`:** write zeros across the whole `virtual_size + 512`.
   Measured at roughly 500 MB/s, so about 40 s for 20 GiB.

   Without it, the data region keeps whatever was on those clusters, exactly
   as `VhdTool.exe /create` leaves it (D9). `create` prints a warning saying
   so. The `UNWRITTEN` recheck in step 4 is therefore informational rather
   than a gate unless `--zero` was requested.
4. FIEMAP again: confirm still one run, and that `UNWRITTEN` has cleared
   per S6.
5. Write the footer in place at `virtual_size` with a seek-and-write, no
   truncation and no extension of length.
6. Final verify: footer readable, checksum valid, run count unchanged.

Writing into a `fallocate`d file does not relocate it; the `fallocate+write`
measurements match `fallocate` alone byte for byte.

### Superseded: the allocation strategy follow-up

`spike/allocation-strategy.sh` compares three strategies on freshly formatted
volumes: one `fallocate` call, a sequential zero write with no `fallocate`,
and `fallocate` followed by an overwrite.

The hypothesis was that a sequential write might land contiguously where
`fallocate` does not, making the mandatory zeroing free.

It did not. `spike/allocation-dirty-volume.sh` measured all three strategies
against a filled-then-emptied volume and against one with deliberately
fragmented free space, and the extent layouts came back byte-identical every
time. The second half of the original prediction is the one that held:
allocation on ntfs3 cannot be steered from userspace, so the tool verifies and
refuses rather than promising contiguity it cannot deliver.

Both scripts are retained for reproducibility.

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
