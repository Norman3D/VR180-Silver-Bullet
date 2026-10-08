//! Frame-time quaternion resampling.
//!
//! Port of `resample_quats_to_frames` in `parse_gyro_raw.py:958-1021`.
//!
//! The GoPro GPMF CORI stream is ~30 Hz, but not perfectly aligned to
//! the video frame clock — and even on a clip recorded at 30 fps, the
//! IMU has its own internal timebase that drifts. For stabilization
//! we need ONE quaternion per output frame, sampled at the right
//! sensor-readout midpoint (because a frame's pixels are captured
//! over a 15.224 ms window — the sensor readout time, SROT — not
//! instantaneously).
//!
//! Two sampling modes:
//!
//! - **Window-average** (`window_s > 0`): mean of all source samples
//!   in `[center - window_s/2, center + window_s/2]` where
//!   `center = i/fps + time_offset_s`. With `window_s = SROT`, this
//!   integrates the per-pixel rotations across the readout window —
//!   the right thing to do for rolling-shutter footage (we encode an
//!   average rotation per frame; the per-scanline residual is handled
//!   separately by the RS warp pass).
//!
//! - **Point-sample** (`window_s = 0`): linear interpolation between
//!   the two source samples bracketing `center`. Cheaper; appropriate
//!   when the user has chosen no smoothing AND no RS correction.
//!
//! Sign continuity: each output quaternion is sign-flipped to match
//! the previous one (`q · prev > 0`). Without this, a w→-w jump
//! between adjacent source samples (mathematically equivalent quat,
//! same rotation) would average to ~zero and produce garbage.

use super::cori_iori::Quat;

/// Sensor readout time of the GoPro Max sensor, 15.224 ms.
/// Used as the natural window width for per-frame averaging in
/// rolling-shutter footage. Same constant as Python `SROT_S`.
pub const SROT_S: f32 = 15.224 / 1000.0;

/// Sample interval helper: the per-source-sample time given a source
/// frequency in Hz. For CORI at 30 Hz this is 33.3 ms.
fn src_dt(src_fps: f32) -> f32 {
    1.0 / src_fps.max(1e-6)
}

/// Resample a quaternion stream to per-frame quaternions.
///
/// # Arguments
/// - `src` — source quaternions, evenly spaced at `src_fps` Hz.
/// - `src_fps` — source sample rate (e.g. 30.0 for CORI).
/// - `n_frames` — how many output samples to produce.
/// - `dst_fps` — output sample rate (typically the video frame rate).
/// - `time_offset_s` — shift the sample times by this amount. Use
///   `SROT_S / 2` to center each window on the sensor readout midpoint
///   (matches the Python convention for rolling-shutter footage).
/// - `window_s` — averaging window width. `0` = point-sample
///   (linear interpolation between the two bracketing src samples).
///   `SROT_S` = integrate over the sensor readout — the right choice
///   when RS correction is on.
///
/// Returns `n_frames` quaternions in sign-continuous form (suitable
/// for smoothing / slerp operations downstream).
pub fn resample_quats_to_frames(
    src: &[Quat],
    src_fps: f32,
    dst_fps: f32,
    n_frames: usize,
    time_offset_s: f32,
    window_s: f32,
) -> Vec<Quat> {
    if src.is_empty() || n_frames == 0 {
        return Vec::new();
    }
    let dt_src = src_dt(src_fps);
    let dt_dst = src_dt(dst_fps);
    let n_src = src.len();
    let mut out = Vec::with_capacity(n_frames);
    let mut prev = Quat::IDENTITY;

    for i in 0..n_frames {
        let center = (i as f32) * dt_dst + time_offset_s;
        let q = if window_s <= 0.0 {
            // Point-sample (linear interp).
            sample_linear(src, dt_src, center)
        } else {
            // Window-average.
            sample_window_average(src, dt_src, center, window_s)
        };
        // Sign-continuity: flip if dot with previous is negative.
        // Without this, two consecutive sample windows that straddle
        // a w→-w sign change produce destructive averaging.
        let q = if i == 0 { q.normalize() } else {
            if q.dot(prev) < 0.0 {
                Quat { w: -q.w, x: -q.x, y: -q.y, z: -q.z }.normalize()
            } else {
                q.normalize()
            }
        };
        prev = q;
        out.push(q);
    }
    out
}

