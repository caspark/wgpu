# progress

Append-only log. Newest entries at the bottom.

---

## 2026-09-07 — Session start

**Task.** Build a standalone app (own cargo workspace, `[patch.crates-io]` onto this wgpu checkout)
with two render passes, each timed on the GPU via timestamp queries. winit window, hidden
("headless") so it doesn't steal focus, 1080p surface. Console logging. Motivation: a report
that GPU timing doesn't work on macOS; verified working on Linux + Windows.

**Pre-existing state found in this checkout (not written by me this session):**

- `examples/bug-repro/03_metal_timestamp_query/` exists but is untracked. Its README asserts a
  Metal timestamp bug on Apple8+ GPUs and a `MTLSharedEvent` serialization fix modelled on Dawn's
  `MetalSerializeTimestampGenerationAndResolution` toggle.
- `git stash@{0}: On ck/metal-timestamp-query-resolve: try to fix metal timestamps`.
- Current `trunk` working tree has **no** such fix in `wgpu-hal/src/metal/`.

I am treating all of that as an unverified prior hypothesis, not as ground truth. The new app is
independent and I will re-derive the failure (or absence of one) from scratch.

**Plan.**
1. New crate at `gpu-pass-timing/`, own workspace, `wgpu` patched to `../wgpu`.
2. Hidden winit window @ 1920x1080, wgpu surface configured to match.
3. Two render passes per frame with genuinely different synthetic fragment load
   (heavy ALU loop, different iteration counts) so the two timings must differ if timing works.
4. Timestamp writes at beginning/end of each pass -> `resolve_query_set` -> readback -> map -> log.
5. Per-frame console log of raw ticks + ns, plus a summary with sanity checks.

---

## Step 1 — App built, first run

Crate is at `gpu-pass-timing/`. Own workspace (`[workspace]` table in its own Cargo.toml), so it
does not join the wgpu workspace at all; `[patch.crates-io] wgpu = { path = "../wgpu" }` points it
at this checkout. Confirmed by the compile errors it hit — it picked up trunk's API drift
(`default_queue`, `immediate_size`, `multiview_mask`, `SurfaceConfiguration::color_space`,
`get_mapped_range() -> Result`), which only exist locally.

Machine: **Apple M5 Max**, Metal backend, timestamp period 1 ns/tick.

**Snag: hidden window = no drawable.** With `.with_visible(false)`, `get_current_texture()` returns
`Occluded` forever, so the first version rendered nothing at all. Fixed by keeping the surface
created and configured at 1920x1080 (so the swapchain path is still exercised) but falling back to
an offscreen 1920x1080 render target for the timed passes when no drawable is available. `VISIBLE=1`
switches back to the real swapchain. The GPU work is identical either way, which is what matters
for timing.

## Step 2 — The bug reproduces, first look

20 frames, pass A = 24 shader iterations, pass B = 96:

```
ok  frame   0 | A 1.6534 ms | B 8.4062 ms      | raw 5959045594355875 5959045596009250 5959045594417416 5959045602823583
BAD frame   1 | A 0.7480 ms | B -6231792 ticks | raw 5959045609034916 5959045609782916 5959045609055375 5959045602823583
ok  frame   2 | A 0.6285 ms | B 3.1671 ms      | raw 5959045613098375 5959045613726875 5959045613105500 5959045616272625
BAD frame   3 | A 0.6375 ms | B -1028000 ticks | raw 5959045617289791 5959045617927291 5959045617300625 5959045616272625
```

15 of 20 frames non-monotonic. The shape of it:

- **Pass A is always fine.** Its begin/end are sane and its duration decays smoothly across frames
  as the GPU clocks up — exactly what you'd expect.
