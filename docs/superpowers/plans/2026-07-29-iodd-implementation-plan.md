# iodd: phased implementation plan

Status: draft, 2026-07-29. Companion to [`SPEC.md`](../../../SPEC.md) and
[the design doc](../specs/2026-07-29-iodd-tool-design.md).

Authority order, highest last: `SPEC.md` → the design doc → `spike/FINDINGS.md`.
Where this plan settles something the three left open, it is marked **OPEN** and
needs sign-off before the phase that consumes it lands.

Scope is fixed: **verify and refuse**. No defragmentation, no libntfs-3g, no GPL
dependency, no attempt to produce contiguity. `docs/licensing-libntfs-3g.md` and
S10 closed that; the project stays PolyForm Shield 1.0.0.

---

## Part 1 — where `SPEC.md` and the design doc conflict

The design doc's thirteen "Spec revisions" are authoritative and not
re-litigated. What follows is the working list for the implementer, including
conflicts the design implies but does not spell out.

| # | `SPEC.md` says | Design/FINDINGS say | Winner | Consequence |
|---|---|---|---|---|
| C1 | Guest-visible region is `virtual_size`; footer outside it (21–24) | F1: device presents the **whole file** | Design | Docs and `doctor` reasoning only. Byte layout unchanged: file = `virtual_size + 512`, footer at `virtual_size`. Do not "fix" the layout. |
| C2 | Footer validity is a structural failure (133, 141) | D2: footer problems are `WARN` | Design | `doctor` never fails a file for footer reasons. |
| C3 | `--strict` promotes fragmentation and sparseness (100, 133) | D3: those are the only **hard** gates; D2 makes footer soft | Design | **Inverts `--strict`.** Fragmentation → 4 and sparseness → 5 **always**; `--strict` promotes footer/geometry warnings to 7. User-visible; must land with `--help` and README. **OPEN — needs sign-off.** |
| C4 | Hard gates include fixed-vs-dynamic, size agreement, checksum | D3: extent count and sparseness only | Design | Everything else advisory. |
| C5 | `posix_fallocate` then verify (156) | S1: does not zero. S7: does not steer placement | FINDINGS | `fallocate` is a cheap early abort; the zero pass makes the file real. |
| C6 | Read and count `fm_mapped_extents` (158) | F3 + revision 1: bound `fm_length`, merge adjacent, treat `UNKNOWN`/`DELALLOC`/`ENCODED`/`DATA_INLINE` as untrusted | Design | Phase 2. |
| C7 | Fall back to `filefrag -k` (158) | Silent | `SPEC.md`, deferred | v1 exits 8 with a clear message. **OPEN (low stakes).** |
| C8 | `-S 0`: "consider" (127) | Revision 2: mandatory | Design | |
| C9 | Write directly to target | Revision 3: temp file, then `rename(2)` | Design | `--force` can never destroy a good file. |
| C10 | `--keep-on-fail` in no synopsis (162, 179) | Revision 4: on `create` and `convert` | Design | |
| C11 | `max(source, --size)` (123) vs `--size >= source` (98) | Revision 5: the latter | Design | `--size` below source is exit 1. |
| C12 | Compression checked on directory (112) | Revision 6: also on the created file, `FS_IOC_GETFLAGS`/`FS_COMPR_FL`, gated on `f_type == 0x7366746e` | Design | Both; post-creation check runs before `rename`. |
| C13 | `verify` omits `Data Offset` | Revision 7: assert `0xFFFFFFFFFFFFFFFF` | Design | |
| C14 | `doctor` top-level by default | Revision 8: recursive by default | Design | Flag inversion. OPEN-1. |
| C15 | `.ima` "(and `.img`/`.bif`/`.vfd`…)" (146) | Revision 9: `.ima` only | Design | |
| C16 | Report largest contiguous free run (162) | Revision 10: `$Bitmap` hidden without `showmeta`; best-effort | Design | `statvfs` free bytes as substitute. |
| C17 | Byte-identical qemu footer cross-check (199) | F7 + revision 13: impossible; invariant fields only | Design | See C20. |
| C18 | — | Revision 12: no 127.9 GiB ceiling | Design | Sizes above `65535*16*255*512` legal; cap branch handles them. |
| C19 | Exit 3 covers "`fallocate` unsupported" (171) | Design: handle the fallback ourselves | Design | `EOPNOTSUPP`/`ENOSYS` is a **warning** — the zero pass allocates anyway. `ENOSPC`/`EDQUOT`/`EFBIG` is exit 3. |
| C20 | CHS pseudocode, floor division (61–73) | F7's wording implied a conflict | **`SPEC.md` — the pseudocode is correct** | See below. |

