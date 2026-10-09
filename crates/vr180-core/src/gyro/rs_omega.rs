//! Per-frame angular velocity for rolling-shutter correction.
//!
//! Port of `vr180_gui.py::get_gyro_angular_velocity` + the 20 ms moving
//! average smoothing done in `GyroStabilizer.__init__`. The output is
//! one `[ω_x, ω_y, ω_z]` triple per video frame, in the equirect shader's
//! coordinate frame (X = right, Y = up, Z = forward), in **rad/s**.
//!
//! ## Why a 20 ms window
//!
//! From `docs/rolling_shutter_correction.md`:
//!
//! > 20ms (empirically optimal) instead of the SROT readout time (15.2ms):
//! > * At 800Hz → ~16 samples → 17 (odd for symmetry)
//! > * 20ms better matches the firmware's internal RS smoothing
//! > * Tested: 20ms closes 37% of jitter gap vs 26% for 15ms
//! > * High-frequency vibrations (82Hz, 164Hz) are already corrected by
//! >   firmware in both eyes — we only need to fix the sign-flip error
//! >   which is a low-frequency phenomenon
//!
//! ## Axis convention
//!
//! Raw GYRO on disk: `raw[0]=Z, raw[1]=X, raw[2]=Y` (GoPro ORIN="ZXY").
//! Body frame from `vr180_pipeline::imu`: `bodyX←raw[1], bodyY←raw[2],
//! bodyZ←raw[0]`.
//!
//! The equirect shader uses (X = right, Y = up, Z = forward). The
//! camera's body axes map to shader axes as:
//!
//! - shader X = body X = raw[1]  (pitch axis)
//! - shader Y = body Z = raw[0]  (yaw axis — Python's "yaw" = bodyZ)
//! - shader Z = body Y = raw[2]  (roll axis — Python's "roll" = bodyY)
//!
//! So directly from raw: `[ω_x, ω_y, ω_z] = [raw[1], raw[0], raw[2]]`.

use super::raw::ImuBlock;

/// Default smoothing window for the per-frame gyro sampler (20 ms).
pub const SMOOTH_WINDOW_S: f32 = 0.020;

/// Compute per-frame angular velocity in the equirect shader frame,
/// in **rad/s**, smoothed with a moving-average window.
///
/// # Arguments
/// - `gyro_blocks` — raw GYRO blocks straight from `parse_raw_imu`.
///   Samples are on-disk axes (`[raw[0]=Z, raw[1]=X, raw[2]=Y]`)
///   in rad/s (GoPro stores them post-SCAL).
/// - `n_frames` — number of video frames to produce a sample for.
/// - `fps` — video frame rate.
/// - `duration_s` — total clip duration in seconds. Used to derive
///   the gyro sample interval (`duration / total_samples`) — matches
///   Python's convention exactly, including the small drift the IMU
///   clock has from video time.
/// - `time_offset_s` — sample at `t = i/fps + time_offset_s`. Use
///   `SROT/2` to land on the center of each frame's sensor readout
///   window.
/// - `smooth_window_s` — moving-average window width. Pass
///   `SMOOTH_WINDOW_S` (20 ms) for the default behavior.
pub fn compute_per_frame_omega(
    gyro_blocks: &[ImuBlock],
    n_frames: usize,
    fps: f32,
    duration_s: f32,
    time_offset_s: f32,
    smooth_window_s: f32,
) -> Vec<[f32; 3]> {
    if gyro_blocks.is_empty() || n_frames == 0 {
        return vec![[0.0_f32; 3]; n_frames];
    }

    // Step 1: flatten gyro to shader-frame axes.
    let mut shader_omega: Vec<[f32; 3]> = Vec::with_capacity(
        gyro_blocks.iter().map(|b| b.samples.len()).sum()
    );
    for blk in gyro_blocks {
        for s in &blk.samples {
            // shader = [raw[1], raw[0], raw[2]] — see module docs.
            shader_omega.push([s[1], s[0], s[2]]);
        }
    }

    let n_gyro = shader_omega.len();
    let dt = duration_s / n_gyro as f32;

    // Step 2: 20ms moving average per axis (centered window).
    let win = if smooth_window_s > 0.0 {
        ((smooth_window_s / dt).round() as usize).max(3) | 1  // force odd
    } else {
        1
    };
    let smoothed = if win <= 1 {
        shader_omega
    } else {
        moving_average_3(&shader_omega, win)
    };

    // Step 3: sample at frame times by nearest-neighbor index.
    // Same algorithm Python uses: `idx = min(int(t / dt), n_gyro - 1)`.
    let mut out = Vec::with_capacity(n_frames);
    let dt_frame = 1.0 / fps.max(1e-6);
    for i in 0..n_frames {
        let t = i as f32 * dt_frame + time_offset_s;
        let idx = ((t / dt) as isize).max(0) as usize;
        let idx = idx.min(n_gyro - 1);
        out.push(smoothed[idx]);
    }
    out
}

