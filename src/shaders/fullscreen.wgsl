// Fullscreen visualizer shader.
//
// Vertex stage emits a single oversized triangle that covers the whole screen,
// so no vertex buffer is needed. Fragment stage draws the reactive visual using
// the audio features supplied in the uniform buffer.

struct Uniforms {
    // x = time (seconds), yz = resolution (px), w = beat (0..1, decays)
    time_res_beat: vec4<f32>,
    // x = bass, y = mid, z = treble, w = overall level (all roughly 0..1)
    bands: vec4<f32>,
    // rgb = accent color, w unused
    accent: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle trick: 3 verts covering clip space.
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var out: VsOut;
    let xy = p[vi];
    out.pos = vec4<f32>(xy, 0.0, 1.0);
    // uv in 0..1 with origin bottom-left.
    out.uv = xy * 0.5 + vec2<f32>(0.5, 0.5);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let time = u.time_res_beat.x;
    let res = u.time_res_beat.yz;
    let beat = u.time_res_beat.w;
    let bass = u.bands.x;
    let mid = u.bands.y;
    let treble = u.bands.z;

    // Centered, aspect-corrected coordinates in roughly [-1, 1].
    let aspect = res.x / max(res.y, 1.0);
    var pos = (in.uv - vec2<f32>(0.5, 0.5)) * 2.0;
    pos.x = pos.x * aspect;

    let r = length(pos);
    let ang = atan2(pos.y, pos.x);

    // --- Bass: a pulsing radial ring that swells with low end + beat kick.
    let pulse = 0.35 + bass * 0.5 + beat * 0.25;
    let ring = smoothstep(0.02, 0.0, abs(r - pulse) - 0.05 - bass * 0.1);

    // --- Mid: rotating petals; mid energy drives count brightness.
    let petals = 6.0;
    let swirl = sin(ang * petals + time * 1.5 + bass * 4.0);
    let petal = smoothstep(0.2, 1.0, swirl) * mid;

    // --- Treble: fine sparkle / high-frequency detail near the edges.
    let sparkle = sin(r * 60.0 - time * 8.0) * sin(ang * 40.0 + time * 3.0);
    let sparkleMask = smoothstep(0.4, 1.0, sparkle) * treble * smoothstep(0.2, 1.0, r);

    // Grayscale: total brightness of all the shapes on a black field.
    let intensity = clamp(ring + petal + sparkleMask * 1.5 + beat * 0.3, 0.0, 1.0);

    // Accent tint rises with bass + beat, so the scene reads as monochrome at
    // rest and washes into the accent color on the low end / kicks. The small
    // baseline keeps the accent's identity faintly present at all times.
    let accent = u.accent.rgb;
    let accentAmt = clamp(0.18 + bass * 0.5 + beat * 0.9, 0.0, 1.0);
    let tint = mix(vec3<f32>(1.0, 1.0, 1.0), accent, accentAmt);

    var col = tint * intensity;

    // Subtle dark vignette so the center pops.
    col = col * (1.0 - smoothstep(0.7, 1.6, r) * 0.6);

    return vec4<f32>(col, 1.0);
}
