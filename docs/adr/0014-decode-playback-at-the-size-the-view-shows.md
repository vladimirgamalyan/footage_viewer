# 0014. Decode playback at the size the view shows

- Status: Accepted
- Date: 2026-07-16

## Context

Playback scaled every frame into a fixed box in the decoder — `PLAYBACK_LONG`,
1600 px — and the UI zoomed whatever came out. On the 4K footage this tool
targets that is a 2.4× downscale applied before the picture ever reaches the
screen, so a zoom magnifies a resample: the detail it is reaching for was thrown
away by swscale two steps earlier. `ZOOM_MAX` let it run to 8× regardless, and
its own comment admitted what that bought — "on the 4K footage this tool targets
the source's detail was resampled away before the zoom could ever reach it …
Raising `PLAYBACK_LONG` is what would buy real detail, at a scaling and upload
cost paid on every frame of every clip whether it is zoomed or not."

That framed the choice as a dilemma: a soft zoom, or 4K decode costs on every
clip. It is a false one, because the two do not coincide. Detail is wanted only
where the zoom is; the cost would be paid everywhere. And a full-resolution decode
buys nothing at fit — a window cannot show more pixels than it has, so those
pixels are paid for and then thrown away a second time on the way to the screen.

The fixed box also quietly assumed the window. It does not hold: this machine
runs a 2560×1440 display at 125%, where a fullscreen video area is **2484
physical pixels** across. The 1600 px box was therefore magnified 1.55× before
anything was zoomed at all — the picture was a resample even at fit, on a window
the tool is used fullscreen on. A constant cannot know that, because the number it
needs is the window's, and the display scaling's.

The zoom's ceiling was the second defect. 8× was a number, not a limit: it bore
no relation to how far the source's pixels actually reach. Past 1:1 a zoom can
only magnify — bigger, never sharper — and where that starts is a fact about the
clip and the window, not a constant to pick.

## Decision

**The decoder scales to what the view displays.** `PlayCommand::SetLongSide`
retargets the live decoder's scaler; the UI sends the long side of the rect it
just painted, in physical display pixels, capped at the source's own. At fit that
is the window, and costs what it always did. Zooming walks it up towards the
source, on the frames of the one clip being studied.

**Zoom stops where the source's pixels run out.** `View::max_zoom` is
`native_scale / base_scale` — the scale at which one source pixel covers one
physical display pixel — replacing `ZOOM_MAX`. Physical, not logical: a point is
whatever the display scaling makes it, so holding 1:1 against egui's points would
still magnify the frame on any screen not set to 100%.

**Nothing is magnified past the source, the fit included.** `base_scale` is
`min(fit, native)`, so a clip smaller than the window is shown at its own size,
centered, rather than blown up to fill it. Such a clip has no zoom at all: floor
and ceiling meet.

**The view is measured against the source, never against the decoded frame.** The
decoded size now follows the zoom, so laying the view out from it would be
circular — the box asked for would derive from the image that box produced.
`PlaybackFrame` carries `src_w`/`src_h`, and `Player` keeps the source's size and
discards the texture's.

**A held frame is re-emitted on a rescale.** Nothing else would ever decode it
again, so a zoom onto a paused frame would leave it soft at the exact moment it
was zoomed into. The re-land is a full seek rather than a `start_move` to
`current_s`: that path skips forward from where the decoder stands and hands back
the *next* frame (ADR-0012 records the same asymmetry), shifting the picture under
the zoom.

**`ScrubCache` is dropped on a rescale.** It holds frames scaled to one box, so a
hit afterwards would put the old scale's pixels back on screen — the soft picture
the rescale exists to remove, on precisely the frames a scrub walks back over.

## Consequences

- A zoom now reaches the source's own pixels: at the cap one source pixel covers
  one display pixel and there is nothing further to uncover. That was the point.
- **The fit got sharper too, which was not the ask.** Driving the real app
  fullscreen on this machine (`camera_8s_4k.mp4`, 3840×2160, GPU decode, display
  at 125%) the view asks for **2484** at fit and the old build decoded 1600, so
  plain watching was magnifying a resample 1.55×. The zoom was the visible half of
  a defect that was there the whole time.
- **The zoom range shrinks, deliberately.** On that same window the cap lands at
  **1.55×**, where `ZOOM_MAX` allowed 8×; two `+` presses reach it and the rest are
  no-ops. At the cap the ask is exactly **3840** — the source. The lost headroom
  only ever magnified.