### C20: resolved by measurement, and the pseudocode wins

An earlier draft of this plan reported that the pseudocode contradicts F7 —
pseudocode gives 1003/12/17 for 100 MiB where F7 measured 1004/12/17 — and
proposed harvesting vectors to find "the rounding that reproduces qemu."
**That remedy would have been wrong.** Measured directly:

```
chs(204612) = 1003/12/17, product 204612   covers exactly
chs(204800) = 1003/12/17, product 204612   does NOT cover
chs(204816) = 1004/12/17, product 204816   covers

qemu-img create -f vpc, size 104761344 (204612 sectors)
  -> footer CHS bytes 03eb 0c11 = 1003/12/17
```

qemu wraps the same algorithm in a **search loop that grows the requested
sector count** until the CHS product covers it. 204800 does not fit under
1003/12/17, so qemu climbs to 204816 and reports 1004. Hand qemu a size the
algorithm covers exactly and it returns 1003, matching the pseudocode.

The defect was in F7's wording — it said the pseudocode "matches qemu's
non-`force_size` output," when it matches qemu's `calculate_geometry` and
qemu's *output* reflects the growth loop. Adopting the earlier remedy would
have baked qemu's size-growing into our geometry function, contradicting D4's
exact-size decision.

**Implement the pseudocode as written, floor division throughout.** Accept that
`cylinders * heads * spt <= total_sectors`; the CHS product may under-cover the
true size, which is fine because the footer's size fields carry the truth and
CHS is legacy geometry.

**Test formulation.** Do not compare our CHS against qemu's for a size we both
requested — qemu may have grown it. Instead, read the harvested footer's
`Original Size`, divide by 512 to get the sector count qemu settled on, and
assert `chs(that_count)` equals qemu's CHS bytes. That exercises the geometry
function directly, independent of the growth loop, and works at every size qemu
can produce.

Branch coverage (harvest at each; the cap branch is exempt because
`qemu-img create -f vpc` refuses above ~127.9 GiB per F7):

| Size | `total_sectors` | Branch |
|---|---|---|
| 100 MiB | 204 800 | 17-spt |
| 102 MiB | 208 896 | 17-spt, `cth` an exact multiple of 1024 (equality path) |
| 200 MiB | 409 600 | 31-spt via the `heads > 16` disjunct |
| 300 MiB | 614 400 | 63-spt via `cth >= heads*1024` |
| 32 GiB | 67 108 864 | 255-spt |
| 200 GiB | 419 430 400 | cap — algorithm plus hand check only |

### Open items

- **OPEN-1 (Phase 5).** Revision 8 makes `doctor` recursive by default, but the
  synopsis still prints `[--recursive]`. Recommend: default recursive, add
  `--no-recursive`, keep `--recursive` as a hidden accepted no-op. Update the
  README synopsis in the same commit.
- **OPEN-2 (Phase 3).** `SIZE` suffix semantics. Recommend: bare `K/M/G/T` and
  explicit `KiB/MiB/GiB/TiB` are binary; `KB/MB/GB/TB` decimal; bare digits
  bytes. Makes `20G` = 21474836480, matching F4.
