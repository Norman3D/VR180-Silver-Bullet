# CLAUDE.md — orientation for Claude Code sessions

Auto-loaded at session start. Read this first; deeper detail is in the
`docs/` pointers at the bottom.

## ⚡ Status: **Unreleased on `main`: X6 automatic per-frame eye-exposure matching (2026-09-27, item 15) + GoPro per-frame exposure timing from SHUT (2026-10-07, item 16).** 2.5.1 — macOS RELEASED 2026-09-25 (tag v2.5.1 @ 7e345ee: DMG + mac app.zip + latest.json live, feed serves 2.5.1); **Windows RELEASED 2026-09-25, RESPUN same day with the version-free product name** (setup.exe + portable zip + MERGED latest.json live on v2.5.1; the installer, shortcuts, Add/Remove entry and exe now say "VR180 Silver Bullet" — AppId and the settings dir deliberately unchanged, `[InstallDelete]` clears the old-named shortcuts on upgrade; live manifest byte-identical to local, installer minisig verified against the updater.rs pubkey; feed serves BOTH platforms). Previous: 2.5.0 RELEASED 2026-09-13, macOS respun 2026-09-14 (adds 3D display output, Snapshot, generic-SBS input toggle, Flat 3D bitrate defaults; the 2.5.0 Windows respin was never done — superseded by 2.5.1) — reframed mode, file-derived stabilization timing, Insta360 X6

VR180 Silver Bullet **2.0** is feature-complete and building/running on both
**macOS (Apple Silicon)** and **Windows (NVIDIA)**. The Windows GPU-resident
NVENC path and the macOS feature batch below were verified on both platforms
(DX12/Vulkan + NVENC on an RTX 4090; Metal on Apple Silicon). See
[CHANGELOG.md](CHANGELOG.md) for the user-facing 2.0 summary.

**2.5.0 bumps from 2.1.0** (auto-update, released) with the reframed output
mode, stabilization timing derived from the file (no IMU phase slider),
Insta360 X6 support, OSMO 360 II support and Auto align — see items 7–9
below and [CHANGELOG.md](CHANGELOG.md). **2.1.0: seamless auto-update** — see
[docs/AUTO-UPDATE.md](docs/AUTO-UPDATE.md) for the full design + release
recipe. `crates/vr180-gui/src/updater.rs`: minisign-verified updates from
GitHub Releases (`latest.json` at the stable `releases/latest/download/`
URL), toolbar badge + popover UX, whole-`.app` swap + relaunch on macOS
(E2E-verified with real signing), silent Inno install on Windows.

**Windows session TODO to catch up (2.1.0):**
1. Pull; `cargo build --release -p vr180-gui` (new deps: ureq,
   minisign-verify — pure Rust, no system libs).
2. Copy the updater signing key from the mac box to
   `C:\Users\<user>\.vr180-updater\vr180-updater.key` (private channel;
   NEVER commit) and install the `minisign` CLI (scoop or release binary).
3. Rebuild the installer with the updated `installer/windows.iss`
   (AppVer 2.5.0 + the new silent-relaunch `[Run]` entry +
   `CloseApplications`).
