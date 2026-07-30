# What VHD Tool++ actually does

Static analysis of `VHD_Tool++(v0.9.1.2).zip`, 2026-07-30. No code was
executed; findings come from the bundled gettext catalogues, the PE string
table, and embedded-binary identification.

The binary is not redistributed with this repo. It was examined under
`scratch/`, which is gitignored.

```
zip sha256  62442eeff76dabd3cb9d245598a96c7166ea6dbd59792f45845812e9af11cfb3
VHD_Tool++.exe  4567040 bytes, PE32+ GUI x86-64, stripped to external PDB
built with      FPC 3.2.2 [2025/11/08] for x86_64-Win64 (Lazarus)
publisher       IODD CO.,LTD.
```

## The headline: it is an orchestrator, not an implementation

VHD Tool++ does not implement VHD creation, defragmentation, or free-space
analysis. It embeds other people's command-line tools and drives them.

Four PE images are embedded in the executable and extracted at runtime:

| Offset | Type | Identity |
|---|---|---|
| 3717124 | PE32 x86 | Sysinternals **Contig** |
| 3925084 | PE32 x86 | Sysinternals **Contig** (variant) |
| 4162028 | PE32+ x64 | Sysinternals **Contig** (64-bit) |
| 4505316 | PE32 x86, ~60 KB | Microsoft **VhdTool.exe** |

The embedded VhdTool's own usage text is visible in the string table:

```
VhdTool.exe /create <FileName> <Size> [/quiet]
VhdTool.exe /convert <FileName> [/quiet]
VhdTool.exe /extend <FileName> <NewSize> [/quiet]
VhdTool.exe /repair <BaseVhdFileName> <FirstSnapshotAVhdFileName> [/quiet]
Create: Creates a new fixed format VHD of size <Size>.
```

So the "++" is a Lazarus GUI wrapping Microsoft's VhdTool and Sysinternals
Contig. Its Windows-only nature is not a deep property of the format; it is
that the three tools it orchestrates are Windows-only.

## V1. Fixed VHD creation does not zero the data region

The decisive evidence:

```
SetFileValidData
AdjustTokenPrivileges / LookupPrivilegeValueW / OpenProcessToken
"Must be run as an administrator or a user with SE_MANAGE_VOLUME privilege.
 Try running from an elevated command prompt."
```

`SetFileValidData` sets a file's valid data length **without writing anything**.
Microsoft requires `SE_MANAGE_VOLUME` for it precisely because it exposes
whatever was previously on those clusters, and their documentation says so
outright. It is the Windows analogue of what our Phase 0 spike measured on
Linux.

This is corroboration of **S1 from the other direction**. Our finding was that
`fallocate` on ntfs3 leaves stale cluster contents; six of six samples read
back a deleted pattern from the raw device. It now appears the official Windows
tool does the same thing **by design**, which is exactly why the vendor
documentation advertises that it "instantly creates fixed-size VHD files".
Instant creation and zeroed contents are mutually exclusive.

Supporting UI evidence: zeroing exists only as *separate, explicitly optional*
operations, never as part of create.

```
"Fill Zero"
"Fill Zero (Dynamic Only, Slow)"
"Wipe Whole VHD image with zero"
```

Note "Dynamic Only" and "Slow". Fixed VHDs are not offered a fill-zero option
at creation at all.

**Bearing on our design.** Decision D5 makes zeroing mandatory, which makes
`iodd` strictly more correct than the official tool — a fresh `iodd` VHD
contains zeros, a fresh VHD Tool++ VHD contains whatever was on the disk. It
also makes us much slower: roughly 40 s for 20 GiB at the measured 500 MB/s
versus effectively instant. The ecosystem clearly tolerates un-zeroed images.
Whether to offer a `--no-zero` fast path that matches VHD Tool++ behaviour is
now a better-informed question than when D5 was taken. See "Open questions".

## V2. Contiguity is produced by Sysinternals Contig

The tool bundles Contig in three builds and exposes it as:

```
"Defrag"  "Defragment"  "Defragment a file"  "defragmented successfully"
```

This is the capability we established Linux does not have. S8 found no
`ntfsdefrag` and no kernel move-extent ioctl for NTFS; S10 measured `ntfsmove`
relocating runs without consolidating them, and misreporting success while
applying one move of three.

**So D8 is confirmed from the vendor side.** VHD Tool++ can produce contiguity
because Contig exists on Windows. `iodd` verifies and refuses because nothing
equivalent exists on Linux. That is a platform gap, not a shortfall in our
implementation, and `SPEC.md` line 162 already points operators at Contig by
name.

Also note contiguity is a **checkbox**, not a guarantee:

```
"Checked means VHD will be maden without fragmentation"
"Create Continuous Fixed VHD"
```

