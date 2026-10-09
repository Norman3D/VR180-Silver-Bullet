//! High-level IMU input prep — file path → arrays VQF can consume.
//!
//! Pipeline (matches `parse_gyro_raw.py::vqf_to_cori_quats`):
//!
//! 1. Extract the GPMF data stream via [`crate::decode::extract_gpmf_stream`].
//! 2. Parse raw GYRO/ACCL/GRAV/MNOR blocks via `vr180_core::gyro::raw`.
//! 3. Flatten gyro samples, applying the ORIN="ZXY" → body-frame axis
//!    remap (`col0 = raw[1], col1 = raw[2], col2 = raw[0]`).
//! 4. Pick accelerometer source: `GRAV × 9.81` if the gravity vector
//!    has plausible magnitude, otherwise raw `ACCL`.
//! 5. Resample acc (and mag, if MNOR present) to gyro rate by
//!    nearest-neighbor proportional indexing — same algorithm Python uses.
//!
//! Output is consumed by `vr180_core::gyro::vqf::run`.

use crate::{Error, Result};
use std::path::Path;
use vr180_core::gyro::raw::{parse_raw_imu, RawImu, ImuBlock};
use vr180_core::gyro::cori_iori::Quat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccSource { Grav, Raw, None }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MagSource { Mnor, None }

/// Sample arrays ready to be fed to `vr180_core::gyro::vqf::run`.
#[derive(Debug)]
pub struct PreparedImu {
    /// Per-sample 3-axis angular velocity, **rad/s, body frame**.
    pub gyro_body: Vec<[f32; 3]>,
    /// Per-sample 3-axis acceleration, **m/s², body frame**, resampled to gyro rate.
    pub acc_body: Vec<[f32; 3]>,
    /// Optional per-sample 3-axis magnetic-north vector, resampled to gyro rate.
    pub mag_body: Option<Vec<[f32; 3]>>,
    /// Gyro sample interval in seconds, computed from the STMP-anchored
    /// span (`(t[-1] - t[0]) / (N - 1)`) — matches Python's `gyro_dt`
    /// at `parse_gyro_raw.py:1508-1509`. Falls back to
    /// `probe_duration / N` only if gyro times can't be built.
    pub gyr_ts: f32,
    /// STMP-anchored per-sample video-time for each gyro sample.
    /// Exposed so callers can drive the per-frame resample with the
    /// same timeline used to derive `gyr_ts` and to linearly
    /// interpolate the acc/mag inputs.
    pub gyro_times: Vec<f32>,
    pub acc_source: AccSource,
    pub mag_source: MagSource,
    /// Raw counts pre-resample for the printout (acc, mag).
    pub n_acc_input: usize,
    pub n_mag_input: usize,
    /// Per-frame exposure (s) from the SHUT stream; empty when absent.
    pub shutter_s: Vec<f32>,
    /// Per-frame cumulative dropped-frame count (MSKP); empty when absent.
    pub frame_skips: Vec<i32>,
}