/// Linear interpolation between the two source samples bracketing
/// `t` (in seconds, relative to `src[0]`). Edge cases: before `src[0]`
/// returns `src[0]`; past the end returns `src[n-1]`.
fn sample_linear(src: &[Quat], dt: f32, t: f32) -> Quat {
    if t <= 0.0 { return src[0]; }
    let last = src.len() - 1;
    let f = t / dt;
    let i = f.floor() as usize;
    if i >= last { return src[last]; }
    let frac = f - i as f32;
    let a = src[i];
    // Use the SAME sign-flip rule the Python does (sign-continuity
    // between the two interpolation endpoints). Without this, a sign
    // flip between consecutive src samples produces a near-zero
    // interpolation result at frac≈0.5.
    let mut b = src[i + 1];
    if a.dot(b) < 0.0 {
        b = Quat { w: -b.w, x: -b.x, y: -b.y, z: -b.z };
    }
    Quat {
        w: a.w + frac * (b.w - a.w),
        x: a.x + frac * (b.x - a.x),
        y: a.y + frac * (b.y - a.y),
        z: a.z + frac * (b.z - a.z),
    }
}

/// Average of all source samples within `[t - w/2, t + w/2]`.
/// If the window covers no samples (e.g. `w << dt`), falls back to
/// `sample_linear`. Sign-continuity is enforced across the window:
/// every sample is flipped if its dot with the running accumulator
/// is negative.
fn sample_window_average(src: &[Quat], dt: f32, center: f32, window_s: f32) -> Quat {
    let half = window_s * 0.5;
    let lo = center - half;
    let hi = center + half;
    let i_lo = (lo / dt).ceil().max(0.0) as isize;
    let i_hi = (hi / dt).floor() as isize;
    let n = src.len() as isize;
    let i_lo = i_lo.max(0);
    let i_hi = i_hi.min(n - 1);

    if i_lo > i_hi {
        // Window narrower than one src sample period — fall back.
        return sample_linear(src, dt, center);
    }

    let mut acc = Quat { w: 0.0, x: 0.0, y: 0.0, z: 0.0 };
    let mut count = 0usize;
    // Use the first sample's sign as the reference for the rest.
    let ref_q = src[i_lo as usize];
    for i in i_lo..=i_hi {
        let mut q = src[i as usize];
        if q.dot(ref_q) < 0.0 {
            q = Quat { w: -q.w, x: -q.x, y: -q.y, z: -q.z };
        }
        acc.w += q.w; acc.x += q.x; acc.y += q.y; acc.z += q.z;
        count += 1;
    }
    let inv = 1.0 / (count as f32);
    Quat {
        w: acc.w * inv, x: acc.x * inv, y: acc.y * inv, z: acc.z * inv,
    }
}

/// Build per-sample timestamps for raw IMU blocks (gyro, accel, etc.)
/// in **video time-base seconds** (where 0.0 = the start of the first
/// CORI block).
///
/// Mirrors `parse_gyro_raw.py::gyro_to_timestamps`. The hard part is
/// the **IMU-clock drift correction**: the GoPro IMU ticks ~1 ms/s
/// faster than the video clock, so naive `i / src_fps` mapping
/// accumulates linear drift (35 ms over a 35 s clip). Result: the
/// VQF quaternion at "video time t" is actually for a slightly later
/// physical time, and the stabilization heading lags more and more
/// as the clip plays. Visible symptom: "footage becomes more and more
/// shaky as time goes" on long VQF runs.
///
/// Algorithm:
/// 1. Build a piecewise-linear mapping `stmp_s → video_s` from the
///    CORI block STMPs (treat each CORI block as N frames at the
///    nominal `fps`).
/// 2. For each IMU block: generate `n_samples` evenly-spaced sample
///    times in STMP-seconds (from this block's STMP to the next
///    block's STMP), then map them through the CORI mapping to land
///    in video-seconds.
///
/// Returns one `f32` time per sample, in video-time seconds. Length
/// matches the total number of samples across all `imu_blocks`.
///
/// Fallback: when `cori_stmps` is empty (no CORI in the file), the
/// returned times use uniform `duration_s / total_samples` spacing.
/// This is the same fallback Python uses (legacy behavior).
pub fn build_imu_sample_times(
    imu_blocks: &[super::raw::ImuBlock],
    cori_stmps: &[super::cori_iori::CoriBlockStmp],
    fps: f32,
    fallback_duration_s: f32,
) -> Vec<f32> {
    build_imu_sample_times_with_skips(imu_blocks, cori_stmps, None, fps, fallback_duration_s)
}