- **OPEN-3 (Phase 3).** `--creator` is 4 bytes. Recommend: accept 1–4 ASCII,
  right-pad with `0x20`, reject non-ASCII or >4 (exit 1). Default `iodd`.
- **OPEN-4 (Phase 4).** What `--keep-on-fail` keeps. Under C9 the file being
  built is a temp file, so "leave it in place" cannot mean the target path
  without reintroducing the destruction C9 prevents. Recommend: leave the temp
  file at its temp path and print it; never rename over the target. Failure is
  never allowed to publish.
- **OPEN-5 (Phase 6).** How `convert` decides a source is raw. Recommend:
  sniff known magics (`QFI\xfb`, `vhdxfile`, `conectix`/`cxsparse`, `KDMV`,
  `# Disk DescriptorFile`, VirtualBox VDI); any hit means non-raw. Add
  `--source-format raw|auto|<qemu name>` as an escape hatch. This is a CLI
  addition and needs sign-off.
- **OPEN-6 (Phase 4).** Generic I/O errors have no code in the table.
  Recommend: map to 1, consistent with "missing file."

---

## Part 2 — cross-cutting notes

**Bounding `fm_length` matters for `UNWRITTEN`, not for the count.** In F3's two
records, `396594480 + 83886081 == 480480561` and the logical offsets abut too —
so `merge_runs` folds them by itself and the count would have been right
regardless. The bound still matters because the merged run's length would exceed
the file size (corrupting diagnostics), and decisively because **the past-EOF
slack is flagged `UNWRITTEN` while the data region is not.** Step 4 asserts
`UNWRITTEN` has cleared (S6); an unbounded query would see the slack's flag
forever and fail every create. Bound the query, or the write path never
succeeds.

**Zero the whole `virtual_size + 512`, not just the data region.** If the zero
pass stops at `virtual_size`, the trailing 512 bytes are still `UNWRITTEN` when
step 4 runs. Zero `[0, total_size)`; the footer write overwrites the last 512.

**The FIEMAP buffer must be 8-byte aligned.** `struct fiemap` is 32 bytes and
`struct fiemap_extent` 56, both with `u64` members. A `Vec<u8>` is 1-aligned and
produces unaligned reads. Allocate `Vec<u64>` and cast, or use a
`#[repr(C, align(8))]` wrapper.

**`fallocate` alone is not the allocation.** S7: layout is byte-identical
whether bytes arrive via `fallocate(2)` or sequential writes. It earns its place
only as an instant ENOSPC detector and an early extent check that saves a ~40 s
zero pass on the failure path.

**`convert` should not zero twice.** For a raw source that overwrites
`[0, source_size)`, zero only `[source_size, total_size)`. The step-4
`UNWRITTEN` recheck over the whole file proves the combination was complete, so
the optimization is verified rather than trusted.

**Separate the pure verdict from I/O in `doctor`.** Define `FileFacts { size,
st_blocks, run_count, any_unwritten, untrusted, head: [u8;4096], tail: [u8;512],
ext }` and a pure `fn grade(&FileFacts, &Policy) -> Verdict`. Nearly every
fixture the design calls for becomes a struct literal in a unit test — no
filesystem, no sudo, no committed binaries. Only genuinely fragmented and
genuinely sparse cases need the feature-gated tier.

---

## Phase 1 — scaffolding, error taxonomy, CI

**Delivers.** A compiling binary with the complete CLI surface, `--help` for
every subcommand, and the exit-code table in exactly one place. Every subcommand
returns "not implemented". CI green from the first commit, including musl.

**Files.**