/// Build [`PreparedImu`] from a .360 file. Equivalent to the input-prep
/// half of `vqf_to_cori_quats` in Python — but matches the
/// **multi-segment** Python pipeline's timing/resampling behaviour:
///
/// - `gyr_ts` is the actual STMP-anchored span / (N - 1).
/// - acc and mag are **time-linearly interpolated** to the gyro
///   timeline (not nearest-neighbour proportional indexing).
pub fn prepare_for_vqf(path: &Path) -> Result<PreparedImu> {
    use crate::decode::{extract_gpmf_stream, probe_video};
    use vr180_core::gyro::{
        resample::build_imu_sample_times_with_skips,
        cori_iori::{parse_cori_with_stmps, parse_shut, parse_mskp},
    };

    let gpmf = extract_gpmf_stream(path)?;
    let probe = probe_video(path)?;
    let raw = parse_raw_imu(&gpmf);
    let shutter_s = parse_shut(&gpmf);
    let frame_skips = parse_mskp(&gpmf);
    if let Some(&last) = frame_skips.last() {
        if last < 0 {
            let at: Vec<usize> = frame_skips.windows(2).enumerate()
                .filter(|(_, w)| w[1] < w[0]).map(|(i, _)| i + 1).collect();
            tracing::info!(
                "GoPro dropped {} sensor frame(s) (MSKP) — video repeats a frame at frame(s) {:?}; gyro timeline re-anchored per frame",
                -last, at,
            );
        }
    }
    let skips_opt: Option<&[i32]> = if frame_skips.is_empty() { None } else { Some(&frame_skips) };
    if raw.gyro.is_empty() {
        return Err(Error::Ffmpeg("no GYRO blocks in GPMF stream".into()));
    }
    let (_cori_q, cori_stmps) = parse_cori_with_stmps(&gpmf);
    let probe_duration = probe.duration_sec as f32;
    let fps = probe.fps;

    // 3a. Flatten gyro with ZXY → body-frame axis remap.
    // GoPro ORIN="ZXY": raw[0]=Z, raw[1]=X, raw[2]=Y.
    // Body frame: bodyX←raw[1], bodyY←raw[2], bodyZ←raw[0].
    let mut gyro_body: Vec<[f32; 3]> = Vec::with_capacity(raw.total(|r| &r.gyro));
    for blk in &raw.gyro {
        for s in &blk.samples {
            gyro_body.push([s[1], s[2], s[0]]);
        }
    }

    // 3b. STMP-anchored per-sample video-time for each gyro sample.
    // Used as the master timeline for everything downstream.
    let gyro_times = build_imu_sample_times_with_skips(&raw.gyro, &cori_stmps, skips_opt, fps, probe_duration);

    // 3c. `gyr_ts` from the actual STMP-anchored span (matches Python's
    // `gyro_dt = (combined_gyro_times[-1] - combined_gyro_times[0]) / (N - 1)`
    // at `parse_gyro_raw.py:1508-1509`). Falls back to
    // `probe_duration / N` if STMPs weren't available (gyro_times then
    // came from the uniform-spacing fallback in `build_imu_sample_times`).
    let gyr_ts = if gyro_times.len() >= 2 {
        let span = gyro_times[gyro_times.len() - 1] - gyro_times[0];
        (span / (gyro_times.len() - 1) as f32).max(1e-6)
    } else {
        (probe_duration / gyro_body.len().max(1) as f32).max(1e-6)
    };

    // 4. Pick acc source: GRAV × 9.81 (already filtered by GoPro firmware,
    //    cleaner than raw ACCL) if gravity magnitude is plausible.
    //    Time-based linear interpolation to gyro times.
    let (acc_body, acc_source, n_acc_input) =
        pick_acc_source(&raw, &cori_stmps, skips_opt, fps, probe_duration, &gyro_times);

    // 5. Pick mag source: MNOR (firmware-calibrated magnetic north).
    //    Same time-based interpolation.
    let (mag_body, mag_source, n_mag_input) =
        pick_mag_source(&raw, &cori_stmps, skips_opt, fps, probe_duration, &gyro_times);

    Ok(PreparedImu {
        gyro_body,
        acc_body,
        mag_body,
        gyr_ts,
        gyro_times,
        acc_source,
        mag_source,
        n_acc_input,
        n_mag_input,
        shutter_s,
        frame_skips,
    })
}

/// Per-frame exposure (s) across a chapter chain, aligned to the global
/// frame index: each segment contributes `round(duration·fps)` entries
/// (its SHUT values, padded with NaN where the stream is short or absent),
/// so entry `i` is frame `i` of the chained timeline. Callers that already
/// hold the GPMF of a single file should use [`vr180_core::gyro::parse_shut`]
/// directly (this re-extracts the data track).
pub fn gopro_shutter_per_frame(segments: &[std::path::PathBuf], fps: f32) -> Vec<f32> {
    use crate::decode::{extract_gpmf_stream, probe_video};
    use vr180_core::gyro::parse_shut;
    let mut out = Vec::new();
    for seg in segments {
        let n = probe_video(seg).map(|p| (p.duration_sec * fps as f64).round() as usize).unwrap_or(0);
        let mut shut = extract_gpmf_stream(seg).map(|g| parse_shut(&g)).unwrap_or_default();
        shut.resize(n, f32::NAN);
        out.extend_from_slice(&shut);
    }
    out
}