/// Moving average of an `Nx3` array with reflect padding at the edges.
/// `win` is the window width in samples (assumed odd).
fn moving_average_3(samples: &[[f32; 3]], win: usize) -> Vec<[f32; 3]> {
    let n = samples.len();
    let half = win / 2;
    // Cumulative sum trick for O(N) moving average.
    let mut csum = vec![[0.0_f32; 3]; n + 1];
    for i in 0..n {
        for j in 0..3 {
            csum[i + 1][j] = csum[i][j] + samples[i][j];
        }
    }
    let inv_win = 1.0 / win as f32;
    let mut out = vec![[0.0_f32; 3]; n];
    for i in 0..n {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(n);
        let cnt = (hi - lo) as f32;
        for j in 0..3 {
            out[i][j] = (csum[hi][j] - csum[lo][j]) / cnt;
        }
        // For the very edges, the window is shorter so we naturally
        // get a smaller average — that's equivalent to "extend by
        // boundary value" reflect padding for slow-varying signals.
        let _ = inv_win;
    }
    out
}

/// Python-parity per-frame angular velocity source. Mirrors
/// `vr180_gui.py` exactly:
/// - **STMP-anchored sample timestamps** (`gyro_to_timestamps`) +
///   linear interpolation — NOT `int(t/dt)` nearest-index, which
///   accumulates ~33 ms of drift over a 71 s clip (the documented
///   Python bug, fixed there the same way).
/// - **`raw`** stream for per-scanline groups (smoothing would destroy
///   the angular-acceleration detail inside the 15 ms readout window).
/// - **`smoothed`** stream (15 ms box, reflect-padded — byte-matches
///   `GyroStabilizer.__init__`) for the single-ω fallback.
///
/// All values are in the equirect-shader frame `[ω_x=pitch, ω_y=yaw,
/// ω_z=roll] = [raw[1], raw[0], raw[2]]`, rad/s, **no factors applied**
/// (the caller multiplies per-axis RS factors).
pub struct GyroAngvel {
    pub times: Vec<f32>,
    pub raw: Vec<[f32; 3]>,
    pub smoothed: Vec<[f32; 3]>,
    /// Running integral of `raw` (trapezoid, rad) at each sample time —
    /// `cum[i] = ∫_{times[0]}^{times[i]} ω dt`. Lets a row group carry the
    /// MEAN rate between the frame's reference instant and the row's
    /// instant, so `ω_group · Δt` is the integrated angle, not the
    /// instantaneous rate at the row.
    pub cum: Vec<[f64; 3]>,
}

impl GyroAngvel {
    /// Build from raw GYRO blocks + CORI STMP blocks.
    /// `fallback_duration_s` is used for uniform spacing when STMP
    /// tags are missing (same fallback as the VQF input prep).
    pub fn build(
        gyro_blocks: &[ImuBlock],
        cori_stmps: &[super::cori_iori::CoriBlockStmp],
        fps: f32,
        fallback_duration_s: f32,
    ) -> Option<Self> {
        Self::build_with_skips(gyro_blocks, cori_stmps, None, fps, fallback_duration_s)
    }