4. VERIFY the Windows update path (built here, not yet run on Windows):
   serve a `latest.json` locally (`VR180_UPDATE_URL` override), click
   Install & Restart → the app must exit, the installer run silently
   (per-user, no UAC), and the app relaunch. Watch for: the spawned
   `-setup.exe` inheriting a lock on itself (it runs from the staging dir
   under `%APPDATA%\VR180SilverBullet2.0\updates\` — fine), and the
   `[Run] Check: WizardSilent` entry actually firing.
5. At release: `node scripts/make-latest-json.mjs --version 2.5.0
   --notes-file <notes>` on BOTH machines (mac first, copy its
   `release-staging/latest.json` over so the entries merge), then upload
   both artifacts + `latest.json` to the `v2.5.0` release.
6. **BeyondVR Hack** (final-stage per-eye output scale, ⚙ Format window)
   landed after the seam fixes — pull + rebuild + re-upload the Windows
   artifacts so both platforms carry it (mac 2.1.0 artifacts already
   respun with it). No Windows-specific code: the stage rides the shared
   color stack (`ColorStackPlan.eye_scale`), incl. the NVENC/d3d11 arms.
7. **Stabilization timing is derived from the file — no phase constants,
   no slider** (`dji_imu::frame_sample_time_s`, 2026-09-09). Each frame's
   pose is sampled at the centre row's mid-exposure, `readout/2 +
   (video_ts − block_ts) − shutter/2` after its motion block starts: the
   readout from the file's line time (`DjiOsvImu::readout_ms`, header
   `[1.13.1]`, II `[1.11.1]`, × rows; `dji_osmo_readout_ms_for_fps` is only
   the fallback), the shutter from the per-frame exposure record
   (`exposure_ms`, `[3.2.4.1]` as a fraction of a second) and the two
   timestamps (`frame_ts_offset_ms`). Insta360 timelines are synthesized
   on the measured content time (`SampleAnchor::BlockMid`). Measured on
   the stabilized output of five clips (1/500 s → ≈9 ms, 1/120 s → ≈5.5 ms,
   1/40 s → ≈−3 ms) and consistent with DJI Studio's output; the former
   8.5 ms and 5.5 ms @ 25 fps rules were this formula on particular clips.
   The "IMU phase" slider and `Settings::dji_imu_phase_ms` are gone — **do
   not reintroduce tuned phase numbers**; a new camera or mode gets its
   timing from its metadata. Diagnostics (env): `VR180_IMU_PHASE_OFFSET_MS`,
   `VR180_DJI_EXPOSURE_COMP=0`, `VR180_DJI_RS_SHIFT_MS`. Pipeline-level,
   shared code — pull + rebuild; no Windows-specific work.
8. **Insta360 X6 `.insv` support** (`SourceKind::Insta360Insv`; the official
   X6 10-bit I-Log→Rec.709 LUT v2 is bundled as `BUILTIN_X6_LUT` and
   auto-applied on load like the DJI/GoPro builtins — every X6 clip seen so
   far is I-Log, no profile flag found in the metadata): trailer
   parser (`vr180-fisheye/src/insta360.rs`), raw-gyro → VQF → the shared
   DJI-style stab stream (`vr180-pipeline/src/insv_imu.rs`), measured X6
   lens preset, stream 0 = back lens = LEFT eye by default
   (`SourceKind::dual_stream_iter_swap`). The dual-stream iterators now
   stage packets per stream (`DualPacketPump` in `fisheye_decode.rs`) —
   the X6 interleaves its two video streams in 1 s chunks, which the old
   feed-as-you-read loops turned into dropped packets + mis-paired eyes.
   Lens calibration is read per file (the trailer's factory string, a
   UCM + radial-polynomial model, converted to KB-5 in stream coords —
   `insv_imu::stream_lens_calib`); the "Insta360 X6" preset is only a fallback. The
   lens is rendered through the shaders' unified-camera-model branch
   (`cal.xi > 0` in every `fisheye_*.wgsl`; uniform grew to 7 vec4s) with
   the principal point from the file's `window_crop_info` and the tracks
   in reverse lens order (stream 0 = calibration entry 2); readout and
   gyro offset come from the file (`rolling_shutter_time`, `gyro_timestamp`).
   Verify the UCM branch compiles/renders on D3D11/DX12 (naga → HLSL).
   Stabilization: the orientation stream is a direct gyro integration
   (`insv_imu::integrate_orientation`, VQF only supplies the bias) and the two
   eyes are timed from their own sensor's exposure record (`lens_b_timeline`,
   `dji_imu::per_eye_rotations` / `per_eye_rs_rows`) — the GPU-resident NVENC
   export path got that per-eye plumbing edited blind; check it renders.
   Windows TODO: verify the `D3d11SharedDualStreamIter` arm (edited blind
   on macOS — same pump, same shape as the VT iterator) on an `.insv` AND
   an `.osv`: frame count == trim length, no "Could not find ref with POC"
   spam, eyes in sync. The X6 gets no built-in LUT yet.
9. **Reframed output mode** (`FisheyeOutputMode::Reframe`, 2026-09-09):
   a pinhole-style side-by-side viewport of each eye instead of the VR180
   half-equirect — per-clip zoom (horizontal FOV), pan/tilt/roll, a
   "defish" blend (k-projection `r = k·tan(θ/k)`: 0 = rectilinear,
   1 = fisheye look) and a 1:1 or 16:9 per-eye aspect. Implemented as a
   second ray generator (`output_ray`, `proj_mode` uniform) inside the
   existing equirect kernels — the four `fisheye_*_to_hequirect*` shaders
   (DJI / Insta360 / BRAW) and `eac_to_equirect.wgsl` (GoPro) — driven by
   `FisheyeCalib::with_reframe` / `EacLensAdjust::with_reframe`
   (`gpu::reframe_k`, `reframe_edge_x`, `reframe_ray` are the CPU twins,
   unit-tested). The pan reuses the view adjust's global angles
   (`Settings::output_view_adjust`, tilt-up = negative pitch); stab, stereo
   offsets, per-row RS and the lens override all still apply. Export
   (`FisheyeExportProjection::Reframe { hfov_deg, defish, spatial }`)
   writes the exact viewport at a per-eye size (1080/1440/2160 lines ×
   aspect → 2:1 or 32:9 SBS) and never tags VR180 metadata (no spatial-
   video side output: Vision Pro viewers watch the VR180 export instead).
   Preview follows the aspect
   via `preview_out_dims`; in this mode dragging the preview pans the view,
   scroll / pinch zooms the FOV and double-click recenters (the alignment
   magnifier is inert), and the BeyondVR eye-scale hack is hidden and not
   applied. Preview sharpness: the zero-copy worker projects the reframed
   view from the NATIVE frame (not the 1280 working res), its output is ≥
   1920 px per eye, and a paused frame always shows the native still
   (`wants_full_res_still`). The Windows D3D11 preview path took the same
   edit blind — verify there.
10. **Windows `.360` ProRes/libx265 GPU arm + Vulkan preference + slow-path
   notes (Windows session, 2026-09-12).** `export_eac_gpu_resident` now
   takes ProRes and libx265 too: the NVENC gate became per-encoder, and the
   arm has two encode tails — NVENC keeps the CUDA/P010 ring untouched;
   ProRes/libx265 get the OSV zc arm's readback tail (format handshake →
   P210→`prores_ks_vulkan` with `warm_prores_vulkan()` before thread spawn /
   RGBA64→swscale, encode thread pipelined against decode + GPU). 8K ProRes
   422: 2.5 → 13.3 fps, arm-vs-portable mean 0.6/255 (the portable EAC loop
   is 8-bit RGB; the arm feeds 10-bit 4:2:2). Wgpu backend: eframe + the
   pipeline's own device creation now explicitly prefer Vulkan on Windows
   (probe first, DX12 fallback kept — a silent DX12 pick disabled every
   fast arm). Slow-path visibility: landing on the portable loop or a CPU
   encoder fallback logs at `warn` AND surfaces in the export bar via
   `fisheye_export::{set,append,}export_path_note`. NOT verifiable on this
   box: the DX12-fallback branch itself (Vulkan is always present here)
   and a genuine `prores_ks_vulkan` init failure — both code-reviewed only.
   FOLLOW-UP (same day): the "stalls at 100%" report — the two-pass
   temp+re-mux finalize rewrote the whole encoded video to add audio
   (longer than the encode at ProRes bitrates). The one-pass inline-audio
   mechanism (`attach_audio_passthrough`, built for the macOS VT + portable
   fisheye paths) is now wired into the Windows OSV zc d3d11 arm, the
   `.360` GPU readback tail and the portable EAC loop (stereo only —
   `one_pass_audio_eligible`); video out byte-identical, chain/trim audio
   re-verified. NVENC tails + ambisonic/APAC keep two-pass and show a
   "muxing audio…" phase note (`FinalizePhaseNote`) in the export bar.
   NEXT DAY: the NVENC tails are one-pass too — `CudaNvencEncoder` gained
   `InlineAudio` (raw-FFI twin of `AudioPassthrough`, keep in lockstep;
   audio as a `new()` param since its header isn't deferred) + the
   duration=1 / dts=pts packet fix from `drain_packets` (NVENC leaves
   duration 0 → track a frame short; the old re-mux masked it). Verified
   one-pass on .360 + OSV, single AND 2-segment chains (ffconcat playlist
   = continuous chain audio, windows cut mid-chain), 30/30 frames
   byte-identical to the two-pass reference. The macOS EAC VT arm
   (`export_eac_zerocopy_vt`) followed the next day — same gate, inline
   attach before the first frame, chain-probed duration when per_eye is
   empty; verified on a trimmed `.360` window (2–5 s: 90 frames + 3.0 s
   AAC muxed inline, no temp). Still two-pass everywhere: ambisonic / APAC.
   NEXT DAY (2026-09-15): **generic SBS sources get the Windows fast path.**
   `SbsFisheyeIter` finally honors its hw flag (d3d11va attach + hw-frame
   download — it had silently software-decoded forever, preview included),
   and the zc readback arm takes SBS: new `D3d11SharedSbsIter` (single
   stream, converts the WHOLE frame, precise-seek run-in) yields through
   `ZcFisheyeSource`/`ZcFisheyeFrame` (the arm's source is now that enum),
   and the arm splits the halves with `gpu::split_sbs_texture_16` (two DtoD
   subregion copies) — everything downstream unchanged. Single-segment
   only; the GPU-resident CUDA/NVENC arm stays dual-stream-only (SBS NVENC
   rides the readback arm's P010 feed: 5.4 → 38 fps at 4K). Verified: SBS
   NVENC/ProRes/reframe + trim + one-pass audio, cpu-vs-zc parity 1.03/255,
   and OSV arm parity byte-stable vs pre-change (same 1.233/255 / max 23).
   macOS untouched (its SBS sources ride the portable loop as before).
   SAME DAY: **playback too** — the preview dispatch gated on
   `is_dual_stream()`, so SBS previews still paid download+swscale+upload
   every frame (worst at 8K). `run_fisheye_zerocopy` now takes
   `ZcFisheyeSource` (same enum as the export arm) and `FrameHold` carries
   the imported full texture for SBS alongside the split halves (deferred
   drop must outlive the in-flight copy); `D3d11SharedSbsIter::new_with_work`
   adds the ~1280 preview downscale (native stays the export's default).
   **Contract bug found + fixed doing this:** the SBS iterator didn't
   `wait_gpu_idle()` after `share_eye_converted` like the dual-stream / EAC
   iterators do — the importer could sample memory D3D11 hadn't finished
   writing. It read as BLACK deterministically in a readback test; the
   export arm had been hiding it behind the decode→main channel hop (a real
   latent race, not just a test artifact). Costs ~2% throughput (38→37 fps).
   Verified by `examples/sbs_zc_check.rs` (gitignored): GPU-left vs CPU-left
   101/65535 while GPU-left vs CPU-RIGHT is 3122 (31× — split provably not
   swapped/offset), work-res downscale exact dims + identical mean level.
11. **VRAM gate on hardware decode (Windows, 2026-09-15).** A d3d11va DPB is
   20 surfaces and the NVIDIA driver commits **~4× the P010 payload per
   `BIND_DECODER` array slice**, so the cost is **~240 MiB per megapixel per
   stream** — 8K SBS ~7.9 GB, OSV/`.insv` ~7.0 GB (two streams), `.360`
   ~5.6 GB, 4K SBS ~2.0 GB. Model verified against measurement within 4% on
   all four. This has shipped since **v2.0.0 (2026-06-07, `4eb9ba4`)** for
   OSV and **v2.1.0 (`13098a4`)** for `.360`; generic SBS only joined on
   2026-09-15 and never shipped. **8 and 12 GB cards were already broken for
   the flagship formats**, and OSV preview+zoom (13.9 GB) fails even at 16 GB.
   The failure was NOT graceful: the pool commits LAZILY (`avcodec_open2`
   succeeds; the allocation only fails on the first frame), so FFmpeg
   silently swapped to a software pix_fmt the zero-copy importer can't use →
   `Err` mid-stream → the worker logged one `warn!` and broke → the app
   logged **"decoder thread exited cleanly"**. A VRAM failure was
   indistinguishable from EOF: frozen preview, no message. The
   constructor-keyed fallback at `decoder.rs` could never fire, because the
   constructor had already returned `Ok`.
   FIX: a pre-flight budget check inside **`decode.rs::try_enable_d3d11va_decode_n`**
   — the one function every hardware attach funnels through. It asks DXGI
   (`interop_windows::vram_headroom_mib`, `IDXGIAdapter3::QueryVideoMemoryInfo`,
   no new dependency) and declines if `need + 1 GiB reserve` doesn't fit.
   Declining up front is what makes the EXISTING ladder work: the zero-copy
   constructors all require this to succeed, so they return `Err` → callers
   take the portable path → the portable path's own attach also declines →
   software decode. Dual-stream sites pass `concurrent_streams = 2` so both
   decoders are budgeted (they commit lazily, so per-stream checks would both
   pass and then both fail). Env: `VR180_NO_HW_DECODE=1` forces software,
   `VR180_FORCE_HW_DECODE=1` bypasses, `VR180_VRAM_BUDGET_MIB=6000` fakes a
   small card for testing on a big one. Verified: simulated 8 GB card →
   decline logged, export falls to software and COMPLETES (1.6 fps);
   4K SBS / OSV / `.360` unaffected on the 4090 (fast path still engages,
   same fps). FOLLOW-UP (same day) — the three
   remaining audit items are now FIXED:
   (a) **Truncated export reported `Done`.** All three zero-copy decode
   threads (`fisheye_export.rs`: EAC gpu-resident, zc readback, OSV
   gpu-resident) now return `Result<()>` instead of `warn!`-and-break, and
   each join result is checked BEFORE finalize. A user cancel is explicitly
   NOT a failure (the partial file is kept deliberately), and
   dropped-receiver / EOF stay `Ok`. Note: FFmpeg treats a CORRUPT stream as
   early EOF (`Ok(None)`), so a damaged input still finalizes short — that is
   a different condition from the VRAM `Err` this fixes, left as-is.
   (b) **Decoder-thread panic wedged Play/Pause.** The `finished` +
   `imu_progress.abort` stores moved into a `SignalFinished` **Drop guard**
   in `app.rs`, so they run on unwind too (`finished` is the only thing that
   clears `decoder_alive`). `drain_frames` also distinguishes
   `TryRecvError::Disconnected` from `Empty` now — they were identical, so a
   dead worker looked like an idle one.
   (c) **No `on_uncaptured_error`.** Installed in `gpu::Device::from_existing`
   — the single funnel for BOTH devices (eframe's shared one and the export's
   dedicated one), so macOS gets it too. Records to `gpu::last_gpu_error()`,
   logs at ERROR, and the export bar shows a dismissable "⚠ GPU error" chip.
   `VR180_WGPU_PANIC=1` restores the panicking default for debugging.
   PROVEN both ways by `examples/gpu_error_check.rs` (gitignored): forcing a
   real validation error, the process SURVIVES and records it; with
   `VR180_WGPU_PANIC=1` the same error panics. Verified the real app records
   nothing at startup (no spurious chip).
   Regression sweep after all three: OSV/`.360`/SBS × nvenc/prores/portable
   all pass at unchanged fps, software-fallback output is pixel-identical to
   hardware (0.72/255 mean, encoder noise). SAME DAY: a **cross-GPU guard**
   went into `VulkanImportCtx::from_wgpu` (refuse the context when the wgpu
   adapter's LUID differed from DXGI adapter 0, i.e. detect a hybrid iGPU+dGPU
   box and drop it to the portable path) — **SUPERSEDED 2026-09-24 by PR #7
   (Norman3D), which is the better fix: steer the decoder onto the right
   adapter instead of detecting the mismatch.** The hazard it addressed is
   real — the zero-copy import is +0 MiB (it aliases D3D11 memory), so the
   only way `import_d3d11_handle_to_wgpu` dies is an **adapter mismatch**:
   ffmpeg's `d3d11va` device used a NULL device name = DXGI adapter 0 while
   wgpu picked its own, and on a hybrid machine the NT handle is then
   meaningless to Vulkan. Now `decode::enable_d3d11va_decode_on_adapter`
   resolves the DXGI index for `vulkan_device_luid()`, creates the d3d11va
   device on THAT adapter (index passed as the device-name string), and
   verifies the LUID off the real `ID3D11Device` before attaching — so a
   hybrid laptop KEEPS the fast path. Every zero-copy iterator (dual-stream,
   EAC pair, generic SBS) takes an `adapter_luid: Option<[u8; 8]>` from the
   `VulkanImportCtx`; `None` or an unresolvable adapter declines at
   construction so the portable ladder still works. The function ALSO carries
   the VRAM pre-flight (`concurrent_streams`) — the PR as submitted bypassed
   it, which would have silently reintroduced the item-11 first-frame death.
   `dxgi_default_adapter_luid` is gone with the guard. PROVEN on this box,
   which really is hybrid, with the PR's own `examples/windows_adapter_smoke`
   (tracked, unlike the rest of that dir): RTX 4090 AND Intel UHD 770 both
   PASS 18 eye imports/readbacks + segment seeks + missing/invalid-LUID
   rejection — the Intel was REFUSED outright under the old guard. 4090
   exports byte-identical before/after. **Still open (deliberately):** the six
   `.expect()`s in `import_d3d11_handle_to_wgpu` — with the decoder now on the
   matching adapter the realistic trigger is gone, and de-panicking them means
   threading `Option`/`Result` through ~12 call sites in hot loops (they also
   leak the `VkImage` on the allocate/bind failure paths).
12. **Matching Eyes gained an EXPOSURE trim (2026-09-15) — CROSS-PLATFORM,
   nothing Windows-specific.** `ColorStackPlan::eye_match_exposure` (stops)
   joins `eye_match_ct`/`eye_match_tint` and is applied in the same one
   place, `ColorStackPlan::for_eye`, as `cdl.gain *= 2^(±stops)` — `gain` in
   `cdl.wgsl` is a straight `x * gain` before the shadow/highlight zones and
   the [0,1] clip, so scaling it IS an exposure trim, and it lands PRE-LUT
   next to the CT/tint trim (both correct the sensor signal before the
   creative transform). Because `for_eye` is the single funnel every preview
   and export arm already calls, no new plumbing was needed. It composes with
   the global Gain slider rather than replacing it, and activates the CDL
   stage on its own (one extra dispatch; the rest of that stage is identity).
   Verified on real 16-bit textures by `examples/eye_exposure_check.rs`
   (gitignored): left/right = `v · 2^±ev` to within 1 LSB of 16-bit, the two
   eyes' geometric mean == the source, composition with Gain 1.2 exact, and
   `for_eye` still BORROWS (no clone) when every trim is 0. End-to-end on an
   OSV at 0.4 stop: 8-bit portable and 10-bit NVENC zero-copy agree to 0.02%
   (left ×1.309, right ×0.755 — short of the nominal ×1.3195 only because
   highlights clip on the brightening eye).
   ONE REAL BUG FIXED doing this: the **8-bit portable arms dropped the
   Matching Eyes trim entirely** (CT and tint too — pre-existing, not new).
   Both of them (`fisheye_export.rs`: the portable SBS loop and the portable
   EAC loop) grade the COMPOSED side-by-side buffer in one
   `apply_color_stack_rgb8` call with the un-eyed plan, while every 10-bit and
   zero-copy arm uses `for_eye`. New `apply_color_stack_rgb8_sbs` splits the
   SBS into halves, grades each with its own plan and re-joins — exact, since
   every stage in that 8-bit stack (CDL/LUT/WB/saturation) is per-pixel, so
   there is no spatial filter to straddle the halves and no seam. It short-
   circuits to the OLD single whole-SBS call when no trim is set, so the
   untrimmed 8-bit path is untouched — PROVEN byte-identical (same frame md5,
   same 4265493-byte file, before vs after).

13. **8-bit SBS was broken on the zero-copy path — FIXED (2026-09-15).**
   Reported from the mac side, reproduced and confirmed here. The d3d11va DPB
   format follows the SOURCE bit depth: 8-bit H.264/HEVC → **NV12**, 10-bit →
   **P010**. `P010Converter::convert` hardcoded `R16_UNORM`/`R16G16_UNORM`
   plane SRVs, and a plane SRV's format must match the plane's own format, so
   on NV12 `CreateShaderResourceView` returned a bare `E_INVALIDARG`
   (0x80070057). Worse, it failed at FRAME 1, not at construction — the same
   unrecoverable shape as item 11's VRAM bug: `D3d11SharedSbsIter::new`
   returned `Ok`, every caller reads that as "zero-copy is on", so there was
   no fallback left. BLAST RADIUS was exactly the generic-SBS iterator (the
   2026-09-15 addition, and the only one handed arbitrary files). The other two
   converter callers were unaffected: four GoPro `.360` files all probe
   `yuv420p10le`, and **`.insv` is 10-bit only by project scope** — the X6 is
   what this app targets, and 8-bit Insta360 footage is explicitly out of
   scope, so do not spend effort on it. (Either way the converter now takes
   NV12, so an 8-bit dual-stream source would work rather than fail.)
   FIX, in two parts: (a) `hw_plane_layout(DXGI_FORMAT)` returns the plane SRV
   formats AND the BT.709 range constants for NV12 vs P010/P016, `convert`
   reads the format off the source texture, and the constants moved into the
   cbuffer so ONE compiled shader serves both depths (the HLSL is now
   `YCBCR_TO_RGBA16_HLSL`; P010 and P016 share constants because P010 keeps
   its 10 bits in the HIGH bits, so both blacks normalize to 4096/65535).
   Output stays RGBA16, so everything downstream is depth-agnostic and 8-bit
   SBS gets the fast path rather than just a graceful fallback. (b) a
   construction-time guard — `interop_windows::hw_convert_supports_pix_fmt`,
   checked in `D3d11SharedSbsIter::new_with_work` against the stream's
   `AVCodecParameters::format` — so a format the converter can't sample
   declines UP FRONT and the portable ladder still works. Only the SBS
   iterator gets the guard: it is the one fed arbitrary files.
   VERIFIED: an 8-bit `yuv420p` SBS clip went from `** FAILED **` on frame 1
   to a full `sbs_zc_check` PASS — GPU-vs-CPU 129.5/65535 (0.20%, better than
   the 10-bit path's documented 1.03/255), split separation 31×, work-res
   downscale exact, sustained loop clean; end-to-end export zc-vs-portable
   1.234/255 max 16, matching the OSV arm's own 1.233/255. NO REGRESSION: the
   10-bit OSV zc export is **byte-identical** to before (same frame md5, same
   3868683-byte file), so the cbuffer refactor is a provable no-op for 10-bit;
   the `.360` GPU-resident arm still engages; OSV/SBS × zc/portable all pass
   with stab on. NOTE for whoever touches this next: `import_p010` has **no
   callers** — every zero-copy import is single-plane `import_rgba16` — so the
   `Features::TEXTURE_FORMAT_P010` test in the preview/export gates is
   vestigial. It is merely over-conservative (it can only decline to a working
   path, and every d3d11va-capable adapter has P010), so it was left alone
   rather than widened on shipped paths.
14. **Windows SBS converter honours the range tag (2026-09-25)** — the
   follow-up the mac side asked for with their shared-code change (`FisheyeCalib`
   gained `src_x0`/`yuv_range`, `FisheyeCalibUniforms` → 9 vec4, `ResolveDims`
   → 3 rows, `resolve_p010_planes_to_rgba16` +2 args). Those were TRANSPARENT
   to Windows — no Windows call site constructs `FisheyeCalib` by literal or
   calls the resolve, and the RGBA16 kernels are fine with the larger uniform:
   the export matrix was byte-identical before and after the pull. ONE build
   break, same class as the `VtSharedSbsIter` gate: `pix_fmt_desc` was gated
   `macos` but is called by the cross-platform `codecpar_luma_depth` (which
   `SbsFisheyeIter` now uses on every platform). Pure ffmpeg — ungated it.
   THE CHANGE: `hw_plane_layout` now returns the source bit depth, and
   `P010Converter::convert` takes `full_range` (the STREAM's
   `color_range == JPEG`, read once at `D3d11SharedSbsIter` construction — the
   same field the CPU `SbsFisheyeIter` reads) and pulls all four expansion
   constants from **`gpu::yuv_range_constants(bits, full_range)`** — one
   source of truth shared with the macOS P010 kernels, so the two platforms
   cannot drift again. (Their convention is `t·scale + off` with NEGATIVE
   offsets; the HLSL's `- off` became `+ off` to match. Limited-range values
   are numerically identical to what the HLSL had baked in.) VERIFIED:
   limited clip parity IDENTICAL to baseline (82.6/82.1 per 65535); a
   `yuvj420p|pc` full-range clip (`ffmpeg -vf scale=out_range=full`,
   `-color_range pc`) lands at **82.5/82.1** — the same parity, where a
   still-hardcoded-limited expansion would read in the THOUSANDS (≈16/255
   black offset + ≈16% gain). Smoke test both GPUs + matrix unchanged.
   **Dual-stream and EAC pass `full_range = false` explicitly**, preserving
   shipped, verified output — and here is the finding worth knowing: **GoPro
   `.360` is tagged `pc` (full range) on BOTH EAC tracks** (`ffprobe`:
   `0|yuv420p10le|pc 4|yuv420p10le|pc`), while OSV and X6 are `tv`. NEITHER
   platform honours it: the macOS EAC VT arm has no range plumbing at all,
   the Windows converter passes `false`, and the Windows portable path is
   swscale, which treats 10-bit as limited regardless of tag — which is WHY
   the Windows arm-vs-portable parity is 0.6/255. So `.360` is consistently
   limited-expanded everywhere. Do NOT “fix” one side alone: whether GoPro's
   `pc` tag is truthful (a known GoPro quirk) needs a deliberate cross-platform
   decision with both boxes comparing against a reference.

15. **Automatic per-frame eye-exposure matching for the X6 (2026-09-27) —
   SHARED code, on `main`, unreleased.** Why: the X6's two sensors auto-expose
   independently (shutter AND gain — trailer record 0x0009 is the right
   sensor's AE log, the left gain is recorded nowhere), and the difference is
   real in the image: measured after the LUT in linear light, the eyes sat up
   to ~0.9 stops apart for ~20 s on a real post-mod clip (regression of
   brightness-vs-shutter ratio: slope 0.5–0.8, r 0.87–0.99). The static
   Matching Eyes trim cannot follow that. WHAT: three new compute passes in
   `gpu.rs` + `shaders/eye_match_{hist,reduce}.wgsl`, `eye_offset_apply.wgsl`
   — per-eye 1024-bin luma histogram of the central quarter of each PROJECTED
   eye texture (one kernel for every path; workgroup-shared atomics — the
   first `atomic<u32>`/`var<workgroup>` in the codebase, core WGSL, no
   feature), a one-thread reduce (both eyes' clipped tails cut by RANK
   jointly — I-Log black 0.091 → lo 0.12, white wall 0.835 → hi 0.80 — then
   the mean shift of 5 quantiles across the remaining rank range → smoothed
   per-eye ADDITIVE code offsets in a GPU buffer, `EyeMatchState`; snap on
   |Δ| > 0.008 code, EMA α 0.3 below; min 4096 unclipped samples; reset on
   seek; `reason` tells the UI why a frame was not measured), and an apply pass as STAGE 0 of the colour stack
   (an exposure gain on log footage — additive, NOT the multiplicative CDL
   gain the static trim uses). No readback in the frame loop; the GUI readout
   uses a 3-deep async MAP_READ ring (`EyeMatchReadback`, never polls — Lesson
   #1). API: `ColorStackPlan.auto_eye_match: Option<EyeMatchMode>`,
   `Device::apply_color_stack_pair_16` (both eyes, ONE submit — the 16-bit
   stack was refactored into `record_color_stack_16`; `apply_color_stack_
   per_eye_16` is now a wrapper and bit-identical when the flag is off),
   `apply_color_stack_to_sbs_bgra_matched`, `match_eyes_8` (CPU-fallback arm),
   `create_eye_match_state`. Plumbing: `Settings.auto_eye_match/_mode`
   (serde default) → `build_color_stack`; preview funnel
   `compose_with_color_and_mode` gained an `eye_match` arg (fisheye arms pass
   `eye_match_arg(state, kind, stab_idx, enabled)`, EAC arms `None`; the single-eye
   RIGHT view now swaps AFTER grading — the per-eye trims used to land on the
   wrong eye there); export arms use the pair API with a per-stream state;
   `build_export_cfg` and `eye_match_arg` gate to `Insta360Insv`, so the
   persisted flag is inert for OSV/GoPro. UI: checkbox + reference combo
   (meet in the middle / to left / to right) + EV readout (÷ 0.067 code/stop)
   in the Colour panel's Matching Eyes block, X6 only. **Edited blind for
   Windows:** the pair-API swaps in `export_fisheye_osv_zerocopy_d3d11` and
   `export_fisheye_osv_gpu_resident` and the state creation in
   `run_fisheye_zerocopy` (pure Rust, same shape as the macOS arm; no cfg
   gates touched — `EyeMatchFrame` import is ungated). Verified: gitignored
   `examples/eye_match_check.rs` (synthetic eyes, 20 checks incl. modes,
   snap/EMA/reset, black-eye rejection, 8-bit helper — ALL PASS); real clip
   036 via `reframe_check` + `EYE_MATCH=avg|left`: the shaded segment went
   from L/R −0.44 stops (off) to +0.01 mean / ±0.06 max (on), the equal-shutter
   control segment −0.017 → −0.004, match-to-left leaves the left eye at
   +0.0000 stops vs off; `cargo test -p vr180-pipeline` 45/45. The user
   ran the GUI on real footage (preview + export work; app control for an
   automated check was declined). ADVERSARIAL REVIEW (6 finders, 1-vote
   verify) found one real defect, fixed: the valid-luma gate was on a
   linear scale while the histogram sees I-Log CODES (black 0.091, white
   wall 0.835 — measured), so clipped sky was never excluded and a per-eye
   value gate truncates the brighter eye more → shift biased toward 0. Now
   `EyeMatchTuning::insta360_ilog()` (lo 0.12 / hi 0.80, the default since
   only the X6 is enabled) and the reduce cuts both tails by RANK jointly
   (max clipped fraction over both eyes + 0.03 margin) and averages 5
   quantiles across the valid rank range — harness cases with 20–32 % of
   one eye on the white wall and I-Log-black padding recover the shift to
   ±0.0001. Also from the review: same frame index = re-render, not a seek
   (`frame_for_index`), `EyeMatchState::clear()` while the passes are off so
   re-enabling starts fresh, readback ring collected oldest→newest, the
   paused loops collect the readout each tick, the still allocates a state
   only when it can use it (the still applies the raw per-frame difference,
   the live preview its smoothing — ≤ snap apart), and the export's colour
   panel says "applied per frame" instead of "measuring" (the export stops
   the preview, which is the only readout source). SECOND PASS (3 finders):
   the tail counts are half-open (`count_bins(eye, start, end)`) — WGSL
   `select` evaluates both arms, so the old `count_range(…, lo_b - 1u)` would
   have spun 2^32 iterations for a tuning with lo = 0 (latent; the CPU side
   now clamps lo/hi too); the readout ring is generation-tagged and returns
   only values that landed in the call (no stale flash after re-enable), the
   walk stops at the first in-flight slot; the panel keys on `decoder_alive`
   (an exited worker used to leave "Measuring…"), and a frame the reduce
   cannot use (mostly sky / black) says so via `EyeMatchReadout::reason`.
   **Windows arms VERIFIED on the Windows box (2026-10-09):** real X6 clip
   (026) through BOTH blind-edited export arms via `export_smoke` +
   `VR180_EYE_MATCH=avg` — eyes sat 0.22 EV apart with matching off; with it
   on, the zc readback arm closed the gap to 0.046 EV (79%) and the
   GPU-resident NVENC arm to 0.048 EV (78%), both moving symmetrically
   toward the middle as Average specifies, and the two arms agree to
   0.1/255. No panics; OSV/SBS/.360 exports byte-identical with it off.
   The PREVIEW arm (`run_fisheye_zerocopy` state creation) is launched
   only — the app takes no file argument, so it was not driven headless.
   Known/accepted: the export does
   not publish a readout; Play stays enabled during an export.

16. **GoPro stabilization investigation + per-frame exposure timing
   (2026-10-07) — SHARED code, on `main`, unreleased.** The user rigged a
   GoPro MAX (.360, 30 fps), an Osmo 360 and an X6 (both 50 fps in set 1,
   ~30 fps in set 2) together and shot the same walk/run; the GoPro looked
   clearly worse. MEASURED (scratchpad `stab/`: resid.py centre-crop phase
   correlation, eye_rot.py whole-eye sphere fit of DIS flow, sweep_eval.py /
   timing_fit.py on camera-lock harness exports, cameras aligned by audio
   envelope): (a) on the user's soft-stab exports the residual per SECOND of
   video is the same for all three (set 1: 1.7 / 1.8 / 1.9 deg/s; set 2 the
   GoPro is ~1.4× by the centre metric) — the GoPro's per-frame residual is
   1.6× because it is 30 fps; (b) under camera LOCK on the walking window the
   GoPro leaves 0.20°/frame vs the X6's 0.056°, but that excess is
   NOT the pipeline: phase ±4/±8 ms, a 1 ms point sample instead of the
   15 ms box, VQF 6D / τ=100 s / fixed bias / plain integration, RS window
   centre and RS off all leave it at 0.20–0.22°; the raw (unstabilized)
   frames follow the pipeline's own rotation increments as well as the X6's
   do (R² 0.88 both), the residual-vs-Δω regression puts the GoPro timing
   error at 1.6 ms (the method recovers deliberate ±8 ms shifts as 7–8.5 ms),
   and the residual field is the forward-walk parallax pattern (the GoPro
   sits lower on the rig and its frames are 33 ms apart); (c) in the DARK
   (set 2 end: 1/30 s shutter) the GoPro residual improves monotonically as
   the pose is moved earlier (0.613 → 0.548 @ −8 ms → 0.542 @ −16.5 ms;
   +16.5 ms → 0.764) and the Δω-correlated component vanishes — the missing
   `−shutter/2` term. GoPro CORI on these clips is a constant (bias drift
   0.2°/s): no in-camera stabilization, so the VQF path is used. WHAT
   CHANGED: `vr180_core::gyro::parse_shut` (per-frame SHUT, type `f`),
   `resample_quats_at_times` + `gopro_frame_sample_times(fps, n, shutter,
   readout)` = `i/fps + readout/2 − shutter_i/2` (fallback to the readout
   midpoint per frame when SHUT is absent/implausible); `imu.rs`
   `vqf_cori_equivalent_stream(_multi)` sample there (`PreparedImu.shutter_s`,
   `gopro_shutter_per_frame(segments, fps)` for chains); decoder.rs RS ω
   window centred on the same instant (was frame start, a Python-parity
   choice — now DJI parity, `t_mid`). Verified: core 53 / pipeline 45 tests
   (new: SHUT parse, sample times, at-times resampler parity); dark lock
   export 0.613 → 0.542°/frame (= the −16.5 ms probe), Δω-only R² 0.060 →
   0.004; daylight 0.202 → 0.199 (unchanged). Not changed (measured not to
   matter here): VQF 9D estimator, 15 ms pose window, RS factors; the GEOC
   inter-lens extrinsics (ANGX/ANGY/ANGZ) remain unparsed — a static stereo
   alignment question, not shake. Harnesses (gitignored): eac_stab_check
   gained GOPRO_NO_STAB / GOPRO_RS_T_MS / GOPRO_RS_F; rider_export takes
   SMOOTH_MS / MAX_CORR / RESP and uses the new times.
   SECOND PASS (2026-10-08, "set 2 is all 30 fps, look again"): whole-clip
   whole-eye metric on the user's set-2 exports — slow residual identical
   for all three (ratio 0.99, r 0.97) but the GoPro carried 3.4× the
   frame-to-frame jitter (0.238° vs 0.071° rms), pitch-only, random frame
   to frame. The SHUT fix took it to 0.144°; the rest sat in two seconds
   (t = 73 and 107) that hold 66 % of the GoPro's remaining jitter energy —
   both are DROPPED FRAMES: the source video repeats a frame (consecutive
   diff ≈ 0, both tracks), the telemetry block covering it spans 29 frame
   periods (0.9676 s, 772 gyro samples), the camera's `MSKP` stream steps
   −1 at the repeat (`parse_mskp`), SHUT repeats too, and the container
   hides it (uniform 33.333 ms PTS). `build_imu_sample_times_with_skips`
   now anchors per frame (sensor frame of video frame i = i + MSKP[i]; the
   hold is placed AFTER the repeat so the preceding frame maps with slope
   1), `gopro_frame_sample_times(.., skips)` gives a repeated frame the
   instant of the frame it copies (its lock residual 2.18° → 0.00°), and
   the RS ω timeline, GRAV/ACCL/MNOR and the GUI's `GyroAngvel` all take
   the skips. Also measured and REJECTED: fine phase sweep −4..+4 ms
   (minimum at 0 on the walking window, flat on the dark one — no missing
   constant), RS readout ×0.5/×2, factor sign flip, per-axis factors, ×5
   (RS is live — ×50 moves pixels by 30 levels — but at these rates its
   effect on the residual is a few %), VQF/GRAV coupling (GRAV drifts
   slowly, spikes are single-frame), the rig blob in the GoPro's left third
   (masking it changes nothing). Still open: a ~4° single-frame spike at
   the peak of a 6 Hz ±160°/s mount oscillation (8000°/s², the clip's
   worst second, coincident with the drop) that no variant touches, and a
   residual pitch-only jitter excess of ~1.3× outside the drop seconds —
   both candidates for an under-strength RS model (readout ×2 and factors
   ×5 both nudge the burst the right way), left untuned by rule.
   `VR180_GOPRO_PHASE_MS` remains as the GoPro twin of the DJI diagnostic
   knob (default 0).
   OPEN after the 2026-10-09 pre-push review: (a) a dropped frame that is the
   FIRST frame of a telemetry block charges the shortened span to the
   previous block (`real = n + skip(f1) − skip(f0)`); which block the
   firmware stamps short is unverified — needs a clip with a boundary drop;
   (b) `gopro_shutter_per_frame`/`gopro_skips_per_frame` re-extract each
   chapter's GPMF uncached on every multi-chapter stab rebuild in the GUI
   (slow on SMB) — should take the cached blobs; (c) the headless
   `vr180-render` CLI still centres its RS window on frame start with no
   MSKP/SHUT (it has pre-existing compile errors from wgpu drift anyway).
   THIRD PASS (2026-10-08, "the mount is fine — look carefully at RS"):
   the RS term was MEASURED for the first time (scratchpad `stab/rs_probe*.py`:
   GOPRO_NO_STAB exports with RS off / on / ×2 / −1, DIS flow → Kabsch →
   9-parameter fit of the residual on the row-time and column-time RS
   bases → slope vs the gyro's Δω at the frame instants). Sign RIGHT (the
   derivation: the shader's Y↔Z swap is improper, so `r + (w×r)·Δt` with
   the raw gyro IS the physical correction — and the old memory claim
   "CORI integrates −GYRO" is false in this code's conventions: CORI
   increments vs ∫gyro slope +0.994); axis RIGHT (rows vertical, top row
   first, T_cols ≈ 0); BOTH EYES identical (L/R slopes agree to 0.1 ms;
   GEOC ANGY = 180.26°, ANGX/ANGZ < 1°: the back lens is a pure yaw of the
   front, same sensor "up"); magnitude: each unit of factor removes
   15.3 ± 0.5 ms (= the file's SROT 15.224 within 1 %). The UNCORRECTED
   readout read 17.2 ms over the full field but 15.6 ± 0.5 in the far
   field (sky/façades) and 13.7 on the near ground — walking parallax
   correlates with pitch acceleration and biases the absolute number; an
   independent skew fit on the raw EAC faces gave 17.7–18.3 (same bias).
   Three principled changes landed, none moves the lock residual
   measurably (walking 0.197→0.197, dark 0.541→0.549, burst 0.775→0.753 —
   noise): the readout now comes from the file's `SROT`
   (`vr180_pipeline::imu::gopro_readout_s`, fallback `SROT_S`; GUI seeds
   `rs_readout_ms` from it per clip, the VQF pose instant uses the same
   value); the shader's row estimate uses the camera-frame ray
   (`xr,yr,zr`) instead of the pre-rotation output ray in both EAC shaders;
   `GyroAngvel::groups_at` returns the MEAN rate over [t_ref, t_row]
   (running trapezoid integral, `from_parts` for merged chapters) so the
   per-row term is the integrated angle. CALM-SEGMENT CHECK (rotation
   without walking: 243 @ 0–8 s and 78–88 s, 244 @ 2–6 s and 96.5–99.5 s;
   the elevator segment 244 @ 112–117 s has no far field and was dropped):
   far-field uncorrected readout **15.60 ± 0.20 ms**, each unit of factor
   removes 14.81 ± 0.21, residual after the 15.224 ms correction
   **+0.78 ± 0.15 ms** (the correction nulls at an effective 16.0 ± 0.2 ms) → the true
   readout is 3–5 % above the file's SROT, an 0.08° edge-to-edge error at
   100 °/s — real but not visible, and by rule left to the file (a
   per-camera readout self-calibration from footage would be the
   file-derived way to close it). T_cols ≈ 0 (rows-only model holds).
   The 1.07 yaw scale was near-field lever-arm parallax: far-field-only
   Kabsch gives 1.01–1.02 for yaw and 0.99–1.01 for pitch/roll. By-product:
   the fitted gyro clock offset drops by one frame between the calm
   segments on either side of a dropped frame (243: −33.4 ms exactly;
   244 across the 73.2 s drop: −36 ± 3 ms) and the correction nulls on
   both far sides — the MSKP re-anchoring verified independently (244's
   second drop at 107.6 s is followed only by the unusable elevator
   segment, so it is unverified).

17. **Osmo 360 "sometimes a roll residue, X6 almost perfect" (2026-10-09) —
   investigation, diagnostic knob only, nothing user-visible changed.**
   Same rig clips as item 16 (set 2: Osmo 0017 / X6 059 / GoPro 244; audio
   offsets t_osmo = t_x6 + 3.332 = t_gopro + 1.016). MEASURED EQUAL between
   Osmo and X6 — so ruled out: per-frame roll jitter of the real exports
   (0.307 vs 0.304°/frame), the orientation inputs at the frame level
   (0.03°/frame roll after sub-frame alignment), the shared smoother's
   correction at identical settings (same to 0.1°), the table-predicted
   output motion at identical settings (0.98–1.05× per band, corr ≥ 0.993),
   inter-eye roll of the real exports (0.2–0.4° mean, all three cameras),
   static horizon tilt. MEASURED DIFFERENT: registering the real soft-stab
   exports to each other once per second (scratchpad `stab/cross_cam.py`,
   far-field DIS→Kabsch) puts the Osmo horizon within ±0.3° of the GoPro's
   except ONE Osmo-only transient at the first big turn (GoPro clock
   29–32 s, correction yaw +27°): a rise over ~2 s to a +1.5° peak held
   ~0.5 s, back to 0 (0.1 s zoom; the X6 stays <0.5° there; a second
   candidate at 62 s is ≤0.45° = noise); the X6 wanders ±1° slowly against
   both. Set 1 (0016/058/243): all three horizons agree to ±0.3° (roll std
   0.12°, p95 0.23–0.28°) for the whole clip — no Osmo event at all. Hand-eye comparison of the lock tables
   (data-audit agent): the Osmo's in-camera fused estimate shifts its tilt
   by 1–4° at each ~90° heading turn and holds it between turns (frame-
   invariant tilt-change magnitude up to 9° late in the clip); the X6's
   gyro-only integration (100 s trim) does not. In soft-stab a slow
   world-frame error cancels — actual and anchor carry it — leaving only the
   smoother-lag transient of each step (≈1–1.5° for ~τ at each turn): that
   is the roll residue. Not fixable from the file for the Osmo 360 I (no raw
   gyro; `[2.10]` is a 30 Hz accelerometer, gait-aliased; the dead
   `horizon_lock_quat`/`smooth_gravity_ema` would make it worse); the 360 II
   writes `[3.3.5]` 1 kHz gyro+gravity, where the X6 treatment would apply.
   Code: `dji_imu::apply_lock_ref_diag` + `VR180_LOCK_REF_FRAME=<k0>`
   (fisheye_export, both arms) re-anchors a camera-lock export to frame k0 —
   a lock anchored at frame 0 points wherever the camera pointed at record
   start. MEASUREMENT TRAPS (see the memory note): `acos((trace−1)/2)` on
   f32 tables floors steps <0.03° to exactly 0 (fake "frozen" output — use
   the antisymmetric part); body-frame rotation-vector "disagreement"
   between two cameras mixes heading disagreement into roll/pitch (fake
   multi-degree roll — use image registration or frame-invariant
   magnitudes); walking-scene lock residuals are parallax-dominated.

18. **Insta360 X6 DNG stills (2026-10-09) — new source kind + still export,
   SHARED code, unreleased.** The X6 `.dng` = one 15520×7760 TIFF/DNG 1.3,
   14-bit GBRG Bayer (WhiteLevel 16383, BlackLevel 0), Deflate strips,
   both fisheye circles side by side (left half = calibration entry 1 =
   LEFT eye post-mod, right half = entry 2 = RIGHT); ColorMatrix1 (D65),
   AsShotNeutral, BaselineExposure ≈ 2.6–2.7 EV per shot, OpcodeList2 = 4
   GainMap opcodes (one per CFA site, 13×34 grid over the whole frame, two
   vignetting centres, gain 1.0 → 2.65 at the rims = the lens shading);
   the factory lens strings (same 27-token format as `.insv`, 15488×7744
   frame) in EXIF UserComment and in the trailer; the trailer is the
   still-photo layout (the 1415 B before the 78 B footer ARE the
   ExtraMetadata protobuf: field 19 dims, 25 rolling shutter 37.5 ms, 27
   window_crop {src 7744² → dst 7760², offset 0}, 65 IMU ranges, 111/112
   lens strings, 210 a 3×3 orientation at capture; before it a backward
   chain `[data][u16 id swapped][u32 size]` with 0x0003 IMU (594 × 20 B,
   0.6 s around capture), 0x0009 AE log, 0x001b, 0x001c). CODE:
   `vr180_fisheye::dng` (reader: IFD walk, parallel inflate, tags, gain
   maps, lens strings, still trailer; 9 tests + an ignored sample test;
   decodes the 142 MB sample byte-identical to the Python reference in
   ~50 ms warm), `vr180_pipeline::raw_develop` (CPU: gain map, Malvar-He-
   Cutler demosaic, camera-linear RGBA64 scaled by `1/l_max` with the
   clipped support flagged in alpha and filled neutral; `develop_display`
   = WB → inv(ColorMatrix1) with Bradford to D65 → 2^(BaselineExposure+EV)
   → ACES-fitted tone → sRGB; 11 tests), `vr180_pipeline::dng_write` +
   `ljpeg` (linear DNG 1.4: IFD0 8-bit thumbnail + SubIFD LinearRaw
   3×16-bit, 256² lossless-JPEG tiles byte-identical to Adobe DNG
   Converter's own tiles, or uncompressed strips; the GainMap is baked in
   and `BaselineExposure += log2(l_max)`; 14 tests + an ignored system
   test: exiftool -validate OK, sips/ImageIO decodes, Adobe DNG Converter
   17.3 accepts sample-identically; NOTE macOS ImageIO rejects Deflate and
   multi-strip LJPEG, and any DNG < 100 KiB), `vr180_pipeline::still_export`
   (`export_still`: per half develop → `project_fisheye_to_equirect_texture_16`
   with `still_eye_calib` (= `stream_lens_calib(entry, crop, 7760, 7760)` →
   `FisheyeCalib::new_omni`) → readback → CPU SBS compose → PNG16 / JPEG /
   DNG; 0.6 s for 8192×4096 on the M5 Max), `SourceKind::Insta360Dng`
   (`is_still()`, detect = ext + TIFF header + Insta360 footer),
   GUI: still-aware load/batch-add (`dng::probe`, no video probe; fps 1,
   1 frame), settings bucket "dng", preset "Insta360 X6", no LUT, stab
   panel note, export bar "Still format" (PNG 16-bit / JPEG / DNG linear
   raw) + Native = 15520×7760, output `<stem>_SBS.<png|jpg|dng>`, the
   export worker branches on `is_still()`, command-line paths open at
   startup (`pending_open`). The preview arm (`run_still`, decoder.rs) renders
   the 2×2-binned develop through the texture-input projection and re-renders
   on settings changes. MEASURED on the sample: eye order correct (near
   objects −3° disparity, far −0.2°), far-field vertical offset +0.34° (the
   rig's relative pitch, same as video — stereo adjust handles it), Apple's
   raw pipeline renders the linear DNG with the same WB/exposure as our
   developed PNG. OPEN: the 7744→7760 padding is assumed centred (+8 px,
   the video rule; ≤0.44° common pan/tilt if wrong); gravity levelling from
   the trailer IMU/field 210 not implemented (pitch sign unverified); raw
   exposure/WB sliders not exposed (develop at 0 EV as-shot); `.insp` JPEG
   not supported; Windows readers (WIC/Lightroom) untested; the headless
   harness is `examples/dng_still_check.rs` (gitignored).
   GRADE (2026-10-09, same day): PNG/JPEG bake the 16-bit colour stack
   (`apply_color_stack_pair_16` on the two eye textures, as video). The
   linear DNG carries the grade as METADATA (`still_export::grade_metadata`,
   validated with exiftool / sips / Adobe DNG Converter): temperature/tint
   → `AsShotNeutral` via n' = n ⊘ ((M⁻¹·m) ⊘ n) with m = the stack's
   encoded gains ^2.2 and M = `camera_to_srgb_matrix` (universal); CDL →
   `ProfileToneCurve` = srgb_eotf(CDL(srgb_oetf(ACES(x)))) sampled at 129
   points, pinned (0,0)/(1,1), only when the CDL is not identity (the tag
   REPLACES the developer's default curve: Adobe + Apple honour it);
   saturation → `ProfileLookTable` dims 1×2×1 (Adobe/DNG-SDK only, Apple
   ignores); exposure_ev → `BaselineExposure`; per-eye trims baked as
   M⁻¹·diag(m)·M in camera space. Not carried: 3D LUT, sharpen, mid-
   detail, eye scale, auto eye match (logged + UI note). `ProfileName`
   "VR180 Silver Bullet - graded|linear", `ProfileEmbedPolicy` 0. Tag ids:
   ProfileLookTableEncoding is 51108 (0xC7A4), NOT 50983. Brightness
   ladder at the left-lens centre of 050 (sRGB 8-bit mean): camera .insp
   JPEG 50 | Apple render of our DNG 41 | our developed PNG/JPEG 69 |
   Adobe ACR preview of our DNG 84 — our ACES develop sits between the two
   developers' defaults. FIXED the same day: PNG16 output was byte-swapped
   (the image crate takes native-endian 16-bit samples), and the full-size
   export failed on a 482 MB readback buffer (now banded, see gpu.rs). The
   harness env SAT/TEMP/TINT/LIFT/GAMMA/GAIN/SHADOW/HIGHLIGHT exercises the
   colour stack on stills.
   HIDE (2026-10-09, same day): for a still the GUI hides (not greys) every
   control that cannot act on one photo — transport row, timeline, stab,
   RS, NR, Auto align, built-in log LUT buttons; with DNG output selected
   also the 3D LUT + Detail sections, and `Settings::still_dng_mode`
   (serde-skipped, set by the GUI each frame) makes `build_color_stack`
   drop LUT / sharpen / auto eye match so preview == DNG. Stills now take
   `FisheyeExportProjection` (Fisheye → `project_fisheye_to_fisheye_texture_16`,
   Reframe → `with_reframe` on the equirect kernel, as `run_still` already
   did for the preview) and the per-eye lens Override via
   `StillLensOverride::from_export_cfg` (same formulas as the video
   resolvers; preset k fallback = explicit preset else "Insta360 X6").
   FREEZE (2026-10-09, evening): "UI stopped responding after exporting
   flat" = the export of the LOADED still called `stop_playback()` (log:
   "decoder thread exited cleanly" 11 ms after "export: spawned worker"),
   and nothing restarts a still's thread (no Play button; sliders bump a
   generation on `control = None`, which is not even logged). Not a
   main-thread block (sample: idle in `-[NSApplication run]`; reviewed by
   three independent agents). Fix: `spawn_export_job` keeps a still's
   preview alive (it holds only textures), "Restart preview" button in the
   still transport row, `stop_playback` clears `decoder_starting`,
   `maybe_reseed_fisheye_override` keeps the last seed across a same-clip
   respawn (reset in `unload_clip`), stills never "play" (Space revives
   paused), no `AudioPlayer::open` on a still, one-shot WARN when settings
   change with no decoder. Known, accepted: during a still export the
   worker's `poll(wait)` holds wgpu's fence read guard per 64 MiB band, so
   a slider drag hitches for ms — same as previewing another clip during
   a video export.
   FULL-RES PREVIEW (2026-10-09, evening): `run_still` renders two tiers —
   QUICK = `preview_eye_w` (slots 0/1) while settings are moving, FULL =
   the binned source size via `preview_out_dims(mode, aspect, src_w,
   src_h)` capped to `max_texture_dimension_2d/2` (slots 20/21), chosen
   by `STILL_SETTLE` = 120 ms since the previous render's end (a change
   within it = drag → quick, then full once the settings rest). First
   frame 7760×3880 SBS; the log line says "(full)"/"(quick)". The
   "Preview size" menu is hidden for stills. The zoom magnifier on a still
   samples `current_display` (no DetailCache for stills), so it is sharp
   now without a separate full-res path.
   NATIVE tier (same evening): `run_still` keeps the mosaic (`bayer`) and
   lazily develops the unbinned 7760² pair (`native`, slots 22/23, one
   failed attempt → binned fallback, logged). Native SOURCE for every
   resting Reframe render (output stays 3880 wide; the viewport samples a
   small lens region so source res is what shows) and native source +
   native OUTPUT (15520×7760 SBS) when `Settings::still_native_zoom` is
   set — the central panel computes `still_native_wanted` =
   zoom × on-screen eye px (physical) > binned eye px × 0.95. Needs the
   16384 texture limit: all three device descriptors (eframe's in main.rs,
   `Device::new`, the Windows dedicated export device) now request
   `adapter.limits().max_texture_dimension_2d.clamp(8192, 16384)`; the
   scratch example `tex_limit_check` confirmed 16384 + a 15520×7760
   compose on the M5 Max. Log line: "(native|full|quick, native|binned
   source)".

**Most recent batch (developed on macOS, then merged with the Windows EAC work):**
- **In-process noise reduction** — `VTTemporalNoiseFilter` via objc2 FFI (no
  Swift helper); GPU-resident zero-copy P010 for OSV **and** `.360`/EAC. The
  output P010 range must match the source range (full for `.360`, video for
  OSV) or you get a uniform brightness shift. See `vt_denoise.rs`.
- **Unified export/batch UX** — a single export is a batch-of-one; one queue
  runner + one `ExportOptions`; a persistent bottom export bar
  (`draw_export_bar`) with overall progress + ETA. The separate batch and
  export-options windows were deleted.
- **Chinese (中文) localization** — process-global `UI_LANG` + `tr()`/`zh()`
  table, EN/中文 toggle, bundled CJK system font (macOS `y_offset_factor`
  0.24; Windows 0.0).
- **GoPro `.360` firmware-RS auto-detect** — `decoder::detect_rs_mode` seeds
  the per-clip RS mode from the CORI signal (`cori_indicates_no_firmware`,
  the same predicate `CoriSource::Auto` uses); the toggle still overrides.
- **Windows GoPro `.360`** — zero-copy EAC preview + GPU-resident NVENC
  export (merged Windows work) + an Inno Setup installer.

> ⚠️ Building the WHOLE workspace on macOS fails on `vr180-render` (a legacy
> CLI on a stale wgpu API) — ignore it, build `-p vr180-gui`.

The macOS feature batch that the Windows session verified (kept as the
engineering record — the checklist was what to *confirm*, not to port):

1. **DJI lens model is now exact** (matched to DJI Studio's output —
   verified against its export). Auto mode loads the per-lens FACTORY
   calibration from the OSV protobuf: fx/fy, raw cx/cy (top-left, y-down,
   NO flip), k1–k4, **k5 (field 15)**, and **Brown-Conrady tangential
   p1/p2 (field 20)**. All KB shaders run the 5-coeff radial + tangential
   (zeros ⟹ byte-identical to the old 4-coeff path, so non-OSV sources are
   unchanged). Verify: load an OSV → dewarp matches DJI Studio.
2. **Fisheye SBS output is now a normalized 195° equidistant fisheye**
   (`FISHEYE_OUT_FULL_FOV_DEG` in `fisheye_export.rs`) — the projection is
   canonical (source-lens distortion removed), disk edge = 97.5° off-axis.
   Both preview + export, incl. the GPU-resident NVENC path. Was: the
   source lens's own projection.
3. **Soft-stab rewritten** — velocity-dampened Gyroflow-style smoothing
   ported from the Python app (`dji_imu.rs::smooth_quats_velocity_dampened`):
   velocity-adaptive τ with ±200 ms look-ahead, symmetric fwd/bwd passes,
   SOFT elastic max-corr limit (no snap). New **Response** slider
   (0.2–3.0). Camera-lock (`smooth_ms = 0`) keeps the hard clamp. Defaults:
   smooth 1200 ms, max corr 15°.
4. **Eye orientation**: "Swap L↔R" now actually applies live (toggling
   auto-reloads the clip — the dual-stream iterators bind eye order at
   open), and the per-eye factory calib **follows the stream** (was a bug:
   swapped view used the wrong lens's calib). New **"Upside-down mount
   (180°)"** checkbox = exact `roll+180°` in `ViewAdjust` + implicit eye
   swap (`Settings::effective_swap_eyes()`).
5. **8K export option** (export dialog → Resolution → 8192×4096): renders
   the PROJECTION at 4096²/eye (single resample from the native source —
   not a last-step upscale). Verify NVENC accepts 8192-wide @ Main10 on
   the target GPU (4090: OK).
6. **UI/perf**: View-adjustment panel above Stabilization (default-open);
   "KB parameters" collapsible (k1–k5 + p1/p2 sliders, OSV-gated); Override
   auto-reseeds from each new clip's in-file calib; camera-preset/Gyroflow
   pickers hidden for OSV; sidebar slider tracks fixed at 150 px (do NOT
   size sliders from `available_width()` — feedback loop, panel swallows
   the preview; panel capped at 420 px); zoomed paused-frame drag re-render
   throttled to 120 ms (was per pointer event on the UI thread).

> ✅ **Windows verification done** (DX12/Vulkan + NVENC, RTX 4090): all six
> confirmed — the new lens model / 195° fisheye / soft-stab flow into the
> GPU-resident CUDA path automatically via the shared `resolve_calib_pair`
> + WGSL; GPU-resident vs portable-CPU diff is encoder-noise only (mean
> ≤2/255); 8K = 8192×4096 Main10 accepted, ~17.5 fps. One real gap found
> and FIXED on Windows: **fisheye-output RS parity** — macOS p010
> RS-corrects fisheye output (`fisheye_p010_to_fisheye_rs_16`), but the
> Windows zero-copy preview + GPU-resident export had no RS-capable
> fisheye projection. Added `fisheye_to_fisheye_rs_16` (same WGSL,
> format-patched to rgba16) + `project_fisheye_rgba16_texture_to_fisheye_rs_16`
> and wired both call sites with the same `(projection, rs)` split as the
> macOS path. Nothing to verify on macOS (the macOS paths were already
> correct); listed so the divergence isn't reintroduced.

> 🔍 **Full cross-platform parity audit (Windows session, 2026-06-10).**
> OSV preview, detail-still, color stack, preview gamma, video-range tags,
> calib (k5/p1/p2/swap-follow/195°), stab + RS: **in parity** (macOS OSV
> preview is the shared CPU worker; Windows zc worker matches it). The
> Windows **readback export path now covers Fisheye too** (same
> `(projection, rs)` split, slots 30/31) so libx265 / 8-bit /
> GPU-resident-fallback fisheye exports keep RS instead of dropping to the
> portable no-RS path. Audit items, as resolved on macOS 2026-06-10:
> 1. ~~libx265 on macOS = no RS~~ / 2. ~~ProRes = no RS on both~~ —
>    **FIXED: the PORTABLE path itself now applies per-row RS** (same CPU
>    `compute_per_row_quaternions_for_frame` build + the same rgba16 RS
>    kernels via `build_eye_eq_16(rs)`, slots 30/31; 8-bit arms use the
>    byte-path `*_rs_texture` kernels). Covers macOS libx265 (8 + 10-bit),
>    ProRes on BOTH platforms, and every future fallback — the portable
>    path is now feature-complete, just slower. Bonus: the 10-bit arms no
>    longer build-and-discard two 8-bit projections per frame (the old
>    unconditional `left_tex`/`right_tex`). Verified vs the macOS p010 VT
>    reference: stab+RS libx265 vs VT mean diff = encoder noise (see
>    `examples/rs_portable_check.rs`, gitignored).
> 3. The GoPro `.360` EAC preview (legacy) ignores the view-adjust panel on
>    both platforms (per-eye bundle predates it) — consistent, just a
>    legacy-path limitation. Still open.
>
> Two MORE real bugs found + fixed while verifying (macOS, 2026-06-10):
> 4. **ProRes-VT was broken at encoder open under ffmpeg 8** —
>    `prores_videotoolbox` rejects `yuv422p10le` (its input list: yuv420p,
>    nv12, ayuv64le, uyvy422, p010le, nv16). Now fed **AYUV64LE** (16-bit
>    packed 4:4:4 — the only listed input keeping ≥10-bit + full chroma; VT
>    converts to the profile's 4:2:2 internally). `prores_ks` keeps
>    yuv422p10le. Verified: ProRes HQ 10-bit 4:2:2 output, RS-corrected
>    (1.6/255 vs the VT HEVC reference). FOLLOW-UP the same day: ProRes-VT
>    now takes the **zero-copy P010 path** (same CVPixelBuffers as VT-HEVC
>    via `create_zero_copy_vt_prores`; profile via ctx field, codec_tag
>    left 0 so the encoder sets apch/…) — ~3 fps → **~17.6 fps** @ 8K, and
>    0.7/255 vs the HEVC reference (identical projected pixels). The
>    AYUV64LE readback route remains as the non-OSV-source fallback.
> 5. **Trimmed exports were wrong on every path**: (a) the iterators'
>    `seek(trim_in)` lands on the keyframe AT/BEFORE the target with no
>    decode-forward, so exports gained up to a GOP of pre-roll (trim 2–3 s
>    → 90 frames from 0.0); (b) the audio mux copied the FULL source audio
>    at original timestamps → over-long file AND audio offset by trim_in.
>    Fixed: all 4 export loops drop pre-trim frames (`pts < t_in − dt/2`),
>    and `mux_video_with_passthrough_audio` takes `(audio_offset_s,
>    max_duration_s)` — skips, rebases to 0, and stops at the video's
>    length. Verified: trim 2..3 s → exactly 30 frames + 1.00 s audio on
>    both portable and zc paths.

Build/run quick-ref (build `-p vr180-gui`, **not** the workspace):
```pwsh
# Windows
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # LLVM 17+
$env:FFMPEG_DIR    = "C:\path\to\ffmpeg-8.1-dev"   # 8.1+ REQUIRED (prores_ks_vulkan)
cargo build --release -p vr180-gui
$env:PATH = "$env:FFMPEG_DIR\bin;$env:PATH"; .\target\release\vr180-gui.exe
```
```sh
# macOS
cargo build --release -p vr180-gui && ./target/release/vr180-gui
```
Full Windows step-by-step (toolchain, FFmpeg, gotchas):
[docs/WINDOWS_BUILD.md](docs/WINDOWS_BUILD.md).

> ⚠️ The Windows-only additions (`nvenc_cuda`, the `cudarc` dep, D3D11→CUDA
> interop) are `#[cfg(target_os = "windows")]` / `[target.'cfg(windows)']`
> gated — they don't build on macOS. The secondary `vr180-render` CLI still
> has a pre-existing non-exhaustive-match error; ignore it (build the GUI).
> The verification examples under `crates/vr180-pipeline/examples/` are
> gitignored local scaffolding (several are Windows/CUDA-only).

## What this is

**VR180 Silver Bullet 2.0** — a clean-room Rust rewrite of a VR180
camera-mod processor. Single self-contained native binary per platform,
no Python. The reference implementation is the Python/PyQt6 app at
`../vr180_processor/vr180_gui.py` (the `main` branch / sibling worktree)
— **we read it to port algorithms, we don't modify it.**

- **Shipped:** a native desktop GUI, `vr180-gui` (**eframe 0.34 + egui +
  egui-wgpu + wgpu 29** — note: NOT 0.28/0.20; that stale version line hid a
  display-gamma bug, see Lesson #2). This is the product.
- **Primary source format:** DJI Osmo 360 `.osv` (dual-stream fisheye).
  Also handles SBS fisheye, Blackmagic `.braw`, and GoPro Max `.360`
  (EAC) as a legacy path.
- **GPU:** `wgpu` only (Metal on macOS, DX12/Vulkan on Windows). Shaders
  are WGSL in `crates/vr180-pipeline/src/shaders/`.
- **Video I/O:** in-process libav via `ffmpeg-next 8.1` (no shelling out
  to a system `ffmpeg`).

## Build & run

| | macOS | Windows |
|---|---|---|
| Build the GUI | `cargo build --release -p vr180-gui` | same (see WINDOWS_BUILD.md for env) |
| Run | `./target/release/vr180-gui` | `.\target\release\vr180-gui.exe` (+ ffmpeg DLLs on PATH) |
| FFmpeg | `brew install ffmpeg pkg-config` (pkg-config auto-finds it) | `FFMPEG_DIR` → avbuild dev dist + `LIBCLANG_PATH` |

First build ~3-5 min (ffmpeg-sys bindgen); incremental builds are seconds.

To launch + verify after a change, the dev loop on macOS is:
`pkill -9 -f target/release/vr180-gui; cargo build --release -p vr180-gui && ./target/release/vr180-gui &`

## Cross-platform stance — the #1 rule

Code must compile and run on **both macOS and Windows**. Two platform
paths exist on purpose, behind `#[cfg(target_os = "...")]`:

- **macOS only:** `interop_macos` (IOSurface↔Metal↔wgpu zero-copy),
  `ZeroCopyDualStreamFisheyeIter` (VT P010 zero-copy decode), the P010
  IOSurface export path — gates on **VT encode (HEVC OR ProRes-VT)**;
  ProRes-VT rides the same P010 CVPixelBuffers into Apple's hardware
  ProRes engine via `create_zero_copy_vt_prores` (~17 fps @ 8K HQ, was
  ~3 fps on the readback path). Only libx265 / prores_ks ride the
  portable readback path, which since 2026-06-10 also carries per-row
  RS. The crates `metal` / `core-foundation` / `objc` / `foreign-types`
  are `[target.'cfg(target_os = "macos")'.dependencies]` — they never
  build on Windows.
- **Cross-platform + Windows fast paths:** `d3d11va` (NVDEC) decode —
  `DualStreamFisheyeIter` for stills, `D3d11SharedDualStreamIter` for the
  **zero-copy live preview** (D3D11 P010→RGBA16→Vulkan, no CPU readback);
  `hevc_nvenc` (H.265, the **Windows default**, GPU-resident — see below)
  with a `libx265` software fallback; `prores_ks` (ProRes); the whole wgpu
  compute pipeline; GUI; audio (cpal→WASAPI); file picker (rfd). Encoder
  backend auto-selected per-OS in `app.rs::commit_export`. Windows now has
  its OWN zero-copy fast paths — it's no longer "macOS fast, Windows slow."

Rules: every cfg gate matters; `pub use` from a macOS module must itself
be gated (see `vr180-pipeline/src/lib.rs`); use `Path`/`PathBuf` (never
slash string literals); `include_str!`'d shaders + `assets/` are portable.

## Architecture (where things live)

- `crates/vr180-core` — 100% portable: gyro/quaternion math, EAC dims,
  `.cube` LUT parse, GEOC calib, segments.
- `crates/vr180-pipeline` — the engine: `decode.rs`, `fisheye_decode.rs`,
  `gpu.rs` (all wgpu kernels + the color stack), `fisheye_export.rs`
  (export pipeline + the zero-copy P010 fast path), `encode.rs` (encoder
  backends), `dji_imu.rs` (OSV stabilization + rolling-shutter),
  `interop_macos.rs` (macOS-only), `panomap.rs`, `shaders/*.wgsl`.
- `crates/vr180-gui` — the app: `app.rs` (egui UI + the full-res/zoom
  "DetailCache" + export wiring), `decoder.rs` (`Settings`, the live
  decode worker, `DetailCache`, color-stack plan, persistence),
  `audio_player.rs`, `assets/` (bundled LUT).
- `crates/vr180-fisheye` — camera presets + fisheye calib (Kannala-Brandt).
- `crates/vr180-render` — legacy CLI (currently doesn't compile; ignore).

## DJI OSV lens model (exact — don't simplify)

Empirically matched to DJI Studio's output, bit-for-bit.
Per-lens calib block in the OSV protobuf (`vr180-fisheye/src/dji_osv.rs`):
`1=fx 2=fy 3=cx 4=cy 5..8=k1..k4 10=W 11=H 15=k5 20=[p1,p2]
21=mount_quat`. Projection (in every KB WGSL shader's `project_kb`):

```
θ_d = θ + k1·θ³ + k2·θ⁵ + k3·θ⁷ + k4·θ⁹ + k5·θ¹¹      # 5-coeff Kannala-Brandt
u' = u + 2·p1·u·v + p2·(r²+2u²)                        # Brown-Conrady tangential
v' = v + p1·(r²+2v²) + 2·p2·u·v                        #   (r² = θ_d²)
src = (cx + fx·u',  cy − fy·v')                        # raw cx/cy, NO flip
```

k5 keeps the radial map monotonic past ~90° (the 4-coeff poly folds at
~87–90°); the tangential is small (~2–4 px at the rim) but it's what
finally matched DJI. Both resolvers (`decoder.rs::resolve_fisheye_calib_pair`
and `fisheye_export.rs::resolve_calib_pair`) must stay in lockstep — every
calib change goes in BOTH or preview ≠ export.

## Color pipeline — match the Python app

The color tools (CDL, 3D LUT, white balance, saturation, sharpen,
mid-detail) were just aligned to the Python app **exactly**. Order
(`gpu.rs::record_color_stack` / `apply_color_stack_per_eye_16`):

> **CDL → 3D LUT → sharpen → temp/tint → mid-detail → saturation**

- temp/tint is **post-LUT** (critical for log footage: it must act on the
  LUT's Rec.709 output, not the log input). White-balance and saturation
  are split passes of the one `color_grade` shader.
- CDL clips to `[0,1]` **before** gamma (matches `build_color_1d_lut`).
- Math constants match Python: temp/tint = `R×(1+0.3t)`,`G×(1−0.3·tint)`,
  `B×(1−0.3t)`; saturation = `lerp(BT.601-luma, color, sat)`;
  shadow/highlight = smoothstep masks ×0.6 about pivot 0.5.
- The DJI Osmo 360 **D-LogM→Rec.709 LUT is bundled** (`include_str!`,
  `crates/vr180-gui/assets/`), autoloaded for OSV, cached per-thread.

## 10-bit end-to-end (when 10-bit output is selected) — non-negotiable

Every stage must hold ≥10-bit precision. NOTE: intermediates are
**`Rgba16Unorm`** (not `Rgba16Float` — the old CLAUDE.md was wrong).
- macOS: VT P010 IOSurface decode → 16-bit project → 16-bit color stack →
  P010 IOSurface encode (Main10), zero-copy.
- Windows: NVDEC P010 decode → D3D11 P010→RGBA16 → 16-bit project → 16-bit
  color stack → P010 compose → **GPU-resident CUDA NVENC** (`--profile
  main10`), zero-copy (see "Windows GPU-resident export"). Fallback:
  P010/RGB48 readback → `hevc_nvenc`/`libx265`.
- The **preview** runs the same 16-bit color stack as the export, then
  composes to an `Rgba8Unorm` SBS for egui. Don't add an 8-bit color
  shortcut into the graded path.

## Windows GPU-resident H.265 export (the fast path)

On Windows, 10-bit H.265 export runs **fully GPU-resident** — no CPU
readback — at the NVENC ceiling (~36 fps @ 8K on a 4090):

> NVDEC P010 decode → D3D11 P010→RGBA16 → wgpu 16-bit project/color/compose
> → P010 plane textures → **exportable Vulkan images → CUDA external memory
> → `cuMemcpy2D` (DtoD) into NVENC's CUDA frame** → `hevc_nvenc`.

Lives in `nvenc_cuda.rs` (`#[cfg(target_os = "windows")]`), dispatched from
`fisheye_export.rs`. `cudarc` is a `[target.'cfg(windows)']` dep. The export
uses a **dedicated** wgpu device (`gpu.rs::new_dedicated_from_adapter`) so it
can't deadlock eframe's renderer (Lesson #1). Falls back to a P010/RGB48
readback `hevc_nvenc`/`libx265` path. Both `HalfEquirect` and `Fisheye`
output modes take the fast path. The `.360` arm (`export_eac_gpu_resident`)
also carries **ProRes and libx265**: same GPU front end, encode tail by
backend — NVENC via CUDA/P010, ProRes via GPU 4:2:2 readback into
`prores_ks_vulkan` (FFmpeg 8.1+; CPU `prores_ks` fallback), libx265 via
RGBA64. Every landing on a slow path warns AND sets
`fisheye_export::export_path_note` (shown in the export bar).

**Color range:** both the GPU-resident and readback paths compose
**video-range** YCbCr and tag `AVCOL_RANGE_MPEG` (Rec.709) — the
distribution standard (YouTube VR180, headsets), matching the source and the
gamma-correct preview. `compose_sbs_to_p010_textures(…, full_range: bool)`
takes a flag but **both callers pass `false`**; the brief full-range
experiment was a band-aid for the preview-gamma bug (Lesson #2) and is gone.

## Hard-won lessons — DON'T regress these

1. **Never do wgpu GPU work on a non-main thread.** The wgpu
   `Device`/`Queue`/swapchain is shared with eframe's renderer; GPU
   submits + `Maintain::Wait` from a second thread wedge Metal's drawable
   queue against the presenting main thread → permanent 0% CPU deadlock.
   `DetailCache` decodes on a background thread (CPU/VideoToolbox only —
   it returns `Vec<u8>` pairs, never touches wgpu) and does the GPU
   project/compose on the **main** thread. Keep it that way.
2. **Preview gamma (egui-wgpu 0.34 — corrected):** egui-wgpu's fragment
   shader expects display textures that are **NOT sRGB-aware** — it samples
   them as gamma/raw bytes (and on an sRGB swapchain linearizes at the very
   end so the hardware re-encode is identity). Our SBS already holds
   Rec.709-gamma bytes, so register it with a **plain `Rgba8Unorm` view** on
   every platform (`preview_view_format` in `app.rs`; used at both
   `register_native_texture` calls). An **sRGB view double-decodes** → the
   preview shows `sRGB⁻¹(v)`, too dark. Windows' non-sRGB `Bgra8Unorm`
   swapchain made it obvious; on macOS's sRGB swapchain it's wrong via a
   different path. The old "register with an sRGB view" advice was correct
   only under **egui-wgpu 0.28** and is now stale — **do not restore it.**
3. **Detail still = exact frame, decode-forward skip:** the native decoder
   seeks to a keyframe and **decodes forward** to the requested pts (else the
   still shows a keyframe up to a GOP away). The OSV dual-stream path uses
   `DualStreamFisheyeIter::decode_forward_to`: it decodes the intermediate
   GOP frames on the HW engine but runs the HW→CPU download + swscale only on
   the **target** frame (≈11 s → ≈0.6 s for a deep seek at 8K). Same
   decode-forward in the live worker's seek.
4. **OSV gyro timing:** the sensor readout (SROT) comes from the file's
   line time (`DjiOsvImu::readout_ms`: 18.301 ms ≤ 30 fps, 16.228 ms @
   50 fps, 16.06 ms on the 360 II @ 60 fps); `dji_osmo_readout_ms_for_fps`
   is the fallback for files without one. The per-frame stab pose is
   sampled at the centre row's mid-exposure (SROT/2 + timestamp gap −
   shutter/2, all from the file — item 7 under Status) and the
   rolling-shutter readout window is centred on the same point (coupled,
   as DJI does). There is no user-facing timing control. See `dji_imu.rs`.
5. **Settings persistence** lives in a per-OS config dir (macOS App
   Support / Windows `%APPDATA%` / Linux XDG) — `Settings::config_path`.
   It survives both relaunch and loading a new clip (only trim resets).

## Porting algorithms from Python

1. Read `../vr180_processor/vr180_gui.py` (the working reference).
2. Port the math exactly; bit-validate against it when feasible. The
   camera-specific constants are already validated against real footage —
   don't re-derive.
3. NOTE for the Windows session: the deep project memory the macOS dev
   used lives in that machine's `~/.claude/...` and is **not on this PC**.
   The essential facts are summarized here and in `docs/`; if you need
   more on GEOC calibration / GoPro stream conventions / VQF, read the
   Python source directly.

## Don't

- Don't run wgpu/GPU work off the main thread (see lesson #1).
- `wgpu` is the GPU answer for compute/render. The ONE sanctioned exception
  is the Windows NVENC export, which uses `cudarc` purely to feed NVENC
  (Windows-gated); don't add CUDA/OpenCL/Numba anywhere else.
- Don't shell out to a system `ffmpeg` — use `ffmpeg-next` in-process.
- Don't remove a `#[cfg(target_os = "...")]` gate without confirming the
  API exists on the other platform; the Mac-only paths are FAST paths
  with deliberate cross-platform fallbacks.
- Don't drop the graded path to 8-bit; don't re-introduce the off-thread GPU
  deadlock; don't switch the preview back to an sRGB texture view (Lesson #2)
  or the H.265 export back to full-range.

## Doc pointers

- [docs/WINDOWS_BUILD.md](docs/WINDOWS_BUILD.md) — **the Windows handoff guide (start here on Windows).**
- [docs/BUILD.md](docs/BUILD.md) — original cross-platform build notes.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — crate boundaries, GPU pipeline shape.
- [docs/ROADMAP.md](docs/ROADMAP.md) — phased plan (note: predates the GUI; treat as historical).