/// [`build_imu_sample_times`] that honours the camera's dropped frames.
///
/// `frame_skips` is the per-video-frame cumulative drop count from the
/// MSKP stream ([`super::cori_iori::parse_mskp`]): video frame `i` shows
/// sensor frame `i + skips[i]`. Where the camera dropped a sensor frame and
/// repeated the previous image, the telemetry block is one frame period
/// SHORT while the video still has 30 frames; spreading the block's gyro
/// uniformly over those 30 frames (the plain anchors) mis-times every
/// frame after the repeat by up to a frame period — one frame of walking
/// motion applied to the wrong image, a ~0.7 s burst of jerks. With the
/// skips the anchors are per frame: real frames keep the sensor period,
/// a repeated frame holds the previous sensor instant.
pub fn build_imu_sample_times_with_skips(
    imu_blocks: &[super::raw::ImuBlock],
    cori_stmps: &[super::cori_iori::CoriBlockStmp],
    frame_skips: Option<&[i32]>,
    fps: f32,
    fallback_duration_s: f32,
) -> Vec<f32> {
    let total_samples: usize = imu_blocks.iter().map(|b| b.samples.len()).sum();
    if total_samples == 0 {
        return Vec::new();
    }

    // Are all IMU blocks STMP-tagged? If any are missing, we can't
    // trust the STMP path — fall back to uniform spacing.
    let all_stmp = imu_blocks.iter().all(|b| b.stmp_us.is_some());

    if !all_stmp || cori_stmps.is_empty() {
        // Uniform-spacing fallback (matches Python's legacy `else` arm).
        let mut out = Vec::with_capacity(total_samples);
        let dt = fallback_duration_s / total_samples as f32;
        for i in 0..total_samples {
            out.push(i as f32 * dt);
        }
        return out;
    }

    // Build the STMP→video mapping anchored on CORI block starts.
    // Each CORI block boundary maps `stmp_s = (stmp_us - cori_start_us)/1e6`
    // to `frame_s = cumulative_frames / fps`. We also append an
    // "end of last block" anchor so samples past the last CORI block
    // don't clamp to a flat value.
    let cori_start_us = cori_stmps[0].stmp_us;
    // Usable skip table: cumulative, non-positive, non-increasing. Anything
    // else is ignored (plain block anchors).
    let skips: Option<&[i32]> = frame_skips.filter(|sk| {
        !sk.is_empty() && sk[0] <= 0 && sk.windows(2).all(|w| w[1] <= w[0])
    });
    let skip_at = |i: u32| -> i64 {
        match skips {
            Some(sk) => sk.get(i as usize).or(sk.last()).copied().unwrap_or(0) as i64,
            None => 0,
        }
    };
    // Per-frame anchors: (sensor time of the frame's capture, video time i/fps).
    let n_video: u32 = cori_stmps.iter().map(|c| c.n_samples).sum();
    let mut anchor_stmp_s: Vec<f32> = Vec::with_capacity(n_video as usize + 1);
    let mut anchor_frame_s: Vec<f32> = Vec::with_capacity(n_video as usize + 1);
    let mut cumulative_frames: u32 = 0;
    let mut prev_period: Option<f64> = None;
    let mut last_t = f64::NEG_INFINITY;
    for (k, c) in cori_stmps.iter().enumerate() {
        let stmp_s = (c.stmp_us.saturating_sub(cori_start_us)) as f64 / 1e6;
        let f0 = cumulative_frames;
        let f1 = f0 + c.n_samples;
        // Sensor frames actually captured in this block = video frames + drops.
        let real = (c.n_samples as i64 + skip_at(f1) - skip_at(f0)).max(1) as f64;
        let period = if k + 1 < cori_stmps.len() {
            let next = (cori_stmps[k + 1].stmp_us.saturating_sub(cori_start_us)) as f64 / 1e6;
            let p = (next - stmp_s) / real;
            if p > 0.0 { p } else { prev_period.unwrap_or(1.0 / fps as f64) }
        } else {
            prev_period.unwrap_or(1.0 / fps as f64)
        };
        prev_period = Some(period);
        for i in f0..f1 {
            // The skip count of the PREVIOUS frame: a repeated video frame
            // keeps the sensor period (so the frame before it maps with
            // slope 1 over its whole exposure), and the frame after it
            // starts the hold. The repeated frame itself takes its sample
            // instant from the frame it copies (`gopro_frame_sample_times`).
            let sensor_idx = (i as i64 - f0 as i64) + skip_at(i.saturating_sub(1).max(f0)) - skip_at(f0);
            let mut t = stmp_s + sensor_idx as f64 * period;
            // A repeated frame shares its sensor instant with the previous
            // video frame; keep the table strictly increasing (1 µs step) so
            // the interpolation stays well defined.
            if t <= last_t { t = last_t + 1e-6; }
            last_t = t;
            anchor_stmp_s.push(t as f32);
            anchor_frame_s.push((i as f64 / fps as f64) as f32);
        }
        cumulative_frames = f1;
    }
    // End-of-last-frame anchor (one more period).
    if let Some(p) = prev_period {
        let mut t = last_t + p;
        if t <= last_t { t = last_t + 1e-6; }
        anchor_stmp_s.push(t as f32);
        anchor_frame_s.push((cumulative_frames as f64 / fps as f64) as f32);
    }

    // Per-sample STMP-space times, then mapped through the anchors.
    let mut out = Vec::with_capacity(total_samples);
    let n_blocks = imu_blocks.len();
    for (i, blk) in imu_blocks.iter().enumerate() {
        let n = blk.samples.len();
        let stmp_s = (blk.stmp_us.unwrap().saturating_sub(cori_start_us)) as f64 / 1e6;
        let next_stmp_s = if i + 1 < n_blocks {
            (imu_blocks[i + 1].stmp_us.unwrap().saturating_sub(cori_start_us)) as f64 / 1e6
        } else if i > 0 {
            // Last block: extrapolate using the previous gap.
            let prev_stmp_s = (imu_blocks[i - 1].stmp_us.unwrap().saturating_sub(cori_start_us)) as f64 / 1e6;
            stmp_s + (stmp_s - prev_stmp_s)
        } else {
            stmp_s + n as f64 / 800.0   // ~800 Hz nominal fallback for single-block clips
        };
        for j in 0..n {
            let t_stmp = stmp_s + (next_stmp_s - stmp_s) * (j as f64 / n as f64);
            let t_video = linear_interp(t_stmp as f32, &anchor_stmp_s, &anchor_frame_s);
            out.push(t_video);
        }
    }
    out
}