fn flatten_with_remap(blocks: &[ImuBlock], remap: [usize; 3]) -> Vec<[f32; 3]> {
    let mut out = Vec::with_capacity(blocks.iter().map(|b| b.samples.len()).sum());
    for b in blocks {
        for s in &b.samples {
            out.push([s[remap[0]], s[remap[1]], s[remap[2]]]);
        }
    }
    out
}

/// Per-component linear interpolation of a 3-vector timeseries
/// (`src[i]` at `src_times[i]`) onto `dst_times`. Edge-clamps. Mirrors
/// Python's `np.interp(dst_times, src_times, src[:, c])` per component
/// — the same operation `vqf_to_cori_quats_multi_segment` uses to drop
/// the acc/mag streams onto the gyro timeline
/// (`parse_gyro_raw.py:1492-1494`).
fn resample_time_linear(
    src: &[[f32; 3]],
    src_times: &[f32],
    dst_times: &[f32],
) -> Vec<[f32; 3]> {
    if src.is_empty() || src_times.len() != src.len() {
        return vec![[0.0; 3]; dst_times.len()];
    }
    if src.len() == 1 {
        return vec![src[0]; dst_times.len()];
    }
    let n_src = src.len();
    let last_t = src_times[n_src - 1];
    let first_t = src_times[0];
    let mut out = Vec::with_capacity(dst_times.len());
    for &t in dst_times {
        if t <= first_t {
            out.push(src[0]);
            continue;
        }
        if t >= last_t {
            out.push(src[n_src - 1]);
            continue;
        }
        // Upper-bound index in src_times for `t`.
        let hi = src_times
            .partition_point(|x| *x <= t)
            .max(1)
            .min(n_src - 1);
        let lo = hi - 1;
        let span = (src_times[hi] - src_times[lo]).max(1e-9);
        let frac = ((t - src_times[lo]) / span).clamp(0.0, 1.0);
        out.push([
            src[lo][0] + frac * (src[hi][0] - src[lo][0]),
            src[lo][1] + frac * (src[hi][1] - src[lo][1]),
            src[lo][2] + frac * (src[hi][2] - src[lo][2]),
        ]);
    }
    out
}

fn pick_acc_source(
    raw: &RawImu,
    cori_stmps: &[vr180_core::gyro::cori_iori::CoriBlockStmp],
    frame_skips: Option<&[i32]>,
    fps: f32,
    probe_duration: f32,
    gyro_times: &[f32],
) -> (Vec<[f32; 3]>, AccSource, usize) {
    use vr180_core::gyro::resample::build_imu_sample_times_with_skips;

    // GRAV uses axis map (0, 2, 1) per project memory; magnitude in g
    // (multiply by 9.81 to get m/s²).
    let grav_body = flatten_with_remap(&raw.grav, [0, 2, 1]);
    if !grav_body.is_empty() {
        let n = grav_body.len().min(30);
        let mut sum = [0.0_f32; 3];
        for s in &grav_body[..n] {
            for j in 0..3 { sum[j] += s[j]; }
        }
        let mean = [sum[0] / n as f32, sum[1] / n as f32, sum[2] / n as f32];
        let mag = (mean[0]*mean[0] + mean[1]*mean[1] + mean[2]*mean[2]).sqrt();
        if mag > 0.1 {
            let scaled: Vec<[f32; 3]> = grav_body.iter()
                .map(|s| [s[0] * 9.81, s[1] * 9.81, s[2] * 9.81])
                .collect();
            let n_input = scaled.len();
            let grav_times = build_imu_sample_times_with_skips(&raw.grav, cori_stmps, frame_skips, fps, probe_duration);
            let resampled = resample_time_linear(&scaled, &grav_times, gyro_times);
            return (resampled, AccSource::Grav, n_input);
        }
    }
    // Fallback: raw ACCL with same axis map as gyro (1, 2, 0).
    let accl_body = flatten_with_remap(&raw.accl, [1, 2, 0]);
    if !accl_body.is_empty() {
        let n_input = accl_body.len();
        let accl_times = build_imu_sample_times_with_skips(&raw.accl, cori_stmps, frame_skips, fps, probe_duration);
        let resampled = resample_time_linear(&accl_body, &accl_times, gyro_times);
        return (resampled, AccSource::Raw, n_input);
    }
    (vec![[0.0; 3]; gyro_times.len()], AccSource::None, 0)
}