- **Watching costs more on a large window, and it should.** Per 4K frame, as RGBA:

  | box | pixels | per frame |
  |---|---|---|
  | old fixed 1600×900 | 1.44 MP | 5.8 MB |
  | fit on this fullscreen window, 2484×1397 | 3.47 MP | 13.9 MB |
  | the source, at the zoom cap | 8.29 MP | 33.2 MB |

  Fit now costs 2.4× the old box because the old box was under the window; the
  extra pixels are ones the screen actually shows. Decoding every clip at full
  resolution regardless would cost another 2.4× on top, on every frame, to look
  identical at fit — and 16× on a small window, where the fit box is ~1000 px.
  That is the cost this decision declines to pay.
- **Zoomed-in playback holds pace.** Measured on the real app, fullscreen, this
  machine, `camera_8s_4k.mp4` (25 fps, so a 40 ms budget): at the 1:1 cap, 104
  frames at a **40.4 ms mean interval with not one frame skipped** — the decoder
  never queued a second frame behind the one on screen. Fit, at 2484, gives the
  same 40.2 ms. Full-resolution playback is not the wall the old box's comment
  implied.
- **What the zoom costs is the texture path**, timed around
  `ColorImage::from_rgba_unmultiplied` and the upload that follows it:

  | box | per frame | of a 40 ms budget |
  |---|---|---|
  | 2484 (fit) | 3.43 ms | 9% |
  | 3106 | 5.35 ms | 13% |
  | 3840 (the cap) | 8.08 ms | 20% |

  It tracks the pixels, as it should, and 20% is affordable. The tester's GTX 1070
  on 4K30 has a 33 ms budget and ADR-0012 measured it ~1.8× slower than here, which
  would put this near 14 ms — tighter, not a wall. Unmeasured on that machine.
- **Scaling only the visible crop was considered and declined.** It would flatten
  the cost across the zoom rather than let it rise to 8 ms. Three things sank it.
  The frame already plays at pace at 1:1, so it fixes nothing that is broken. A
  fullscreen window shows **43%** of a 4K frame at the cap — a 2.3× saving, not the
  ~6× a small window suggests, and any pan margin worth having eats most of it
  back. And cropping in the decoder makes **panning a decoder round-trip**: today
  the whole frame sits in the texture and a drag is instant, where a crop would
  leave the newly exposed edges black until a re-scale lands. That is a certain
  regression on the one gesture a zoom exists for, bought against a speculative
  one. It would also make `ScrubCache`'s frames view-specific, which ADR-0012 did
  not intend. Revisit if a tester's log shows drops while playing zoomed in.
- **A zoom sharpens a beat late.** The frame on screen keeps the old scale until
  the re-land arrives — one precise seek, ~30 ms on 4K per ADR-0009. Scaling in
  the decoder is what makes that unavoidable.
- **The scrub cache holds fewer frames when zoomed**: ~22 at fit against ~3 at 1:1
  on 4K within the same `SCRUB_CACHE_BYTES`, and it starts empty after every
  rescale. Stepping across a stretch while zoomed in re-decodes where ADR-0012
  made it free. Zoom is for studying a frame, not walking a stretch.
- **A wheel ramp asks for a new scale per notch**, each ask rebuilding the scaler
  and, when paused, re-landing. `play_stream` already folds its whole command
  queue before acting, so a burst collapses to its last value and the decoder pays
  once per batch rather than once per notch. There is no debounce beyond that; if
  a ramp feels heavy, quantizing the ask is the cheap fix.
- **Clips smaller than the window no longer fill it.** Nothing in the target
  archive is — it is 4K throughout — but a stray SD clip now sits small and
  centered. That is the honest rendering of "never magnified past the source", and
  it may still read as a bug to someone who did not ask for it.
- `PLAYBACK_LONG` survives as `PLAYBACK_LONG_START`, a starting guess until the
  first repaint reports the real figure — measured at 8 ms after the first frame,
  and while playing, so it costs only a scaler rebuild. It is a poor guess for a
  fullscreen window (1600 against 2484) and that no longer matters, which is the
  point of it being a guess.
- Bilinear downscaling at fit is untouched and still aliases 4K on the way to
  1600. It is the picture the tool has always shown, and it is its own decision.