- `Cargo.toml` — single binary crate `iodd`, `edition = "2024"`,
  `license = "Polyform-Shield-1.0.0"`, `rust-version` 1.95. Deps: `clap` 4
  (derive), `thiserror` 2, `libc`, `nix` (`fs`, `ioctl`), `uuid` (`v4`), `serde`
  (derive), `serde_json`, `walkdir`. Dev-deps: `assert_cmd`, `predicates`,
  `tempfile`. **No `anyhow`** — `SPEC.md` line 185 lists it, but a single
  `thiserror` enum with `exit_code()` is the point of `error.rs`, and `anyhow`
  erases the variant carrying the code. Note the deviation in the commit.
- `src/main.rs` — `main()` only does
  `exit(run().unwrap_or_else(|e| { eprintln!("iodd: {e}"); e.exit_code() }))`.
  `run() -> Result<i32, Error>` returns a code on success because `doctor`'s
  success path can still be 4.
- `src/cli.rs` — full clap derive tree, every flag from the README synopsis plus
  `--keep-on-fail` (C10) and the recursion flag (OPEN-1).
- `src/error.rs` — one `thiserror` enum, one variant per exit-code row, plus
  `Io(#[from] std::io::Error)` per OPEN-6, and `exit_code()`. Variants carry
  structured data, not formatted strings: `Fragmented { path, runs, free_bytes,
  largest_free_run }`, `Sparse { path, allocated, expected }`, so `SPEC.md` line
  162's diagnostics render from data at the edge.
- `src/lib.rs` — so `tests/` can call `merge_runs`, `chs`, `grade`, and the size
  parser directly rather than only through the binary.
- Module stubs matching the design's Architecture layout exactly.
- `.github/workflows/ci.yml` — modelled on citare's: `push`/`pull_request` to
  `main`, `CARGO_TERM_COLOR: always`, `dtolnay/rust-toolchain@stable`,
  `actions/cache@v4` keyed on `hashFiles('**/Cargo.toml')`. Four jobs: `test`,
  `clippy -D warnings`, `fmt --check`, and `musl`
  (`--target x86_64-unknown-linux-musl`). Drop citare's `submodules: recursive`
  and wasm job. The musl job earns its ~90 s because the design flags musl's
  `posix_fallocate` divergence; every dependency above is pure Rust, so no
  cross-toolchain is needed.

**Tests.** `--help`/`--version` for every subcommand exit 0 and mention the
documented flags. An exhaustive `match` in `error.rs` asserting every variant's
code, so adding a variant without one fails to compile. Usage errors exit **1**,
not clap's default 2 — `SPEC.md` reserves 2 for "target exists," and this is
easy to forget.

**Done when.** CI green on all four jobs. `cargo install --path .` yields an
`iodd` whose `--help` matches the README. No `unwrap()`/`expect()` outside
tests, enforced via `[lints]`.

---

## Phase 2 — the contiguity engine

Per D6 this is what everything depends on and what is most likely to be wrong.
It ships with a read-only command attached so it can be exercised against the
physical device on day one.

**Delivers.** `extents.rs`, `fsinfo.rs`, and a `verify` implementing D3's two
hard gates and nothing else.

**Files.**

- `src/extents.rs` — port `spike/fiemap.py` structurally; the ioctl and the merge
  stay separate, because that separation is what makes the merge testable.
  - `#[repr(C)] Fiemap` (32 B) and `FiemapExtent` (56 B), with
    `const _: () = assert!(size_of::<Fiemap>() == 32)` and the same for 56 — the
    ioctl number `0xC020660B` encodes the 32 in its size field, so layout drift
    silently changes the request.
  - `read_extents(fd, size)` — `fm_length = size - start` (**the F3 bound**),
    `FIEMAP_FLAG_SYNC`, `fm_extent_count = 1024`, loop until `LAST`, with
    fiemap.py's `nxt <= start || nxt >= size` guard against a spinning driver.
    `ENOTTY`/`EOPNOTSUPP` → exit 8. `mapped == 0` on a non-empty file → 8.
  - `merge_runs(&[Extent]) -> Vec<Run>` — pure. Drop zero-length, sort by
    logical, join only when a record continues the run in **both** dimensions.
  - `untrusted_flags`, `any_unwritten` (the S3/S6 detector), and `map()` which
    bundles them and returns 8 rather than a verdict when the map is untrusted.
