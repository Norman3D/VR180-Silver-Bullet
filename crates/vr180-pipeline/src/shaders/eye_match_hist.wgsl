// Automatic eye-exposure matching, pass 1 of 3: per-eye luma histogram.
//
// Builds a 1024-bin histogram of BT.709 luma over the CENTRAL region of
// one eye's projected output (the region both eyes see; the periphery
// is excluded so black padding, the fisheye rim and parallax-only
// content never enter the statistic). The luma is of the values as
// stored in the texture — on log footage that is the log code, so a
// per-eye exposure (gain) difference is a pure SHIFT of this histogram
// (I-Log: ≈0.067 code per stop). `eye_match_reduce.wgsl` turns the two
// histograms into an offset per eye; `eye_offset_apply.wgsl` applies it.
//
// Each thread accumulates a 4×4 pixel patch into a workgroup-shared
// histogram; one flush of the non-empty bins per workgroup keeps global
// atomics low (≈1k per 64×64 block instead of one per pixel).
//
// Dispatch: ceil(region_w / 64) × ceil(region_h / 64) workgroups.

const BINS: u32 = 1024u;
const PATCH: u32 = 4u;

struct HistParams {
    x0: u32,        // region [x0, x1) × [y0, y1) in texels
    y0: u32,
    x1: u32,
    y1: u32,
    eye: u32,       // 0 = left, 1 = right → this eye's histogram lives at hist[eye * BINS ..]
    _p0: u32,
    _p1: u32,
    _p2: u32,
};

@group(0) @binding(0) var in_tex: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> hist: array<atomic<u32>>;   // 2 × BINS
@group(0) @binding(2) var<uniform> p: HistParams;

var<workgroup> local_hist: array<atomic<u32>, BINS>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>,
        @builtin(local_invocation_index) li: u32) {
    for (var i = li; i < BINS; i += 256u) {
        atomicStore(&local_hist[i], 0u);
    }
    workgroupBarrier();

    let base_x = p.x0 + gid.x * PATCH;
    let base_y = p.y0 + gid.y * PATCH;
    for (var dy = 0u; dy < PATCH; dy++) {
        let y = base_y + dy;
        if (y >= p.y1) { break; }
        for (var dx = 0u; dx < PATCH; dx++) {
            let x = base_x + dx;
            if (x >= p.x1) { break; }
            let c = textureLoad(in_tex, vec2<i32>(i32(x), i32(y)), 0).rgb;
            let luma = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
            let b = u32(clamp(luma, 0.0, 1.0) * f32(BINS - 1u) + 0.5);
            atomicAdd(&local_hist[b], 1u);
        }
    }
    workgroupBarrier();

    for (var i = li; i < BINS; i += 256u) {
        let v = atomicLoad(&local_hist[i]);
        if (v != 0u) {
            atomicAdd(&hist[p.eye * BINS + i], v);
        }
    }
}