- **Pass B's *end* timestamp is the broken one.** On frame 1 it is `...602823583`, which is
  *byte-for-byte frame 0's pass B end*. Same on frame 3 (repeats frame 2's), frame 14 (repeats
  frame 13's). The resolve is reading a slot the GPU has not written yet, so it gets the previous
  frame's value.
- The frames that *pass* the check are not actually healthy either — pass B's duration bounces
  between 1.1 ms and 8.4 ms with no trend, while pass A is stable. Those are stale values that
  happen to land later than the begin timestamp, so the monotonic check doesn't catch them.

Pass B's end-of-pass timestamp is the last timestamp written in the command buffer, and it is
immediately followed by `resolveCounters`. That is consistent with a race between timestamp
write-back and query resolution rather than anything to do with which query indices are used.

Next: confirm the mechanism (is it specifically "the last timestamp before the resolve"?) rather
than assuming it.

---

## Step 3 — Isolating the mechanism

Generalised the app to N passes (`ITERS=a,b,c,...`) so I could ask a sharper question than
"is pass B broken".

**Experiment 1 — four passes, `ITERS=16,32,64,128`:**

```
  pass A   (  16 iters)  : mean 0.3335 ms | zero 0 | non-monotonic 0
  pass B   (  32 iters)  : mean 0.9546 ms | zero 0 | non-monotonic 0
  pass C   (  64 iters)  : mean 2.1942 ms | zero 0 | non-monotonic 0
  pass D   ( 128 iters)  : mean 3.2491 ms over 2/12 usable | zero 1 | non-monotonic 10
```

Passes A/B/C are *perfect* — 0.23 / 0.68 / 1.57 ms per frame for 16 / 32 / 64 iterations, i.e. the
measured time doubles exactly as the work doubles. So timestamp queries on macOS are not broken in
general. Only **pass D**, whose end-of-pass timestamp is the last one written before
`resolveCounters`, is corrupt. On frame 0 it reads literally `0` (never written); on later frames it
reads a value from *before that frame even started*.

So the failure is positional, not per-pass: it is always the final timestamp preceding the resolve.

**Experiment 2 — same, but `SPLIT_SUBMIT=1`** (resolve + copy moved into a second command buffer,
submitted separately after the rendering one):

```
  pass D   ( 128 iters)  : mean 3.4851 ms over 4/12 usable | zero 1 | non-monotonic 8 | stale-repeat 1
```

Unchanged. Putting the resolve in a *different command buffer on the same queue* does not help.
Metal's ordinary intra-queue ordering guarantees do not cover counter write-back, so the resolve
still runs before the last timestamp has landed. That rules out "wgpu is encoding the resolve too
early in one command buffer" and points at a genuine hardware/driver race needing explicit
synchronisation.

This matches the Dawn toggle `MetalSerializeTimestampGenerationAndResolution`, whose comment says
newer Apple GPUs race on query-set resolution against timestamp writing, and whose fix is to signal
and wait on an `MTLSharedEvent` between timestamp generation and resolution. Independently arrived
at the same place, which is reassuring, but I derived it from the traces above rather than from that
prior note.

Next: implement the serialisation in `wgpu-hal`'s Metal backend and re-run these same experiments.

---

## Step 4 — Fix

Three files in `wgpu-hal/src/metal/`:

- **`adapter.rs`** — new private capability
  `serialize_timestamp_generation_and_resolution`, set when the device reports
  `MTLGPUFamily::Apple8` or newer. Same gate Dawn uses.
- **`mod.rs`** — `QueueShared` gains a lazily created `MTLSharedEvent` and a monotonic counter,
  plus `next_timestamp_resolve_fence()` which hands out `(event, next_value)`. If
  `newSharedEvent()` returns `None` (restricted sandboxes do this) the caller just skips the
  workaround rather than failing.
- **`command.rs`** — in `copy_query_results`, for `QueryType::Timestamp` on an affected device,
  close any open encoder and encode `encodeSignalEvent(event, v)` immediately followed by
  `encodeWaitForEvent(event, v)` on the command buffer, before opening the blit encoder that
  issues `resolveCounters`. Signalling then immediately waiting on the same value forces the GPU
  to drain in-flight work — including counter write-back — before the resolve runs.

Encoding events is a command-buffer-level operation in Metal, so the open blit/acceleration-
structure encoder has to be ended first; `enter_blit()` reopens one straight after.

### Result

Same experiment, `ITERS=16,32,64,128`, 12 frames:

```
  pass A   (  16 iters)  : mean 0.3513 ms over 12/12 usable | zero 0 | non-monotonic 0 | stale-repeat 0
  pass B   (  32 iters)  : mean 0.9556 ms over 12/12 usable | zero 0 | non-monotonic 0 | stale-repeat 0
  pass C   (  64 iters)  : mean 2.1833 ms over 12/12 usable | zero 0 | non-monotonic 0 | stale-repeat 0
  pass D   ( 128 iters)  : mean 4.2324 ms over 12/12 usable | zero 0 | non-monotonic 0 | stale-repeat 0

  verdict:
    PASS - GPU pass timings look correct.
```

Steady-state per-frame ladder is 0.234 / 0.681 / 1.572 / 3.352 ms for 16 / 32 / 64 / 128
iterations — a clean 2x per doubling of work, including for pass D. That's a stronger check than
"the numbers stopped being negative": pass D's value now lands exactly where the trend from the
three known-good passes predicts.

Default two-pass config, 60 frames: pass A 0.345 ms, pass B 1.681 ms steady state, 60/60 usable,
no anomalies.

### Cost

100 frames, three runs each, wall clock:

| | run 1 | run 2 | run 3 | pass A mean |
|---|---|---|---|---|
| with workaround | 0.38 s | 0.41 s | 0.34 s | 0.362-0.380 ms |
| without | 0.37 s | 0.33 s | 0.33 s | 0.362-0.369 ms |

In the noise. Pass A's measured duration is unchanged either way, so the extra event does not
perturb what is being measured — it only gates the resolve.

Worth noting how total the failure is without the fix: **97-100 of 100 frames** had a broken pass B
end timestamp. The handful that passed the monotonic check in the earlier 12- and 20-frame runs were
stale values that happened to land after the begin timestamp, not healthy readings.

### Regressions

`cargo xtask test` (full suite, 1072 tests): 1063 passed, 9 failed. All 9 failures reproduce
on clean trunk with my changes stashed — 6 naga snapshot / SPIR-V debug-info tests and 3
`passthrough` shader tests. Pre-existing, unrelated to this change. The 25 timestamp/query-set
tests all pass.

Note: none of the existing tests caught this bug. They resolve either a single pass, or resolve in
a later submit, or don't check the *last* timestamp specifically.

### Caveat on the gate

I can only test one machine (Apple M5 Max). The `Apple8` lower bound is taken from Dawn rather than
measured — I have not verified whether Apple7 and earlier are affected, or whether the bound could
safely be raised. Following Dawn's gate is the conservative choice.

---

## Step 5 — Wrap-up

Added alongside the fix:

- **Regression test** `timestamp_query::timestamps_survive_resolve_in_same_encoder` in
  `tests/tests/wgpu-gpu/timestamp_query.rs`. Four compute passes doing real work, resolved in the
  same encoder, repeated over 8 submits; asserts every pass has non-zero, increasing timestamps.
  Verified it actually catches the bug — with the fix stashed it fails immediately:

  ```
  round 0, pass 3: timestamps must be non-zero and increasing, got begin=0 end=0
  (all: [5959674644823083, 5959674645305416, 5959674645310083, 5959674645797416,
         5959674645801833, 5959674646284833, 0, 0])
  ```

  Note that with compute passes the last pass loses *both* its timestamps, not just the end one.
  The existing `TIMESTAMP_QUERY` test misses this because its compute passes are empty and finish
  before the resolve can race them.

- **CHANGELOG entry** under Unreleased / Bug Fixes / Metal. Author handle and PR number are left as
  literal `@TODO` / `#TODO` — I don't know the right values and would rather not guess.

Final `cargo xtask test`: 1073 tests, 1064 passed, 9 failed — the same 9 that fail on clean trunk.

### Files changed

```
 M CHANGELOG.md
 M tests/tests/wgpu-gpu/timestamp_query.rs
 M wgpu-hal/src/metal/adapter.rs
 M wgpu-hal/src/metal/command.rs
 M wgpu-hal/src/metal/mod.rs
?? gpu-pass-timing/              (this app)
```

### Left alone

`examples/bug-repro/03_metal_timestamp_query/` was already in the working tree, untracked, when I
started, along with `git stash@{0}: On ck/metal-timestamp-query-resolve: try to fix metal
timestamps`. Neither is mine. That example **does not compile against trunk** (missing
`DeviceDescriptor::default_queue`), which breaks `cargo xtask test` for the whole workspace — I had
to move it aside to run the suite and then put it back exactly where it was. It is superseded by
this app; worth deleting or repairing.

### Open questions

- The `Apple8` gate is copied from Dawn, not measured. I only have an M5 Max. Whether Apple7 and
  earlier are affected, and whether the gate could be narrowed, is untested.
- The workaround costs one shared-event signal/wait per timestamp resolve. Measured as noise here,
  but that's one app on one machine; an app resolving query sets very frequently might notice.
- Only the Metal backend was touched. I did not check whether the same class of race exists on
  other backends, and the report was macOS-specific.

---

## Step 6 — Compared against the pre-existing attempt

Went back and read `git stash@{0}` (branch `ck/metal-timestamp-query-resolve`, based on an old trunk)
and `examples/bug-repro/03_metal_timestamp_query/`, to see whether the earlier attempt landed in the
same place.

**It did.** Same diagnosis (Apple8+ race between timestamp write-back and `resolveCounters`), same
remedy (`MTLSharedEvent` signal-then-wait encoded on the command buffer immediately before the
resolve), same gate (`family_check && supportsFamily(Apple8)`), same Dawn precedent. Independent
arrival, same answer.

Differences:

| | prior attempt | mine |
|---|---|---|
| event lives on | `CommandEncoder` | `QueueShared` **(worse - see below)** |
| cap name | `serialize_timestamp_query_resolution` | `serialize_timestamp_generation_and_resolution` |
| closes AS encoder before encoding events | no | yes |
| regression test | none | yes |
| repro app | fixed 8 passes, no window, offscreen 1x1 | N passes, winit surface, 1080p, load knobs |

### The comparison found a real flaw in my version, now fixed

I had put the shared event on `QueueShared`, handing out values via `fetch_add` — one event for the
whole queue. That is wrong. `encodeWaitForEvent(event, V)` is satisfied by *any* signal that raises
the event to `>= V`. With a queue-wide event, a concurrently executing command buffer that was
handed a higher value can satisfy this command buffer's wait before this command buffer's own
counter write-back has drained — silently reinstating the exact race the workaround exists to
prevent. It degrades to "no workaround" rather than corrupting anything, and it needs multiple
threads encoding on one queue to bite, but it is a genuine hole.

The prior attempt's per-encoder placement does not have this problem: only that encoder ever
touches its own event. It also matches Dawn more closely, which holds the event on
`CommandRecordingContext` (per command buffer).

Switched to per-encoder placement (`timestamp_resolve_event: Option<...>` +
`timestamp_resolve_value: u64` on `CommandEncoder`, initialised in `Device::create_command_encoder`),
with a comment recording why it must not be shared. Kept my
`leave_acceleration_structure_builder()` call, which the prior version omitted — `enter_blit()`
ends that encoder, but the event encoding happens before `enter_blit()`, so an open
acceleration-structure encoder would otherwise still be live at that point.

Re-verified after the rework: app reports PASS (15/15 usable frames on all four passes, ladder
0.31 / 0.88 / 2.01 / 4.01 ms), all 26 timestamp/query-set tests pass, and the regression test still
fails with the fix stashed.

---

## Step 7 — Correction: why the existing test misses this

I said earlier that the existing `TIMESTAMP_QUERY` test misses this bug "because its compute passes
are empty". That is only half the reason, and stated on its own it is misleading. Checked it
properly.

The existing test's only assertion on a written query is:

```rust
assert_ne!(query_data[query_index as usize], init_constant);
```

with `init_constant = 0x0123_4567_89AB_CDEF`. Its own comment explains why it is that weak — WebGPU
does not define timestamp values, and they "can be `0` in some situations". But the two failure
modes of this bug are exactly *zero* and *a stale value from a previous submit*, and both are
`!= init_constant`, so both pass.

Verified rather than reasoned: temporarily swapped my regression test's assertion for the existing
test's assertion style, kept the real GPU load, stashed the fix, and ran it. **It passed** — the bug
was present and went undetected.

So extending the existing test "a little" would not have been enough. It needs two independent
changes:

1. **Real work in the passes**, so counter write-back is still in flight when the resolve runs.
   Empty passes finish before there is anything to race.
2. **Stronger assertions** — non-zero, and end > begin. The `!= init_constant` check cannot catch
   this no matter how much load is added.

My regression test does both. Left the existing test alone; its job is checking that resolves land
in the right buffer slots and leave the others untouched, which it does fine.

---

## Step 8 — Strengthened the existing test, folded the app in as the example

**Existing `TIMESTAMP_QUERY` test.** Added the one extra property that is safe to assert there: a
pass cannot end before it began, guarded on both ends of the pair being non-zero. The guard matters
— the test's own comment notes a timestamp is allowed to come back as `0`, and comparing against
one that did would fail for reasons unrelated to ordering. I did not add a non-zero assertion: that
is exactly what the original author declined to assert, this test runs on every backend in CI, and
I can only validate Metal here.

Checked the new assertion isn't vacuous on Metal by temporarily inverting it — it fires with real
values (`begin=5961936205289125 end=5961936205296208`), so the guard is live rather than skipping
everything. It catches the stale-value failure mode; the zero failure mode is covered by
`timestamps_survive_resolve_in_same_encoder`, which loads the passes heavily enough to provoke it.

**The app is now the example.** `gpu-pass-timing/` is gone; it lives at
`examples/bug-repro/03_metal_timestamp_query/`, replacing the stale example that was sitting there
untracked and failing to compile against trunk. No more own-workspace or `[patch.crates-io]` — it
inherits `wgpu`, `winit`, `env_logger`, `pollster` and `bytemuck` from the workspace (bytemuck with
`features = ["derive"]`, which the workspace default doesn't carry).

Side effect worth noting: `cargo xtask test` now runs without intervention. The old example's
compile error had been breaking the whole workspace test build, and I'd been moving it aside for
every suite run.

Verified after the move: builds and runs clean as a workspace member (`ITERS=16,32,64,128` gives
0.36 / 1.02 / 2.35 / 4.60 ms, PASS), and `cargo xtask test` reports 1073 tests, 1064 passed, the
same 9 pre-existing failures that reproduce on clean trunk.

---

## Step 9 — Rebased onto the last crates.io release

Moved the work off trunk and onto a branch forked from the latest published wgpu.

**Deleted the old branch.** `ck/metal-timestamp-query-resolve` (tip `e68c004fe`) had **zero commits
not already in trunk** — `git merge-base --is-ancestor` confirms its tip is an ancestor of trunk, so
the branch was just a pointer at an old trunk commit. Nothing was lost by deleting it. Its actual
content lived in `git stash@{0}`, which is an independent ref and still exists; I left it alone
since removing it wasn't asked for.

**New base.** crates.io reports `max_stable_version: 30.0.1`, and the repo has a matching `v30.0.1`
tag (`40f4a34eb`, "Release v30.0.1", 2026-08-21). Branched from that tag, reusing the same branch
name.

**Porting the patch.** The wgpu-hal fix applied *cleanly* to v30.0.1 — all four Metal files, no
conflicts. Three things needed adapting:

1. `CHANGELOG.md` conflicted, because the 3-way merge tried to drag trunk's whole Unreleased section
   in. Discarded that and placed the single entry under a fresh `Unreleased / Bug Fixes / Metal`
   heading instead.
2. The example used `DeviceDescriptor::default_queue`, which does not exist in v30.0.1 (it is a
   post-release trunk addition). Removed. Everything else I expected to break — `immediate_size`,
   `multiview_mask`, `SurfaceConfiguration::color_space`, `get_mapped_range() -> Result` — is
   already present at v30.0.1, so no other changes were needed.
3. The regression test used trunk's `#[apply(gpu_test!)]`. v30.0.1 predates
   `Replace wgpu-macros package with use of macro_rules_attribute (#9963)` and uses the older
   `#[gpu_test]` proc-macro attribute. Switched to match the surrounding code.

### Verification on the release branch

**The bug is in the shipped release, not just trunk.** With the fix stashed, `ITERS=16,32,64,128`:

```
  pass A   (  16 iters)  : mean 0.3374 ms over 12/12 usable | non-monotonic 0
  pass B   (  32 iters)  : mean 0.9545 ms over 12/12 usable | non-monotonic 0
  pass C   (  64 iters)  : mean 2.1680 ms over  3/12 usable | non-monotonic 0
  pass D   ( 128 iters)  : mean 5.8006 ms over  3/12 usable | non-monotonic 9 | stale-repeat 1
    FAIL - pass D: 9/12 frames ended before they began
```

With the fix: 12/12 usable on every pass, ladder 0.31 / 0.91 / 2.11 / 4.08 ms, PASS.

Tests: 26 timestamp/query-set tests pass; the regression test still fails with the fix stashed.

Full suite on this branch: **1002 tests, 993 passed, 9 failed**. Different set from trunk's 9 —
here it is 2 naga snapshot tests, `naga_capabilities::validate_capabilities`, 3 `passthrough`
tests, 2 `binding_array::buffers` tests, and the `Computepass Encoding` benchmark. Checked every
one of them against clean v30.0.1 with my changes stashed: **all 9 fail there too**. Pre-existing,
none caused by this patch.

---

## Step 10 — Upstream search (read-only)

Searched `gfx-rs/wgpu` and `gfx-rs/wgpu-native` via `gh`. Read-only queries only — no issue, PR,
comment or push.

**No issue filed for this exact bug, and no PR fixing it.**

**Closest match: [gfx-rs/wgpu#9414](https://github.com/gfx-rs/wgpu/issues/9414)** (open, filed
2026-04-12 by @hxyulin) — "Metal timestamp queries return all zeros on macOS 26 (Metal 4) - New
timestamp API". Filed as an *unconditional* platform failure: on an M3 Pro,
`supportsCounterSampling` reports `atDrawBoundary: false`, all timestamps come back zero, and the
conclusion is that wgpu-hal must adopt `MTL4CounterHeap`.

That headline symptom is **not** what I have. On this M5 Max draw-boundary sampling plainly works —
passes A/B/C measure perfectly and scale with load. **My fix does not address #9414 as written.**

But buried in that issue's comments, @matthewgapp (2026-07-27, Apple M4 Max, macOS 26.5, wgpu
29.0.4) independently reached the same root cause I did:

| Resolve strategy | Complete, ordered, nonzero |
|---|---:|
| CPU `resolveCounterRange` after sampling completion | 100/100 |
| GPU blit resolve committed before sampling completion | 4/100 |
| GPU blit resolve committed from the sampling buffer's completion handler | 100/100 |

— "committing the resolve before host-observed sampling completion loses counter data." Same fault,
different remedy: theirs defers the resolve to a CPU-side completion handler, mine (and Dawn's) uses
a GPU-side `MTLSharedEvent` signal/wait, which avoids the CPU round trip. Their comment drew
pushback from another participant for being a wall of LLM-generated text and appears to have been
dropped; the issue title still frames this as "all zeros / needs the Metal 4 API".

My machine is macOS **26.5.1** (25F80) — the same macOS 26.5 cluster as that M4 Max report.

**Related but distinct, all open:**

- [wgpu-native#624](https://github.com/gfx-rs/wgpu-native/issues/624) — encoder-level
  `writeTimestamp` makes the submission never complete. Different symptom.
- [wgpu-native#625](https://github.com/gfx-rs/wgpu-native/issues/625) — a compute pass with
  `timestampWrites` but no dispatch resolves both timestamps to zero. This is the empty-pass
  behaviour, and it is exactly why the regression test here has to load the passes with real work.
- [wgpu#6406](https://github.com/gfx-rs/wgpu/issues/6406) — "Vulkan timestamp queries can return 0
  if resolved too soon" (2024). Same *class* of fault, different mechanism: on Vulkan it is a
  missing barrier, and moving the copy to a separate encoder fixes it. On Metal that workaround does
  **not** help — `SPLIT_SUBMIT=1` still fails — so the two are not the same bug.
- [wgpu#9955](https://github.com/gfx-rs/wgpu/issues/9955) — Intel UHD 630 timestamp queries
  disabled. Unrelated.
- [wgpu#5195](https://github.com/gfx-rs/wgpu/issues/5195) — macOS deadlock with timestamp queries.
  Unrelated.

Prior art in the same code: PR #6322 "metal: fix query set result copies" (merged 2024-09-25).

**If this were to be filed**, it is arguably a separate issue from #9414 rather than a comment on
it, since #9414's title, diagnosis and proposed fix (`MTL4CounterHeap`) are about a different
failure mode. Not filed — read-only was the instruction.
