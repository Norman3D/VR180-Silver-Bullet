//! Kandao QooCam 3 Ultra: the `kfix`/`kvar` metadata → the stabilizer's
//! [`DjiOsvImu`] (factory lens calibration per eye + the fused orientation
//! stream), the way `insv_imu` does it for the Insta360 X6.
//!
//! ## Lens
//!
//! The factory record (`vr180_fisheye::kandao`) is a Kannala-Brandt lens
//! per eye in the photo canvas (`13888 × 6944`, one `6944²` half per lens).
//! The video frame is each half scaled to the eye size (`3840²`: scale
//! `3840 / 6944`, no crop — the file's `V_CROP_SCALE` is the identity), so
//! `focal`, `cx`, `cy` scale by that and the dimensionless `k1..k4` pass
//! through; `k5 = p1 = p2 = 0`. Lens 0 is the top half = the LEFT eye after
//! the VR180 mod (measured on the near-field parallax of the sample clips:
//! near objects move LEFT from the top half to the bottom one), lens 1 the
//! bottom = RIGHT. The stills are the canvas itself: lens 0 in the left
//! half, at scale 1. Model and polynomial form confirmed against QooCam
//! Studio's own stitching shader (see the `kandao` module docs).
//!
//! ## Motion
//!
//! The raw gyro + accelerometer stream (1 kHz on the sh3001 units, ~788 Hz
//! on the bmi260 ones — the sample timestamps rule) is fused exactly as the
//! X6's (`integrate_orientation_samples`: trapezoid gyro integration with a
//! whole-clip VQF bias estimate, batch gravity alignment, the slow
//! inclination trim). The per-frame content time is the centre row's
//! mid-exposure, from the file:
//! `PTS + readout / 2 − shutter / 2 + latency` — the frame's `PTS` entry is
//! where the first row's READOUT starts (the end of its exposure),
//! `shutter` the frame's `EXP` record, and `readout` / `latency` depend on
//! the recording mode: the `kfix` **HDR** flag ([`kandao_readout_ms`],
//! [`kandao_gyro_latency_ms`]). Measured on two cameras (sh3001 / bmi260
//! IMUs, firmware 1.1.3 / 1.1.6), image motion vs gyro:
//! - HDR off (every indoor clip, most outdoor ones): the offset is
//!   +14.0…+15.0 ms at 0.2–0.6 ms shutters, +12.0 at 5.7 ms, +7.5…+9.25 at
//!   9.1–10 ms — i.e. `14.5 − shutter/2`; readout 19.4 ms (the app's own
//!   RS-scale probe: residual crosses zero at 0.79 × 24.4; raw stretch
//!   regressions 17.8–19.3; the two IMUs agree within 0.7 ms).
//! - HDR on (one clip): offset +17.25…+18 at 3.6–6.6 ms shutters, readout
//!   24.3–25.5 — the sensor reads slower and stamps later.
//! The sign of the shutter term is the measured one; the first reading of
//! the HDR clip alone (`+shutter/2`) put indoor 10 ms-shutter clips ~9 ms
//! off — visibly worse stabilization indoors.
//!
//! The IMU → camera basis (rows = the camera's x right / y up / z axes in
//! IMU coordinates, the X6 convention) was settled the way the X6's was:
//! export a window from the clip START with an uncapped camera lock under
//! each axis-aligned candidate (`VR180_KANDAO_BASIS`) and measure the
//! residual inter-frame motion of the output. The identity leaves 0.10× the
//! un-stabilised motion (0.08 px of 0.84 at 512); every sign flip makes it
//! WORSE than no stabilization (1.3–2.0×). The image-motion correlations
//! (image x ↔ −ω_y, image y ↔ −ω_x, roll ↔ +ω_z, gravity −1 g on IMU y
//! upright) are consistent with that under the stabilizer's conventions —
//! the identity is what they mean here, not a derivation. Readout
//! 24.4 ± 0.8 ms (three windows, both sensors), top row first, from the
//! rolling-shutter stretch regressed on the gyro's pitch-rate change.

use vr180_core::gyro::cori_iori::Quat;
use vr180_fisheye::insta360::InsvImuSample;
use vr180_fisheye::kandao::{KandaoLens, KandaoMeta};
use vr180_fisheye::{DjiLensCalib, DjiOsvImu};

use crate::insv_imu::{integrate_orientation_samples, HR_PER_FRAME, INSV_TAU_ACC_S};