Even the official tool will happily produce a fragmented VHD if you untick it.

## V3. "Largest linear space" comes from scraping `defrag /A`

Four regexes in the string table:

```
Average fragmentation\s*:\s*([0-9]+)
Free cluster space\s*:\s*([0-9]+)
Free space fragments\s*:\s*([0-9]+)
Largest free space block\s*:\s*([0-9]+)
```

Those are the field labels Windows' `defrag /A` prints. The UI strings that
consume them:

```
"Largest linear space"   "Free space fragments"   "Free Size"
"The file is over than the largest linear space"
"The linear space is less than required"
```

So their pre-flight check is: run `defrag /A`, scrape the largest free block,
refuse if the requested file will not fit in it. That is precisely the refusal
our exit 4 implements, arrived at by a route Linux does not offer.

**Design revision 10 stands.** We wrote off the largest-contiguous-free-run
diagnostic as unobtainable because ntfs3 hides `$Bitmap` without `showmeta`.
The vendor does not read `$Bitmap` for this number either — they scrape a
Windows utility that has no Linux counterpart. Our `largest_free_run()`
returning `None` remains honest.

(The binary *does* contain bitmap-parsing code — `[+] Bitmap: firstCluster=%u,
dataLength=%u`, `[-] Failed to find Allocation Bitmap Entry`, `[+] exFAT`,
`[+] FAT`, `[-] Failed to read BPB` — but that belongs to its raw-device
write-to-HDD feature and its exFAT/FAT support, not to the VHD free-space
check.)

## V4. Constraints we do not currently enforce

```
"The Size must be larger than 12MB"      "is less than 12MB"
"Test Size Must be multiple of 512"
"NTFS File System only is supported"
```

- **A 12 MB minimum.** `iodd` enforces no minimum beyond "greater than zero and
  512-aligned". Whether 12 MB is a device constraint or a VhdTool constraint is
  not determinable from strings alone, but it is cheap to warn about.
- **512-byte alignment** matches what we already enforce.
- **NTFS only** matches our assumption.

## V5. Incidental observations

- The tool is licensed per device: `"No License"`, `"License Error"`,
  `"Test Lic"`, `"Disable Lic"`, `"select only one IODD to license, before hit
  this button"`. It verifies the target really is an IODD:
  `"The drive that you select is not IODD"`, `"is not IODD"`,
  `"what disk this util execute in is not IODD"`.
- Raw-device features exist beyond VHD handling: `\\.\PhysicalDrive`,
  `LockAndDismount`, `"Write to HDD (*.* or *.zip) - Very Be Careful"`,
  `"Failed to lock the Drive. the Drive is Open in Another Program"`.
- Dynamic and differencing VHDs are handled (`cxsparse` is present alongside
  `conectix`), and `"dynamic vmdk is not supported"`.
- `"Dual Mode"` corresponds to the `&D` / `&DW` filename tags documented for
  the 2531/2541.

## Open questions this raises

1. ~~Should `create` offer a `--no-zero` fast path?~~ **Decided 2026-07-30:
   zeroing is off by default with a warning, `--zero` opts in.** Recorded as
   D9, superseding D5. Matching vendor behaviour won: instant creation is the
   point of the tool.

   One asymmetry worth keeping in view, since it is the strongest argument the
   other way and it did not decide the question. On Windows,
   `SetFileValidData` requires `SE_MANAGE_VOLUME`; Microsoft gates it precisely
   because it exposes deleted data, so VHD Tool++ needs elevation. On Linux
   `fallocate` needs no privilege at all, so `iodd` makes the same exposure
   available unelevated. The practical impact is limited — the operator already
   owns the volume — but the exposure reaches further than the operator: an
   IODD gets handed to other people and booted on other machines, and a guest
   reading unwritten regions of the virtual disk sees deleted contents of the
   host volume. The warning text should say that plainly rather than
   gesturing at it.

2. **Should we enforce or warn about the 12 MB minimum?** Cheap to add as a
   warning. Worth confirming against the hardware first, since a tool-imposed
   floor and a device-imposed floor have different consequences.

3. **Does the device care about CHS at all?** Still unanswered. VHD Tool++
   delegates footer authoring entirely to Microsoft's VhdTool, so whatever
   geometry an IODD-blessed file carries is Microsoft's algorithm output — which
   is what design decision D4 already chose. This is weak corroboration of D4,
   not proof, and issue #2 stays open.

## Method note

Everything above came from `unzip`, `strings`, `file`, and a short Python script
that located embedded PE headers. The gettext `.pot` shipped alongside the
binary contains all 206 UI strings in English, which made the behavioural map
almost free — worth checking for translation catalogues before reaching for a
disassembler.