    /// [`Self::build`] honouring the camera's dropped frames (`frame_skips`
    /// = per-video-frame MSKP, see `build_imu_sample_times_with_skips`).
    pub fn build_with_skips(
        gyro_blocks: &[ImuBlock],
        cori_stmps: &[super::cori_iori::CoriBlockStmp],
        frame_skips: Option<&[i32]>,
        fps: f32,
        fallback_duration_s: f32,
    ) -> Option<Self> {
        let times = super::resample::build_imu_sample_times_with_skips(
            gyro_blocks, cori_stmps, frame_skips, fps, fallback_duration_s,
        );
        if times.len() < 10 {
            return None;
        }
        let raw: Vec<[f32; 3]> = gyro_blocks.iter()
            .flat_map(|b| b.samples.iter())
            .map(|s| [s[1], s[0], s[2]])
            .collect();
        if raw.len() != times.len() {
            return None;
        }

        // 15 ms box smooth with reflect padding — matches Python:
        //   win = max(3, round(0.015 / dt_med)) forced odd,
        //   padded = col[pad:0:-1] + col + col[-2:-2-pad:-1]
        let n = raw.len();
        let mut diffs: Vec<f32> = times.windows(2).take(200)
            .map(|w| w[1] - w[0]).collect();
        diffs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let dt_med = diffs.get(diffs.len() / 2).copied().unwrap_or(1.0 / 800.0);
        let mut win = ((0.015 / dt_med.max(1e-6)).round() as usize).max(3);
        if win % 2 == 0 { win += 1; }
        let pad = win / 2;

        let mut smoothed = vec![[0.0_f32; 3]; n];
        for ax in 0..3 {
            // Reflect-padded column (Python's col[pad:0:-1] … col[-2:-2-pad:-1]).
            let mut padded = Vec::with_capacity(n + 2 * pad);
            for i in (1..=pad.min(n - 1)).rev() { padded.push(raw[i][ax]); }
            while padded.len() < pad { padded.push(raw[0][ax]); } // degenerate short input
            for s in raw.iter() { padded.push(s[ax]); }
            for i in 1..=pad.min(n - 1) { padded.push(raw[n - 1 - i][ax]); }
            while padded.len() < n + 2 * pad { padded.push(raw[n - 1][ax]); }

            let mut csum = vec![0.0_f64; padded.len() + 1];
            for (i, v) in padded.iter().enumerate() {
                csum[i + 1] = csum[i] + *v as f64;
            }
            let inv = 1.0 / win as f64;
            for i in 0..n {
                smoothed[i][ax] = ((csum[i + win] - csum[i]) * inv) as f32;
            }
        }

        Some(Self::from_parts(times, raw, smoothed))
    }

    /// Assemble from already-built streams (e.g. several chapters merged
    /// on one timeline); computes the running integral.
    pub fn from_parts(times: Vec<f32>, raw: Vec<[f32; 3]>, smoothed: Vec<[f32; 3]>) -> Self {
        // Trapezoid running integral of the raw rate (rad), for the
        // mean-rate row groups.
        let n = raw.len().min(times.len());
        let mut cum = vec![[0.0_f64; 3]; n];
        for i in 1..n {
            let dt = (times[i] - times[i - 1]).max(0.0) as f64;
            for ax in 0..3 {
                cum[i][ax] = cum[i - 1][ax]
                    + 0.5 * (raw[i - 1][ax] as f64 + raw[i][ax] as f64) * dt;
            }
        }
        Self { times, raw, smoothed, cum }
    }

