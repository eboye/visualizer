// 3D scrolling wireframe terrain.
//
// Vertices are an implicit COLS×ROWS grid — positions are computed from
// @builtin(vertex_index), no vertex buffer. Per-vertex height is read from a
// scrolling heightmap storage buffer (one new row per frame). A LineList index
// buffer (built on the CPU) connects neighbours into a cross-hatched mesh.

struct Uniforms {
    view_proj: mat4x4<f32>,
    accent: vec4<f32>, // rgb = accent color, w = level
    grid: vec4<f32>,   // cols, rows, head (ring write pointer), beat
    shape: vec4<f32>,  // depth, width_front, width_back, height_scale
    misc: vec4<f32>,   // time, _, _, _
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var<storage, read> heights: array<f32>;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) height: f32,
    @location(1) depth: f32, // 0 = front/newest .. 1 = far/oldest
    @location(2) edge: f32,  // 1 = center .. 0 = left/right edge
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    let cols = u32(u.grid.x);
    let rows = u32(u.grid.y);
    let head = u32(u.grid.z);

    let col = vi % cols;
    let d = vi / cols; // logical depth, 0 = front

    // Map logical depth to the physical ring row. The most recently written row
    // (head - 1) is the front edge; older rows recede into the distance.
    let phys = (head + rows - 1u - d) % rows;
    let h = heights[phys * cols + col];

    let fx = f32(col) / f32(cols - 1u) - 0.5; // -0.5 .. 0.5
    let fz = f32(d) / f32(rows - 1u);         //  0   .. 1

    // Width grows from front to back to match the camera frustum, so the grid
    // fills the viewport at every depth. Edges taper to zero for an infinite,
    // borderless feel (no hard left/right boundary).
    let width = mix(u.shape.y, u.shape.z, fz);
    let ax = abs(fx) * 2.0;                    // 0 center .. 1 edge
    let edge = 1.0 - smoothstep(0.78, 1.0, ax);

    let x = fx * width;
    let z = fz * u.shape.x;
    let y = h * u.shape.w * edge;

    var out: VsOut;
    out.clip = u.view_proj * vec4<f32>(x, y, z, 1.0);
    out.height = h * edge;
    out.depth = fz;
    out.edge = edge;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let accent = u.accent.rgb;
    let beat = u.grid.w;

    // Dim with distance so far ridges recede; brighten with height.
    let fade = 1.0 - 0.85 * in.depth;
    let bright = (0.25 + 0.85 * in.height) * fade;

    let level = u.accent.w;

    var col = accent * bright;
    // Crisp white-ish ridge tops + a beat lift, both faded by distance.
    col += vec3<f32>(1.0, 1.0, 1.0) * (smoothstep(0.55, 1.0, in.height) * 0.30 * fade);
    col += accent * (beat * 0.25 * fade);
    // Gentle global lift with overall energy so quiet passages aren't flat-dark.
    col += accent * (level * 0.12 * fade);
    // Dissolve the left/right edges so the terrain has no hard side boundary.
    col *= in.edge;

    return vec4<f32>(col, 1.0);
}