/// Resample a quaternion stream to per-frame quaternions using
/// **per-sample timestamps** (instead of uniform-rate spacing).
///
/// Required when the source's effective sample rate drifts relative
/// to video time — most notably the GoPro IMU at ~1 ms/s drift.
/// Same shape as [`resample_quats_to_frames`] but the `src_times`
/// parameter carries each sample's video-time. Sign-continuity pass
/// runs across the input before sampling.
pub fn resample_quats_to_frames_timed(
    src_times: &[f32],
    src_quats: &[Quat],
    dst_fps: f32,
    n_frames: usize,
    time_offset_s: f32,
    window_s: f32,
) -> Vec<Quat> {
    assert_eq!(src_times.len(), src_quats.len(),
        "src_times and src_quats must have the same length");
    if src_quats.is_empty() || n_frames == 0 {
        return Vec::new();
    }
    let dt_dst = 1.0 / dst_fps.max(1e-6);

    // Pre-flip for sign continuity (do it once on the whole input).
    let mut aligned: Vec<Quat> = Vec::with_capacity(src_quats.len());
    aligned.push(src_quats[0]);
    for j in 1..src_quats.len() {
        let mut q = src_quats[j];
        if q.dot(aligned[j - 1]) < 0.0 {
            q = Quat { w: -q.w, x: -q.x, y: -q.y, z: -q.z };
        }
        aligned.push(q);
    }

    let times: Vec<f32> = (0..n_frames).map(|i| i as f32 * dt_dst + time_offset_s).collect();
    resample_at_times_aligned(&aligned, src_times, &times, window_s)
}

