# Licensing analysis: libntfs-3g and PolyForm Shield

Research completed 2026-07-29. Input to the S9 option-2 decision in
[`spike/FINDINGS.md`](../spike/FINDINGS.md): whether this project can use
libntfs-3g to build a cluster-consolidation planner.

**Not legal advice.** This records what the research found so the decision is
made on accurate premises. If option 2 is pursued, it warrants actual counsel.

## Correction to an earlier premise

An earlier version of this analysis stated that Tuxera Inc. holds the
copyright in libntfs-3g and that their dual-licensing business gives them
commercial incentive for an expansive GPL reading. **That is wrong.** An audit
of the copyright notices in `libntfs-3g/` and `include/`:

| Holder | Notices |
|---|---|
| Anton Altaparmakov | 48 |
| Jean-Pierre André | 39 |
| Szabolcs Szakacsits | 30 |
| Yura Pakhuchiy | 23 |
| Richard Russon | 19 |
| **Tuxera Inc.** | **2** (only `device.c`, `device.h`) |

There is no contributor-assignment agreement in the tree. Copyright is
**distributed across at least six individuals**, several from the pre-Tuxera
linux-ntfs lineage. Jean-Pierre André is the active maintainer.

This inverts the enforcement analysis. The question is not one company's
disposition, which could be profiled; it is whether any of six individuals
would act. That is harder to reason about, not easier.

Szakacsits founded Tuxera, so his share may have been assigned, but the
notices do not say so and no assignment could be verified.

### And it is not dual-licensing

A follow-up check found that Tuxera does not dual-license ntfs-3g, and could
not: there is no CLA or assignment agreement in the repository at all. The
only governance file is `AUTHORS`, which is attribution. True dual-licensing
(MySQL, Qt) requires the vendor to own the whole copyright, normally via a CLA
that assigns it or grants a broad relicensing right.

The commercial product, "Microsoft NTFS by Tuxera", is a separate proprietary
embedded implementation sold to OEMs — advertised as up to six times faster,
with patented enhancements. It is not ntfs-3g under another licence.

So the arrangement is two products from one company: a GPL project Tuxera
maintains but does not own, and a proprietary product it wrote and does own.

This **weakens** the commercial-incentive argument above. Under true
dual-licensing, enforcement directly sells a licence to the same code, which
is a tight feedback loop. Here it could at best nudge a prospect toward a
different product. The incentive is diffuse rather than sharp.

The general trap is worth naming: "community edition plus commercial edition"
reads like dual-licensing but frequently is not. Open core, true
dual-licensing, and two-codebases-one-brand have quite different consequences,
and vendors rarely say which they are running.

## Confirmed: no LGPL escape hatch

The `README` is explicit that the drivers, the ntfsprogs utilities **and
libntfs-3g itself** are GPL-2.0-or-later, and that `COPYING.LIB` covers only
`libfuse-lite`. Zero files under `libntfs-3g/` or `include/` reference the
LGPL. The one clean, uncontroversial path does not exist here.

## The derivative-work question is genuinely unsettled

**No court in any jurisdiction has ever held that dynamic linking to a GPL
library creates a derivative work.** Nor has any court held the opposite as
applied to the GPL. Anyone asserting the answer is clear, in either direction,
is overstating.

### The case against the FSF position is substantive

Lawrence Rosen, former OSI general counsel, in *The Unreasonable Fear of
Infection* (2001):

> "Dynamic linking, on the other hand, is a transitory relationship between two
> programs for which they are each pre-designed. The linking program need not
> be modified to implement the linkage... Such linkage does not constitute the
> creation of a derivative work."

> "A derivative work is not created by merely touching... An author must
> consciously recast, transform, or adapt the GPL-licensed software... before
> the GPL applies."

Rosen hedges on *static* linking while being firm that dynamic linking does
not create a derivative work — an asymmetry that favours the skeptical
reading.

### 2024 gave the skeptics real appellate support

**Oracle v. Rimini Street**, 9th Cir. No. 23-16038 (Dec. 16, 2024), rejected
an interoperability test for software derivative works:

> "In effect, the district court adopted an 'interoperability' test for
> derivative works... But neither the text of the Copyright Act nor our
> precedent supports this interoperability test."

> "Without more, mere interoperability isn't enough to make a work
> derivative... derivative status does not turn on interoperability, **even
> exclusive interoperability**, if the work doesn't substantially incorporate
> the preexisting work's copyrighted material."

Caveats that matter: it is a vacate-and-remand rather than a merits ruling on
linking, it did not involve the GPL, it left open what counts as nonliteral
incorporation, and it binds only the Ninth Circuit. It is nonetheless the
closest on-point appellate authority that exists, and it cuts against the FSF.

### The FSF's own test is not a bright line

