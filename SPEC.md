# iodd: tool specification

A Linux command-line tool for creating, converting, and verifying fixed-size virtual disk files for IODD hardware devices (Mini, ST400, 2531/2541, LK100). The tool produces `.vhd` (fixed) and `.rmd` (removable) files that satisfy IODD's zero-fragmentation mount requirement, targeting NTFS volumes mounted via the in-kernel `ntfs3` driver.

## Problem statement

IODD devices read a virtual disk file by mapping its on-disk sector extents once at mount time and streaming sectors directly from the underlying filesystem. Two properties are mandatory:

1. The file must be a **fixed-size VHD**: `virtual_size` bytes of raw disk image followed by a 512-byte VHD footer. Dynamic VHDs and VHDX are rejected by the firmware.
2. The file must be **fully allocated and stored in a single contiguous extent** (zero fragmentation). A fragmented VHD or RMD will not mount.

The official tooling (VHD Tool++) runs only on Windows. The standard Linux path (`qemu-img create -f vpc -o subformat=fixed`) fails both properties: it truncates any pre-allocated target and writes the data region **sparse**, leaving only the footer allocated. This tool exists to produce a fully-allocated, verified-contiguous fixed VHD/RMD directly on Linux without a Windows round-trip.

## Format model

### The file is a fixed VHD in all cases

Both `.vhd` and `.rmd` files carry the identical byte structure:

```
offset 0 .................. virtual_size-1    raw disk image (guest-visible sectors)
offset virtual_size ....... virtual_size+511  VHD footer (512 bytes)
total file size = virtual_size + 512
```

The VHD footer's `Disk Type` field is always `2` (fixed) for both extensions. **Fixed vs. removable is an IODD presentation concept selected by file extension, not a VHD-format concept.** The tool must never emit `Disk Type = 6` (removable is not a VHD footer disk type) or attempt to distinguish the two files at the byte level. `--removable` changes only the default output extension and nothing in the footer.

Consequence: converting a `.vhd` to a `.rmd` (or back) is a rename. The tool should expose this as a zero-cost operation rather than a rewrite.

### VHD footer layout (512 bytes, big-endian)

| Offset | Size | Field | Value |
|---|---|---|---|
| 0 | 8 | Cookie | `conectix` (ASCII) |
| 8 | 4 | Features | `0x00000002` |
| 12 | 4 | Format version | `0x00010000` |
| 16 | 8 | Data offset | `0xFFFFFFFFFFFFFFFF` (fixed) |
| 24 | 4 | Timestamp | seconds since 2000-01-01 00:00:00 UTC |
| 28 | 4 | Creator application | 4-char tag, e.g. `iodd` |
| 32 | 4 | Creator version | `0x00010000` |
| 36 | 4 | Creator host OS | `0x5769326B` (`Wi2k`) |
| 40 | 8 | Original size | `virtual_size` in bytes |
| 48 | 8 | Current size | `virtual_size` in bytes |
| 56 | 4 | Disk geometry | CHS: cylinders(2) heads(1) sectors-per-track(1) |
| 60 | 4 | Disk type | `2` (fixed) |
| 64 | 4 | Checksum | ones' complement of footer sum with this field zeroed |
| 68 | 16 | Unique ID | random UUID (v4) |
| 84 | 1 | Saved state | `0` |
| 85 | 427 | Reserved | zero |

The tool generates this footer natively; it must not shell out to `qemu-img` to obtain a footer.

### Checksum

Zero the checksum field, sum all 512 footer bytes as unsigned, store `(~sum) & 0xFFFFFFFF`.

### CHS geometry