/// IMU→camera basis for the VR180-modded QooCam 3 Ultra — rows are the
/// camera axes (x right, y up, z) in IMU coordinates, the same convention
/// as `INSV_X6_IMU_TO_CAM`: the identity, verified on the sample clip's
/// stabilized output against every axis-aligned alternative (see the module
/// docs). Both lenses share it (one IMU, same mount).
pub const KANDAO_IMU_TO_CAM: [[f32; 3]; 3] = [
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 0.0, 1.0],
];

/// Sensor readout (rolling-shutter sweep) time of the 3840×7680 @ 30 fps
/// video with HDR off, ms — measured (the file states none), top row first:
/// the app's RS-scale probe on a bmi260 clip puts the zero crossing at
/// 0.79 × 24.4 = 19.3 ms; raw stretch regressions on both cameras give
/// 17.8–19.3 (r up to −0.91). The default when no IMU block says otherwise.
pub const KANDAO_READOUT_MS: f32 = 19.4;
/// Readout with the camera's HDR mode on: 24.3 / 23.6 / 25.2 / 24.5 / 25.5 ms
/// on five windows of the one HDR clip (both sensors).
pub const KANDAO_READOUT_HDR_MS: f32 = 24.6;

/// Readout by recording mode (`kfix` `HDR` flag).
pub fn kandao_readout_ms(hdr: bool) -> f32 {
    if hdr { KANDAO_READOUT_HDR_MS } else { KANDAO_READOUT_MS }
}

/// The residual between the measured content time and
/// `PTS + readout/2 − shutter/2`, ms, by recording mode — the gyro's own
/// delay plus whatever the sensor's frame timestamp hides: HDR off
/// 14.5 − 9.7 → **4.6 ms** (both cameras, 0.2–10 ms shutters, seven clips);
/// HDR on 18 + 2.3 − 12.3 → **8.0 ms** (one clip).
pub fn kandao_gyro_latency_ms(hdr: bool) -> f32 {
    if hdr { 8.0 } else { 4.6 }
}

/// `VR180_KANDAO_BASIS="r00,r01,…,r22"` (row-major) overrides the basis for
/// headless A/B tests, like `VR180_INSV_BASIS`.
pub fn kandao_imu_to_cam() -> [[f32; 3]; 3] {
    let m = std::env::var("VR180_KANDAO_BASIS").ok()
        .and_then(|v| {
            let f: Vec<f32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            (f.len() == 9).then(|| [[f[0], f[1], f[2]], [f[3], f[4], f[5]], [f[6], f[7], f[8]]])
        })
        .unwrap_or(KANDAO_IMU_TO_CAM);
    orthonormalize(m)
}

/// Gram-Schmidt on the rows, z = x × y.
fn orthonormalize(m: [[f32; 3]; 3]) -> [[f32; 3]; 3] {
    let norm = |v: [f32; 3]| {
        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-12);
        [v[0] / n, v[1] / n, v[2] / n]
    };
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let x = norm(m[0]);
    let dy = dot(m[1], x);
    let y = norm([m[1][0] - dy * x[0], m[1][1] - dy * x[1], m[1][2] - dy * x[2]]);
    let z = [
        x[1] * y[2] - x[2] * y[1],
        x[2] * y[0] - x[0] * y[2],
        x[0] * y[1] - x[1] * y[0],
    ];
    [x, y, z]
}

/// One lens's calibration for a `stream_w × stream_h` eye: the canvas half
/// scaled uniformly (the eye is square like the half; a non-square eye
/// scales x and y separately), principal point straight from the record.
///
/// QooCam Studio's own VR180 render of the sample clip places the VIDEO
/// principal point (+63, +23) canvas px from the record's value on both
/// lenses (a dense flow fit against the app's render of the same frame:
/// 6.5 px residual with the record, 0.8 px with the shift, no rotation or
/// focal substitutes). That is NOT followed, by decision: Studio's
/// calibration is not trusted as ground truth for the mod; the app renders
/// from the camera's lens record, and the user aligns the rig with
/// Auto-align / the stereo offset. The same fit does confirm the model
/// form and the plain-scaled focal (−0.05 % / +0.28 %).
pub fn stream_lens_calib(lens: &KandaoLens, stream_w: u32, stream_h: u32) -> DjiLensCalib {
    let half_w = lens.half_w().max(1) as f32;
    let half_h = lens.canvas_h.max(1) as f32;
    let sx = stream_w as f32 / half_w;
    let sy = stream_h as f32 / half_h;
    let (px, py) = lens.half_principal_point();
    DjiLensCalib {
        fx: Some(lens.focal * sx),
        fy: Some(lens.focal * sy),
        cx: Some(px * sx),
        cy: Some(py * sy),
        k1: Some(lens.k[0]),
        k2: Some(lens.k[1]),
        k3: Some(lens.k[2]),
        k4: Some(lens.k[3]),
        k5: Some(0.0),
        p1: Some(0.0),
        p2: Some(0.0),
        width: Some(stream_w as f32),
        height: Some(stream_h as f32),
        omni: None,
        ..Default::default()
    }
}

