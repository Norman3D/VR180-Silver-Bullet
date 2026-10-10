# Changelog

## Unreleased

### Insta360 X6: automatic per-frame eye-exposure matching

The X6's two sensors auto-expose independently — shutter *and* gain — so the
two eyes of a VR180 clip differ in brightness, and the difference moves with
the light: on real post-mod footage the eyes sat up to about a stop apart
for seconds at a time, then drifted back. The static "Matching Eyes"
exposure trim cannot follow that.

- **Auto-match eye exposure (per frame)** — a new switch in the Colour
  panel's Matching Eyes section, shown for X6 clips. Every frame, the
  brightness of the two eyes is measured on the GPU over the central region
  both eyes share (a luma histogram; the mean shift of five quantiles across
  the rank range both eyes keep once their clipped tails are cut is robust
  to the content only one eye sees), and the difference is
  cancelled with a log-domain code offset before the LUT — on I-Log that is
  an exposure gain, so colour is untouched. Clipped highlights and crushed
  blacks are cut out by rank, jointly for both eyes, so sky in the frame
  does not water the correction down. The frame loop never waits on the
  GPU: the export reads nothing back, and the preview's readout arrives
  through a small asynchronous copy ring. A reference picker chooses
  whether the eyes meet in the middle (default) or one eye follows the
  other; a readout shows the measured difference and the offsets applied,
  in EV.
- Temporal behaviour: the camera's AE steps are taken at once (a lagged
  step would leave the eyes mismatched for several frames); the small
  frame-to-frame jitter is low-passed. A seek resets the smoothing.
- The static Eye Exposure trim still composes on top, and the whole thing
  is off for other cameras, whose sensors expose together.
- Also fixed in the preview: in the single-eye "right" view the per-eye
  trims (CT / tint / exposure) were applied to the wrong eye; they now
  follow the eye they belong to.

### GoPro .360: stabilization timed to each frame's exposure, and to the frames the camera actually dropped

Two timing gaps in the GoPro path, found by rigging a GoPro MAX, a DJI
Osmo 360 and an Insta360 X6 together and stabilizing the same walk and
run through all three.

- **Per-frame exposure.** The orientation sample now sits at the centre
  row's mid-exposure — frame start + readout/2 − shutter/2, from the
  per-frame shutter the camera records (`SHUT`) — the rule the DJI and X6
  paths use, and the rolling-shutter correction is centred on that same
  instant. In daylight nothing changes (a 1 ms shutter moves the sample
  by half a millisecond; the old midpoint measured right to within
  ~1.5 ms); in low light it matters: at a 1/30 s shutter the pose was
  sampled 16 ms late, shake that followed the motion. On a dark running
  section the locked residual drops 12 % and its motion-correlated part
  disappears.
- **Dropped frames.** When the camera's encoder falls behind it drops a
  sensor frame and repeats the previous image to keep 30 fps; the gyro
  and the telemetry block timestamps keep running, so that block is one
  frame period short. The gyro timeline used to be stretched evenly over
  the block's 30 video frames, mis-timing every frame after the repeat by
  up to a frame period — a ~0.7 s burst of jerks per drop. The camera
  marks these in its `MSKP` ("frame skip") stream; the gyro is now
  re-anchored per frame from it, and a repeated frame receives the pose
  and RS correction of the image it copies. The test clips had one to two
  drops each, both under heavy motion.

With both in place the GoPro's frame-to-frame jitter on the 30 fps test
clip matches the Osmo and X6 second by second; the GoPro still looks
worse than the 50 fps cameras simply because each 30 fps frame carries
1.7× the motion.

### Insta360 X6 stills: open a `.dng`, export a VR180 still — as PNG, JPEG or a linear DNG

