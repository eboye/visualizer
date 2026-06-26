// Scene shader: a graded backdrop plus the spectrum drawn as thick, feathered,
// camera-facing line quads — wrapped on terrain or a globe. Renders into an HDR
// target; post.wgsl adds bloom and tone-mapping.

struct Uniforms {
    view_proj: mat4x4<f32>,
    accent: vec4<f32>, // rgb = accent, w = level
    grid: vec4<f32>,   // cols, rows, head, beat
    shape: vec4<f32>,  // depth, width_front, width_back, height_scale
    misc: vec4<f32>,   // time, globe?(>0.5), fade_near, fade_far
    res: vec4<f32>,    // width, height, line_thickness_px, _
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> heights: array<f32>;

const TAU: f32 = 6.2831853;
const PI: f32 = 3.1415927;
const GLOBE_R: f32 = 1.3;
const GLOBE_HEIGHT: f32 = 1.1;

// ---------------------------------------------------------------- grid → point

struct Pt {
    clip: vec4<f32>,
    height: f32,
    shade: f32, // brightness/opacity factor (edge taper, distance/far-side fade)
};

fn grid_point(vid: u32) -> Pt {
    let cols = u32(u.grid.x);
    let rows = u32(u.grid.y);
    let head = u32(u.grid.z);

    let col = vid % cols;
    let d = vid / cols;
    let phys = (head + rows - 1u - d) % rows;
    let h = heights[phys * cols + col];

    let fx = f32(col) / f32(cols - 1u) - 0.5;
    let fz = f32(d) / f32(rows - 1u);

    var p: Pt;
    if u.misc.y > 0.5 {
        // Globe: longitude = frequency, latitude = time, height bumps outward.
        let theta = (f32(col) / f32(cols - 1u)) * TAU;
        let phi = fz * PI;
        let radius = GLOBE_R + h * GLOBE_HEIGHT + u.grid.w * 0.08;
        let sp = sin(phi);
        let pos = vec3<f32>(radius * sp * cos(theta), radius * cos(phi), radius * sp * sin(theta));
        p.clip = u.view_proj * vec4<f32>(pos, 1.0);
        p.height = h;
        // Far hemisphere fades (clip.w is linear camera distance).
        p.shade = max(1.0 - smoothstep(u.misc.z, u.misc.w, p.clip.w), 0.04);
    } else {
        // Terrain: width frustum-matched, edges tapered, distance-faded.
        let width = mix(u.shape.y, u.shape.z, fz);
        let ax = abs(fx) * 2.0;
        let edge = 1.0 - smoothstep(0.78, 1.0, ax);
        let x = fx * width;
        let z = fz * u.shape.x;
        let y = h * u.shape.w * edge;
        p.clip = u.view_proj * vec4<f32>(x, y, z, 1.0);
        p.height = h * edge;
        p.shade = edge * (1.0 - 0.85 * fz);
    }
    return p;
}

// ---------------------------------------------------------------- line quads

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) cross: f32,  // -1..1 across the line width (for feathering)
    @location(1) height: f32,
    @location(2) shade: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, @location(0) seg: vec2<u32>) -> VsOut {
    let a = grid_point(seg.x);
    let b = grid_point(seg.y);

    // Two triangles → quad. Per-corner endpoint (0=a,1=b) and side (±1).
    var ends = array<u32, 6>(0u, 0u, 1u, 0u, 1u, 1u);
    var sides = array<f32, 6>(-1.0, 1.0, -1.0, 1.0, 1.0, -1.0);
    let end = ends[vid];
    let side = sides[vid];

    let res = u.res.xy;
    let wa = max(a.clip.w, 1e-4);
    let wb = max(b.clip.w, 1e-4);
    let pa = ((a.clip.xy / wa) * 0.5 + vec2<f32>(0.5)) * res; // pixel coords
    let pb = ((b.clip.xy / wb) * 0.5 + vec2<f32>(0.5)) * res;

    var dir = pb - pa;
    if length(dir) < 1e-4 {
        dir = vec2<f32>(1.0, 0.0);
    }
    dir = normalize(dir);
    let nrm = vec2<f32>(-dir.y, dir.x);

    // Expand the chosen endpoint perpendicular by half the line thickness.
    let pp = select(pa, pb, end == 1u);
    let corner = pp + nrm * side * (u.res.z * 0.5);
    let ndc = (corner / res) * 2.0 - vec2<f32>(1.0);

    var out: VsOut;
    out.clip = vec4<f32>(ndc, 0.0, 1.0);
    out.cross = side;
    out.height = select(a.height, b.height, end == 1u);
    out.shade = select(a.shade, b.shade, end == 1u);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let accent = u.accent.rgb;
    let beat = u.grid.w;
    let level = u.accent.w;

    // Soft edge across the line width.
    let feather = 1.0 - smoothstep(0.45, 1.0, abs(in.cross));
    let h = in.height;

    // HDR color: accent body + a hot (>1) white core on peaks so bloom blooms.
    var col = accent * (0.30 + 1.10 * h);
    col += vec3<f32>(1.0, 1.0, 1.0) * smoothstep(0.6, 1.0, h) * 0.8;
    col += accent * (beat * 0.45 + level * 0.15);

    let alpha = in.shade * feather;
    return vec4<f32>(col, alpha);
}

// ---------------------------------------------------------------- backdrop

struct BgOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_bg(@builtin(vertex_index) vi: u32) -> BgOut {
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let xy = p[vi];
    var out: BgOut;
    out.clip = vec4<f32>(xy, 0.0, 1.0);
    out.uv = xy * 0.5 + vec2<f32>(0.5);
    return out;
}

@fragment
fn fs_bg(in: BgOut) -> @location(0) vec4<f32> {
    // uv.y: 0 bottom .. 1 top (NDC y up). Subtle near-black vertical gradient.
    var c = mix(vec3<f32>(0.0, 0.0, 0.0), vec3<f32>(0.015, 0.016, 0.022), in.uv.y);
    // Faint accent glow low-center, where the action is.
    let glow = 1.0 - smoothstep(0.0, 0.75, distance(in.uv, vec2<f32>(0.5, 0.4)));
    c += u.accent.rgb * glow * 0.03;
    // Vignette darkens the corners.
    let vig = smoothstep(1.05, 0.35, distance(in.uv, vec2<f32>(0.5)));
    c *= mix(0.55, 1.0, vig);
    return vec4<f32>(c, 1.0);
}