/// The two lenses as the stabilizer's `(lens_a, lens_b)` pair: `lens_b` =
/// lens 0 = the TOP video half / LEFT still half (the LEFT eye with the
/// user swap off, the resolvers' default), `lens_a` = lens 1. The still
/// callers want `(left, right)` = `(lens 0, lens 1)`, i.e. the pair
/// reversed. A file with fewer than two lens records gets the generic
/// QooCam preset geometry (principal point at the eye centre).
pub fn lens_pair(lenses: &[KandaoLens], stream_w: u32, stream_h: u32) -> (DjiLensCalib, DjiLensCalib) {
    let pick = |i: u8| lenses.iter().find(|l| l.index == i).or_else(|| lenses.get(i as usize));
    match (pick(0), pick(1)) {
        (Some(l0), Some(l1)) => (stream_lens_calib(l1, stream_w, stream_h), stream_lens_calib(l0, stream_w, stream_h)),
        _ => {
            tracing::warn!("kandao: {} lens record(s) in the file — using the generic QooCam 3 Ultra profile", lenses.len());
            (generic_lens_calib(stream_w, stream_h), generic_lens_calib(stream_w, stream_h))
        }
    }
}

/// The preset's geometry when a file carries no lens record: the sample
/// camera's focal and distortion with the principal point at the centre.
pub fn generic_lens_calib(stream_w: u32, stream_h: u32) -> DjiLensCalib {
    let preset = vr180_fisheye::presets::find(KANDAO_PRESET_NAME);
    let (fx, k) = preset
        .map(|p| ((p.calib.fx as f32) * stream_w as f32 / p.calib.calib_w.max(1) as f32,
                  [p.calib.k[0] as f32, p.calib.k[1] as f32, p.calib.k[2] as f32, p.calib.k[3] as f32]))
        .unwrap_or((2142.13 * stream_w as f32 / 6944.0, [-0.011315, 0.001868, -0.003552, 0.000244]));
    DjiLensCalib {
        fx: Some(fx), fy: Some(fx),
        cx: Some(stream_w as f32 * 0.5), cy: Some(stream_h as f32 * 0.5),
        k1: Some(k[0]), k2: Some(k[1]), k3: Some(k[2]), k4: Some(k[3]),
        k5: Some(0.0), p1: Some(0.0), p2: Some(0.0),
        width: Some(stream_w as f32), height: Some(stream_h as f32),
        omni: None,
        ..Default::default()
    }
}

/// The camera-preset name the QooCam kinds resolve to.
pub const KANDAO_PRESET_NAME: &str = "Kandao QooCam 3 Ultra";

/// Calibration-only IMU (no quats): enough to dewarp and seed the Override
/// UI; stabilization stays identity.
pub fn kandao_calib_only(meta: &KandaoMeta, stream_w: u32, stream_h: u32) -> DjiOsvImu {
    let (lens_a, lens_b) = lens_pair(&meta.lenses, stream_w, stream_h);
    DjiOsvImu {
        lens_a,
        lens_b,
        camera_model: Some(if meta.product.is_empty() { "Kandao QooCam".to_string() } else { meta.product.clone() }),
        imu_to_cam: Some(kandao_imu_to_cam()),
        readout_ms: Some(kandao_readout_ms(meta.hdr)),
        sample_anchor: vr180_fisheye::SampleAnchor::BlockMid,
        ..Default::default()
    }
}

