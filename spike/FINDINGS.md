# Phase 0 spike findings

Run 2026-07-29 on Linux 6.8.0-124-generic, `ntfs3`, 4 GiB loop-backed volume,
1 GiB `fallocate` target.

**Scope caveat.** Everything below measures the behaviour of the in-kernel
`ntfs3` driver, which is what this tool calls. The IODD device is not ntfs3; it
has its own NTFS reader in firmware. Where a conclusion depends on device
behaviour rather than driver behaviour it is marked as inference and deferred
to the hardware acceptance phase.

## S1. `fallocate(2)` on ntfs3 does not zero. Confirmed.

The decisive result. After filling the volume with a repeating pattern,
deleting it, and `fallocate`-ing a fresh 1 GiB file over the freed space:

```
read THROUGH the filesystem:
   first 1 MiB: all zeros = True
   pattern present via filesystem = False

read the BACKING IMAGE at the physical offsets FIEMAP reported:
   start @ 1867776          1048576 bytes -> PATTERN
     mid @ 269375488        1048576 bytes -> PATTERN
     end @ 535834624        1048576 bytes -> PATTERN
   start @ 2168954880       1048576 bytes -> PATTERN
     mid @ 2438318080       1048576 bytes -> PATTERN
     end @ 2706632704       1048576 bytes -> PATTERN

   samples with pattern: 6   zeros: 0   other: 0
```

Six of six samples held the old pattern. The filesystem synthesizes zeros
because the range sits past the valid data length; the bytes on the device are
untouched.

**Inference, to be confirmed on hardware:** since the device maps extents and
streams sectors without consulting the filesystem, a guest reading a
`fallocate`d-but-unwritten VHD would see these stale bytes rather than zeros.
That is both a correctness bug and a disclosure leak. Confirming it is a task
for the hardware acceptance phase.

**Resolves D5: zeroing is mandatory, not optional.** The earlier framing
offered zeroing as opt-in. That would be wrong by default.

## S2. `fallocate(2)` on ntfs3 does not produce a contiguous file

Unplanned, and more consequential than S1 for the tool's core contract.

A 1 GiB `fallocate` on a **freshly emptied 4 GiB volume** came back in two
runs:

```
raw records   2
merged runs   2   contiguous=False
  rec 0: logical=0         physical=1867776     length=535015424  flags=unwritten,merged
  rec 1: logical=535015424 physical=2168954880  length=538726400  flags=last,unwritten,merged
```

The runs are ~510 MiB and ~514 MiB, separated by a physical gap of
1632071680 bytes (~1.5 GiB) between offset 536883200 and 2168954880.

**Revised by S5 below.** This is not a property of `fallocate`. It is a
property of the volume's allocation history, and the cause is now measured
rather than hypothesized.

This matters because zero fragmentation is the tool's entire reason to exist.
If a single `fallocate` call cannot deliver one extent on an *empty* volume,
then "call `posix_fallocate` and verify" — the approach in `SPEC.md` — will
refuse to create files most of the time.

**The engine needs an allocation strategy, not a single syscall.** Follow-up
experiment: `spike/allocation-strategy.sh`.

### The promising hypothesis

S1 says we must write zeros across the whole data region anyway. So the
sequential zero-write may as well *be* the allocation strategy, with
`fallocate` skipped entirely. If a plain sequential write yields one extent
where `fallocate` yields two, then one pass both allocates contiguously and
zeroes, and S1 stops being a cost.

That is what the follow-up spike measures.

## S3. Fresh allocations are flagged `UNWRITTEN`. Confirmed.

`any unwritten: True` on the `fallocate`d file, on both runs.

This answers the F6 question left open in the design. It gives a cheap,
direct detector for "this region will hand the guest undefined data" that
costs one ioctl rather than reading the whole file:

- after allocating, any `FIEMAP_EXTENT_UNWRITTEN` extent means the region is
  uninitialized
- after zeroing, the flag should clear, which makes it a post-write
  verification rather than a matter of trust

Whether writing zeros actually clears the flag on ntfs3 is untested and is
included in the follow-up spike.

## S5. The S2 split was the MFT zone, and it depends on volume history

Second run, three strategies on **freshly formatted** volumes:

```
   fallocate      CONTIGUOUS   unwritten=True  sparse=False  0s
        run 0: physical=2168954880  length=1073741824
   seqwrite       CONTIGUOUS   unwritten=False sparse=False  2s
        run 0: physical=2168954880  length=1073741824
   fallocate+w    CONTIGUOUS   unwritten=False sparse=False  2s
        run 0: physical=2168954880  length=1073741824
```

All three produce one run, at the same physical offset. So `fallocate` is not
inherently incapable of contiguous allocation, and S2 as originally written
was misleading.

`ntfsinfo` explains the difference:

```
Cluster Size: 4096
Volume Size in Clusters: 1048575
MFT Zone Start: 0
MFT Zone End: 131075
```

131075 clusters × 4096 = **536,883,200**. The S2 run 0 was physical 1867776
with length 535015424, ending at 1867776 + 535015424 = **536,883,200**. It
ended exactly on the MFT zone boundary and the remainder jumped to
2168954880, which is where all three fresh-volume allocations begin.

So the split happened because the allocation *started inside the MFT zone*
and had to leave it at the boundary. On a pristine volume the allocator skips
the zone and finds one run past it.

The variable is volume history: S2's volume had been filled to ENOSPC and
emptied, which pushes NTFS into using the MFT zone. A pristine volume behaves
differently.

**This makes the S5 comparison unrepresentative.** The strategies were
compared only on pristine volumes, while the interesting case — a volume with
history — was measured only with `fallocate`. A real IODD device is neither
pristine nor empty; the test bed measured here is 87% used with years of
history.

Follow-up: `spike/allocation-dirty-volume.sh` compares all three strategies on
a filled-then-emptied volume and on one with deliberately fragmented free
space. Until that runs, no strategy has been shown better than any other under
realistic conditions.

## S6. Writing zeros clears `FIEMAP_EXTENT_UNWRITTEN`. Confirmed.

From the same run:

- `fallocate` alone: `unwritten=True`
- sequential write: `unwritten=False`
- `fallocate` then overwrite: `unwritten=False`

So the flag is a sound post-write verification. After zeroing, any remaining
`UNWRITTEN` extent means the zeroing did not take, and that is checkable with
one ioctl rather than by reading the file back.

Throughput was roughly 500 MB/s for the 1 GiB writes, putting a 20 GiB create
around 40 seconds. Acceptable for a mandatory zeroing pass.

## S7. Allocation strategy is irrelevant. Resolves D7.

Third run, all three strategies against two realistic scenarios.

**dirty** (filled to ENOSPC, then emptied):

```
   fallocate       2 RUNS   unwritten=True   0s
   seqwrite        2 RUNS   unwritten=False  1s
   fallocate_write 2 RUNS   unwritten=False  1s

   every one of them:
        run 0: physical=5382144     length=531501056
        run 1: physical=2168954880  length=542240768
```

**fragmented** (16 files written, every other one deleted):

```
   fallocate       5 RUNS   unwritten=True   0s
   seqwrite        5 RUNS   unwritten=False  1s
   fallocate_write 5 RUNS   unwritten=False  1s

   every one of them:
        run 0: physical=190513152   length=78528512
        run 1: physical=694775808   length=253706240
        run 2: physical=1202188288  length=253706240
        run 3: physical=1709600768  length=253706240
        run 4: physical=2422661120  length=234094592
```

The layouts are not merely the same length. They are **byte-identical**: same
physical offsets, same run lengths, across all three strategies in both
scenarios.

**Conclusion: there is no allocation strategy to choose.** ntfs3's allocator
places blocks identically whether the request arrives as `fallocate(2)` or as
sequential writes. Userspace cannot steer it.

This resolves D7 in the negative and settles the tool's honest claim: it
cannot *produce* contiguity, it can only *verify* it and refuse. Where a
volume's free space permits a single run, any approach gets one; where it does
not, no approach does.

Measured on kernel 6.8.0-124, ntfs3, one 4 GiB volume geometry, two scenarios.

### The consequence for the engine

Since layout is strategy-independent, the choice falls to other criteria, and
`fallocate` earns its place as a **cheap early abort** rather than as an
allocation technique:

1. `fallocate(2)` the full size. Instant (0s measured), fails fast on ENOSPC.
2. FIEMAP, merged. **If more than one run, abort here** — before writing
   anything. On a 20 GiB target this saves the entire ~40 second zeroing pass
   on the failure path.
