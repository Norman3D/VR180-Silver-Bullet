// Automatic eye-exposure matching, pass 3 of 3: apply one eye's offset.
//
// Adds the offset computed by `eye_match_reduce.wgsl` to every channel of
// one eye's texture. On log footage an additive code offset IS an
// exposure gain (the log curve is linear in stops), so this equalises the
// two sensors' exposure without touching colour; it runs as the FIRST
// stage of the colour stack, before the CDL and the LUT, exactly where
// the static "Matching Eyes" exposure trim acts.
//
// The 16-bit variant is produced at pipeline-creation time by the usual
// `rgba8unorm, write` → `rgba16unorm, write` replacement.

struct ApplyParams {
    eye: u32,       // 0 = left (uses off_l), 1 = right (uses off_r)
    _p0: u32,
    _p1: u32,
    _p2: u32,
};

struct EyeMatchState {
    off_l: f32,
    off_r: f32,
    d: f32,
    d_raw: f32,
    valid: u32,
    count_l: u32,
    count_r: u32,
    reason: u32,
};

@group(0) @binding(0) var in_tex:  texture_2d<f32>;
@group(0) @binding(1) var out_tex: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> u: ApplyParams;
@group(0) @binding(3) var<storage, read> st: EyeMatchState;

@compute @workgroup_size(8, 8, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(out_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) { return; }
    let px = textureLoad(in_tex, vec2<i32>(gid.xy), 0);
    let off = select(st.off_r, st.off_l, u.eye == 0u);
    let rgb = clamp(px.rgb + vec3<f32>(off), vec3<f32>(0.0), vec3<f32>(1.0));
    textureStore(out_tex, vec2<i32>(gid.xy), vec4<f32>(rgb, px.a));
}
