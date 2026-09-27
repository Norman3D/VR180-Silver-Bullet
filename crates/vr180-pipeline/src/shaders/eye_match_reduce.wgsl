// Automatic eye-exposure matching, pass 2 of 3: histograms → offsets.
//
// One thread. Reads the two per-eye luma histograms written by
// `eye_match_hist.wgsl`, estimates the brightness difference between the
// eyes as the mean shift of five quantiles spread across the rank range
// both eyes keep once their clipped tails are cut (robust to the content
// the eyes do NOT share — occlusions, the stereo parallax at the edges of
// the region), smooths it over time, and writes the offset each eye must
// receive so they match. Everything stays on the GPU: no
// readback, no stall, no one-frame lag.
//
// Temporal model: the two sensors' auto-exposure moves in rare large
// steps (an AE decision, up to ~0.7 stop in one frame) on top of a slow
// drift, while the per-frame measurement jitters by ~0.01 stop. A
// change larger than `snap` is therefore taken at once (a lagged step
// would leave the eyes mismatched for several frames); smaller changes
// are low-passed with `alpha`. `reset` (first frame, or a seek) takes
// the measurement as-is.
//
// Clipped tails carry no exposure information: the sensor's white wall
// (X6 I-Log: code ≈ 0.835) and its black level (≈ 0.091 — also what the
// padding outside the image circle reads) pile up at the same code in
// both eyes whatever the exposure, and would drag a shift estimate toward
// zero. `lo`/`hi` are therefore expressed on the SOURCE's code scale, and
// the cut is made by RANK, jointly for both eyes: the larger of the two
// eyes' clipped fractions (top and bottom) is removed from BOTH, so a pure
// shift of the unclipped content is recovered exactly even when the
// brighter eye clips more. Quantiles are then averaged across the valid
// rank range. If either eye keeps fewer than `min_count` unclipped
// samples, or the valid rank range is too narrow, the previous offsets
// are kept.

const BINS: u32 = 1024u;

struct ReduceParams {
    mode: u32,        // 0 = meet in the middle, 1 = match RIGHT to left, 2 = match LEFT to right
    reset: u32,       // 1 = discard the temporal state (first frame / seek)
    alpha: f32,       // EMA factor for sub-threshold changes (0..1]
    snap: f32,        // |Δ| in code units above which the new value is taken at once
    lo: f32,          // code below which samples count as crushed black
    hi: f32,          // code above which samples count as clipped white
    min_count: f32,   // minimum unclipped samples per eye
    max_abs: f32,     // clamp on |d| (code units) — a runaway guard
};

struct EyeMatchState {
    off_l: f32,       // additive code offset for the left eye
    off_r: f32,       // additive code offset for the right eye
    d: f32,           // smoothed luma difference L − R (code units)
    d_raw: f32,       // this frame's raw measurement
    valid: u32,       // 1 once a measurement has been accepted
    count_l: u32,     // unclipped samples this frame
    count_r: u32,
    reason: u32,      // this frame: 0 measured, 1 too few unclipped samples, 2 rank range too narrow
};

@group(0) @binding(0) var<storage, read> hist: array<u32>;             // 2 × BINS
@group(0) @binding(1) var<storage, read_write> st: EyeMatchState;
@group(0) @binding(2) var<uniform> p: ReduceParams;

const MARGIN: f32 = 0.03;   // rank margin kept clear of the clipped tails
const NQ: u32 = 5u;         // quantiles averaged across the valid rank range
const MIN_RANGE: f32 = 0.1; // narrower valid rank range → no measurement

// Samples of `eye` in bins [start, end) — half-open, so an empty range is
// a no-op and no index is ever computed by subtraction (WGSL `select`
// evaluates both arms; a wrapped bound here would loop for 2^32 steps).
fn count_bins(eye: u32, start: u32, end: u32) -> u32 {
    var n = 0u;
    for (var b = start; b < end; b++) {
        n += hist[eye * BINS + b];
    }
    return n;
}

// Value (code units) below which a fraction `q` of the eye's valid
// samples lie, interpolated linearly inside the bin that crosses `q`.
fn percentile(eye: u32, total: u32, q: f32, lo_b: u32, hi_b: u32) -> f32 {
    let goal = q * f32(total);
    var cum = 0.0;
    for (var b = lo_b; b <= hi_b; b++) {
        let c = f32(hist[eye * BINS + b]);
        if (cum + c >= goal) {
            let part = select(0.5, (goal - cum) / c, c > 0.0);
            // bin b spans [(b - 0.5), (b + 0.5)] / (BINS - 1)
            return (f32(b) - 0.5 + part) / f32(BINS - 1u);
        }
        cum += c;
    }
    return f32(hi_b) / f32(BINS - 1u);
}

@compute @workgroup_size(1, 1, 1)
fn main() {
    let lo_b = u32(clamp(p.lo, 0.0, 1.0) * f32(BINS - 1u) + 0.5);
    let hi_b = u32(clamp(p.hi, 0.0, 1.0) * f32(BINS - 1u) + 0.5);
    let total_l = count_bins(0u, 0u, BINS);
    let total_r = count_bins(1u, 0u, BINS);
    // Clipped tails, per eye: bins below lo_b and above hi_b.
    let nbot_l = count_bins(0u, 0u, lo_b);
    let nbot_r = count_bins(1u, 0u, lo_b);
    let ntop_l = count_bins(0u, hi_b + 1u, BINS);
    let ntop_r = count_bins(1u, hi_b + 1u, BINS);
    st.count_l = total_l - nbot_l - ntop_l;
    st.count_r = total_r - nbot_r - ntop_r;

    var ok = false;
    var d_raw = 0.0;
    st.reason = 1u;
    if (total_l > 0u && total_r > 0u
        && f32(st.count_l) >= p.min_count && f32(st.count_r) >= p.min_count) {
        let f_bot = max(f32(nbot_l) / f32(total_l), f32(nbot_r) / f32(total_r));
        let f_top = max(f32(ntop_l) / f32(total_l), f32(ntop_r) / f32(total_r));
        let q_lo = f_bot + MARGIN;
        let q_hi = 1.0 - f_top - MARGIN;
        st.reason = 2u;
        if (q_hi - q_lo >= MIN_RANGE) {
            st.reason = 0u;
            var sum = 0.0;
            for (var k = 0u; k < NQ; k++) {
                let q = q_lo + (q_hi - q_lo) * (f32(k) + 0.5) / f32(NQ);
                sum += percentile(0u, total_l, q, 0u, BINS - 1u)
                     - percentile(1u, total_r, q, 0u, BINS - 1u);
            }
            d_raw = clamp(sum / f32(NQ), -p.max_abs, p.max_abs);
            ok = true;
        }
    }

    var d = st.d;
    if (p.reset != 0u || st.valid == 0u) {
        d = 0.0;
        st.valid = 0u;
    }
    if (ok) {
        if (st.valid == 0u || abs(d_raw - d) > p.snap) {
            d = d_raw;
        } else {
            d = d + p.alpha * (d_raw - d);
        }
        st.valid = 1u;
    }
    st.d = d;
    st.d_raw = d_raw;

    // d = L − R. Adding off_l to L and off_r to R must give L' = R'.
    switch p.mode {
        case 1u: { st.off_l = 0.0;      st.off_r = d;   }   // right follows left
        case 2u: { st.off_l = -d;       st.off_r = 0.0; }   // left follows right
        default: { st.off_l = -0.5 * d; st.off_r = 0.5 * d; }
    }
}