3. One run: write zeros across the data region. Mandatory per S1.
4. FIEMAP again: still one run, and `UNWRITTEN` cleared per S6.
5. Write the footer in place at `virtual_size`.
6. Final verify.

Step 3 not relocating the file is confirmed by the `fallocate_write` rows
above, which match `fallocate` alone exactly.

The fragmented scenario also validates the refusal path: 2.1 GiB free was
enough for the 1 GiB target, so nothing hit ENOSPC, but the free space came in
~242 MiB chunks and every strategy produced 5 runs. The tool must refuse there
rather than emit a file the device will not mount.

## S8. Survey: is there any Linux-native NTFS defragmenter?

D8 says the tool verifies contiguity rather than producing it. That rests on
an assumption worth checking: that no Linux tool can relocate NTFS clusters.

### Does not exist

- **`ntfsdefrag`.** Search results claim it ships with ntfsprogs. It does not.
  It is absent from ntfs-3g, from this system, and from the upstream
  `ntfsprogs` man page.
- **A kernel move-extent ioctl for NTFS.** ext4 has `EXT4_IOC_MOVE_EXT`, which
  is what `e4defrag` drives; XFS and btrfs have their own. ntfs3 has no
  equivalent. Linux 6.19 adds `NTFS3_IOC_SHUTDOWN`, nothing for relocation. So
  an `e4defrag` equivalent is not merely missing, it is not currently
  implementable without kernel work.

### Exists, but dubious

- **`ntfsmove`**, in ntfs-3g. Copyright 2003, Richard Russon, still shipping
  as of 2022.10.3. It has exactly the right primitives: `-B` best place, `-C`
  specific cluster, `-S`/`-E` start and end, `-n` dry run.

  Against it: it takes a **device**, so the volume must be unmounted; it marks
  the volume dirty unless `-D`, meaning Windows `chkdsk` afterwards; it has no
  man page in this distribution; and the upstream `ntfsprogs` documentation
  does not list it among the suite's tools. That combination suggests it was
  never finished.

  **Untested.** `spike/ntfsmove-defrag.sh` builds a fragmented file on a
  scratch volume and measures whether `ntfsmove -B` consolidates it, destroys
  it, or does nothing.

- **UltraDefrag4Linux**, a port of the Windows tool. 13 commits; the
  maintainer states "At this moment I am unable to render support to this
  project."

### Irrelevant, given S7

Rewrite-and-hope defragmenters such as `shake` work by copying a file and
letting the allocator do better on the second attempt. S7 measured that ntfs3
places blocks identically however the request arrives, so a userspace
rewriter cannot help here by construction. We already ran that experiment
without meaning to.

### Community consensus

Copy the data off, reformat, copy it back. Or use Windows.

### Aside

