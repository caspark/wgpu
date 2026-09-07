# Metal timestamp query repro

Renders several render passes with synthetic GPU load, times each one on the GPU with timestamp
queries, and checks the results are self-consistent.

```sh
cargo run -p wgpu-bug-repro-03-metal-timestamp-query
```

The winit window is created hidden so running this doesn't steal focus or cover anything. A real
wgpu surface is still created and configured at 1920x1080; when the hidden window has no drawable
(macOS reports `Occluded`) the timed passes render to an offscreen target of the same size, so the
GPU work is identical either way.

Output is a per-frame line with each pass's measured duration and the raw resolved tick values,
then a summary with per-pass anomaly counts and a PASS/FAIL verdict.

## Env knobs

| var | default | meaning |
|---|---|---|
| `FRAMES` | `60` | frames to render |
| `ITERS` | `24,96` | fragment-shader loop count per pass; the number of entries is the number of passes |
| `WIDTH` / `HEIGHT` | `1920` / `1080` | surface size |
| `VISIBLE` | unset | show the window and use the real swapchain |
| `SPLIT_SUBMIT` | unset | resolve the query set in a second, separately submitted command buffer (diagnostic) |

Increasing `ITERS` monotonically is the sharpest check: measured durations should scale with the
workload, so `ITERS=16,32,64,128` should give a clean 2x per pass.

```sh
FRAMES=12 ITERS=16,32,64,128 cargo run -p wgpu-bug-repro-03-metal-timestamp-query
```

## The bug

On Apple8-and-newer GPUs, counter write-back for timestamps sampled at pass boundaries could still
be in flight when `resolveCounters` ran, so the **last** timestamp before the resolve came back as
zero or as a stale value from an earlier submission:

```text
ok   frame   0 | A 1.1226 ms | B 3.2177 ms | C 7.3831 ms | D  <zero> !!
BAD  frame   1 | A 0.5295 ms | B 1.4233 ms | C 3.2236 ms | D -7058292 !!
```

Passes A/B/C measured perfectly (durations doubling exactly as the work doubled) while pass D was
corrupt, which is what identifies the fault as positional rather than per-pass. Moving the resolve
into a separately submitted command buffer (`SPLIT_SUBMIT=1`) does not help — ordinary intra-queue
ordering does not cover counter write-back.

The fix in `wgpu-hal/src/metal/` encodes an `MTLSharedEvent` signal immediately followed by a wait
on the same value, on the command buffer, just before the resolve. Dawn has the same workaround
under `MetalSerializeTimestampGenerationAndResolution`, gated the same way; see
`src/dawn/native/Toggles.cpp`, `PhysicalDeviceMTL.mm` and `CommandBufferMTL.mm` there.

`progress.md` in this directory is the investigation log.

Regression coverage lives in
`tests/tests/wgpu-gpu/timestamp_query.rs::timestamps_survive_resolve_in_same_encoder`.