/// The stabilizer's per-frame orientation stream from the `kvar` motion
/// tables. `n_frames_hint` is the video's frame count (the PTS table
/// normally matches; extra frames extrapolate at `1/fps`). Returns the
/// calibration-only structure when the file has no usable IMU / PTS.
pub fn build_kandao_imu(meta: &KandaoMeta, fps: f32, n_frames_hint: usize, stream_w: u32, stream_h: u32) -> DjiOsvImu {
    let mut out = kandao_calib_only(meta, stream_w, stream_h);
    let n_imu = meta.imu.len();
    if n_imu < 16 || meta.frame_pts_us.is_empty() || fps <= 0.0 {
        tracing::warn!(
            "kandao_imu: no usable motion data (imu={} pts={} fps={}) — calib only",
            n_imu, meta.frame_pts_us.len(), fps
        );
        return out;
    }
    let dt = meta.imu_period_s() as f32;
    if !dt.is_finite() || dt <= 0.0 {
        tracing::warn!("kandao_imu: bad IMU period {dt} — calib only");
        return out;
    }
    let samples: Vec<InsvImuSample> = meta.imu.iter()
        .map(|s| InsvImuSample { t_us: s.t_us, acc_g: s.acc_g, gyr_dps: s.gyr_dps })
        .collect();
    const DEG: f32 = std::f32::consts::PI / 180.0;
    const G: f32 = 9.80665;
    let gyro: Vec<[f32; 3]> = samples.iter()
        .map(|s| [s.gyr_dps[0] * DEG, s.gyr_dps[1] * DEG, s.gyr_dps[2] * DEG]).collect();
    let acc: Vec<[f32; 3]> = samples.iter()
        .map(|s| [s.acc_g[0] * G, s.acc_g[1] * G, s.acc_g[2] * G]).collect();
    let t0 = std::time::Instant::now();
    let pass1 = vr180_core::gyro::vqf::run(&gyro, &acc, None, dt);
    let quats = integrate_orientation_samples(&samples, pass1.bias_rad_s, INSV_TAU_ACC_S);
    tracing::info!(
        "kandao_imu: gyro integration over {} samples @ {:.1} Hz in {:.1?}, bias = {:?} °/s",
        n_imu, 1.0 / dt, t0.elapsed(), pass1.bias_deg_s()
    );
    let timeline = crate::insv_imu::QuatTimeline {
        t_s: samples.iter().map(|s| s.t_us as f64 * 1e-6).collect(),
        quats: &quats,
    };
    let readout_s = kandao_readout_ms(meta.hdr) as f64 * 1e-3;
    let latency_s = kandao_gyro_latency_ms(meta.hdr) as f64 * 1e-3;
    let frame_dur = 1.0 / fps as f64;
    let pts = &meta.frame_pts_us;
    let last = pts.len() - 1;
    // Centre row mid-exposure: PTS is the first row's readout start, the
    // centre row reads out readout/2 later and its exposure ended shutter/2
    // before its own readout; the gyro reports that much late.
    let content_time = |i: usize| -> f64 {
        let shutter = meta.shutter_s(i).unwrap_or(0.0);
        let base = readout_s * 0.5 - shutter * 0.5 + latency_s;
        if i < pts.len() {
            pts[i] as f64 * 1e-6 + base
        } else {
            pts[last] as f64 * 1e-6 + base + (i - last) as f64 * frame_dur
        }
    };
    let n_frames = n_frames_hint.max(pts.len());
    if pts.len() != n_frames_hint && n_frames_hint > 0 {
        tracing::info!("kandao_imu: {} PTS entries vs {} video frames — extrapolating the tail at 1/fps", pts.len(), n_frames_hint);
    }
    out.frame_quats.reserve(n_frames);
    out.high_rate_quats.reserve(n_frames);
    out.gravity.reserve(n_frames);
    for i in 0..n_frames {
        let tc = content_time(i);
        out.frame_quats.push(timeline.at(tc));
        let start = tc - frame_dur * 0.5;
        let hr: Vec<Quat> = (0..HR_PER_FRAME)
            .map(|k| timeline.at(start + frame_dur * (k as f64) / (HR_PER_FRAME as f64)))
            .collect();
        out.high_rate_quats.push(hr);
        out.gravity.push([0.0, -1.0, 0.0]);
    }
    out.exposure_ms = (0..n_frames).map(|i| meta.shutter_s(i).map(|s| (s * 1e3) as f32).unwrap_or(f32::NAN)).collect();
    tracing::info!(
        "kandao_imu: {} frames ({} stamped), content t0 = {:.3} s (HDR {}: readout {:.1} ms, latency {:.1} ms; IMU {:?}), imu span {:.3}..{:.3} s",
        n_frames, pts.len(), content_time(0), meta.hdr, readout_s * 1e3, latency_s * 1e3, meta.imu_model,
        timeline.t_s[0], timeline.t_s[timeline.t_s.len() - 1]
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lens(index: u8, focal: f32, cx: f32, cy: f32) -> KandaoLens {
        KandaoLens {
            index, canvas_w: 13888, canvas_h: 6944, focal, cx, cy,
            k: [-0.011315, 0.001868, -0.003552, 0.000244],
            rotation: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0], translation: [0.0; 3],
        }
    }

    #[test]
    fn stream_calib_scales_the_canvas_half() {
        let l1 = lens(1, 2134.94, 10416.16, 3465.44);
        let c = stream_lens_calib(&l1, 3840, 3840);
        let s = 3840.0 / 6944.0;
        assert!((c.fx.unwrap() - 2134.94 * s).abs() < 1e-3);
        assert!((c.cx.unwrap() - (10416.16 - 6944.0) * s).abs() < 1e-3);
        assert!((c.cy.unwrap() - 3465.44 * s).abs() < 1e-3);
        assert_eq!(c.k1, Some(-0.011315));
        assert_eq!(c.k5, Some(0.0));
        assert!(c.omni.is_none());
        // the still: scale 1
        let c1 = stream_lens_calib(&l1, 6944, 6944);
        assert!((c1.cx.unwrap() - 3472.16).abs() < 1e-3 && (c1.fx.unwrap() - 2134.94).abs() < 1e-3);
    }

    #[test]
    fn lens_pair_puts_lens0_in_lens_b() {
        let (a, b) = lens_pair(&[lens(0, 2142.13, 3458.49, 3479.25), lens(1, 2134.94, 10416.16, 3465.44)], 3840, 3840);
        assert!((b.fx.unwrap() - 2142.13 * 3840.0 / 6944.0).abs() < 1e-3);
        assert!((a.fx.unwrap() - 2134.94 * 3840.0 / 6944.0).abs() < 1e-3);
        assert!((b.cx.unwrap() - 3458.49 * 3840.0 / 6944.0).abs() < 1e-3);
        let (_, b_still) = lens_pair(&[lens(0, 2142.13, 3458.49, 3479.25), lens(1, 2134.94, 10416.16, 3465.44)], 6944, 6944);
        assert!((b_still.cx.unwrap() - 3458.49).abs() < 1e-3);
        let (a2, _) = lens_pair(&[], 3840, 3840);
        assert_eq!(a2.cx, Some(1920.0));
    }

    #[test]
    fn basis_is_a_proper_rotation() {
        let m = orthonormalize(KANDAO_IMU_TO_CAM);
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        assert!((det - 1.0).abs() < 1e-6);
        assert_eq!(m, KANDAO_IMU_TO_CAM);
    }

    #[test]
    fn content_time_and_stream_from_tables() {
        let mut meta = KandaoMeta::default();
        meta.lenses = vec![lens(0, 2142.13, 3458.49, 3479.25), lens(1, 2134.94, 10416.16, 3465.44)];
        let t0 = 1_000_000u64;
        meta.frame_pts_us = (0..30).map(|i| t0 + i * 33_333).collect();
        meta.exposures = (0..30).map(|i| vr180_fisheye::kandao::KandaoExposure { pts_ms: ((t0 + i * 33_333) / 1000) as u32, shutter_us: 6000 }).collect();
        meta.imu = (0..3000).map(|k| vr180_fisheye::kandao::KandaoImuSample {
            t_us: t0 - 500_000 + k * 1000, gyr_dps: [0.0, 0.0, 0.0], acc_g: [0.0, -1.0, 0.0],
        }).collect();
        meta.imu_model = "bmi260".into();
        let imu = build_kandao_imu(&meta, 30.0, 32, 3840, 3840);
        assert_eq!(imu.frame_quats.len(), 32);
        assert_eq!(kandao_readout_ms(false), KANDAO_READOUT_MS);
        assert_eq!(kandao_readout_ms(true), KANDAO_READOUT_HDR_MS);
        assert!(kandao_gyro_latency_ms(true) > kandao_gyro_latency_ms(false));
        meta.hdr = true;
        let imu_hdr = build_kandao_imu(&meta, 30.0, 32, 3840, 3840);
        assert_eq!(imu_hdr.readout_ms, Some(KANDAO_READOUT_HDR_MS));
        assert_eq!(imu.high_rate_quats[0].len(), HR_PER_FRAME);
        assert_eq!(imu.readout_ms, Some(KANDAO_READOUT_MS));
        assert_eq!(imu.sample_anchor, vr180_fisheye::SampleAnchor::BlockMid);
        assert!((imu.exposure_ms[3] - 6.0).abs() < 1e-6);
        // A still camera: every frame quat equals the first (no drift).
        let q0 = imu.frame_quats[0];
        let q9 = imu.frame_quats[29];
        assert!((q0.w - q9.w).abs() < 1e-4 && (q0.x - q9.x).abs() < 1e-4);
        let calib = kandao_calib_only(&meta, 3840, 3840);
        assert!(calib.frame_quats.is_empty());
        assert_eq!(calib.imu_to_cam, Some(KANDAO_IMU_TO_CAM));
    }
}