- `src/fsinfo.rs` — `NTFS3_MAGIC = 0x7366746e` (F6) and `is_ntfs3` via
  `fstatfs`; `is_compressed` via `FS_IOC_GETFLAGS` testing `FS_COMPR_FL`,
  returning `Ok(None)` on `ENOTTY` since unsupported is not failure;
  `free_space` via `statvfs`; `largest_free_run` returning `None` today with a
  comment citing revision 10, so the diagnostic slot exists.
- `src/cmd/verify.rs` — partial; footer section prints "not checked (phase 3)".

**Tests.** Unit: `merge_runs` over synthetic records — adjacent fold; physical
gap splits; **logical** gap splits even when physical offsets abut (the case a
physical-only merge gets wrong); zero-length dropped; empty input; unsorted
input. The F3 pair as a literal fixture folding to one run, plus a variant with
the slack moved non-adjacent asserting **two**, which is what the bound
prevents. The S2 pair asserting two runs. The S7 five-run layout asserting five.
Layout `const` assertions.

Integration, no sudo (runs on CI's ext4): `read_extents` on a written tempfile
returns runs summing to the file size with none past EOF — catching the
unbounded-query bug on any filesystem. A file with >1024 extents exercises the
resume loop. A zero-length file returns empty without erroring. `verify` on an
allocated file exits 0; on a deliberately sparse one exits 5.

Cross-check: compare `iodd verify --format json` against
`spike/fiemap.py --json` on the same paths, asserting identical `merged_runs`,
`contiguous`, `any_unwritten`. The cheapest possible proof the port is faithful,
and fiemap.py is already known-good against the device.

**Done when.** Rust and fiemap.py agree on the whole physical IODD corpus:
one merged run for `VHDs/Windows_Server_2012R2.rmd` (F3), three for
`_ISO/pop-os_24.04_amd64_nvidia_22.iso` (F5), one for every other measured ISO.
No write occurs — assert mtime and `st_blocks` unchanged. This is hardware
acceptance item A's extent half, achieved before any write path exists.

---

## Phase 3 — pure bytes: `size.rs`, `footer.rs`, full `verify`, `retype`

**Files.**

- `src/size.rs` — `parse` per OPEN-2, `require_512_aligned` (exit 1),
  `warn_unless_mib_aligned` per `SPEC.md` line 79, `humanize`.
- `src/footer.rs` — no I/O, no clock, no RNG inside the pure functions.
  - `chs(total_sectors) -> Chs` implementing the pseudocode **as written**, per
    C20.
  - `checksum(&[u8;512]) -> u32` — zero 64..68, wrapping-sum all 512, return
    `!sum`.
  - `Footer::new(virtual_size, creator, timestamp, uuid)` — timestamp and UUID
    are **parameters**, not ambient reads, so golden vectors are deterministic.
    A thin `Footer::now(..)` at the call site supplies them.
  - `to_bytes` via `to_be_bytes`, no serialization crate (`SPEC.md` line 185).
  - `parse` returning a **problem list**, not a single error, because D2 needs
    every problem reportable as a warning rather than the first aborting.
  - Timestamp epoch 2000-01-01Z = unix 946684800; clamp below to 0.
- `src/cmd/verify.rs` — complete, with C3's exit policy.
- `src/cmd/retype.rs` — validate extension, refuse if target exists (exit 2),
  `rename(2)` in the same directory. No bytes read or written.
- `tests/fixtures/chs_vectors.json` — harvested vectors with the `qemu-img`
  version and exact commands recorded.

**Tests.** CHS across every branch and both escalation disjuncts, using the C20
test formulation (`chs(footer.original_size / 512)` vs the footer's CHS bytes).
The cap branch asserted against the algorithm with a comment on why qemu cannot
supply it. Checksum against F7's already-measured `ffffe586` and `ffffe5c7` —
free regression vectors. Invariant-field comparison naming the excluded fields
in a comment with the F7 citation, or someone will "fix" it later. Round-trip
over a size table. Each `FooterProblem` from a hand-corrupted buffer. Size
parsing across a wide table. `retype` preserving content hash and inode,
refusing an existing target (2), rejecting `.iso` (1). `verify` on a flipped
checksum byte: 0 with a warning by default, 7 under `--strict` — the C3 test,
commented as such.

**Done when.** Every CHS branch has a measured vector, and `iodd verify` on the
device's `.rmd` reports mountable with a footer warning ("no `conectix` cookie;
last sector is a GPT backup header") and exits 0 — D2 proven against the one
file known to mount.

---

## Phase 4 — `alloc.rs` and `create`: the write path

**Files.**

- `src/alloc.rs`.
  - `TempTarget` with a `Drop` that unlinks unless the build succeeded or
    `keep_on_fail`. Name `.iodd-<pid>-<random>.tmp` in the **same directory**
    (rename must not cross filesystems), `O_CREAT|O_EXCL|O_RDWR`, mode 0600. The
    dot prefix and `.tmp` suffix keep it out of `doctor`'s scan set.
  - `allocate` — raw `fallocate(2)` mode 0, **not** `posix_fallocate`, per the
    musl note. `ENOSPC`/`EDQUOT`/`EFBIG` → 3; `EOPNOTSUPP`/`ENOSYS` → warning,
    not abort (C19).
  - `early_extent_check` — step 2, `runs.len() != 1` → exit 4 **before any data
    is written**. Skipped when `fallocate` was unsupported.
  - `zero_region` — 4 MiB buffer, `write_all_at` loop, `[0, total_size)` for
    create (see Part 2). At S6's ~500 MB/s a 20 GiB create is ~40 s, so emit
    throttled progress to stderr when it is a tty. Follow with `fdatasync`.
  - `verify_written` — step 4. `runs.len() != 1` → 4; `any_unwritten` → 5;
    `st_blocks * 512 < total_size` → 5.
  - `write_footer_in_place` — `write_all_at` at `virtual_size`. Never `set_len`,
    never append.
  - `finalize` — re-verify footer and run count, post-creation compression check
    (exit 6, C12), `fsync`, `rename(2)`, `fsync` the directory. Without
    `--force`, prefer `renameat2(RENAME_NOREPLACE)`, falling back on
    `EINVAL`/`ENOSYS` since ntfs3 may not support it.
- `src/cmd/create.rs` — orchestration.

**Tests.** Unit: `TempTarget` naming not matched by `doctor`'s real matcher; the
`Drop` guard on both paths and under `keep_on_fail`; errno mapping table.

Integration, feature-gated `ntfs-integration`, requires sudo. Reuse the loopback
harness from `spike/fallocate-zeroing.sh` — including the no-partition-table
trick that makes FIEMAP physical offsets index straight into the backing image.
Skip with a printed message unless `IODD_TEST_SUDO=1`.

- `create --size 64M` on fresh NTFS: one run, non-sparse, `any_unwritten` false,
  size exactly `virtual_size + 512`, footer parses and checksums.
- **The S1 regression, the most important test in the suite.** Fill with a
  pattern, delete, `create` over the freed space, unmount, read the *backing
  image* at the reported physical offsets, assert zeros not the pattern. This
  proves the zero pass reaches the device rather than being synthesized by the
  filesystem — the exact failure the spike found and the reason D5 exists.
- **The refusal path.** S7's fragmented scenario: exit 4, extent map and
  free-space diagnostics printed, **no target file afterward**, temp gone.
- `--keep-on-fail`: exit 4, temp survives at the printed path, target still
  absent (OPEN-4).
- `--force` over an existing good file where allocation then fails: the original
  is byte-identical afterward. C9's whole reason for existing.
- Rename preserves extents: capture the map before and after, assert identical
  physical offsets. Revision 3 asserts this; assert it rather than trust it.
- `.vhd` and `.rmd` byte-identical for the same size and injected
  timestamp/UUID.
- Compressed target → exit 6.

**Done when.** All pass on the loopback, and hardware acceptance item B step 2
succeeds on the device — `iodd verify` agrees with `filefrag` and `stat` on a
file the tool created on the device, in a scratch folder, under the safety
rules.

---

## Phase 5 — `doctor`

**Files.** `src/cmd/doctor.rs` with `Policy`, `FileFacts`, pure `grade`, and
`Verdict` including `WillNotMountMaybeUnlisted` (`SPEC.md` line 148 requires the
distinction so an operator understands why a copied file is missing from the
menu).

Rulesets. `.vhd`/`.rmd`/`.vmdk`: the two hard gates decide mountable vs not;
every footer finding is a `WARN` (C2/D2), **including a missing footer entirely**
— the design's fixture list requires a footerless raw file under `.rmd` to grade
mountable with a warning, because that is what the one known-mounting file looks
like. Container misdetection (VHDX magic, dynamic/differencing VHD, genuine VMDK
under `.vmdk`) is reported with its specific reason and gets
`WillNotMountMaybeUnlisted` **only** if a hard gate also fails. `.iso`: run count
`<= iso_max_fragments` (default 24); no footer or allocation check. `.ima`:
report the count, `WARN` on fragmentation, reason stating the threshold is
undocumented; scanned set is `.ima` only (C15).

Traversal via `walkdir`, recursive by default (C14/OPEN-1), symlinks not
followed, every file `O_RDONLY | O_NOFOLLOW`, reading only the first 4096 bytes,
the last 512, `fstat`, and the extent map. No `O_RDWR` anywhere in the module.

Aggregate exit: 0 when all mountable; **4** when any will-not-mount; under
`--strict`, **4** when any warn (revision 11 — reuse the code, do not invent
one).

`src/report.rs` — `Record` with `Serialize`, hand-rolled table widths (no table
crate, per `SPEC.md` line 185), summary by verdict, `--format json` emitting the
same records.

`tests/fixtures/doctor/` — small committed byte fixtures: dynamic VHD footer,
VHDX header, real VMDK descriptor, clean fixed VHD, footerless raw under `.rmd`.

**Tests.** Unit, the bulk of coverage thanks to pure `grade`: one test per
verdict class per type as struct literals, including the D2 footerless-`.rmd`
fixture the design names explicitly, ISO at 3 runs vs 25 vs 25 with
`--iso-max-fragments 32`, `.ima` at 4 runs → warn. Aggregate codes over
synthetic vectors in both modes. JSON field names and ordering against a
committed snapshot, since that is the scripting interface.

Integration: a fixture directory run with `--format json`, asserting the record
set and exit code, with mtime, atime, and `st_blocks` captured before and after
and asserted unchanged — both the design and `SPEC.md` line 199 require proof of
no writes. `--type` restriction, `--no-recursive`, and default-recursive
behaviour.

**Done when.** `doctor` against the real corpus produces exactly the design's
hardware-acceptance table A grades, aggregate exit 0, mtimes and `st_blocks`
unchanged. That corpus becomes the standing regression fixture.

---

## Phase 6 — `convert`

**Files.** `src/cmd/convert.rs`. Source probing per OPEN-5; non-raw with no
`qemu-img` exits 9 before allocating. `--size < source_virtual_size` is exit 1
(C11) — do **not** implement `max()`. Non-raw virtual size from
`qemu-img info --output=json`. Allocation reuses `alloc.rs` unchanged. Zeroing
narrowed to `[source_virtual_size, total_size)` per Part 2. Raw population:
4 MiB chunks with `write_all_at`; **no `copy_file_range`**, which can share
extents on some filesystems — the opposite of what this tool wants. Non-raw:
`qemu-img convert -n -S 0 -O raw SOURCE TEMP`, with `-S 0` mandatory (C8);
capture stderr and surface it in the exit-9 message.

**Tests.** Raw source smaller than `--size` (tail all zeros, source region
byte-identical); exactly equal; `--size` below source → 1; `qemu-img` absent →
9; magic sniffing across qcow2, dynamic VHD, VHDX.

Feature-gated: qcow2 → contiguous fixed VHD, one run, non-sparse,
`any_unwritten` false, guest-visible bytes matching `qemu-img convert -O raw`.
**The `-S 0` regression** — convert a source with a large interior zero run
without `-S 0` and assert the result is sparse, then with it and assert not.
Pins the behaviour revision 2 exists to enforce.

---

## Phase 7 — hardware acceptance, then packaging

Deliberately last, gated on physical hardware, not a CI concern. Under the
design's safety rules: read-only tests before any read-write remount, writes to
a scratch folder, never `--force` against a pre-existing device file, sizes well
below the ~33 GiB free, read-only restored afterward.

1. **Item B.** Create on the device; confirm one run and non-sparse; unmount;
   mount through the IODD; assert the presented block device size equals the
   **file** size, re-confirming F1 against a file we generated; partition,
   format, write and read back through the device; re-attach and re-run
   `filefrag` to confirm guest writes did not fragment the backing file; record
   whether the footer survived.
2. **Item C, issue #1.** Two files, one footer'd and one footer-zeroed, copied
   without booting either — which does the device list and mount? Settles
   whether the footer gates listing using only this tool and this hardware, no
   Windows needed, contrary to how issue #1 is written. **If the footer does
   gate listing, D2 needs revisiting and `doctor`'s verdict mapping changes.**
   That is why this runs before packaging.
3. **Item D.** The refusal path on the genuinely 87%-full volume.
4. **Item E, optional.** Mount a generated `.vhd` in Windows Disk Management to
   confirm D4's CHS choice satisfies the only consumer that reads the field.
5. **Packaging.** `dist init` for musl static binaries on x86_64 and aarch64 via
   `cargo-zigbuild`. Per `SPEC.md` line 195, do not adopt the generated
   `release.yml` wholesale — reconcile tag scheme, artifact naming, and
   toolchain pinning against existing repo conventions. No existing repo uses
   cargo-dist, so this establishes a convention rather than conforming to one,
   which is a reason to do it last.
6. **Optional.** The `filefrag -k` fallback (C7), if a real need appears.

**Done when.** The device mounts a file this tool created, at the exact size
intended, and refuses nothing it should accept. Issue #1 closed with evidence.

---

## Risk register

| Risk | Where it bites | Mitigation |
|---|---|---|
| `UNWRITTEN` never clears (unbounded query, or ntfs3 flags something unexpected) | Phase 4 — every create fails | Bound `fm_length` (Phase 2, tested); zero the full `total_size`; if it still fires, S6 is the reference and the discrepancy is a finding worth writing down |
| `rename(2)` on ntfs3 relocates the file | Phase 4 — the published file is fragmented although the temp was not | Explicit before/after extent-map assertion |
| Zero pass too slow at 100+ GiB | Phase 4 — usability | ~3.5 min at S6's measured rate; progress output; the early check means the failure path never pays it |
| C3's `--strict` inversion surprises readers of `SPEC.md` | Phase 3 — support burden | Land the change with README and `--help` together; state the D2/D3 reasoning in the help text |
| D2 is wrong and the footer *does* gate listing | Phase 7 item C — `doctor` grades a broken file mountable | The experiment is cheap and needs no Windows; run it before packaging so a verdict change is pre-1.0 |
| Integration tests need sudo and so never run | Phases 4–6 — the write path is only unit-tested | Feature gate plus a loud skip; one documented command for the harness; run it in the pre-merge checklist for every write-path change |