    /// `∫_{times[0]}^{t} ω dt` (rad), exact for the piecewise-linear raw
    /// stream; clamped to the stream's span.
    fn integral_at(&self, t: f32) -> [f64; 3] {
        let times = &self.times;
        let n = times.len();
        let t = t.clamp(times[0], times[n - 1]);
        let mut idx = match times.binary_search_by(|x| x.partial_cmp(&t)
            .unwrap_or(std::cmp::Ordering::Equal)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        idx = idx.min(n - 2);
        let (t0, t1) = (times[idx], times[idx + 1]);
        let dt = (t1 - t0) as f64;
        if dt < 1e-9 { return self.cum[idx]; }
        let a = (((t - t0) as f64) / dt).clamp(0.0, 1.0);
        let (s0, s1) = (self.raw[idx], self.raw[idx + 1]);
        let mut out = self.cum[idx];
        for ax in 0..3 {
            let (v0, v1) = (s0[ax] as f64, s1[ax] as f64);
            // ∫_0^{a·dt} (v0 + (v1 − v0)·τ/dt) dτ
            out[ax] += v0 * a * dt + 0.5 * (v1 - v0) * a * a * dt;
        }
        out
    }

    fn lerp_at(stream: &[[f32; 3]], times: &[f32], t: f32) -> [f32; 3] {
        let n = stream.len();
        let t = t.clamp(times[0], times[n - 1]);
        // bisect_right(times, t) - 1, clamped to [0, n-2]
        let mut idx = match times.binary_search_by(|x| x.partial_cmp(&t)
            .unwrap_or(std::cmp::Ordering::Equal)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        idx = idx.min(n - 2);
        let (t0, t1) = (times[idx], times[idx + 1]);
        let dt = t1 - t0;
        if dt < 1e-9 { return stream[idx]; }
        let a = ((t - t0) / dt).clamp(0.0, 1.0);
        let (s0, s1) = (stream[idx], stream[idx + 1]);
        [
            s0[0] * (1.0 - a) + s1[0] * a,
            s0[1] * (1.0 - a) + s1[1] * a,
            s0[2] * (1.0 - a) + s1[2] * a,
        ]
    }

    /// Smoothed ω lerped at frame time `t` — Python's
    /// `get_angular_velocity_at_time` (GYRO path).
    pub fn single_at(&self, t: f32) -> [f32; 3] {
        Self::lerp_at(&self.smoothed, &self.times, t)
    }

    /// Per-row-group rate for `n_groups` row instants spanning
    /// `t ± srot/2` (without factors; caller applies). Group `g` sits at
    /// `t_g = t + (g/(n−1) − ½)·srot` and carries the MEAN raw rate over
    /// `[t, t_g]`, i.e. `(∫_t^{t_g} ω dt) / (t_g − t)`: the shader multiplies
    /// it by the pixel's row offset `t_g − t`, so the product is the
    /// integrated camera rotation between the reference row and that
    /// row — exact under angular acceleration, where the instantaneous
    /// rate at the row (the previous definition) overshoots by `α·Δt²/2`.
    /// Groups within a microsecond of `t` degrade to the instantaneous rate.
    pub fn groups_at(&self, t: f32, srot_s: f32, n_groups: usize) -> Vec<[f32; 3]> {
        let n_groups = n_groups.max(2);
        let base = self.integral_at(t);
        (0..n_groups).map(|g| {
            let frac = g as f32 / (n_groups - 1) as f32; // 0..=1
            let gt = t + (frac - 0.5) * srot_s;
            let span = (gt - t) as f64;
            if span.abs() < 1e-6 {
                return Self::lerp_at(&self.raw, &self.times, gt);
            }
            let end = self.integral_at(gt);
            let mut out = [0.0_f32; 3];
            for ax in 0..3 { out[ax] = ((end[ax] - base[ax]) / span) as f32; }
            out
        }).collect()
    }

    /// RAW ω lerped at `t` (instantaneous rate).
    pub fn raw_at(&self, t: f32) -> [f32; 3] {
        Self::lerp_at(&self.raw, &self.times, t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(samples: Vec<[f32; 3]>) -> ImuBlock {
        ImuBlock { samples, scal: 1.0, stmp_us: Some(0) }
    }

    #[test]
    fn constant_gyro_smooths_to_constant() {
        // 800 samples at 800 Hz = 1s. Constant gyro at (0.1, 0.2, 0.3)
        // rad/s on raw axes. After axis remap, shader frame is (raw[1],
        // raw[0], raw[2]) = (0.2, 0.1, 0.3). Should be the same across
        // all output frames.
        let raw_sample = [0.1, 0.2, 0.3];
        let samples = vec![raw_sample; 800];
        let blocks = vec![block(samples)];
        let out = compute_per_frame_omega(
            &blocks, 30, 30.0, 1.0, 0.0, SMOOTH_WINDOW_S,
        );
        assert_eq!(out.len(), 30);
        for s in &out {
            assert!((s[0] - 0.2).abs() < 1e-4, "shader X = {}", s[0]);
            assert!((s[1] - 0.1).abs() < 1e-4, "shader Y = {}", s[1]);
            assert!((s[2] - 0.3).abs() < 1e-4, "shader Z = {}", s[2]);
        }
    }

    #[test]
    fn smoothing_attenuates_high_freq_jitter() {
        // 800 samples at 800 Hz. Slow ramp on shader-X plus per-sample
        // high-freq jitter. After 20 ms smoothing (16 samples wide),
        // the mid-frame value should track the slow ramp.
        let mut samples = Vec::with_capacity(800);
        for i in 0..800 {
            let slow = (i as f32) * 0.001;
            let jitter = if i % 2 == 0 { 0.5 } else { -0.5 };
            // raw[1] = shader X. Put slow+jitter there.
            samples.push([0.0, slow + jitter, 0.0]);
        }
        let blocks = vec![block(samples)];
        let out = compute_per_frame_omega(
            &blocks, 30, 30.0, 1.0, 0.0, SMOOTH_WINDOW_S,
        );
        // Frame 15 lands at gyro index ~400 → slow value ≈ 0.4.
        let mid = out[15][0];
        assert!((mid - 0.4).abs() < 0.05, "expected ~0.4 after smoothing; got {mid}");
    }

    #[test]
    fn no_smoothing_passes_through() {
        let samples = vec![[1.0, 2.0, 3.0]; 800];
        let blocks = vec![block(samples)];
        let out = compute_per_frame_omega(
            &blocks, 5, 30.0, 1.0, 0.0, 0.0,  // smooth_window_s = 0
        );
        for s in &out {
            assert!((s[0] - 2.0).abs() < 1e-6);
            assert!((s[1] - 1.0).abs() < 1e-6);
            assert!((s[2] - 3.0).abs() < 1e-6);
        }
    }

    #[test]
    fn row_groups_carry_the_mean_rate_over_their_interval() {
        // Linear angular acceleration on shader X: ω(t) = α·t. The group at
        // row instant t_g must carry (∫_t^{t_g} ω)/(t_g − t) = α·(t + t_g)/2,
        // so ω_g·(t_g − t) is the integrated angle — not α·t_g (the
        // instantaneous rate at the row, which overshoots by α·Δt²/2).
        use super::super::cori_iori::CoriBlockStmp;
        let alpha = 3.0_f32; // rad/s²
        let rate = 800.0_f32;
        let mut blocks = Vec::new();
        for b in 0..2u64 {
            let samples: Vec<[f32; 3]> = (0..800).map(|i| {
                let t = b as f32 + i as f32 / rate;
                [0.0, alpha * t, 0.0] // raw[1] → shader X
            }).collect();
            blocks.push(ImuBlock { samples, scal: 1.0, stmp_us: Some(b * 1_000_000) });
        }
        let cori = vec![
            CoriBlockStmp { stmp_us: 0, n_samples: 30 },
            CoriBlockStmp { stmp_us: 1_000_000, n_samples: 30 },
        ];
        let av = GyroAngvel::build(&blocks, &cori, 30.0, 2.0).expect("angvel");
        let (t, srot) = (1.0_f32, 0.016_f32);
        let groups = av.groups_at(t, srot, 32);
        assert_eq!(groups.len(), 32);
        for (g, w) in groups.iter().enumerate() {
            let gt = t + (g as f32 / 31.0 - 0.5) * srot;
            let expect = alpha * (t + gt) / 2.0;
            assert!((w[0] - expect).abs() < 2e-4, "group {g}: {} vs mean {expect} (instantaneous would be {})", w[0], alpha * gt);
            assert!(w[1].abs() < 1e-5 && w[2].abs() < 1e-5);
        }
        // Edge rows: the product with the row offset is the integrated angle.
        let dt = -0.5 * srot;
        let got = groups[0][0] * dt;
        let want = alpha * ((t + dt).powi(2) - t.powi(2)) / 2.0;
        assert!((got - want).abs() < 2e-6, "{got} vs {want}");
    }

    #[test]
    fn empty_gyro_returns_zeros() {
        let out = compute_per_frame_omega(
            &[], 10, 30.0, 0.33, 0.0, SMOOTH_WINDOW_S,
        );
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], [0.0_f32; 3]);
    }
}
