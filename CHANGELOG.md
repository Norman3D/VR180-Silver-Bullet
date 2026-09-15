# Changelog

## Unreleased

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