Per the Microsoft VHD spec. `virtual_size` must be a multiple of 512; `total_sectors = virtual_size / 512`, capped at `65535 * 16 * 255` for geometry purposes (the footer size fields still carry the true byte size, matching qemu's `force_size` behavior).

```
if total_sectors > 65535 * 16 * 255:
    total_sectors = 65535 * 16 * 255
if total_sectors >= 65535 * 16 * 63:
    spt = 255; heads = 16; cth = total_sectors // spt
else:
    spt = 17; cth = total_sectors // spt
    heads = max(4, (cth + 1023) // 1024)
    if cth >= heads * 1024 or heads > 16:
        spt = 31; heads = 16; cth = total_sectors // spt
    if cth >= heads * 1024:
        spt = 63; heads = 16; cth = total_sectors // spt
cylinders = cth // heads
```

## Constraints the tool must enforce

- **Fixed only.** No dynamic, no VHDX. This is implicit in the format but must be asserted in `verify` when inspecting existing files.
- **512-byte sector alignment.** Reject sizes not divisible by 512. Warn on sizes not divisible by 1 MiB (MB-aligned sizes are what Windows/Hyper-V and IODD expect; misaligned sizes work but invite geometry surprises).
- **Zero fragmentation.** The core contract. After allocation the tool verifies the data-plus-footer file occupies exactly one extent and aborts otherwise. There is no in-place NTFS defragmenter on Linux, so the tool cannot repair fragmentation; it fails with diagnostics.
- **Full allocation, not sparse.** Verify `st_blocks * 512 >= total_size`. A sparse file defeats the extent-mapping firmware even when logically the right length.
- **No NTFS compression on the target.** A compressed target makes "fixed allocation" a fiction and reintroduces fragmentation on write. Detect the compression attribute on the target directory/file where the `ntfs3` driver exposes it; refuse or warn loudly.

## CLI

```
iodd create  --size SIZE   --out PATH [--removable] [--force] [--creator TAG]
iodd convert --source PATH --out PATH [--removable] [--force] [--size SIZE]
iodd verify  PATH [--strict]
iodd doctor  ROOT [--recursive] [--type LIST] [--format table|json] [--strict] [--iso-max-fragments N]
iodd retype  PATH --to {fixed|removable}
```

`SIZE` accepts `20G`, `20GiB`, `512M`, raw byte counts; parsed to an exact byte value and validated for 512-alignment. Default extension follows `--removable` (`.rmd`) or its absence (`.vhd`); if `--out` already carries an extension it is honored and only a mismatch with `--removable` produces a warning.

`create` makes a blank (zero-filled) fixed VHD/RMD of the given size.

`convert` writes an existing image into a contiguous fixed VHD/RMD. Raw sources are handled natively. Non-raw sources (qcow2, dynamic VHD, VMDK) require `qemu-img` on `PATH` as an optional dependency; the tool converts them into the pre-allocated target with `qemu-img convert -n` (see below) rather than letting qemu create the file. `--size` may enlarge the target beyond the source image (remaining sectors stay zero); it must be `>=` the source virtual size.

`verify` inspects a single existing file: footer validity, checksum, disk type, size-field/file-size agreement, extent count, allocation. `--strict` turns fragmentation and any sparseness into a non-zero exit; without it they are reported as warnings.

`doctor` audits every IODD-mountable file under `ROOT`, which is expected to be a mounted IODD volume (or a subtree of one). It is the "will everything on this device mount?" check. It is strictly read-only: files are opened `O_RDONLY` and only the footer, extent map, and stat are read; the tool never writes to the device. `--type` restricts to a comma-separated set of extensions (default: all recognized types). `--iso-max-fragments` overrides the ISO fragment ceiling for device families that differ from the default of 24. `--strict` promotes warnings to failures in the aggregate exit code. `--format json` emits machine-readable results for scripting.

`retype` renames between `.vhd` and `.rmd` without rewriting bytes.

## Operations

### create

1. Parse and validate `SIZE`; compute `total_size = virtual_size + 512`.
2. Refuse to overwrite an existing `--out` unless `--force`.
3. Check the target directory is not NTFS-compressed; warn/refuse.
4. Allocate `total_size` bytes as a single contiguous run (see Contiguity engine).
5. Verify contiguity and full allocation. Abort with diagnostics on failure.
6. Generate the footer for `virtual_size` and write it in place at offset `virtual_size` with a seek-and-write on the already-allocated file (`r+b`, no truncation, no extension of length).
7. Re-verify: footer readable, checksum valid, extent count unchanged.

The data region is left zero-filled by the allocation step, which is a blank disk. No formatting or partitioning is performed; that is the guest's concern.

### convert

1. Determine source virtual size. For raw, that is the file size. For non-raw, read it from `qemu-img info --output=json`.
2. `virtual_size = max(source_virtual_size, --size)` if `--size` given, else source size; validate 512-alignment; `total_size = virtual_size + 512`.
3. Allocate `total_size` contiguous; verify (as create steps 2–5).
4. Populate the data region **in place**, preserving the allocation:
   - Raw source: copy `source` bytes to target offset 0 with plain writes into the pre-allocated extents (open target `r+b`, no truncation). Any tail beyond the source stays zero.
   - Non-raw source: `qemu-img convert -n -O raw SOURCE TARGET`. The `-n` flag skips target creation so qemu writes into the existing pre-allocated file instead of truncating and re-creating it. Consider `-S 0` to disable qemu's zero-run sparsification if a specific `ntfs3`/qemu combination is observed to punch holes; on a non-CoW filesystem writing over pre-allocated zeros is otherwise a no-op and allocation is retained.
5. Write the footer in place at offset `virtual_size` (as create step 6).
6. Re-verify contiguity, allocation, and footer.

### verify

Read the last 512 bytes; confirm the `conectix` cookie, recompute and match the checksum, confirm `Disk Type == 2`, confirm `Current Size == file_size - 512` and `== Original Size`. Query the extent map; report extent count. Query `st_blocks` for sparseness. Exit non-zero on any structural failure; on fragmentation/sparseness exit non-zero only under `--strict`.

### doctor (device audit)

Walk `ROOT` (top-level only by default, recursively with `--recursive`, since IODD reads files from nested folders) and collect every file whose extension matches a recognized IODD type. For each file, dispatch to a type-specific ruleset and produce a verdict of `MOUNTABLE`, `WARN`, or `WILL-NOT-MOUNT` with the governing reason. All I/O is read-only.

The recognized types and their rulesets:

- **`.vhd`, `.rmd`, `.vmdk` (expected: fixed VHD bytes).** Run the full single-file `verify` logic: footer cookie, checksum, `Disk Type == 2`, size-field/file-size agreement, `virtual_size` 512-aligned. Then the two hard IODD gates: extent count must be exactly 1, and the file must be non-sparse (`st_blocks * 512 >= file_size`). Fragmentation or sparseness is `WILL-NOT-MOUNT`, not a warning, because the device rejects it. Additionally detect files that carry a fixed-VHD extension but are not fixed-VHD bytes and report them as `WILL-NOT-MOUNT` with a specific reason:
  - VHDX (magic `vhdxfile` at offset 0): unsupported container.
  - Dynamic or differencing VHD (footer copy at offset 0 with `Disk Type` 3 or 4, `Data Offset` pointing at a header rather than `0xFFFFFFFFFFFFFFFF`): unsupported, only fixed mounts.
  - Genuine VMDK descriptor or stream (`KDMV` magic or a `# Disk DescriptorFile` text header) under a `.vmdk` name: IODD expects fixed-VHD bytes renamed to `.vmdk`, not real VMDK, so flag it.
- **`.iso` (ISO 9660 optical image).** The one type with a fragment tolerance: extent count `<= iso_max_fragments` (default 24) is `MOUNTABLE`; above it is `WILL-NOT-MOUNT`. No footer or allocation check applies. Deep ISO 9660 structural validation is out of scope; a primary-volume-descriptor sanity check at sector 16 is optional.
- **`.ima` (and `.img`/`.bif`/`.vfd` that were renamed to `.ima`).** Uncompressed floppy image. The device's fragment tolerance for floppy images is not documented; report extent count and emit a `WARN` on fragmentation rather than a hard failure, noting the threshold is unverified.

Beyond the per-file verdict, `doctor` predicts listing behavior: IODD hides files whose contents it considers invalid, so a structurally broken file may never appear in the device's on-screen list at all. Mark such files distinctly (`WILL-NOT-MOUNT (may not be listed)`) so the operator understands why a file they copied is missing from the device menu rather than assuming a copy failure.

Output is a per-file table (path, type, size, extents, verdict, reason) followed by a summary count by verdict. `--format json` emits the same as an array of records. The aggregate exit code is `0` when every file is `MOUNTABLE`; `4` when any file is `WILL-NOT-MOUNT`; and, under `--strict`, non-zero when any file is `WARN`. `doctor` never modifies, renames, or deletes anything; remediation (defragment on Windows, re-lay the file, correct an extension) is reported, not performed.

## Contiguity engine

Allocation and its verification are the heart of the tool and must be filesystem-syscall level, not shell-parsing.

**Allocate.** Call `posix_fallocate(3)` (via `nix`, or `libc::posix_fallocate` directly) for offset 0, length `total_size`, to request real, non-sparse clusters in one call. Do not `ftruncate` to grow, which produces a sparse hole. Do not append the footer as a separate write that extends the file, which risks a second extent; allocate the full `virtual_size + 512` up front and overwrite the trailing 512 bytes in place.

**Verify extents.** Issue the `FS_IOC_FIEMAP` ioctl directly (`libc::ioctl` with a `#[repr(C)]` `struct fiemap` header followed by a heap buffer sized for the extent array, reading `fm_mapped_extents`). One extent is contiguous. Fall back to parsing `filefrag -k` only if FIEMAP is unavailable. Treat an extent count of `0` or an ioctl error as untrusted rather than as success: `ntfs3`'s FIEMAP implementation has been uneven, and the tool should surface "could not verify contiguity on this filesystem" instead of silently passing.

**Verify allocation.** From `fstat(2)`, require `st_blocks * 512 >= total_size`. Anything less means the data region is sparse; abort.

**On failure.** The tool cannot defragment NTFS from Linux. Emit the extent map, the file size, and, where obtainable, the largest contiguous free run on the target volume, then instruct the operator to consolidate free space on Windows (built-in defrag or Sysinternals Contig on the single file) or to lay the file down first on an empty/freshly-formatted volume so the large allocation lands contiguous. Exit non-zero. Do not leave a fragmented file in place unless `--keep-on-fail` is passed.

## Error handling and exit codes

| Code | Meaning |
|---|---|
| 0 | success (or verify passed) |
| 1 | usage / argument error (bad size, missing file, extension conflict) |
| 2 | target exists without `--force` |
| 3 | allocation failed (no space, `fallocate` unsupported/refused by filesystem) |
| 4 | contiguity check failed (fragmented) |
| 5 | sparseness check failed (data region not fully allocated) |
| 6 | target on compressed NTFS |
| 7 | footer verification failed (verify mode, or post-write self-check) |
| 8 | contiguity unverifiable on this filesystem (FIEMAP unavailable/unreliable) |
| 9 | external `qemu-img` required but absent or failed (non-raw convert) |

Every abort after allocation removes the partial file unless `--keep-on-fail`.

## Platform and dependencies

- Linux only. Relies on `posix_fallocate`, `FS_IOC_FIEMAP`, and `fstat`, none of which have a portable equivalent, which is why the tool does not target macOS or Windows.
- Rust, built as a fully static binary against `x86_64-unknown-linux-musl` (and `aarch64-unknown-linux-musl`) so it runs on any Linux host the device is plugged into with no runtime.
- Crate surface kept lean: `clap` (derive) for the CLI, `anyhow`/`thiserror` for errors, `libc` for the raw ioctl and `#[repr(C)]` structs, `nix` for `posix_fallocate`/`fstat` conveniences, `uuid` (v4) for the footer GUID, `walkdir` for the `doctor` traversal. The footer's big-endian packing and checksum are done with `to_be_bytes` and manual summation, no serialization crate.
- `qemu-img` is an optional runtime dependency, invoked via `std::process::Command`, used only by `convert` on non-raw sources.
- Target filesystem is assumed `ntfs3`. The engine is filesystem-agnostic, but the compression check and the FIEMAP-reliability caveat are `ntfs3`-specific and should be gated on detecting an NTFS target.

## Distribution and packaging

Ship prebuilt static binaries, not source. `cargo install` is the wrong channel here: it compiles from scratch and requires the full toolchain on the user's machine, which defeats the point for a utility run against a plugged-in device.

Use cargo-dist (invoked as `dist`). `dist init` generates the tag-triggered GitHub Actions release pipeline (plan, build, host, publish, announce) that produces the musl static binaries plus a `curl … | sh` installer and checksums; it cross-compiles the Linux targets through `cargo-zigbuild`, so no musl cross-toolchain needs standing up by hand. A plain `cargo build --release --target x86_64-unknown-linux-musl` remains the zero-infra fallback for a one-off binary to scp.

Do not adopt the `dist init` output wholesale. Reconcile the generated `release.yml`, the release-tag scheme, binary/artifact naming, `rust-toolchain.toml` pinning, and any MSRV or reusable-workflow conventions against the patterns already established across the existing GitHub repositories, and fold cargo-dist into those rather than letting it overwrite them. Where the existing repos already use a shared release workflow or a house installer convention, cargo-dist's generated CI should conform to it.

## Testing

Cross-check generated footers against `qemu-img create -f vpc -o subformat=fixed,force_size` output: for the same size, `qemu-img info -f vpc` on the tool's file must report `file format: vpc` and the exact virtual size, and the tool's footer must be byte-identical to qemu's harvested footer for that size. Validate the full matrix of sizes that exercise each CHS geometry branch (below the 17-spt threshold, the 31- and 63-spt fallbacks, and the CHS cap). Validate that `create` on a freshly-formatted NTFS loopback yields one extent and non-sparse `st_blocks`, and that a deliberately fragmented free-space layout is detected and refused. Validate that `.vhd` and `.rmd` outputs are byte-identical for the same size and that `retype` is a pure rename. For `doctor`, build a fixture directory holding one of each verdict class per type (a clean fixed VHD, a fragmented one, a sparse one, a dynamic VHD and a VHDX under `.vhd` names, an ISO under and over the fragment ceiling, a real VMDK under a `.vhd`/`.vmdk` name) and assert both the per-file verdicts and the aggregate exit code, in default and `--strict` modes, without any write occurring to the fixture (verify mtimes and inode block counts are unchanged after the run).

## Out of scope

- In-place grow/shrink of an existing VHD (VHD Tool++ offers grow; deferred).
- Writing to a raw block device rather than a file on a filesystem.
- ISO *creation*; the tool only validates existing ISOs during `doctor`, it does not author them.
- Deep ISO 9660 structural validation during `doctor`; only the fragment count (and an optional primary-volume-descriptor sanity check) is examined.
- 4Kn / 4K-native sector emulation; the tool assumes 512-byte logical sectors.
- Any Windows code path; the tool is Linux-native by design.