/// Run the full VQF pipeline (input prep + 9D fusion + Y↔Z swap +
/// resample to video frame rate) and return a per-video-frame
/// quaternion stream that is **CORI-equivalent in component order**.
///
/// This is the Phase E entry point. The per-frame quaternions can be
/// substituted for the GPMF-stream `parse_cori(...)` output in any
/// downstream code (gravity alignment, bidirectional smoothing,
/// per-eye heading split) — they are in the same `(w, x, Z_stored,
/// Y_stored)` "on-disk byte order" convention that the rest of the
/// stabilization pipeline assumes (see `vr180_core::gyro::resample::
/// cori_swap_yz` doc-comment for why Y↔Z is necessary here).
///
/// # Sampling convention
///
/// VQF emits one quaternion per gyro sample (~798 Hz). We resample to
/// `n_frames` outputs at `fps` Hz using a **window-averaging** sampler
/// centered on each frame's SROT-midpoint, matching Python's
/// `resample_quats_to_frames(..., time_offset_s=SROT/2,
/// window_s=SROT)` — appropriate when RS correction (Phase D) is on
/// downstream, and a safe default otherwise (the average over a
/// short window collapses to point-sample when the rotation is
/// slow-varying).
///
/// Resample window is **SROT (15.224 ms)** — the full readout-window
/// average. This matches what the Python GUI actually passes: its
/// "Gyro window" slider defaults to 15.224 ms (`vr180_gui.py:10956`,
/// `ProcessingConfig.gyro_window_ms`), flowing into
/// `vqf_to_cori_quats(window_ms=...)`. The 15 ms average attenuates
/// 800 Hz vibration BEFORE the velocity-dampened smoother — without
/// it, per-frame quats carry raw vibration (shaky output) AND the
/// pre-smoother sees inflated velocities, collapsing τ exactly when
/// vibration is worst.
///
/// NOTE: an earlier port used **1 ms** here, matching the *function
/// default* (`parse_gyro_raw.py:1152`) instead of the GUI's value —
/// that made the Rust stab visibly shakier than the Python app on
/// vibrating footage (motorcycle clips) at "the same settings".
const VQF_RESAMPLE_WINDOW_S: f32 = 15.224 / 1000.0;

/// The sensor readout time of a GoPro recording, in seconds: the file's
/// `SROT` (a float-ms Global Setting in `moov/udta/GPMF`), falling back
/// to the MAX constant [`SROT_S`] only when the file carries none. Every
/// timing that depends on the readout — the mid-exposure pose instant
/// and the per-row RS window — takes it from here, never from a setting.
pub fn gopro_readout_s(path: &Path) -> f32 {
    use vr180_core::gyro::resample::SROT_S;
    match vr180_core::geoc::lookup_srot_s(path, None) {
        Ok(Some(s)) if s > 0.0005 && s < 0.1 => s,
        Ok(Some(s)) => {
            tracing::warn!("GoPro SROT {:.3} ms implausible — using {:.3} ms", s * 1e3, SROT_S * 1e3);
            SROT_S
        }
        _ => SROT_S,
    }
}