/// [`resample_quats_to_frames_timed`] at arbitrary instants: one output
/// per entry of `times` (seconds on the same clock as `src_times`), each
/// the window average (or linear sample when `window_s <= 0`) of the
/// stream around that instant. Lets a caller place every frame's sample at
/// its own mid-exposure (see [`gopro_frame_sample_times`]).
pub fn resample_quats_at_times(
    src_times: &[f32],
    src_quats: &[Quat],
    times: &[f32],
    window_s: f32,
) -> Vec<Quat> {
    assert_eq!(src_times.len(), src_quats.len(),
        "src_times and src_quats must have the same length");
    if src_quats.is_empty() || times.is_empty() {
        return Vec::new();
    }
    let mut aligned: Vec<Quat> = Vec::with_capacity(src_quats.len());
    aligned.push(src_quats[0]);
    for j in 1..src_quats.len() {
        let mut q = src_quats[j];
        if q.dot(aligned[j - 1]) < 0.0 {
            q = Quat { w: -q.w, x: -q.x, y: -q.y, z: -q.z };
        }
        aligned.push(q);
    }
    resample_at_times_aligned(&aligned, src_times, times, window_s)
}

fn resample_at_times_aligned(aligned: &[Quat], src_times: &[f32], times: &[f32], window_s: f32) -> Vec<Quat> {
    let mut out = Vec::with_capacity(times.len());
    for &t in times {
        let q = if window_s <= 0.0 {
            sample_linear_timed(aligned, src_times, t)
        } else {
            sample_window_average_timed(aligned, src_times, t, window_s)
        };
        out.push(q.normalize());
    }
    out
}

/// Pose sample instant for each GoPro video frame: the centre row's
/// MID-EXPOSURE, `i/fps + readout/2 − shutter_i/2` — the same rule the
/// DJI (`video_ts + readout/2 − shutter/2`) and X6 paths use. `shutter_s`
/// is the per-frame exposure from the SHUT stream ([`crate::gyro::parse_shut`]);
/// a missing or implausible value (≤ 0, longer than the frame period)
/// falls back to the readout midpoint for that frame. Measured on real
/// walking/running footage: in daylight (1 ms shutter) the readout
/// midpoint is right to ~1.6 ms; at a 1/30 s shutter ignoring the term
/// leaves a ~16 ms pose error that shows as shake tracking the motion.
///
/// `frame_skips` (MSKP, see [`build_imu_sample_times_with_skips`]): a video
/// frame that repeats the previous image gets the previous frame's instant,
/// so it receives the same pose and RS correction as the image it copies.
pub fn gopro_frame_sample_times(fps: f32, n_frames: usize, shutter_s: &[f32], readout_s: f32, frame_skips: Option<&[i32]>) -> Vec<f32> {
    let dt = 1.0 / fps.max(1e-6);
    let mut out: Vec<f32> = Vec::with_capacity(n_frames);
    for i in 0..n_frames {
        let repeat = i > 0 && frame_skips.map_or(false, |sk| {
            matches!((sk.get(i), sk.get(i - 1)), (Some(a), Some(b)) if a < b)
        });
        if repeat {
            out.push(out[i - 1]);
            continue;
        }
        let base = i as f32 * dt + readout_s * 0.5;
        out.push(match shutter_s.get(i) {
            Some(&s) if s.is_finite() && s > 0.0 && s <= dt * 1.0001 => base - s * 0.5,
            _ => base,
        });
    }
    out
}

