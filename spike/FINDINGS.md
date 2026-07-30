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

The volume had just been emptied, so this is not free-space fragmentation in
the ordinary sense. The likely cause is the NTFS MFT zone, which the allocator
reserves and prefers to route around, but that is a hypothesis rather than a
measurement.

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