pub fn vqf_cori_equivalent_stream(
    input: &Path,
    fps: f32,
    n_frames: usize,
) -> Result<Vec<Quat>> {
    use vr180_core::gyro::{
        vqf,
        resample::{resample_quats_at_times, gopro_frame_sample_times, cori_swap_yz},
    };
    let srot_s = gopro_readout_s(input);

    // 1. Prep the IMU arrays. `prepare_for_vqf` now returns the
    //    STMP-anchored gyro timeline and computes `gyr_ts` from it,
    //    and time-linearly interpolates acc/mag onto the same timeline
    //    (matches the multi-segment Python pipeline).
    let prep = prepare_for_vqf(input)?;
    let vqf_fps = 1.0 / prep.gyr_ts.max(1e-6);
    tracing::info!(
        "VQF: {} gyro samples @ {:.2} Hz nominal, acc_source={:?}, mag_source={:?}",
        prep.gyro_body.len(), vqf_fps, prep.acc_source, prep.mag_source,
    );

    // 2. Run VQF — 9D when mag present, 6D otherwise. Matches Python's
    //    parameter choices (mag_dist_rejection_enabled=false, tau_mag=5.0).
    let run = vqf::run(
        &prep.gyro_body, &prep.acc_body, prep.mag_body.as_deref(), prep.gyr_ts,
    );
    tracing::info!(
        "VQF: bias_deg_s=[{:.3}, {:.3}, {:.3}] σ={:.4} rad/s",
        run.bias_deg_s()[0], run.bias_deg_s()[1], run.bias_deg_s()[2],
        run.bias_sigma,
    );

    // 3. Apply Y↔Z swap to each output so the result is in CORI's
    //    on-disk component order. Without this, downstream
    //    `quat_to_rotation_matrix` builds a rotation that's reflected
    //    on one axis.
    let cori_equiv: Vec<Quat> = run.quats.into_iter().map(cori_swap_yz).collect();

    // 4. Use the prep-built STMP-anchored gyro times for the per-video-
    //    frame resample. Identical timeline to the one used for `gyr_ts`,
    //    so VQF's per-sample step matches the per-sample interpretation
    //    of the resampler.
    let sample_times = prep.gyro_times;
    if sample_times.len() != cori_equiv.len() {
        tracing::warn!(
            "VQF: per-sample time count ({}) != VQF sample count ({}); \
             falling back to uniform-rate resample (drift not corrected)",
            sample_times.len(), cori_equiv.len(),
        );
        // Fallback to the legacy uniform-rate path. 1 ms window matches
        // Python's `window_ms=1.0` default — effectively point-sampling
        // so VQF's high-frequency content survives into the per-frame
        // CORI stream.
        use vr180_core::gyro::resample::resample_quats_to_frames;
        return Ok(resample_quats_to_frames(
            &cori_equiv, vqf_fps, fps, n_frames, srot_s * 0.5, VQF_RESAMPLE_WINDOW_S,
        ));
    }
    tracing::info!(
        "VQF: STMP-anchored timing — {} samples spanning {:.3}s (avg {:.2} Hz effective)",
        sample_times.len(),
        sample_times.last().copied().unwrap_or(0.0),
        sample_times.len() as f32 / sample_times.last().copied().unwrap_or(1.0).max(1e-3),
    );

    // 5. Resample to per-video-frame quaternions at each frame's
    //    MID-EXPOSURE: `i/fps + SROT/2 − shutter_i/2` from the per-frame
    //    SHUT stream (the DJI/X6 rule), window-averaged over SROT. Without
    //    SHUT this is the readout midpoint, as before.
    let mut times = gopro_frame_sample_times(fps, n_frames, &prep.shutter_s, srot_s, skips_of(&prep.frame_skips));
    apply_diag_phase(&mut times);
    log_shutter(&prep.shutter_s, n_frames);
    Ok(resample_quats_at_times(&sample_times, &cori_equiv, &times, VQF_RESAMPLE_WINDOW_S))
}

fn skips_of(v: &[i32]) -> Option<&[i32]> { if v.is_empty() { None } else { Some(v) } }

/// DIAGNOSTIC: `VR180_GOPRO_PHASE_MS` shifts every GoPro frame's sample
/// instant (the GoPro twin of `VR180_IMU_PHASE_OFFSET_MS`). Default 0.
fn apply_diag_phase(times: &mut [f32]) {
    if let Some(ms) = std::env::var("VR180_GOPRO_PHASE_MS").ok().and_then(|v| v.parse::<f32>().ok()) {
        if ms != 0.0 {
            tracing::warn!("GoPro DIAGNOSTIC phase offset {ms:+.2} ms on the pose sample instants");
            for t in times.iter_mut() { *t += ms / 1000.0; }
        }
    }
}

