#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""FS_IOC_FIEMAP reader and extent-run merger.

Prototype for the Rust `extents` module. Two jobs, kept separate on purpose:

  read_extents()  issues the ioctl and returns raw records
  merge_runs()    folds records into physically contiguous runs, pure function

Two things this prototype established against a real IODD device:

  * fm_length MUST be bounded to the file size. Query past EOF, as filefrag
    does, and ntfs3 returns an extra `unwritten` record covering the slack in
    the final cluster (3584 bytes of a 4096-byte cluster, for a file ending
    512 bytes into it). That record is cluster tail, not file data, and
    counting it would inflate the extent count.

  * Merging adjacent records is defensive, not mandatory for the observed
    case. Bounded correctly, a contiguous file returns a single record. But
    NTFS run lists can split a physically contiguous allocation, so adjacent
    records must still fold into one run.

Usage:
    ./fiemap.py PATH [PATH ...]
    ./fiemap.py --json PATH
"""

from __future__ import annotations

import array
import fcntl
import json
import os
import sys

# _IOWR('f', 11, struct fiemap), where sizeof(struct fiemap) == 32
FS_IOC_FIEMAP = 0xC020660B

FIEMAP_HEADER_SIZE = 32
FIEMAP_EXTENT_SIZE = 56

FIEMAP_FLAG_SYNC = 0x0001

EXTENT_FLAGS = {
    0x0001: "last",
    0x0002: "unknown",
    0x0004: "delalloc",
    0x0008: "encoded",
    0x0080: "data_encrypted",
    0x0100: "not_aligned",
    0x0200: "inline",
    0x0400: "tail",
    0x0800: "unwritten",
    0x1000: "merged",
    0x2000: "shared",
}

# Flags that mean "this extent map cannot be trusted for contiguity".
# The tool exits 8 rather than silently passing when any of these appear.
UNTRUSTED = 0x0002 | 0x0004 | 0x0008 | 0x0200


class Extent:
    __slots__ = ("logical", "physical", "length", "flags")

    def __init__(self, logical: int, physical: int, length: int, flags: int):
        self.logical = logical
        self.physical = physical
        self.length = length
        self.flags = flags

    @property
    def flag_names(self) -> list[str]:
        return [name for bit, name in EXTENT_FLAGS.items() if self.flags & bit]

    def as_dict(self) -> dict:
        return {
            "logical": self.logical,
            "physical": self.physical,
            "length": self.length,
            "flags": self.flag_names,
        }


def read_extents(path: str, count: int = 1024) -> list[Extent]:
    """Issue FS_IOC_FIEMAP until the LAST flag is seen. Returns raw records."""
    out: list[Extent] = []
    start = 0
    fd = os.open(path, os.O_RDONLY)
    try:
        size = os.fstat(fd).st_size
        while True:
            buf = array.array(
                "B", bytes(FIEMAP_HEADER_SIZE + count * FIEMAP_EXTENT_SIZE)
            )
            header = bytearray(FIEMAP_HEADER_SIZE)
            header[0:8] = start.to_bytes(8, "little")  # fm_start
            header[8:16] = (size - start).to_bytes(8, "little")  # fm_length
            header[16:20] = FIEMAP_FLAG_SYNC.to_bytes(4, "little")  # fm_flags
            header[20:24] = (0).to_bytes(4, "little")  # fm_mapped_extents
            header[24:28] = count.to_bytes(4, "little")  # fm_extent_count
            header[28:32] = (0).to_bytes(4, "little")  # fm_reserved
            buf[0:FIEMAP_HEADER_SIZE] = array.array("B", header)

            fcntl.ioctl(fd, FS_IOC_FIEMAP, buf, True)

            raw = buf.tobytes()
            mapped = int.from_bytes(raw[20:24], "little")
            if mapped == 0:
                break

            saw_last = False
            for i in range(mapped):
                off = FIEMAP_HEADER_SIZE + i * FIEMAP_EXTENT_SIZE
                logical = int.from_bytes(raw[off : off + 8], "little")
                physical = int.from_bytes(raw[off + 8 : off + 16], "little")
                length = int.from_bytes(raw[off + 16 : off + 24], "little")
                flags = int.from_bytes(raw[off + 40 : off + 44], "little")
                out.append(Extent(logical, physical, length, flags))
                if flags & 0x0001:
                    saw_last = True

            if saw_last:
                break
            last = out[-1]
            nxt = last.logical + last.length
            if nxt <= start or nxt >= size:
                break
            start = nxt
    finally:
        os.close(fd)
    return out


def merge_runs(extents: list[Extent]) -> list[Extent]:
    """Fold raw records into physically contiguous runs.

    Zero-length records are dropped: they are EOF markers, not data. A record
    joins the current run when it continues it in BOTH the logical and physical
    dimensions.
    """
    real = sorted(
        (e for e in extents if e.length > 0), key=lambda e: e.logical
    )
    if not real:
        return []

    runs: list[Extent] = []
    cur = Extent(real[0].logical, real[0].physical, real[0].length, real[0].flags)
    for e in real[1:]:
        contiguous = (
            e.physical == cur.physical + cur.length
            and e.logical == cur.logical + cur.length
        )
        if contiguous:
            cur.length += e.length
            cur.flags |= e.flags
        else:
            runs.append(cur)
            cur = Extent(e.logical, e.physical, e.length, e.flags)
    runs.append(cur)
    return runs


def analyze(path: str) -> dict:
    st = os.stat(path)
    raw = read_extents(path)
    runs = merge_runs(raw)
    untrusted = [e.flag_names for e in raw if e.flags & UNTRUSTED]
    return {
        "path": path,
        "size": st.st_size,
        "st_blocks": st.st_blocks,
        "allocated": st.st_blocks * 512,
        "sparse": st.st_blocks * 512 < st.st_size,
        "raw_records": len(raw),
        "merged_runs": len(runs),
        "contiguous": len(runs) == 1,
        "any_unwritten": any(e.flags & 0x0800 for e in raw),
        "untrusted": untrusted,
        "runs": [r.as_dict() for r in runs],
        "records": [e.as_dict() for e in raw],
    }


def main(argv: list[str]) -> int:
    args = [a for a in argv[1:] if not a.startswith("-")]
    as_json = "--json" in argv
    if not args:
        print(__doc__)
        return 2

    results = []
    failed = False
    for p in args:
        try:
            results.append(analyze(p))
        except OSError as exc:
            print(f"{p}: {exc.strerror}", file=sys.stderr)
            failed = True
    if not results:
        return 1
    if as_json:
        print(json.dumps(results, indent=2))
        return 0

    for r in results:
        print(f"{r['path']}")
        print(f"  size          {r['size']}")
        print(f"  allocated     {r['allocated']}  (st_blocks {r['st_blocks']})")
        print(f"  sparse        {r['sparse']}")
        print(f"  raw records   {r['raw_records']}")
        print(f"  merged runs   {r['merged_runs']}   contiguous={r['contiguous']}")
        print(f"  any unwritten {r['any_unwritten']}")
        if r["untrusted"]:
            print(f"  UNTRUSTED     {r['untrusted']}")
        for i, rec in enumerate(r["records"]):
            print(
                f"    rec {i}: logical={rec['logical']} physical={rec['physical']} "
                f"length={rec['length']} flags={','.join(rec['flags']) or '-'}"
            )
        print()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