Their FAQ says pipes and sockets "normally" mean separate programs, but that
"if the semantics of the communication are intimate enough, exchanging complex
internal data structures, that too could be a basis to consider the two parts
as combined" — and that shared address space "almost surely" means one
program.

That is a two-factor test of mechanism plus intimacy in which **mechanism is
explicitly not dispositive**. It cuts both ways: it weakens "dynamic linking is
always infringing," and it equally destroys "dlopen across a boundary is always
safe."

The Linux syscall exception is the strongest evidence the line is arbitrary.
Torvalds had to write an explicit carve-out saying syscalls do not create
derived works — nobody drafts an exception to a rule they believe does not
exist. Torvalds also rejects the mechanism test directly: "Linking is just a
technical step, and as such is not the answer to whether something is derived
or not. Intent matters. How you do something technically does not."

### The strongest rebuttal

GPLv2 is a *license*, and a licensor may condition permission on terms broader
than copyright would independently reach. The Software Freedom Conservancy's
framing shifts from "derivative work" to "combined work" under GPLv2 §2,
sidestepping §101 entirely.

More concretely: linking against libntfs-3g means compiling its headers, which
carry substantial struct layouts and inline functions, into our binary. That is
literal copying of protected expression, and it collapses the §101 argument on
the facts regardless of the theory.

## Industry practice cuts against the optimistic reading

The revealed preference of the industry is that GPL linking is a real
constraint. The pattern is not widespread defiance — it is **widespread,
expensive avoidance**:

- The Linux **syscall note**, the **GPL Classpath Exception**, and Oracle's
  **MySQL FOSS License Exception** all exist because sophisticated, well-advised
  parties concluded copyleft would otherwise reach the linking program.
- **GNU readline** is the canonical case: rather than test the theory, the
  industry built BSD `libedit`, `linenoise`, and `replxx`. Twenty-five years of
  avoidance and no litigation, because everyone avoided it.
- **FFmpeg** ships `--enable-gpl` as opt-in so commercial users stay on the
  LGPL build.
- **Google** bans AGPL, treats GPL as "Restricted," and requires LGPL libraries
  be dynamically linked — treating the static/dynamic distinction as meaningful
  only where the license text says so, and not extending it to GPL.

"Everybody does it" is weak evidence here, because mostly everybody doesn't.

## The decisive risks are not about derivative works

Three factors matter more than the debate above:

1. **Distributed copyright.** Six-plus individual holders, any one of whom can
   sue independently. No single entity's disposition provides comfort.
2. **The Vizio contract theory.** In SFC v. Vizio the court held (Dec. 2023)
   that a GPL contract claim is not preempted by the Copyright Act, opening a
   third-party-beneficiary route. If that holds, *users* could demand source
   without holding any copyright — bypassing the derivative-work question
   entirely, which is the whole battlefield the optimistic reading relies on.
3. **PolyForm Shield is not an open source license.** It is source-available
   with a non-compete restriction. FOSS exceptions like MySQL's enumerate
   OSI-approved licenses, so no exception path would ever cover this project.
   And GPLv2 §6 forbids imposing further restrictions, which a non-compete
   plainly is. **If a court found a combined work, the incompatibility is
   incurable** — there is no drafting fix short of relicensing.

## Bearing on the decision

- **Relicensing to GPL-2.0-or-later and linking is legally clean.** The entire
  conflict exists only because of PolyForm Shield. This was always the
  unambiguous path and the research confirms it.
- **Keeping PolyForm and loading the library dynamically is unsettled**, with
  genuinely respectable arguments on the skeptical side and, since December
  2024, real appellate wind behind them. But it is the *worst* configuration in
  which to run that experiment: no exception path exists for a non-OSI license,
  the copyright is scattered across individuals, and losing is uncurable.
- **Reimplementing in Rust** avoids the question entirely at a large cost.
- **Shipping verify-and-refuse** avoids the question entirely at no cost.

The honest summary: the intellectual argument against the FSF is stronger than
most compliance guidance admits, and it got materially stronger in 2024. But
winning the intellectual argument and losing the practical one is the likely
shape of a bad outcome, and the practical risks here are specific to this
project's license choice rather than to the general debate.

## Unverified

Flagged rather than inferred:

- Whether Szakacsits' or Altaparmakov's copyrights were ever assigned to
  Tuxera. No assignment appears in the tree.
- A Tuxera marketing line about customers who "cannot comply with the terms of
  the GPLv2" is indexed by search engines but the page no longer loads;
  treat as probable but unconfirmed.
- The FSF FAQ passages were retrieved via search rather than from gnu.org,
  which was unreachable during the research. They match the long-published
  text.
- Whether Tuxera or any individual holder has ever sent a private cease-and-
  desist. Unknowable from open sources.
- Whether the Dec. 2025 tentative ruling in SFC v. Vizio was adopted.

No public GPL enforcement action by Tuxera was found.