/// Per-frame MSKP across a chapter chain, aligned like
/// [`gopro_shutter_per_frame`] (each segment padded to its frame count with
/// its last value; the count restarts per segment, which is fine — only the
/// steps matter).
pub fn gopro_skips_per_frame(segments: &[std::path::PathBuf], fps: f32) -> Vec<i32> {
    use crate::decode::{extract_gpmf_stream, probe_video};
    use vr180_core::gyro::parse_mskp;
    let mut out = Vec::new();
    for seg in segments {
        let n = probe_video(seg).map(|p| (p.duration_sec * fps as f64).round() as usize).unwrap_or(0);
        let mut sk = extract_gpmf_stream(seg).map(|g| parse_mskp(&g)).unwrap_or_default();
        let last = sk.last().copied().unwrap_or(0);
        sk.resize(n, last);
        out.extend_from_slice(&sk);
    }
    out
}

fn log_shutter(shutter_s: &[f32], n_frames: usize) {
    let mut v: Vec<f32> = shutter_s.iter().copied().filter(|s| s.is_finite() && *s > 0.0).collect();
    if v.is_empty() {
        tracing::info!("GoPro pose timing: no SHUT stream — sampling at the readout midpoint");
        return;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    tracing::info!(
        "GoPro pose timing: mid-exposure from SHUT ({} of {} frames; shutter {:.2}–{:.2} ms, median {:.2} ms → sample {:.2} ms earlier than the readout midpoint at the median)",
        v.len(), n_frames, v[0] * 1e3, v[v.len() - 1] * 1e3, v[v.len() / 2] * 1e3, v[v.len() / 2] * 500.0,
    );
}

/// Multi-segment VQF: aggregate the raw IMU of all GoPro `.360` segments
/// into ONE continuous stream and integrate as a single VQF run, then
/// resample to `n_frames` (the TOTAL across segments). This eliminates
/// the orientation discontinuity per-segment integration would inject at
/// every boundary — mirrors Python's `vqf_to_cori_quats_multi_segment`
/// (each segment's gyro times are shifted by the cumulative video
/// duration so the merged timeline is continuous). `segments.len() == 1`
/// is identical to [`vqf_cori_equivalent_stream`].
pub fn vqf_cori_equivalent_stream_multi(
    segments: &[std::path::PathBuf],
    fps: f32,
    n_frames: usize,
) -> Result<Vec<Quat>> {
    use vr180_core::gyro::{
        vqf,
        resample::{resample_quats_at_times, gopro_frame_sample_times, cori_swap_yz},
    };
    use crate::decode::probe_video;

    if segments.len() == 1 {
        return vqf_cori_equivalent_stream(&segments[0], fps, n_frames);
    }
    // One recording, one sensor: the readout comes from the first chapter.
    let srot_s = gopro_readout_s(&segments[0]);

    let mut gyro_body: Vec<[f32; 3]> = Vec::new();
    let mut acc_body: Vec<[f32; 3]> = Vec::new();
    let mut mag_body: Vec<[f32; 3]> = Vec::new();
    let mut gyro_times: Vec<f32> = Vec::new();
    let mut shutter_all: Vec<f32> = Vec::new();
    let mut skips_all: Vec<i32> = Vec::new();
    let mut all_have_mag = true;
    let mut time_off = 0.0_f32;

    for seg in segments {
        let prep = prepare_for_vqf(seg)?;
        let seg_dur = probe_video(seg).map(|p| p.duration_sec as f32)
            .unwrap_or_else(|_| prep.gyro_times.last().copied().unwrap_or(0.0));
        // Per-frame shutter, padded to the segment's frame count so the
        // global frame index stays aligned across the chain.
        let n_seg = (seg_dur * fps).round() as usize;
        let mut shut = prep.shutter_s.clone();
        shut.resize(n_seg, f32::NAN);
        shutter_all.extend_from_slice(&shut);
        let mut sk = prep.frame_skips.clone();
        let last = sk.last().copied().unwrap_or(0);
        sk.resize(n_seg, last);
        skips_all.extend_from_slice(&sk);
        for &t in &prep.gyro_times { gyro_times.push(t + time_off); }
        gyro_body.extend_from_slice(&prep.gyro_body);
        acc_body.extend_from_slice(&prep.acc_body);
        match prep.mag_body {
            Some(m) => mag_body.extend_from_slice(&m),
            None => all_have_mag = false,
        }
        // Advance by the segment's video duration (matches the decode
        // chaining's `seg_start_s`), so frame i/fps lands in the right
        // segment on resample.
        time_off += seg_dur;
    }
    if gyro_body.is_empty() {
        return Err(Error::Ffmpeg("multi-segment VQF: no gyro samples".into()));
    }

    // Constant gyro dt over the merged span (Python uses one gyro_dt for
    // the whole aggregated stream).
    let span = (gyro_times.last().copied().unwrap_or(0.0)
        - gyro_times.first().copied().unwrap_or(0.0)).max(1e-6);
    let gyr_ts = span / (gyro_body.len().saturating_sub(1).max(1) as f32);

    let mag = if all_have_mag && mag_body.len() == gyro_body.len() {
        Some(mag_body.as_slice())
    } else {
        None
    };
    tracing::info!(
        "VQF (multi): {} segments, {} gyro samples spanning {:.2}s, mag={}",
        segments.len(), gyro_body.len(), span, mag.is_some(),
    );

    let run = vqf::run(&gyro_body, &acc_body, mag, gyr_ts);
    let cori_equiv: Vec<Quat> = run.quats.into_iter().map(cori_swap_yz).collect();

    if gyro_times.len() != cori_equiv.len() {
        use vr180_core::gyro::resample::resample_quats_to_frames;
        return Ok(resample_quats_to_frames(
            &cori_equiv, 1.0 / gyr_ts.max(1e-6), fps, n_frames,
            srot_s * 0.5, VQF_RESAMPLE_WINDOW_S));
    }
    let mut times = gopro_frame_sample_times(fps, n_frames, &shutter_all, srot_s, skips_of(&skips_all));
    apply_diag_phase(&mut times);
    log_shutter(&shutter_all, n_frames);
    Ok(resample_quats_at_times(&gyro_times, &cori_equiv, &times, VQF_RESAMPLE_WINDOW_S))
}

fn pick_mag_source(
    raw: &RawImu,
    cori_stmps: &[vr180_core::gyro::cori_iori::CoriBlockStmp],
    frame_skips: Option<&[i32]>,
    fps: f32,
    probe_duration: f32,
    gyro_times: &[f32],
) -> (Option<Vec<[f32; 3]>>, MagSource, usize) {
    use vr180_core::gyro::resample::build_imu_sample_times_with_skips;

    // MNOR — magnetic-north unit vector. Python remaps it into the VQF
    // body frame with the GRAV axis map `(0, 2, 1)` (NOT the gyro map)
    // and scales the unit vector to ~50 µT, matching `parse_gyro_raw.py`
    // (`MNOR_VQF_MAP = GRAV_AXIS_MAP`, then `remapped *= 50.0`). MNOR and
    // GRAV share the same body-frame convention in GoPro GPMF; using the
    // gyro map (or no map) puts magnetic north in the wrong frame and the
    // 9D fusion's heading correction is 65-130° off — a large, time-
    // aligned, motion-independent orientation error vs Python. The ×50
    // matches the field magnitude VQF's (disabled) disturbance check
    // expects; harmless with mag_dist_rejection off but kept for parity.
    if raw.mnor.is_empty() {
        return (None, MagSource::None, 0);
    }
    let mnor_body: Vec<[f32; 3]> = flatten_with_remap(&raw.mnor, [0, 2, 1])
        .into_iter()
        .map(|s| [s[0] * 50.0, s[1] * 50.0, s[2] * 50.0])
        .collect();
    let n_input = mnor_body.len();
    let mnor_times = build_imu_sample_times_with_skips(&raw.mnor, cori_stmps, frame_skips, fps, probe_duration);
    let resampled = resample_time_linear(&mnor_body, &mnor_times, gyro_times);
    (Some(resampled), MagSource::Mnor, n_input)
}