There is an active LKML effort ("ntfsplus", posted as an "ntfs filesystem
remake") to replace ntfs3, on the stated grounds that ntfs3 "has many problems
and is poorly maintained." Worth watching; changes nothing today.

### Bearing on the design

Unless `ntfsmove` surprises us, D8 stands and the survey strengthens rather
than weakens it: the tool cannot defragment because on Linux nothing can.
Even if `ntfsmove` works, the offline-volume and dirty-volume requirements
make it unsuitable as a default code path, and at most an opt-in with loud
warnings.

## S9. Open-source NTFS stacks that work on a raw device

S8 asked whether an existing tool can defragment. This asks the harder
question behind it: if the kernel driver cannot be steered, can we bypass it
and do the allocation ourselves against the raw device? That is essentially
what VHD Tool++ does on Windows.

### The candidates

| Stack | Write? | License | Verdict |
|---|---|---|---|
| `libntfs-3g` | yes | **GPL-2.0-or-later** | the only real option |
| `ntfs` crate (ColinFinck) | **no**, read-only | MIT OR Apache-2.0 | useful for the read side only |
| Redox OS | n/a | MIT | no NTFS driver at all; RedoxFS only |
| ReactOS NTFS | partial | GPL-2.0 | kernel-mode C, not usable as a library |
| `ntfsplus` (LKML) | yes | GPL-2.0 | in-development kernel driver, not a library |

### libntfs-3g has exactly the right primitive

```c
typedef enum {
    MFT_ZONE  = 0,   /* Allocate from $MFT zone. */
    DATA_ZONE = 1,   /* Allocate from $DATA zone. */
} NTFS_CLUSTER_ALLOCATION_ZONES;

extern runlist *ntfs_cluster_alloc(ntfs_volume *vol, VCN start_vcn, s64 count,
        LCN start_lcn, const NTFS_CLUSTER_ALLOCATION_ZONES zone);
```

`start_lcn` is a placement hint and `zone` selects the allocation zone. So
"allocate N clusters starting at logical cluster X" is directly expressible.
Reading `$Bitmap` through the same library would also solve the largest
contiguous free run problem that the design currently lists as unobtainable.

### The blocker: license

The ntfs-3g README is explicit:

> All the NTFS related components: the file system drivers, the ntfsprogs
> utilities and the shared library libntfs-3g are distributed under the terms
> of the GNU General Public License [...] either version 2 of the License, or
> (at your option) any later version.
>
> The fuse-lite library is distributed under the terms of the GNU LGPLv2.

`COPYING.LIB` exists in the tree but covers **fuse-lite only**, not the
library. Every file checked in `libntfs-3g/` (`lcnalloc.c`, `attrib.c`,
`volume.c`, `bitmap.c`, `mft.c`) carries a GPL header.

Linking libntfs-3g makes the combined work GPL-2.0-or-later. PolyForm Shield
1.0.0 imposes a noncompete restriction, which GPL section 6 forbids adding.
**The two cannot be combined in one binary.**

### Ways around it

1. **Shell out to `ntfsmove`** as a separate process, exactly the pattern the
   spec already uses for `qemu-img`. Separate programs exchanging data over a
   CLI boundary do not form a combined work, so no license conflict arises.
   Cleanest option by far, and it costs nothing to try since S8's test script
   is already written.
2. **Relicense to GPL-2.0-or-later** and link the library directly. Buys the
   full API, costs the PolyForm noncompete.
3. **Roll our own, narrowly.** Note that *relocating an existing file* is a
   far smaller problem than *creating* one: it avoids directory B-tree
   insertion entirely, touching only the `$DATA` runlist and `$Bitmap`. The
   read side could use the MIT/Apache `ntfs` crate, leaving only the write
   path bespoke. Still writes NTFS metadata by hand, with the corruption risk
   that implies on a volume holding real data.
4. **Do none of it** and ship verify-and-refuse per D8.

### Licensing researched separately

The GPL question raised here is analysed in
[`docs/licensing-libntfs-3g.md`](../docs/licensing-libntfs-3g.md). Headline:
copyright in libntfs-3g is **not** held by Tuxera as assumed here — it is
distributed across at least six individuals — and while the derivative-work
question is genuinely unsettled, PolyForm Shield is the worst license from
which to test it, because no FOSS-exception path exists for a non-OSI license
and GPLv2 §6 makes the incompatibility incurable.

### Bearing on the design

The `ntfsmove` test in S8 is now the pivot rather than a curiosity. If it
consolidates a fragmented file, option 1 becomes available and the tool can
plausibly offer opt-in defragmentation with no license entanglement. If it
fails, options 2 and 3 are the only paths to *producing* contiguity, and both
are large enough to be their own project.

## S10. `ntfsmove` cannot defragment, and misreports success

Run 2026-07-29. 512 MiB target on a volume with fragmented free space, three
runs before the attempt.

### It reported success while applying one move of three

Cluster size 4096, so converting its own log to physical offsets:

| Run | Before (LCN) | Claimed move | After (LCN) | Applied? |
|---|---|---|---|---|
| 0 | 46512 | → 292 | 292 | yes |
| 1 | 169623 | → 417383 | 169623 | **no** |
| 2 | 293503 | → 169623 | 293503 | **no** |

`ntfsmove` printed `success` and exited 0. The extent map says otherwise.

Its plan also had run 2 targeting LCN 169623, which is where run 1 was still
sitting. The move only avoided overlapping live data because it did not
apply. Separately, run 0 landed at LCN 292, inside the MFT zone
(clusters 0–131075) that NTFS reserves for MFT growth.

### The architectural reason it was never going to work

From the source: `move_file` iterates attributes, `move_attribute` iterates
runs, `move_datarun` moves a single run, and `find_unused` is called **per
run**. There is no whole-file consolidation step.

So even had all three moves applied, the targets (292, 417383, 169623) are not
adjacent to one another. The file would have remained in three runs at
different locations. `ntfsmove` relocates runs; it does not merge them. The
name is accurate.

### The dry run is not a dry run

`-n` produced `!write -1 / cannot data_copy / failed` and exit 1, because
suppressing writes makes the data copy fail. It cannot be used to preview an
operation.

### Flaw in this test

The target file was filled with zeros, so the integrity check ("first 1 MiB
all zeros: True") was vacuous: a corrupted file of zeros is indistinguishable
from an intact one. The script now writes a verifiable pattern instead. This
did not affect the conclusion, which rests on the extent map, but the check as
originally written would not have caught corruption.

### Consequence

**Option 1 from S9 is dead.** Shelling out to `ntfsmove` cannot produce
contiguity, so it offers no route to defragmentation and no way to preserve
PolyForm Shield while gaining one. D8 stands: the tool verifies contiguity and
refuses, it does not produce it.

One useful positive: `bitmap_alloc`, `data_copy` and the runlist rewrite
demonstrably **worked** for run 0. The libntfs-3g primitives function. What is
missing in `ntfsmove` is the consolidation policy, not the machinery. So S9
option 2 (link libntfs-3g under GPL and write the consolidation ourselves)
remains technically viable, and is now better understood: we would be supplying
the planner, not the mechanism.

## S11. ntfs3 does not hold the dirty flag during a read-write mount. Confirmed, but the control failed.

The question was whether `doctor` can report a dirty volume it is currently
scanning, or whether that would fire on every healthy read-write mount.

`spike/ntfs3-dirty-while-mounted.sh`, against a loopback NTFS volume:

```
1. unmounted baseline            flags 0x0000
2. mounted rw with ntfs3         flags 0x0000
3. after writing a file + sync   flags 0x0000
4. after clean unmount           flags 0x0000
5. ntfs-3g (FUSE), mounted rw    flags 0x0000
   after clean unmount           flags 0x0000
```

So ntfs3 does not set the on-disk flag for the duration of a mount. The
mounted-path check in `doctor` will not cry wolf, which is what was being
asked.

**But step 5 was meant to be a positive control and was not one.** ntfs-3g was
expected to show dirty while mounted — that premise was simply wrong. Nothing
in this run demonstrates the reading would have detected the flag had it been
set by a driver, as opposed to set by our own byte patch in
`tests/volume_real.rs`. A clean reading from an instrument not shown to detect
the signal is weaker evidence than it appears.

### And the flag may never reach the disk in the failure we care about

Re-reading the kernel message captured during hardware acceptance:

```
ntfs3: sdb1: ino=3, ntfs_set_state failed, -5.
ntfs3: sdb1: Mark volume as dirty due to NTFS errors
```

`ino=3` is MFT record 3 — `$Volume`, the record holding the flag.
`ntfs_set_state failed, -5` is `EIO`: ntfs3 *tried to write the dirty flag and
could not*, because the device had already left the bus. The second line
records the in-memory state, not a successful write.

That is the difference between "this volume is marked dirty" and "the driver
wanted to mark it and the bus was already gone". Whether the on-disk flag ends
up set in the IODD's mode-switch scenario is **open**, and needs the hardware:
get the device into the state where it refuses to mount, then read it with
`sudo iodd doctor /dev/sdX1`. If that reports `0x0000`, the refusal has a
different cause and the diagnosis needs rethinking.

`spike/ntfs3-dirty-set-on-error.sh` supplies the missing positive control by
reproducing the disconnect with a dm-error target: a device that fails I/O and
then returns, which is what the IODD does when it switches modes.

## S4. Incidental confirmations

- `fallocate(2)` mode 0 succeeds on ntfs3. It does not return `EOPNOTSUPP`, so
  the musl-versus-glibc `posix_fallocate` fallback question is moot for this
  driver, though the tool should still handle the error path.
- `st_blocks * 512` equals the file size exactly (2097152 blocks, 1073741824
  bytes). The sparseness check works as specified.
- Filling the volume stopped at 4270850048 bytes on a 4 GiB volume, so
  roughly 25 MiB of overhead. Consistent with a quick-formatted NTFS.

## Raw output

The full run is reproducible with `./spike/fallocate-zeroing.sh`. Tunables:
`IMG_SIZE`, `ALLOC_SIZE`, `PATTERN`, `KEEP=1` to preserve artifacts.