/// Linear time-interpolation: find the two source samples whose
/// timestamps bracket `t` and lerp between them. Edge-clamp.
fn sample_linear_timed(src: &[Quat], times: &[f32], t: f32) -> Quat {
    let n = src.len();
    if t <= times[0] { return src[0]; }
    if t >= times[n - 1] { return src[n - 1]; }
    // Binary search for the upper-bound index.
    let hi = match times.binary_search_by(|x| x.partial_cmp(&t).unwrap_or(std::cmp::Ordering::Equal)) {
        Ok(i)  => i,
        Err(i) => i,
    };
    let hi = hi.max(1).min(n - 1);
    let lo = hi - 1;
    let span = (times[hi] - times[lo]).max(1e-9);
    let frac = ((t - times[lo]) / span).clamp(0.0, 1.0);
    let a = src[lo];
    let b = src[hi];
    Quat {
        w: a.w + frac * (b.w - a.w),
        x: a.x + frac * (b.x - a.x),
        y: a.y + frac * (b.y - a.y),
        z: a.z + frac * (b.z - a.z),
    }
}

/// Window-average around `t` over `±window_s/2` seconds. Uses
/// `binary_search` to find the index range, then mean of the
/// pre-sign-aligned quats in that range.
fn sample_window_average_timed(src: &[Quat], times: &[f32], t: f32, window_s: f32) -> Quat {
    let half = window_s * 0.5;
    let lo_idx = lower_bound(times, t - half);
    let hi_idx = upper_bound(times, t + half);
    if hi_idx <= lo_idx {
        return sample_linear_timed(src, times, t);
    }
    let mut acc = Quat { w: 0.0, x: 0.0, y: 0.0, z: 0.0 };
    let count = (hi_idx - lo_idx) as f32;
    for i in lo_idx..hi_idx {
        let q = src[i];
        acc.w += q.w; acc.x += q.x; acc.y += q.y; acc.z += q.z;
    }
    Quat {
        w: acc.w / count, x: acc.x / count,
        y: acc.y / count, z: acc.z / count,
    }
}

fn lower_bound(times: &[f32], t: f32) -> usize {
    times.partition_point(|x| *x < t)
}
fn upper_bound(times: &[f32], t: f32) -> usize {
    times.partition_point(|x| *x <= t)
}

/// 1-D linear interpolation through a sorted (xs, ys) lookup table.
/// Edge-clamps; assumes `xs.len() == ys.len()` and `xs` is sorted.
fn linear_interp(x: f32, xs: &[f32], ys: &[f32]) -> f32 {
    if xs.is_empty() { return x; }
    if x <= xs[0] { return ys[0]; }
    let n = xs.len();
    if x >= xs[n - 1] { return ys[n - 1]; }
    let hi = xs.partition_point(|v| *v <= x).max(1).min(n - 1);
    let lo = hi - 1;
    let span = (xs[hi] - xs[lo]).max(1e-9);
    let frac = (x - xs[lo]) / span;
    ys[lo] + frac * (ys[hi] - ys[lo])
}