The X6 writes its photos as one 15520×7760 DNG with both fisheye circles
side by side, 14-bit Bayer, the factory lens model in the EXIF and an
Insta360 trailer. The app now opens them like any other source: the raw
stage (the camera's per-channel gain maps, a 5×5 linear demosaic, black /
white levels) runs on the CPU, each eye goes through the same exact
UCM fisheye projection the X6 video path uses, and the result is one
side-by-side half-equirect still — 8192×4096 or the full 15520×7760.

Three containers in the export bar:

- **PNG (16-bit)** and **JPEG** — developed: as-shot white balance, the
  DNG colour matrix, the camera's baseline exposure, a filmic tone curve.
- **DNG (linear raw)** — the camera-native linear data, demosaiced and
  reprojected but not white-balanced or tone-mapped, with the source's
  colour matrix, neutral, baseline exposure and EXIF carried over. Raw
  developers (Lightroom / Camera Raw / Apple's) open it as the X6's own
  raw and apply their profile, so the VR180 still can be graded with the
  full raw latitude. Lossless-JPEG tiles (the layout Adobe's own
  converter writes); validated with exiftool, macOS ImageIO and Adobe
  DNG Converter.

Stills have no timeline or stabilization (those sections are hidden);
view adjust, stereo alignment, eye swap and the colour stack apply as for
video. The grade bakes into PNG / JPEG; the **linear DNG carries it as
metadata** instead: white balance → `AsShotNeutral`, the lift / gamma /
gain / shadow / highlight tone → `ProfileToneCurve` (the app's tone curve
composed with the CDL, honoured by Adobe and Apple), saturation →
`ProfileLookTable` (Adobe), the raw exposure offset → `BaselineExposure`;
the per-eye "Matching Eyes" trims are baked exactly in camera-linear
space (one DNG has one profile). A 3D LUT and sharpening cannot travel as
DNG metadata and are left out of the DNG (the export panel says so).

With a still loaded, everything that cannot act on one photo is hidden
rather than greyed: the transport row (play / stop / snapshot / trim),
the timeline, stabilization, rolling shutter, noise reduction, Auto align
and the built-in log LUT buttons. With DNG output selected, the 3D LUT and
Detail sections go too, and the preview drops them so it shows exactly
what the DNG will carry. The Output section's Fisheye and Reframed modes
and the eye swap / upside-down toggles now work for stills, and the lens
Override is honoured by the still export as it already was by the preview.

Fixed: exporting the loaded still left the photo frozen — the export
stopped the preview thread (as it does for a video that shares the file)
and a still has no Play button to start it again, so the sidebar and the
pan drag no longer did anything until the clip was reloaded. The still's
preview now stays live through its own export (it holds only textures;
the export re-reads the file), and a still whose preview did stop gets a
"Restart preview" button. Found alongside: Stop before the first frame
left Play dead until a reload; the snapshot key on a still waited out a
15 s timeout and saved the working-res preview (it now points at Export);
Space on a still spun the playback repaint cadence; a restart of the same
clip re-seeded hand-tuned lens Override values from the file; and the
audio probe opened the whole DNG on the UI thread.

The still preview is no longer the video's 768-px working size: a photo
has one frame, so once the sliders rest it renders at the source's
working resolution (7760×3880 side by side on the X6; the reframed view
at 3880 px per eye), and the zoom magnifier looks into that image. While
a slider or the pan is moving, quick renders at the preview size keep it
fluid; the full one follows ~120 ms after the last change. The "Preview
size" menu is not offered for photos.

### GoPro .360: rolling-shutter correction audited and tightened

The GoPro rolling-shutter term was measured directly for the first time:
raw (unstabilized) frames exported with the correction off, on, doubled
and negated, and the residual inter-frame warp regressed against the
change in angular rate. The result: the correction has the right sign and
the right axis (the sensor is read top to bottom, on both lenses, and the
left eye behaves exactly like the right), and each unit of correction
removes 15.3 ms of readout — the file's 15.224 ms within 1 %. Three
things that were assumptions are now derived or physical:

- **Readout time from the file.** The per-row RS window and the
  mid-exposure pose instant now take the sensor readout from the
  recording's `SROT` setting; 15.224 ms is only the fallback for a file
  without one.
- **The sensor row of a pixel is computed from the ray the lens actually
  imaged** (the camera-frame direction after the stabilization rotation),
  not from the output direction before it. The old choice was a
  small-rotation approximation inherited from the reference pipeline; it
  was off by the whole applied rotation, which in camera-lock mode is
  tens of degrees.
- **Each row group carries the camera's mean rate between the reference
  row and that row**, so the per-row correction is the integrated
  rotation rather than the instantaneous rate at the row — the two differ
  by half the angular acceleration times the row offset squared, which
  only matters in jolts.

### The app is just "VR180 Silver Bullet" now
- The Windows installer, its Start-menu and desktop shortcuts, the Add/Remove
  Programs entry and the executable's own name (Task Manager, Properties) all
  dropped the "2.0" — the product name is no longer version-specific; the
  version lives in the version field. Upgrading replaces the old-named
  shortcuts; settings and the install location are untouched.

## 2.5.1 — 2026-09-25

### macOS: side-by-side sources — 8-bit joins the zero-copy path, and it uses far less memory

Two changes to the zero-copy path for generic side-by-side `.mp4` / `.mov`
sources on macOS.

- **8-bit sources now take the fast path.** Until now only 10-bit HEVC/H.264
  qualified; an 8-bit clip fell back to the CPU decoder. VideoToolbox hands
  8-bit frames back as NV12; those planes are now wrapped as 8-bit textures
  and the kernels expand the range from a per-source constant instead of a
  hard-coded 10-bit one. On an 8192×4096 8-bit clip a 120-frame export went
  from **21.95 s to 3.73 s** and peak memory from **3.14 GB to 1.03 GB**.
  Colour checked against the BT.709 formula applied to the raw codes: the
  new path lands within 0.28/255 on flat areas (the CPU path it replaces
  was 0.78/255 dark); the preview within 0.01/255. The range tag is
  honoured too — a full-range (`pc`) source no longer gets limited-range
  constants — on both 8- and 10-bit, and on the CPU fallback as well (its
  scaler now passes the tag to swscale), so the preview, the paused still
  and every export agree.
- **No more whole-frame intermediate.** The side-by-side path used to
  convert the entire frame to RGBA16 and then copy out two per-eye halves
  before projecting: at 8K that is a 268 MB texture plus two 134 MB halves
  per frame, and two full-frame passes. The projection kernels now sample
  each eye straight from the decoded planes, the same way the DJI and X6
  paths always have, and the preview resolves each eye from its own half.
  Peak memory for the 10-bit 8K export: **2.20 GB → 1.65 GB**. The DJI and
  X6 paths are bit-identical to before. A side effect worth knowing: the old
  path put the left eye's last column exactly on the chroma boundary with the
  right eye and blended it in — one column of wrong colour along the seam,
  in both preview and export. Sampling per eye fixes that.

Accepted: 8- and 10-bit H.264/HEVC in 4:2:0. ProRes and 4:2:2 profiles still
use the CPU path.

### Insta360 X6: eye order and stabilization direction fixed for real post-mod footage

The X6 path was wired before any modded camera existed, on an assumption about
which way the mod turns the back lens. The first post-mod footage showed the
assumption was mirrored: the right eye landed on the left of the frame, and
stabilization pushed the wrong way on both eyes — every correction added shake
instead of removing it ("twice as shaky").

- **Eye order.** Stream 0 (the back lens) is now the RIGHT eye by default,
  the same convention as the DJI Osmo 360. Nothing in the shared pipeline
  changed — the X6 loader simply labels stream 0 as lens A, as DJI does, and
  the iterator, calibration, per-eye rotation and rolling-shutter rows all
  follow. If you had turned **Swap eyes** on as a workaround, turn it off.
- **Stabilization.** The IMU→camera basis was measured on a stock camera
  against the back lens. The mod turns that lens 180° about the body's
  vertical axis to face the screen-lens side, so both lenses now share the
  stock frame with x and z reversed. Measured on three post-mod clips: with
  the old basis a camera-lock left 1.6–1.9× the *unstabilised* frame-to-frame
  motion; with the corrected basis 0.00× (a perfect lock) on windows with
  pitch/roll jitter, and soft-stab removed 74–100 % of the jitter on every
  window tested while the old basis made each one worse.

Pre-mod (stock, 360°) X6 files are not a target of this app, so no behaviour
was kept for them.

### Fixed: Windows machines with two GPUs
- On a hybrid Windows machine (an integrated GPU alongside a discrete one,
  which is most laptops) the hardware decoder could land on a different GPU
  from the renderer. The zero-copy preview would then hang before the first
  frame, stuck on "Loading stabilization data…", and a stopgap that shipped
  briefly dropped every such machine to the slow CPU path instead. The decoder
  is now created on the same GPU the renderer chose and its identity verified
  before use, so hybrid machines keep the GPU fast path. Verified on both GPUs
  of a 4090 + Intel UHD box. Contributed by @Norman3D.

### macOS: side-by-side sources get the zero-copy GPU path

`.360`, `.osv` and `.insv` clips have decoded straight into GPU memory on
macOS for a while: VideoToolbox hands back an IOSurface, the app wraps it as
a Metal texture, and the frame never touches the CPU. Generic side-by-side
`.mp4` / `.mov` sources were the one format left out — they still decoded on
the CPU and uploaded every frame. That gap is now closed, for both preview
and export.

- **Side-by-side export is ~3.7x faster and uses ~6x less memory.** On a
  3840x1920 10-bit HEVC clip a 120-frame export went from **4.73 s / 697 MB**
  to **1.29 s / 118 MB**. The whole frame is resolved once on the GPU and the
  two eyes are split with a subregion copy, so there is no swscale pass and no
  per-frame upload.
- **Colour is closer to the source, not just faster.** Measured against a
  known-good reference the new path lands at a mean error of 0.46/255 where
  the CPU path measures 0.72/255 — the CPU path's swscale conversion rounds
  slightly dark. Eye order, trim and frame-accurate seek all match the old
  path exactly.
- Requires a **10-bit H.264/HEVC** source, which is what VideoToolbox can
  hand over as P010. Anything else — 8-bit, ProRes, a multi-segment clip, or
  an export with temporal noise reduction on — falls back to the existing CPU
  path automatically, with the reason in the log. Windows is untouched.
- **Fixed:** the source bit depth was read from `bits_per_raw_sample`, which
  is frequently `0` even for genuine 10-bit files (any x265-muxed clip, for
  one). The depth now falls back to the stream's pixel format, so the fast
  path actually engages instead of silently declining every clip.

### macOS: side-by-side sources decode several times faster
- **Generic side-by-side `.mp4` / `.mov` sources now decode multi-threaded
  on macOS.** libavcodec defaults a decoder to ONE thread and nothing in
  the app ever set otherwise, so these files — the catch-all path for any
  clip that isn't `.360` / `.osv` / `.insv` — decoded on a single core.
  A 301-frame reframe export of an 8256x4128 10-bit HEVC clip went from
  **114.6 s to 52.1 s (2.2x)**, and preview decode from 4.3 to ~18 fps.
  Output is byte-identical to before. Thread count is capped at 6 (the
  knee of the speed/memory curve) and is skipped entirely when a hardware
  decoder is driving, which leaves Windows exactly as it was.
- Hardware (VideoToolbox) decode for these sources is available behind
  `VR180_SBS_VT=1` but is **off by default**: it measured *slower* than
  threaded software decode here, because the frame has to be copied back
  from the GPU for the CPU pipeline, which costs more than the decode it
  saves. (On Windows the equivalent frames stay on the GPU, which is why
  it pays there.) It is restricted to H.264/HEVC and skipped for 8-bit
  full-range clips, both cases where it would otherwise change colour.
- **Fixed:** the cached scaler was built once from the first frame and
  reused forever. It now revalidates per frame, so a decoder that changes
  pixel format mid-stream rebuilds instead of failing every subsequent
  frame, and a frame that changes *size* mid-stream is now a clear error
  rather than a mis-split image.

### Fixed: 8-bit side-by-side input on Windows
- **8-bit SBS sources failed on the Windows GPU fast path.** The hardware
  decoder's frame format follows the source bit depth — 8-bit H.264/HEVC
  decodes to NV12, 10-bit to P010 — but the D3D11 YCbCr→RGB converter asked
  for 16-bit plane views unconditionally, which an NV12 surface rejects
  outright. The path was chosen on a GPU capability rather than on the
  source's actual format, so it committed to the fast path and then died on
  the first frame, with no fallback left. Since the fast path only started
  taking generic SBS files two days ago, this never shipped.
- The converter now reads the source's format and picks matching plane views
  and range constants, so **8-bit side-by-side input gets the same GPU fast
  path 10-bit already had** rather than merely falling back. Output is RGBA16
  either way, so nothing downstream changes, and 10-bit output is unchanged
  bit-for-bit. Anything the converter genuinely cannot sample is now declined
  up front, where falling back to the portable path still works.
- The same converter now honours the clip's **colour range tag**: a
  full-range (`pc`-tagged) side-by-side clip renders identically on the GPU
  fast path and the CPU fallback, using the same four expansion constants
  the macOS path uses. Previously the fast path always assumed limited range.

### Matching Eyes: exposure
- The **Matching Eyes** panel gains an **Eye Exposure (±EV)** slider
  alongside Eye CT and Eye Tint. Like them it applies oppositely to the two
  eyes — left `+`, right `−`, in stops — so a brightness difference between
  the two lenses can be dialled out without changing the overall level. A
  lens-to-lens exposure mismatch is a common cause of binocular rivalry (the
  eyes "fighting" instead of fusing), and until now only its colour could be
  corrected. Range ±0.5 stop, 0 = off; ↑/↓ nudge by 0.01.
- **Fixed:** the Matching Eyes trim was silently dropped on **8-bit exports**.
  That arm grades the composed side-by-side frame in one pass, so both eyes
  got the same (un-trimmed) plan, while every 10-bit and zero-copy arm applied
  it per eye. 8-bit now grades each half with its own plan. Output with no
  trim set is byte-identical to before.

### Snapshot
- **📷 Snapshot** button in the transport bar (`S`; Stop no longer has a
  hotkey): saves the
  frame on screen — grade, stabilization, reframe and all — as a 92 %
  JPEG in the export output folder (next to the source when none is
  set), named `<source>_frame<N>.jpg` (a second shot of the same frame
  gets `-2`, `-3`, …, so several reframed compositions of one frame all
  keep). Rendered at the **export's resolution** — native / 8K for
  VR180, the chosen per-eye size for Reframed — through the full-detail
  still path; a snapshot mid-playback pauses on that frame first.

### 3D display output (new)
- **"3D display" toggle** in the toolbar: shows the stereo pair on a
  side-by-side 3D monitor or AR glasses that appear to the OS as one wide
  screen (e.g. 3840×1080) — left eye in the left half, right eye in the
  right, on a chrome-less fullscreen output that follows the live preview
  (pan / zoom / grade / stabilization included). When a 3840×1080-class
  side-by-side screen is connected (macOS and Windows) the output goes
  fullscreen on it automatically; the preview switches to SBS and each eye
  renders at half the screen width so a 16:9 reframed eye fills its half
  1:1 (other aspects are letterboxed inside each half). `Esc` closes it.
  With no such screen connected a movable window opens instead — drag it
  onto the 3D screen and press `F` for fullscreen. macOS: honours
  "Displays have separate Spaces" (native fullscreen on the glasses when
  on, a borderless window at the screen bounds when off — each is the
  only mode that works under that setting).

### Reframed (Flat 3D) export bitrate
- Reframed exports now have their **own H.265 bitrate** (10–300 Mbps),
  separate from the VR180 rate, **seeded from a per-size recommendation**
  — about 0.28 bits per pixel per frame at 30 fps, ×1.5 at 60 fps:
  3840×1080 → 35, 5120×1440 → 60, 7680×2160 → 140 Mbps at 30 fps
  (20 / 35 / 80 for the 2:1 sizes). The Format window shows the
  recommendation for the current size and clip frame rate; dragging the
  slider pins a custom value, "Use recommended" un-pins it. Previously
  the shared 200 Mbps VR180 default applied to Flat 3D too — 5–15× more
  than those frames can use.

### Generic side-by-side input
- **"Dewarp fisheye input" toggle** (Source panel) for plain `.mp4` /
  `.mov` side-by-side sources. Off by default: the file is taken as an
  already-dewarped VR180 half-equirect SBS and sampled directly, so a
  finished VR180 export can be reframed to Flat 3D (or re-aligned /
  re-graded) without the fisheye dewarp distorting it. Turn it on for raw
  dual-fisheye SBS recordings, which then use the Fisheye lens settings
  as before. `.360` / `.osv` / `.insv` are unaffected — their lens model
  always comes from the file.
- Fixed: 10-bit (H.265 10-bit / ProRes) exports of a plain side-by-side
  source failed at the first frame with a wgpu validation error — the
  SBS decoder always produced 8-bit frames while the 10-bit arms expect
  16-bit ones. The SBS decoder now follows the output bit depth, so a
  10-bit SBS source also keeps its precision.
- **Laptops with two GPUs no longer crash on the fast path.** When the
  video decoder and the renderer ended up on different GPUs (common on
  laptops with both an integrated and a discrete chip), the accelerated
  path could take the app down. It now detects the mismatch and uses the
  compatible path instead — slower, but it works.
- **Failures are now reported instead of looking like success.** An export
  that lost its decoder mid-run used to finalize a short file and report
  "done"; it now fails with the reason (cancelling still keeps the partial
  file, as before). A crash inside the preview decoder no longer leaves
  Play/Pause toggling a dead worker. And a GPU error is caught and shown
  as a dismissable warning instead of taking the whole app down.
- **Windows: hardware decode now checks free GPU memory first, and falls
  back to software when it will not fit.** A hardware video decoder needs
  roughly 240 MB of GPU memory per megapixel of video, so an 8K clip wants
  about 8 GB and a dual-lens camera about 7 GB. On a card that cannot spare
  that, the app now decodes in software automatically — slower, but it
  works — instead of the preview silently freezing or an export stopping
  early. The export bar says why ("not enough GPU memory for hardware
  decode (7.9 GB needed, 5.9 GB free)"). Cards with room are unaffected.
  `VR180_NO_HW_DECODE=1` forces software decode; `VR180_FORCE_HW_DECODE=1`
  skips the check.
- **Windows: side-by-side sources are now GPU-accelerated end to end** —
  hardware (NVDEC) decode, GPU eye split, projection, color and encode,
  with the audio muxed inline. Exports measured 5 → 37 fps for a 4K
  H.265 export (~7×; ProRes rides the GPU encoder the same way), and
  **playback now uses the same zero-copy path** instead of downloading
  and converting every frame on the CPU — the SBS decoder had never used
  hardware decode at all, which hit 8K side-by-side files hardest.

## 2.5.0

### Reframed output mode (new)
- **Format → "Reframed (Flat 3D)"**: a pinhole-style side-by-side view
  of each eye instead of the VR180 half-equirect — zoom (horizontal FOV),
  pan / tilt / roll, a **Defish** blend from rectilinear to a fisheye look,
  and a 1:1 or 16:9 per-eye frame. Stabilization, stereo offsets, per-row
  rolling-shutter correction and the lens override all still apply.
  Available for DJI OSMO, Insta360 X6 and GoPro sources.
- Drag the preview to pan, scroll or pinch to zoom, double-click to
  recenter. The preview renders from the native frame and a paused frame
  shows the native-resolution still.
- Export writes the exact viewport at 1080 / 1440 / 2160 lines per eye —
  2:1 square or **32:9 side-by-side** (3840×1080 … 7680×2160) for AR
  glasses — with no VR180 metadata. The BeyondVR hack does not apply.

### Stabilization
- **Timing is derived from the file, no more IMU phase slider.** Each
  frame's pose is sampled at the centre row's mid-exposure using the
  file's sensor readout, exposure record and timestamps, so dark and
  bright clips and every frame rate are timed right automatically
  (verified on 25 / 30 / 50 fps DJI clips, consistent with DJI Studio's
  output).
- **Insta360 X6 (`.insv`)**: factory lens model, gyro stabilization
  matched to Insta360 Studio's output, per-sensor exposure timing, and
  Insta360's official X6 I-Log→Rec.709 LUT (v2, 65-point) bundled and
  auto-applied like the DJI and GoPro ones.
- **OSMO 360 II** support and **Auto align** stereo alignment; GoPro
  chapter-safe firmware-RS detection.
- **Camera lock** is now an explicit toggle in every stabilization panel
  (the GoPro `.360` panel gains one; OSV/INSV already had it). It locks the
  view to the first frame and ignores the smoothing *and* max-correction
  controls — fully locked no matter what the camera does. The old
  "Smooth = 0 means lock" convention is gone; Smooth is now a pure
  smoothing amount and grays out (with Max corr / Response) under the lock.

### App / UX
- Removing the currently loaded clip from the clip list now unloads it:
  the app activates the next remaining clip, or returns to the empty
  "no clip loaded" state when the list is emptied.
- Export progress / ETA fixed: totals now honour each clip's trim (a
  trimmed export used to stall short of 100% with an inflated ETA), and
  the rate is measured from the first written frame instead of the run
  start, so load / encoder start-up no longer counts as encode time.
- **Windows: GoPro `.360` ProRes (and software H.265) exports now run on
  the GPU fast path** — GPU decode, assembly, projection, color and 4:2:2
  compose feeding the GPU ProRes encoder (~5× at 8K, was a serial CPU
  loop with the GPU idle). Merged multi-chapter recordings included.
- Windows: the app now explicitly prefers the Vulkan GPU backend (all
  fast export paths require it), and when an export does land on a slow
  path the export bar says why (e.g. "CPU export path — ProRes GPU
  encoder unavailable") instead of just being slow.
- **Exports no longer stall at 100%**: stereo-audio exports on every
  path on both platforms — Windows hardware H.265 (NVENC) and the macOS
  GoPro `.360` zero-copy arm included — now mux the audio inline while
  encoding, straight into the final file (merged
  multi-segment recordings too) — previously the whole encoded video was
  rewritten afterwards to add the audio, which at ProRes bitrates could
  take longer than the encode itself. Paths that still need the second
  pass (ambisonic / APAC) now show "muxing audio…" in the export bar
  instead of sitting silently at 100%.

### Since 2.0.0
- Seamless auto-update (2.1.0), `.360` lens calibration override, ProRes
  4:2:2 with GPU compose, 8K export default, Matching Eyes white-balance
  trim, BeyondVR Hack (VR180 output only), frame-exact multi-segment seams.

## 2.0.0

The `2.0` clean-room rewrite of VR180 Silver Bullet — a native Rust + wgpu
application replacing the Python/PyQt6 app. **The headline addition is full
support for the OSMO VR180 Mod** (`.osv`). One self-contained binary
per platform, no Python runtime, no system `ffmpeg`. Runs on **macOS (Apple
Silicon)** and **Windows (NVIDIA)**.

### Cameras & formats
- **OSMO VR180 Mod** (`.osv`) — **the headline of 2.0.** Exact
  per-lens factory dewarp loaded from the file (5-coefficient Kannala-Brandt
  + Brown-Conrady tangential), with output on par with DJI Studio.
- **GoPro Max 2 VR180 Mod** (`.360`, EAC) — full GPU pipeline: zero-copy decode,
  noise reduction, and **automatic firmware vs no-firmware rolling-shutter
  detection** from the CORI stream (manual override retained).

### Engine
- GPU-first: `wgpu` compute (Metal / DX12 / Vulkan) with WGSL shaders.
- In-process video I/O via `ffmpeg-next` 8.1 (no subprocess).
- **macOS:** VideoToolbox zero-copy P010 decode/encode through IOSurface,
  HEVC and hardware ProRes.
- **Windows:** GPU-resident export — NVDEC → wgpu → CUDA → NVENC, no CPU
  readback (~36 fps @ 8K on a 4090), libx265 fallback.
- 10-bit end-to-end (Rgba16Unorm intermediates) when 10-bit output is
  selected — decode, projection, color stack, and encode all hold ≥10-bit.

### Stabilization & rolling shutter
- Camera-lock and velocity-dampened soft-stab (adaptive smoothing with a
  **Response** slider and a soft elastic correction limit).
- Per-scanline rolling-shutter correction from measured sensor-readout
  timing; gravity/horizon alignment.
- Precise OSV IMU stabilization + rolling-shutter timing (SROT, IMU phase), matched to DJI Studio.
- **New:** GoPro Max 2 VR180 Mod (`.360`) firmware-RS mode is auto-detected
  per clip from the CORI signal (the toggle still overrides).

### Color
- CDL, 3D LUT (DJI D-LogM→Rec.709 bundled + autoloaded), white balance,
  saturation, sharpen, mid-detail — identical stack in preview and export,
  matched to the Python app.

### Noise reduction
- Temporal NR via Apple `VTTemporalNoiseFilter`, ported **in-process** (objc2
  FFI, no Swift helper), fully 10-bit, GPU-resident zero-copy for OSV and
  `.360`. Export-only; macOS-only (auto-hidden where unsupported).

### Output & delivery
- Half-equirect VR180 SBS, or a normalized equidistant fisheye SBS matched
  to the lens — **195°** for the OSMO VR180 Mod, **185°** for the
  GoPro Max 2 VR180 Mod.
- Native or **8192×4096 (8K)** resolution.
- H.265 or ProRes; **Vision Pro (APMP)** and **YouTube VR180** metadata
  injection; **APAC spatial** / ambisonic / stereo audio; OSV audio
  passthrough.
- Trim-accurate exports (video + audio aligned to the trim range).

### App / UX
- Native desktop GUI (eframe/egui + egui-wgpu + wgpu 29).
- **Unified batch + export:** one queue for single or many clips, a
  persistent bottom export bar with overall progress + ETA, per-clip
  multi-select, and a completion notification. (The separate batch and
  export-options windows were removed.)
- Preview modes (SBS / anaglyph / 50% overlay / single eye), zoom magnifier
  with a native-resolution still, per-eye view adjustment, upside-down mount.
- **Localized UI: English / 简体中文** (live toggle, bundled CJK font).
- Settings persist per-OS; RS mode and IMU phase are per-clip.

### Packaging
- macOS: signed + notarized `.app` / `.dmg`.
- Windows: Inno Setup installer (per-user, Start Menu shortcut).
