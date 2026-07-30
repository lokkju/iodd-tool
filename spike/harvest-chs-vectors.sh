#!/usr/bin/env bash
#
# Harvest CHS golden vectors from qemu-img, resolving plan item C20.
#
# WHY THE OBVIOUS APPROACH IS WRONG
#
# You cannot ask qemu for a size and compare its CHS against ours for that
# size. Without force_size, qemu wraps the Microsoft geometry algorithm in a
# search loop that GROWS the requested sector count until cylinders * heads *
# spt covers it. Ask for 100 MiB (204800 sectors) and qemu returns a footer
# describing 204816 sectors, because 204800 does not fit under 1003/12/17.
#
#   chs(204612) = 1003/12/17, product 204612   covers exactly
#   chs(204800) = 1003/12/17, product 204612   does NOT cover
#   chs(204816) = 1004/12/17, product 204816   covers
#
# So the vector is keyed on the footer's OWN Original Size field, not on what
# was requested. Then `chs(original_size / 512)` must equal the footer's CHS
# bytes, which tests the geometry function directly and is immune to the
# growth loop. See FINDINGS F7 and the design doc.
#
# The cap branch (> 65535*16*255 sectors) cannot be harvested: qemu-img
# refuses vpc images above ~127.9 GiB. It is asserted against the algorithm by
# hand instead.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${1:-$HERE/../tests/fixtures/chs_vectors.json}"
WORK="$(mktemp -d /tmp/iodd-chs.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

command -v qemu-img >/dev/null || { echo "qemu-img not installed" >&2; exit 1; }

# One size per reachable branch, plus extras where a branch has more than one
# entry path. 102M is the equality case of the 31-spt escalation: its cth lands
# exactly on heads * 1024.
SIZES=(100M 102M 200M 300M 500M 1G 8G 32G 127G)

mkdir -p "$(dirname "$OUT")"

python3 - "$WORK" "$OUT" "$(qemu-img --version | head -1)" "${SIZES[@]}" <<'PY'
import json, os, struct, subprocess, sys

work, out, qemu_version, *sizes = sys.argv[1:]
vectors = []

for size in sizes:
    path = os.path.join(work, f"{size}.vhd")
    subprocess.run(
        ["qemu-img", "create", "-f", "vpc", "-o", "subformat=fixed", path, size],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    with open(path, "rb") as f:
        f.seek(os.path.getsize(path) - 512)
        footer = f.read(512)
    os.unlink(path)

    cylinders, heads, spt = struct.unpack_from(">HBB", footer, 56)
    original, current = struct.unpack_from(">QQ", footer, 40)
    disk_type = struct.unpack_from(">I", footer, 60)[0]
    checksum = struct.unpack_from(">I", footer, 64)[0]

    # Recompute the checksum to prove our algorithm against qemu's value.
    z = bytearray(footer)
    z[64:68] = b"\0\0\0\0"
    recomputed = (~sum(z)) & 0xFFFFFFFF

    assert original == current, f"{size}: size fields disagree"
    assert disk_type == 2, f"{size}: not a fixed disk"
    assert recomputed == checksum, f"{size}: checksum algorithm mismatch"

    vectors.append({
        "requested": size,
        "original_size": original,
        "total_sectors": original // 512,
        "cylinders": cylinders,
        "heads": heads,
        "sectors_per_track": spt,
        "product_sectors": cylinders * heads * spt,
        "checksum": checksum,
    })

doc = {
    "_comment": [
        "Golden CHS vectors harvested from qemu-img. DO NOT derive these from",
        "the SPEC.md pseudocode; that would be circular. Regenerate with",
        "spike/harvest-chs-vectors.sh.",
        "",
        "Keyed on the footer's own Original Size, NOT on the requested size.",
        "Without force_size qemu grows the sector count until the CHS product",
        "covers it, so a requested size and the footer's size differ. Assert",
        "chs(total_sectors) == (cylinders, heads, sectors_per_track).",
        "",
        "The cap branch (> 65535*16*255 sectors) is absent because qemu-img",
        "refuses vpc images above ~127.9 GiB. It is asserted by hand.",
    ],
    "qemu_version": qemu_version,
    "command": "qemu-img create -f vpc -o subformat=fixed <path> <size>",
    "vectors": vectors,
}
with open(out, "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")

print(f"wrote {len(vectors)} vectors to {out}")
for v in vectors:
    print(f"  {v['requested']:>5}  sectors={v['total_sectors']:<12} "
          f"CHS={v['cylinders']}/{v['heads']}/{v['sectors_per_track']}  "
          f"product={v['product_sectors']}")
PY