/// Swap the `y` and `z` components of an on-disk CORI quaternion to
/// recover the standard `(w, x, y, z)` convention used by rotation-
/// matrix math. The GoPro `.360` GPMF stores CORI in `(w, x, Z, Y)`
/// order — the y/z slots are swapped relative to the standard. We
/// preserve that in `parse_cori` so the raw bytes are inspectable,
/// and apply the swap here at the boundary where the quaternion
/// turns into a 3D rotation.
///
/// (Python: `quat[..., [0, 1, 3, 2]]` permutation at the integration
/// boundary, see `vqf_to_cori_quats` line 1273.)
pub fn cori_swap_yz(q: Quat) -> Quat {
    Quat { w: q.w, x: q.x, y: q.z, z: q.y }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dropped_frame_holds_the_sensor_instant() {
        use super::super::raw::ImuBlock;
        use super::super::cori_iori::CoriBlockStmp;
        // 30 fps-like clip with a 10 ms frame period for round numbers:
        // block 0 = video frames 0..3 (frame 1 repeats frame 0 → 2 real
        // frames, block spans 2 periods), block 1 = frames 3..6 (3 real).
        let period_us = 10_000u64;
        let stmps = vec![
            CoriBlockStmp { stmp_us: 1_000_000, n_samples: 3 },
            CoriBlockStmp { stmp_us: 1_000_000 + 2 * period_us, n_samples: 3 },
        ];
        let skips = [0, -1, -1, -1, -1, -1];
        // Gyro: block A = 10 samples spread over the 50 ms to block B's
        // stamp (5 ms apart, sensor time j*5 ms), block B = 1 sample.
        let gyro = vec![
            ImuBlock { stmp_us: Some(1_000_000), scal: 1.0, samples: vec![[0.0; 3]; 10] },
            ImuBlock { stmp_us: Some(1_050_000), scal: 1.0, samples: vec![[0.0; 3]; 1] },
        ];
        let fps = 100.0; // frame period 10 ms in VIDEO time
        let t = super::build_imu_sample_times_with_skips(&gyro, &stmps, Some(&skips), fps, 0.05);
        let video_of = |sensor_ms: f64| -> f32 {
            let j = (sensor_ms / 5.0).round() as usize; t[j]
        };
        // sensor 0..10 ms (frame 0's period) maps with slope 1 onto video
        // frame 0; the hold sits AFTER the repeated frame 1, so sensor
        // 15 ms (half-way through sensor frame 1, shown by video frame 2)
        // lands at 25 ms video time, and sensor 20 ms (= video frame 3,
        // block 1 start) at 30 ms.
        assert!((video_of(0.0) - 0.0).abs() < 1e-4);
        assert!((video_of(5.0) - 0.005).abs() < 1e-4, "slope 1 before the repeat, got {}", video_of(5.0));
        assert!((video_of(15.0) - 0.025).abs() < 1e-4, "sensor frame 1 half-way → video frame 2.5, got {}", video_of(15.0));
        assert!((video_of(20.0) - 0.030).abs() < 1e-4, "got {}", video_of(20.0));
        // The repeated frame samples at the instant of the frame it copies.
        let ts = super::gopro_frame_sample_times(fps, 4, &[], 0.0, Some(&skips));
        assert!(ts[1] == ts[0] && ts[2] > ts[1] && ts[3] > ts[2]);
        // Without skips the same block is stretched: sensor 20 ms ↔ video 30 ms still
        // (block anchor), but sensor 10 ms lands at 15 ms (uniform spread) — the bug.
        let t0 = super::build_imu_sample_times_with_skips(&gyro, &stmps, None, fps, 0.05);
        assert!((t0[2] - 0.015).abs() < 1e-4, "plain anchors spread the block uniformly: {}", t0[2]);
    }

    #[test]
    fn gopro_sample_times_are_mid_exposure() {
        let t = super::gopro_frame_sample_times(30.0, 4, &[0.001, 1.0 / 30.0, 0.0, f32::NAN], 0.015224, None);
        let dt = 1.0 / 30.0;
        assert!((t[0] - (0.0 + 0.007612 - 0.0005)).abs() < 1e-6, "short shutter: readout mid − shutter/2");
        assert!((t[1] - (dt + 0.007612 - dt / 2.0)).abs() < 1e-6, "full-frame shutter: 16.7 ms earlier");
        assert!((t[2] - (2.0 * dt + 0.007612)).abs() < 1e-6, "zero shutter → readout midpoint");
        assert!((t[3] - (3.0 * dt + 0.007612)).abs() < 1e-6, "NaN shutter → readout midpoint");
        // No SHUT at all → the classic readout-midpoint convention for every frame.
        let t0 = super::gopro_frame_sample_times(30.0, 2, &[], 0.015224, None);
        assert!((t0[1] - (dt + 0.007612)).abs() < 1e-6);
    }

    #[test]
    fn resample_at_times_matches_frame_resampler() {
        use super::*;
        let src_times: Vec<f32> = (0..100).map(|i| i as f32 * 0.00125).collect();
        let src: Vec<Quat> = src_times.iter().map(|&t| Quat { w: (t * 1.0).cos(), x: 0.0, y: (t * 1.0).sin(), z: 0.0 }).collect();
        let a = resample_quats_to_frames_timed(&src_times, &src, 30.0, 3, 0.007612, 0.015224);
        let times: Vec<f32> = (0..3).map(|i| i as f32 / 30.0 + 0.007612).collect();
        let b = resample_quats_at_times(&src_times, &src, &times, 0.015224);
        for (x, y) in a.iter().zip(&b) {
            assert!((x.w - y.w).abs() < 1e-6 && (x.x - y.x).abs() < 1e-6 && (x.y - y.y).abs() < 1e-6 && (x.z - y.z).abs() < 1e-6);
        }
    }

    use super::*;

    fn q(w: f32, x: f32, y: f32, z: f32) -> Quat { Quat { w, x, y, z } }

    #[test]
    fn point_sample_at_exact_src_time() {
        // src = identity, 0.28 rad / 0.56 rad rotations around Y axis.
        // Using cos(θ/2), sin(θ/2) form so the quats are unit-length.
        let src = vec![
            q(1.0, 0.0, 0.0, 0.0),
            q(0.14f32.cos(), 0.0, 0.14f32.sin(), 0.0),
            q(0.28f32.cos(), 0.0, 0.28f32.sin(), 0.0),
        ];
        let out = resample_quats_to_frames(&src, 30.0, 30.0, 3, 0.0, 0.0);
        for i in 0..3 {
            assert!((out[i].w - src[i].w).abs() < 1e-5, "frame {i} w");
            assert!((out[i].y - src[i].y).abs() < 1e-5, "frame {i} y");
        }
    }

    #[test]
    fn point_sample_midpoint_interpolates() {
        let src = vec![
            q(1.0, 0.0, 0.0, 0.0),
            q(0.0, 0.0, 1.0, 0.0),  // 180° around Y
        ];
        // 1 dst sample at exactly the midpoint between src[0] and src[1].
        // src_dt = 1/30 ≈ 0.0333. Midpoint = 0.01666.
        // Use 60 fps dst, frame 1 lands at 1/60 ≈ 0.01666 (close enough).
        let out = resample_quats_to_frames(&src, 30.0, 60.0, 3, 0.0, 0.0);
        // Frame 1 should be the lerp of identity and (0,0,1,0) = (0.5, 0, 0.5, 0),
        // then normalized = (0.707, 0, 0.707, 0).
        let m = &out[1];
        assert!((m.w - 0.707).abs() < 0.05, "w = {}", m.w);
        assert!((m.y - 0.707).abs() < 0.05, "y = {}", m.y);
    }

    #[test]
    fn window_average_with_sign_continuity() {
        // Two src samples representing the SAME rotation in opposite
        // hemispheres (q and -q are the same rotation). Window-avg
        // without sign-continuity → garbage; WITH it → correct.
        let src = vec![
            q( 0.9, 0.4, 0.0, 0.0),
            q(-0.9, -0.4, 0.0, 0.0),  // -q, same rotation
        ];
        let out = resample_quats_to_frames(
            &src, 30.0, 30.0, 1, 1.0 / 60.0, 1.0 / 15.0,  // wide window
        );
        // With sign-correction, the average is ≈ (0.9, 0.4, 0, 0)
        // normalized = (0.913, 0.408, 0, 0). Without it, the average
        // would be ≈ 0 in all components → broken normalize.
        let r = &out[0];
        assert!((r.w - 0.913).abs() < 0.02, "w={}", r.w);
        assert!((r.x - 0.408).abs() < 0.02, "x={}", r.x);
    }

    #[test]
    fn cori_swap_yz_roundtrip() {
        let q1 = q(1.0, 2.0, 3.0, 4.0);
        let q2 = cori_swap_yz(q1);
        assert_eq!(q2.y, q1.z);
        assert_eq!(q2.z, q1.y);
        assert_eq!(cori_swap_yz(q2), q1);
    }
}
