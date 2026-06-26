// Post-processing: bright-pass → separable Gaussian blur → composite with
// additive bloom, ACES tone-mapping, and a saturation/contrast grade.

// Tunables.
const BLOOM_THRESHOLD: f32 = 0.7;
const BLOOM_INTENSITY: f32 = 1.15;
const EXPOSURE: f32 = 1.15;
const SATURATION: f32 = 1.12;
const CONTRAST: f32 = 1.06;
const VIGNETTE: f32 = 0.18;

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex0: texture_2d<f32>; // scene (bright/blur input)
// Composite-only second texture (bloom). Declared in a separate bind group.

struct FsIn {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> FsIn {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = p[vi];
    var out: FsIn;
    out.clip = vec4<f32>(xy, 0.0, 1.0);
    // Flip Y so uv origin is top-left (matches texture sampling).
    out.uv = vec2<f32>(xy.x * 0.5 + 0.5, 1.0 - (xy.y * 0.5 + 0.5));
    return out;
}

// Bright-pass: keep only energy above the threshold (soft knee).
@fragment
fn fs_bright(in: FsIn) -> @location(0) vec4<f32> {
    let c = textureSample(tex0, samp, in.uv).rgb;
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    let k = max(luma - BLOOM_THRESHOLD, 0.0) / max(luma, 1e-4);
    return vec4<f32>(c * k, 1.0);
}

// Separable 9-tap Gaussian. Direction (in uv) comes from the uniform.
struct BlurDir { texel: vec4<f32> };
@group(0) @binding(2) var<uniform> blur: BlurDir;

@fragment
fn fs_blur(in: FsIn) -> @location(0) vec4<f32> {
    let d = blur.texel.xy;
    var acc = textureSample(tex0, samp, in.uv).rgb * 0.227027;
    let w1 = 0.1945946;
    let w2 = 0.1216216;
    let w3 = 0.0540541;
    let w4 = 0.0162162;
    acc += textureSample(tex0, samp, in.uv + d * 1.0).rgb * w1;
    acc += textureSample(tex0, samp, in.uv - d * 1.0).rgb * w1;
    acc += textureSample(tex0, samp, in.uv + d * 2.0).rgb * w2;
    acc += textureSample(tex0, samp, in.uv - d * 2.0).rgb * w2;
    acc += textureSample(tex0, samp, in.uv + d * 3.0).rgb * w3;
    acc += textureSample(tex0, samp, in.uv - d * 3.0).rgb * w3;
    acc += textureSample(tex0, samp, in.uv + d * 4.0).rgb * w4;
    acc += textureSample(tex0, samp, in.uv - d * 4.0).rgb * w4;
    return vec4<f32>(acc, 1.0);
}

// ---- Composite (own bind group: samp + scene + bloom; distinct bindings so
// all globals in this module stay unique) ----
@group(0) @binding(3) var scene_tex: texture_2d<f32>;
@group(0) @binding(4) var bloom_tex: texture_2d<f32>;

fn aces(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_composite(in: FsIn) -> @location(0) vec4<f32> {
    let scene = textureSample(scene_tex, samp, in.uv).rgb;
    let bloom = textureSample(bloom_tex, samp, in.uv).rgb;
    var hdr = (scene + bloom * BLOOM_INTENSITY) * EXPOSURE;

    // Tone-map, then grade.
    var c = aces(hdr);
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = mix(vec3<f32>(luma), c, SATURATION);
    c = (c - vec3<f32>(0.5)) * CONTRAST + vec3<f32>(0.5);
    c = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));

    // Gentle vignette.
    let vig = 1.0 - VIGNETTE * smoothstep(0.4, 1.2, distance(in.uv, vec2<f32>(0.5)));
    c *= vig;

    return vec4<f32>(c, 1.0);
}
