# iodd

A Linux command-line tool for creating, converting, and verifying fixed-size
virtual disk files for IODD hardware devices (Mini, ST400, 2531/2541, LK100).

**Status: pre-implementation.** The format model has been validated against a
physical device and the design is settled. No code yet.

## Why

IODD devices map a virtual disk file's on-disk sector extents once at mount
time and stream sectors straight from the underlying filesystem. Two properties
are mandatory:

- the file must be **fully allocated**, not sparse
- the file must occupy a **single contiguous extent**

The official tooling (VHD Tool++) is Windows-only. The obvious Linux
substitute, `qemu-img create -f vpc -o subformat=fixed`, fails both properties:
it truncates any pre-allocated target and writes the data region sparse. On a
100 MiB image it leaves 16 blocks allocated out of 104858112 bytes.

This tool fully allocates and zeroes the file, verifies that it landed in a
single extent, and refuses to hand you one that did not. No Windows
round-trip.

**What it cannot do:** make a fragmented file contiguous. Measurement showed
ntfs3's allocator places blocks identically whether you `fallocate` or write
sequentially, so there is no userspace lever on placement. Where a volume's
free space allows a single run you get one; where it does not, no tool running
on Linux can change that, and this one tells you so instead of emitting a file
the device will silently refuse to mount. Consolidating free space remains a
Windows-side job.

## Planned commands

```
iodd create  --size SIZE   --out PATH [--removable] [--force] [--creator TAG] [--keep-on-fail]
iodd convert --source PATH --out PATH [--removable] [--force] [--size SIZE] [--keep-on-fail]
iodd verify  PATH [--strict]
iodd doctor  ROOT [--recursive] [--type LIST] [--format table|json] [--strict] [--iso-max-fragments N]
iodd retype  PATH --to {fixed|removable}
```

`doctor` is the "will everything on this device mount?" audit. It is strictly
read-only: files are opened `O_RDONLY`, and only the footer, extent map, and
`stat` are read.

## What the device actually does

Some of this contradicts what the format documentation implies, so it is worth
stating plainly. Measured against a physical device:

- **The device exposes the entire file as the virtual disk.** A 42949673472-byte
  `.rmd` presents as a 42949673472-byte disk. The 512-byte VHD footer is inside
  the guest-visible region, not excluded from it, so a guest that partitions
  with GPT overwrites the footer with its backup header.
- **A valid VHD footer is not required to mount.** The one file observed
  mounted through the device has a GPT backup header where its footer should
  be, and mounts anyway. Whether the footer matters before a guest overwrites
  it is untested.
- **Fragmentation and sparseness are the only hard gates**, which is why they
  are the only things this tool refuses to produce.

Details, including the measurements behind each claim, are in
[the design document](docs/superpowers/specs/2026-07-29-iodd-tool-design.md).
The original format specification is in [SPEC.md](SPEC.md).

## Platform

Linux only. Relies on `fallocate(2)`, `FS_IOC_FIEMAP`, and `fstat(2)`, none of
which have a portable equivalent. The target filesystem is assumed to be
`ntfs3`.

`qemu-img` is an optional runtime dependency, used only by `convert` on
non-raw sources.

## License

[PolyForm Shield 1.0.0](LICENSE.md)
