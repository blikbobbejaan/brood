//! GPU render target for the GUI window (the `gui-gpu` feature), on wgpu.
//!
//! Replaces the CPU softbuffer present + per-pixel blit with two instanced-quad
//! pipelines. Every op becomes quads: a solid-fill op (Clear / Rect / FRect / VSpans /
//! Cells / CellsRgb / Quad, a text op's cell background, a cursor, an underline) a
//! coloured quad, a glyph or a `Sprite` a textured one. The quads are drawn in op order
//! in **batches** — a run of consecutive solids is one instanced draw, a run of textured
//! quads from the same texture another — so a frame costs a handful of draw calls
//! whatever its op count, and a sprite drawn after a rect lands on top of it, as the CPU
//! painter's op order promises. Glyphs share one texture (an atlas the rasterised
//! clusters are packed into as they first appear), so a page of text is one batch, not
//! one draw per glyph. The swapchain presents without vsync unless the window asked for
//! it, so a sim's frame rate is bounded by work, not the monitor refresh.
//!
//! The op walk mirrors the CPU painter's (`paint::render_ops`) arm for arm — the same
//! grid origin, the same scroll shift, the same region metrics and clip band, the same
//! cursor geometry — so a frame lands on the same pixels whichever target paints it. A
//! rounded or sub-cell rect is a signed-distance field in the fragment shader, the same
//! 1 px coverage ramp `fill_rrect` computes per pixel; a clip band is a per-instance rect
//! the fragment shader discards outside of. The one visible difference is text
//! anti-aliasing: the GPU samples one coverage per pixel (grey AA) where the CPU path
//! can render subpixel (LCD) text at 1×, and the contrast lift is not applied.
//!
//! This module is MECHANISM only. It knows a quad, a texture, a UV rect, a tint and an
//! angle; what a sprite sheet is, how a line is a thin quad at an angle, which frame an
//! animation is on, are Brood (`std/gui.blsp` and the engine above it).
//!
//! wgpu, not OpenGL: the runtime is meant to run on Windows and macOS as well as Linux,
//! and the GLES 3.0 context the first prototype requested exists on neither (CGL is
//! desktop-GL only and deprecated; Windows needs ANGLE). wgpu picks Vulkan / Metal / DX12
//! per platform behind one code path — and is what an in-browser build would draw with.

use std::collections::HashMap;
use std::rc::Rc;

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::Window;

use unicode_segmentation::UnicodeSegmentation;

use crate::host::gui::backend::{
    CURSOR_FG, Renderer, paint_stall_ms, snap_hairline, text_px_width, text_units, unit_cells,
};
use crate::host::gui::effects::{EmitterStep, FrameEffects};
use crate::host::gui::{Align, CursorStyle, MAX_PARTICLES, Op};
use crate::host::text_width::{TAB_WIDTH, cluster_cells, cluster_cells_at};

const DEFAULT_FG: [u8; 3] = [0xe5, 0xe5, 0xe5];
const DEFAULT_BG_RGB: [u8; 3] = [12, 12, 16];

/// The most a scroll region shifts its ops, in pixels — the CPU painter's cap, so a
/// runaway `dy_frac` moves a region off-screen rather than to infinity.
const MAX_SCROLL_PX: f32 = 16384.0;

/// Both pipelines. A quad is four vertices of a triangle strip generated from the vertex
/// index (no vertex buffer): `corner` is (0,0) (1,0) (0,1) (1,1), and every per-quad
/// value rides in the instance buffer. Pixel coordinates, top-left origin, mapped to NDC
/// through the viewport uniform — the CPU painter's coordinate contract. A quad turns
/// about its own centre by `rot` radians (clockwise on screen, since y points down).
/// Every quad carries a clip rect `[x0 y0 x1 y1]` the fragment stage discards outside
/// of — how a cell region's band and a scroll region's grid-top clip are honoured.
const SHADER_SRC: &str = r#"
struct Viewport { size: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> viewport: Viewport;

fn corner(vi: u32) -> vec2<f32> {
    return vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
}

// The screen position of a quad corner: the corner's offset from the quad's centre,
// rotated, plus the centre.
fn place(rect: vec4<f32>, c: vec2<f32>, rot: f32) -> vec2<f32> {
    let centre = rect.xy + rect.zw * 0.5;
    let offset = (c - vec2<f32>(0.5, 0.5)) * rect.zw;
    let s = sin(rot);
    let co = cos(rot);
    return centre + vec2<f32>(offset.x * co - offset.y * s, offset.x * s + offset.y * co);
}

fn to_ndc(px: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(px.x / viewport.size.x * 2.0 - 1.0,
                     1.0 - px.y / viewport.size.y * 2.0, 0.0, 1.0);
}

fn clipped(pos: vec2<f32>, clip: vec4<f32>) -> bool {
    return pos.x < clip.x || pos.y < clip.y || pos.x >= clip.z || pos.y >= clip.w;
}

// --- solid quads -------------------------------------------------------------------

struct SolidInst {
    @location(0) rect: vec4<f32>,   // x, y, w, h in pixels
    @location(1) color: vec4<f32>,  // straight rgba, 0..1
    @location(2) rot: f32,          // radians about the centre
    @location(3) radius: f32,       // corner radius in pixels (with `aa`)
    @location(4) aa: f32,           // 1 = SDF edge (rounded / fractional), 0 = plain fill
    @location(5) clip: vec4<f32>,   // x0, y0, x1, y1 in pixels
};
struct SolidOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local: vec2<f32>,  // the fragment's offset from the quad's top-left, px
    @location(2) @interpolate(flat) half: vec2<f32>,
    @location(3) @interpolate(flat) radius: f32,
    @location(4) @interpolate(flat) aa: f32,
    @location(5) @interpolate(flat) clip: vec4<f32>,
};

@vertex
fn vs_solid(@builtin(vertex_index) vi: u32, inst: SolidInst) -> SolidOut {
    var out: SolidOut;
    let c = corner(vi);
    out.pos = to_ndc(place(inst.rect, c, inst.rot));
    out.color = inst.color;
    out.local = c * inst.rect.zw;
    out.half = inst.rect.zw * 0.5;
    out.radius = inst.radius;
    out.aa = inst.aa;
    out.clip = inst.clip;
    return out;
}

@fragment
fn fs_solid(in: SolidOut) -> @location(0) vec4<f32> {
    if (clipped(in.pos.xy, in.clip)) {
        discard;
    }
    if (in.aa == 0.0) {
        return in.color;
    }
    // The rounded-box signed distance from the pixel centre to the rect's rounded core
    // (the rect inset by the radius), as `fill_rrect` computes it: a 1 px coverage ramp
    // at the edge, so corners and fractional edges read smooth.
    let p = in.local - in.half;
    let r = min(in.radius, min(in.half.x, in.half.y));
    let q = abs(p) - (in.half - vec2<f32>(r, r));
    let d = length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
    let coverage = clamp(0.5 - d, 0.0, 1.0);
    if (coverage <= 0.0) {
        discard;
    }
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}

// --- textured quads ------------------------------------------------------------------

struct TexInst {
    @location(0) rect: vec4<f32>,   // x, y, w, h in pixels
    @location(1) uv: vec4<f32>,     // u0, v0, du, dv in texture space (a negative extent mirrors)
    @location(2) tint: vec4<f32>,   // straight rgba, 0..1
    @location(3) mode: u32,         // 1 = coverage mask recoloured with tint, 0 = colour
    @location(4) rot: f32,          // radians about the centre
    @location(5) clip: vec4<f32>,   // x0, y0, x1, y1 in pixels
};
struct TexOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) tint: vec4<f32>,
    @location(2) @interpolate(flat) mode: u32,
    @location(3) @interpolate(flat) clip: vec4<f32>,
};

@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

@vertex
fn vs_tex(@builtin(vertex_index) vi: u32, inst: TexInst) -> TexOut {
    var out: TexOut;
    let c = corner(vi);
    out.pos = to_ndc(place(inst.rect, c, inst.rot));
    out.uv = inst.uv.xy + c * inst.uv.zw;
    out.tint = inst.tint;
    out.mode = inst.mode;
    out.clip = inst.clip;
    return out;
}

@fragment
fn fs_tex(in: TexOut) -> @location(0) vec4<f32> {
    if (clipped(in.pos.xy, in.clip)) {
        discard;
    }
    let t = textureSample(tex, samp, in.uv);
    if (in.mode == 1u) {
        return vec4<f32>(in.tint.rgb, t.a * in.tint.a);
    }
    return t * in.tint;
}
"#;

/// The effects' shaders (`effects.rs` holds the CPU painter's copy of every formula in
/// them): lights drawn additively into a light map, here; the passes over a whole
/// texture (`POST_SRC`); and the particles — a compute shader that spawns and steps them
/// (`PARTICLES_SRC`) and the instanced quads that draw them straight out of the same
/// buffer (`PARTICLE_DRAW_SRC`).
const LIGHT_SRC: &str = r#"
struct Viewport { size: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> viewport: Viewport;

fn corner(vi: u32) -> vec2<f32> {
    return vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
}

fn to_ndc(px: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(px.x / viewport.size.x * 2.0 - 1.0,
                     1.0 - px.y / viewport.size.y * 2.0, 0.0, 1.0);
}

// --- lights: a quad per light, its falloff added into the light map --------------------

struct LightInst {
    @location(0) at: vec4<f32>,     // x, y, radius, intensity
    @location(1) color: vec4<f32>,  // rgb 0..1
};
struct LightOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,  // -1..1 across the light's square
    @location(1) color: vec3<f32>,
};

@vertex
fn vs_light(@builtin(vertex_index) vi: u32, inst: LightInst) -> LightOut {
    var out: LightOut;
    let c = corner(vi);
    let r = inst.at.z;
    out.pos = to_ndc(inst.at.xy - vec2<f32>(r, r) + c * 2.0 * r);
    out.local = c * 2.0 - 1.0;
    out.color = inst.color.rgb * inst.at.w;
    return out;
}

// effects.rs `light_falloff`
@fragment
fn fs_light(in: LightOut) -> @location(0) vec4<f32> {
    let d = length(in.local);
    if (d >= 1.0) {
        discard;
    }
    let k = 1.0 - d * d;
    return vec4<f32>(in.color * k * k, 1.0);
}

"#;

/// The passes over a whole texture: the light map multiplied onto the scene, bloom's bright
/// pass and blur, and the final composite with the CRT.
const POST_SRC: &str = r#"
// --- passes over a whole texture -----------------------------------------------------

struct Post {
    threshold: f32, knee: f32, bloom: f32, curvature: f32,
    scanlines: f32, vignette: f32, step: vec2<f32>,
    sigma: f32, taps: f32, _pad: vec2<f32>,
};
@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var glow: texture_2d<f32>;
@group(0) @binding(2) var smooth_sampler: sampler;
@group(0) @binding(3) var<uniform> post: Post;

struct FullOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle that covers the target; uv 0..1 from the top-left.
@vertex
fn vs_full(@builtin(vertex_index) vi: u32) -> FullOut {
    var out: FullOut;
    let uv = vec2<f32>(f32((vi << 1u) & 2u), f32(vi & 2u));
    out.uv = uv;
    out.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return out;
}

// The light map, blended as source × destination onto the scene.
@fragment
fn fs_multiply(in: FullOut) -> @location(0) vec4<f32> {
    return vec4<f32>(min(textureSample(source, smooth_sampler, in.uv).rgb, vec3<f32>(1.0)), 1.0);
}

// effects.rs `apply_bloom`'s bright pass: the light above the threshold, soft-kneed.
@fragment
fn fs_bright(in: FullOut) -> @location(0) vec4<f32> {
    let c = textureSample(source, smooth_sampler, in.uv).rgb;
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    let k = clamp((luma - post.threshold) / post.knee, 0.0, 1.0);
    return vec4<f32>(c * k, 1.0);
}

// One axis of a gaussian blur, `post.step` apart (a texel along x, then along y).
@fragment
fn fs_blur(in: FullOut) -> @location(0) vec4<f32> {
    var sum = textureSample(source, smooth_sampler, in.uv).rgb;
    var weight = 1.0;
    let taps = i32(post.taps);
    for (var i = 1; i <= taps; i = i + 1) {
        let w = exp(-f32(i * i) / (2.0 * post.sigma * post.sigma));
        let off = post.step * f32(i);
        sum = sum + (textureSample(source, smooth_sampler, in.uv + off).rgb
                   + textureSample(source, smooth_sampler, in.uv - off).rgb) * w;
        weight = weight + 2.0 * w;
    }
    return vec4<f32>(sum / weight, 1.0);
}

// effects.rs `crt_uv`
fn crt_uv(uv: vec2<f32>, curvature: f32) -> vec2<f32> {
    let c = uv * 2.0 - 1.0;
    let b = vec2<f32>(c.x * (1.0 + curvature * c.y * c.y * 0.25),
                      c.y * (1.0 + curvature * c.x * c.x * 0.25));
    return b * 0.5 + 0.5;
}

// The scene (exact pixels), plus the bloom, through the CRT: effects.rs `apply_crt`.
@fragment
fn fs_final(in: FullOut) -> @location(0) vec4<f32> {
    let uv = crt_uv(in.uv, post.curvature);
    if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let dims = vec2<f32>(textureDimensions(source));
    let texel = vec2<i32>(min(uv * dims, dims - vec2<f32>(1.0)));
    var c = textureLoad(source, texel, 0).rgb;
    c = c + textureSample(glow, smooth_sampler, uv).rgb * post.bloom;
    let row = u32(in.pos.y);
    var shade = 1.0;
    if (row % 2u == 1u) {
        shade = 1.0 - post.scanlines;
    }
    let d = in.uv - vec2<f32>(0.5);
    shade = shade * clamp(1.0 - post.vignette * dot(d, d) * 2.0, 0.0, 1.0);
    return vec4<f32>(min(c, vec3<f32>(1.0)) * shade, 1.0);
}
"#;

/// The particle compute shader and the particle quads. Separate from `EFFECTS_SRC`
/// because the compute entry point's bindings (a writable buffer) are not allowed in a
/// vertex stage.
const PARTICLES_SRC: &str = r#"
struct Particle { pos: vec2<f32>, vel: vec2<f32>, age: f32, life: f32, _pad: vec2<f32> };
struct Params {
    origin: vec2<f32>, gravity: vec2<f32>,
    speed: vec2<f32>, life: vec2<f32>,
    size: vec2<f32>, angle: f32, spread: f32,
    color: vec4<f32>, color_end: vec4<f32>,
    dt: f32, drag: f32, spawn_first: u32, spawn_count: u32,
    capacity: u32, seed: u32, square: u32, _pad: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read_write> particles: array<Particle>;

// effects.rs `hash` (lowbias32) and `rand`.
fn hash(x: u32) -> u32 {
    var v = x;
    v = v ^ (v >> 16u);
    v = v * 0x7feb352du;
    v = v ^ (v >> 15u);
    v = v * 0x846ca68bu;
    v = v ^ (v >> 16u);
    return v;
}
fn rand(seed: u32, slot: u32, k: u32) -> f32 {
    return f32(hash(seed ^ hash(slot * 4u + k)) >> 8u) / 16777216.0;
}

// effects.rs `spawn` then `integrate`.
@compute @workgroup_size(64)
fn cs_step(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.capacity) {
        return;
    }
    var p = particles[i];
    let rel = (i + params.capacity - params.spawn_first) % params.capacity;
    if (rel < params.spawn_count) {
        let angle = params.angle + (rand(params.seed, i, 0u) - 0.5) * params.spread;
        let speed = params.speed.x + (params.speed.y - params.speed.x) * rand(params.seed, i, 1u);
        let life = params.life.x + (params.life.y - params.life.x) * rand(params.seed, i, 2u);
        p.pos = params.origin;
        p.vel = vec2<f32>(cos(angle), sin(angle)) * speed;
        p.age = 0.0;
        p.life = max(life, 0.001);
    }
    if (p.life > 0.0 && p.age < p.life) {
        let keep = max(1.0 - params.drag * params.dt, 0.0);
        p.vel = (p.vel + params.gravity * params.dt) * keep;
        p.pos = p.pos + p.vel * params.dt;
        p.age = p.age + params.dt;
    }
    particles[i] = p;
}
"#;

const PARTICLE_DRAW_SRC: &str = r#"
struct Viewport { size: vec2<f32>, _pad: vec2<f32> };
@group(0) @binding(0) var<uniform> viewport: Viewport;
struct Params {
    origin: vec2<f32>, gravity: vec2<f32>,
    speed: vec2<f32>, life: vec2<f32>,
    size: vec2<f32>, angle: f32, spread: f32,
    color: vec4<f32>, color_end: vec4<f32>,
    dt: f32, drag: f32, spawn_first: u32, spawn_count: u32,
    capacity: u32, seed: u32, square: u32, _pad: u32,
};
@group(1) @binding(0) var<uniform> params: Params;

struct ParticleInst {
    @location(0) pos: vec2<f32>,
    @location(1) age_life: vec2<f32>,
};
struct ParticleOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_particle(@builtin(vertex_index) vi: u32, inst: ParticleInst) -> ParticleOut {
    var out: ParticleOut;
    let age = inst.age_life.x;
    let life = inst.age_life.y;
    if (life <= 0.0 || age >= life) {
        // a free slot: a quad with no area, nothing rasterised
        out.pos = vec4<f32>(0.0, 0.0, 0.0, 1.0);
        out.local = vec2<f32>(0.0);
        out.color = vec4<f32>(0.0);
        return out;
    }
    let t = clamp(age / life, 0.0, 1.0);
    let size = mix(params.size.x, params.size.y, t);
    let c = vec2<f32>(f32(vi & 1u), f32(vi >> 1u));
    let px = inst.pos + (c - vec2<f32>(0.5)) * size;
    out.pos = vec4<f32>(px.x / viewport.size.x * 2.0 - 1.0,
                        1.0 - px.y / viewport.size.y * 2.0, 0.0, 1.0);
    out.local = c - vec2<f32>(0.5);
    out.color = mix(params.color, params.color_end, t);
    return out;
}

// effects.rs `particle_coverage`
@fragment
fn fs_particle(in: ParticleOut) -> @location(0) vec4<f32> {
    var coverage = 1.0;
    if (params.square == 0u) {
        let edge = clamp(1.0 - 2.0 * length(in.local), 0.0, 1.0);
        coverage = edge * edge;
    }
    let a = in.color.a * coverage;
    if (a <= 0.0) {
        discard;
    }
    return vec4<f32>(in.color.rgb, a);
}
"#;

/// The uniform an emitter's compute step and its quads read (`Params` in the shaders).
const PARTICLE_PARAMS_BYTES: u64 = 112;
/// One light instance: x, y, radius, intensity, then rgb and a pad.
const LIGHT_FLOATS: usize = 8;
/// The `Post` uniform: twelve floats, padded to 48 bytes.
const POST_BYTES: u64 = 48;

/// One solid instance: rect (4) + rgba (4) + rot (1) + radius (1) + aa (1) + clip (4).
const SOLID_FLOATS: usize = 15;
/// One textured instance: rect (4) + uv (4) + tint (4) + mode (1 u32 as its bits) + rot (1)
/// + clip (4).
const TEX_WORDS: usize = 18;

/// The glyph atlas page size. 2048² RGBA is 16 MB — a few thousand cells' worth of
/// glyphs at an editor size, and every device wgpu runs on allows at least 8192².
const ATLAS_SIZE: u32 = 2048;

/// Which texture a run of textured quads samples: a glyph-atlas page, or a texture the
/// app uploaded for its sprites.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TexKey {
    Atlas(usize),
    User(u32),
}

/// A texture the app uploaded, bound for the textured pipeline.
struct GpuTexture {
    bind_group: wgpu::BindGroup,
}

/// One page of the glyph atlas: the texture plus a shelf packer's cursor. Glyphs are
/// packed left to right along a shelf whose height is the tallest glyph on it; a glyph
/// that does not fit starts a new shelf below, and one that does not fit the page starts
/// a new page. Simple, and enough: every glyph of one font size is about the same height.
struct AtlasPage {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    next_x: u32,
    next_y: u32,
    shelf_h: u32,
}

/// Where a rasterised cluster sits in the atlas: page + pixel rect + whether it is a
/// coverage mask (`mode` 1 in the shader) rather than a colour bitmap.
#[derive(Clone, Copy)]
struct AtlasGlyph {
    page: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    mono: bool,
}

/// A growable instance buffer: re-created (never mapped) when a frame outgrows it.
struct InstanceBuffer {
    buffer: wgpu::Buffer,
    capacity_bytes: u64,
}

impl InstanceBuffer {
    fn new(device: &wgpu::Device, label: &str, capacity_bytes: u64) -> InstanceBuffer {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity_bytes,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        InstanceBuffer {
            buffer,
            capacity_bytes,
        }
    }

    /// Upload `data`, growing (doubling) the buffer first when it does not fit.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, label: &str, data: &[u8]) {
        let needed = data.len() as u64;
        if needed > self.capacity_bytes {
            let mut capacity = self.capacity_bytes.max(1);
            while capacity < needed {
                capacity *= 2;
            }
            *self = InstanceBuffer::new(device, label, capacity);
        }
        if !data.is_empty() {
            queue.write_buffer(&self.buffer, 0, data);
        }
    }
}

/// A run of consecutive quads drawn with one pipeline state: `count` instances starting
/// at `first` in that pipeline's instance buffer.
enum Batch {
    Solid {
        first: u32,
        count: u32,
    },
    Tex {
        key: TexKey,
        first: u32,
        count: u32,
    },
    /// Emitter `id`'s particles, straight out of its buffer.
    Particles {
        id: u32,
    },
}

/// A solid quad's edge treatment: a plain fill, or the SDF ramp (with a corner radius in
/// pixels) a rounded or sub-cell rect wants.
#[derive(Clone, Copy)]
enum Edge {
    Plain,
    Rounded(f32),
}

/// The frame under construction: both instance buffers' contents and the batch list, in
/// op order. Solids and textured quads keep separate buffers (different strides) and the
/// batch list says in which order to draw runs of each. `clip` is the rect every quad
/// pushed while it is set is clipped to — the whole viewport outside any region.
struct FrameBuilder {
    solids: Vec<f32>,
    texs: Vec<u32>,
    batches: Vec<Batch>,
    fw: f32,
    fh: f32,
    clip: [f32; 4],
    /// The batch the top-level `Overlay` starts at: no batch merges across it, so the scene
    /// and the unlit layer draw as separate runs.
    overlay_at: Option<usize>,
    /// How many regions deep the walk is (an `Overlay` counts only at the top).
    nested: u32,
}

impl FrameBuilder {
    fn new(fw: f32, fh: f32) -> FrameBuilder {
        FrameBuilder {
            solids: Vec::new(),
            texs: Vec::new(),
            batches: Vec::new(),
            fw,
            fh,
            clip: [0.0, 0.0, fw, fh],
            overlay_at: None,
            nested: 0,
        }
    }

    /// Whether a quad can put a pixel on screen and inside the clip. A rotated quad is
    /// judged by the disc its rotation sweeps, so a long thin quad turned across the
    /// viewport's corner is kept rather than culled by its unrotated rect.
    fn visible(&self, x: f32, y: f32, w: f32, h: f32, rot: f32) -> bool {
        let [cx0, cy0, cx1, cy1] = self.clip;
        if cx1 <= cx0 || cy1 <= cy0 {
            return false;
        }
        if rot == 0.0 {
            return quad_visible(x, y, w, h, self.fw, self.fh)
                && x < cx1
                && y < cy1
                && x + w > cx0
                && y + h > cy0;
        }
        let radius = (w * w + h * h).sqrt() * 0.5;
        let (cx, cy) = (x + w * 0.5, y + h * 0.5);
        quad_visible(
            cx - radius,
            cy - radius,
            radius * 2.0,
            radius * 2.0,
            self.fw,
            self.fh,
        )
    }

    /// A solid quad; `rgba[3]` is its alpha.
    fn solid(&mut self, x: f32, y: f32, w: f32, h: f32, rgba: [u8; 4], rot: f32, edge: Edge) {
        if !self.visible(x, y, w, h, rot) {
            return;
        }
        let (radius, aa) = match edge {
            Edge::Plain => (0.0, 0.0),
            Edge::Rounded(r) => (r.max(0.0), 1.0),
        };
        let index = (self.solids.len() / SOLID_FLOATS) as u32;
        self.solids.extend_from_slice(&[
            x,
            y,
            w,
            h,
            rgba[0] as f32 / 255.0,
            rgba[1] as f32 / 255.0,
            rgba[2] as f32 / 255.0,
            rgba[3] as f32 / 255.0,
            rot,
            radius,
            aa,
            self.clip[0],
            self.clip[1],
            self.clip[2],
            self.clip[3],
        ]);
        let merge = self.can_merge();
        match self.batches.last_mut() {
            Some(Batch::Solid { count, .. }) if merge => *count += 1,
            _ => self.batches.push(Batch::Solid {
                first: index,
                count: 1,
            }),
        }
    }

    /// Whether a quad may join the last batch: not when that batch ends the scene.
    fn can_merge(&self) -> bool {
        self.overlay_at.is_none_or(|at| at < self.batches.len())
    }

    /// A plain opaque solid — the common cell-aligned fill.
    fn fill(&mut self, x: f32, y: f32, w: f32, h: f32, rgb: [u8; 3]) {
        self.solid(x, y, w, h, [rgb[0], rgb[1], rgb[2], 255], 0.0, Edge::Plain);
    }

    /// A textured quad sampling `uv` (`[u0 v0 du dv]`, texture space) of `key`.
    #[allow(clippy::too_many_arguments)]
    fn textured(
        &mut self,
        key: TexKey,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        uv: [f32; 4],
        tint: [u8; 4],
        mono: bool,
        rot: f32,
    ) {
        if !self.visible(x, y, w, h, rot) {
            return;
        }
        let index = (self.texs.len() / TEX_WORDS) as u32;
        self.texs.extend_from_slice(&[
            x.to_bits(),
            y.to_bits(),
            w.to_bits(),
            h.to_bits(),
            uv[0].to_bits(),
            uv[1].to_bits(),
            uv[2].to_bits(),
            uv[3].to_bits(),
            (tint[0] as f32 / 255.0).to_bits(),
            (tint[1] as f32 / 255.0).to_bits(),
            (tint[2] as f32 / 255.0).to_bits(),
            (tint[3] as f32 / 255.0).to_bits(),
            u32::from(mono),
            rot.to_bits(),
            self.clip[0].to_bits(),
            self.clip[1].to_bits(),
            self.clip[2].to_bits(),
            self.clip[3].to_bits(),
        ]);
        let merge = self.can_merge();
        match self.batches.last_mut() {
            Some(Batch::Tex { key: k, count, .. }) if *k == key && merge => *count += 1,
            _ => self.batches.push(Batch::Tex {
                key,
                first: index,
                count: 1,
            }),
        }
    }
}

/// A render pipeline over one shader module's entry points, the parts every pipeline here
/// shares spelled once.
#[allow(clippy::too_many_arguments)]
fn pipeline(
    device: &wgpu::Device,
    label: &str,
    module: &wgpu::ShaderModule,
    (vertex, fragment): (&str, &str),
    layouts: &[&wgpu::BindGroupLayout],
    instances: Option<wgpu::VertexBufferLayout>,
    format: wgpu::TextureFormat,
    blend: Option<wgpu::BlendState>,
    topology: wgpu::PrimitiveTopology,
) -> wgpu::RenderPipeline {
    let layouts: Vec<Option<&wgpu::BindGroupLayout>> = layouts.iter().map(|l| Some(*l)).collect();
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &layouts,
        immediate_size: 0,
    });
    let buffers = [instances];
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some(vertex),
            compilation_options: Default::default(),
            buffers: if buffers[0].is_some() { &buffers } else { &[] },
        },
        primitive: wgpu::PrimitiveState {
            topology,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Colour added onto what is there: a light into the light map.
const ADD: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::One,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent::OVER,
};

/// Colour × alpha added onto what is there: an additive particle.
const ADD_ALPHA: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::SrcAlpha,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent::OVER,
};

/// What is there × the colour drawn: the light map onto the scene.
const MULTIPLY: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Dst,
        dst_factor: wgpu::BlendFactor::Zero,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
};

/// The lighting and post-processing pipelines, made the first time a frame asks for an
/// effect, and the frame-sized textures they render through.
struct EffectsGpu {
    light_pipeline: wgpu::RenderPipeline,
    multiply_pipeline: wgpu::RenderPipeline,
    bright_pipeline: wgpu::RenderPipeline,
    blur_pipeline: wgpu::RenderPipeline,
    final_pipeline: wgpu::RenderPipeline,
    post_layout: wgpu::BindGroupLayout,
    smooth_sampler: wgpu::Sampler,
    light_instances: InstanceBuffer,
    /// One `Post` uniform per pass that reads one: bright, blur across, blur down, final.
    post_bright: wgpu::Buffer,
    post_across: wgpu::Buffer,
    post_down: wgpu::Buffer,
    post_final: wgpu::Buffer,
    targets: Option<EffectTargets>,
}

/// The textures an effect frame renders through, at one frame size: the scene, the light
/// map, and bloom's two half-size ping-pong targets — with each pass's bind group.
struct EffectTargets {
    size: (u32, u32),
    scene: wgpu::TextureView,
    light: wgpu::TextureView,
    bloom_a: wgpu::TextureView,
    bloom_b: wgpu::TextureView,
    half: (u32, u32),
    multiply: wgpu::BindGroup,
    bright: wgpu::BindGroup,
    across: wgpu::BindGroup,
    down: wgpu::BindGroup,
    composite: wgpu::BindGroup,
}

impl EffectsGpu {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        viewport_layout: &wgpu::BindGroupLayout,
    ) -> EffectsGpu {
        let light_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("brood gui lights"),
            source: wgpu::ShaderSource::Wgsl(LIGHT_SRC.into()),
        });
        let post_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("brood gui post"),
            source: wgpu::ShaderSource::Wgsl(POST_SRC.into()),
        });
        let texture_entry = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let post_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let strip = wgpu::PrimitiveTopology::TriangleStrip;
        let list = wgpu::PrimitiveTopology::TriangleList;
        let light_pipeline = pipeline(
            device,
            "lights",
            &light_module,
            ("vs_light", "fs_light"),
            &[viewport_layout],
            Some(wgpu::VertexBufferLayout {
                array_stride: (LIGHT_FLOATS * 4) as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4],
            }),
            format,
            Some(ADD),
            strip,
        );
        let full = |label: &str, fragment: &str, blend: Option<wgpu::BlendState>| {
            pipeline(
                device,
                label,
                &post_module,
                ("vs_full", fragment),
                &[&post_layout],
                None,
                format,
                blend,
                list,
            )
        };
        let uniform = |label: &str| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: POST_BYTES,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        EffectsGpu {
            light_pipeline,
            multiply_pipeline: full("light map", "fs_multiply", Some(MULTIPLY)),
            bright_pipeline: full("bloom bright pass", "fs_bright", None),
            blur_pipeline: full("bloom blur", "fs_blur", None),
            final_pipeline: full("final composite", "fs_final", None),
            smooth_sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("linear"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            }),
            post_layout,
            light_instances: InstanceBuffer::new(device, "light instances", 4 * 1024),
            post_bright: uniform("post: bright"),
            post_across: uniform("post: blur across"),
            post_down: uniform("post: blur down"),
            post_final: uniform("post: final"),
            targets: None,
        }
    }

    /// The textures for a `w`×`h` frame, made (or remade, on a resize) as needed.
    fn targets(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        w: u32,
        h: u32,
    ) -> &EffectTargets {
        if self.targets.as_ref().is_none_or(|t| t.size != (w, h)) {
            let view = |label: &str, w: u32, h: u32| {
                device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some(label),
                        size: wgpu::Extent3d {
                            width: w,
                            height: h,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    })
                    .create_view(&wgpu::TextureViewDescriptor::default())
            };
            let half = ((w / 2).max(1), (h / 2).max(1));
            let scene = view("scene", w, h);
            let light = view("light map", w, h);
            let bloom_a = view("bloom a", half.0, half.1);
            let bloom_b = view("bloom b", half.0, half.1);
            // the `glow` of every pass but the final one, which reads no glow: a pass may not
            // bind a texture it renders into
            let none = view("no glow", 1, 1);
            let group = |label: &str,
                         source: &wgpu::TextureView,
                         glow: &wgpu::TextureView,
                         post: &wgpu::Buffer| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(label),
                    layout: &self.post_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(source),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(glow),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.smooth_sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: post.as_entire_binding(),
                        },
                    ],
                })
            };
            let targets = EffectTargets {
                size: (w, h),
                multiply: group("light map", &light, &none, &self.post_final),
                bright: group("bright pass", &scene, &none, &self.post_bright),
                across: group("blur across", &bloom_a, &none, &self.post_across),
                down: group("blur down", &bloom_b, &none, &self.post_down),
                composite: group("final", &scene, &bloom_a, &self.post_final),
                scene,
                light,
                bloom_a,
                bloom_b,
                half,
            };
            self.targets = Some(targets);
        }
        self.targets.as_ref().expect("made above")
    }
}

/// The particle pipelines — the compute step and the two ways of drawing a particle —
/// made the first time a frame carries an emitter, and each live emitter's buffers.
struct ParticlesGpu {
    step_pipeline: wgpu::ComputePipeline,
    step_layout: wgpu::BindGroupLayout,
    draw_layout: wgpu::BindGroupLayout,
    blend_pipeline: wgpu::RenderPipeline,
    add_pipeline: wgpu::RenderPipeline,
    emitters: HashMap<u32, GpuEmitter>,
}

/// One emitter on the device: its ring of particles (`capacity` × 32 bytes, read by the
/// compute step and drawn as instances), and the `Params` uniform both read.
struct GpuEmitter {
    capacity: u32,
    particles: wgpu::Buffer,
    params: wgpu::Buffer,
    step_group: wgpu::BindGroup,
    draw_group: wgpu::BindGroup,
    additive: bool,
}

impl ParticlesGpu {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        viewport_layout: &wgpu::BindGroupLayout,
    ) -> ParticlesGpu {
        let step_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("brood gui particle step"),
            source: wgpu::ShaderSource::Wgsl(PARTICLES_SRC.into()),
        });
        let draw_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("brood gui particles"),
            source: wgpu::ShaderSource::Wgsl(PARTICLE_DRAW_SRC.into()),
        });
        let uniform_entry = |visibility| wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let step_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("particle step"),
            entries: &[
                uniform_entry(wgpu::ShaderStages::COMPUTE),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("particle draw"),
            entries: &[uniform_entry(
                wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            )],
        });
        let step_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("particle step"),
            bind_group_layouts: &[Some(&step_layout)],
            immediate_size: 0,
        });
        let step_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("particle step"),
            layout: Some(&step_pipeline_layout),
            module: &step_module,
            entry_point: Some("cs_step"),
            compilation_options: Default::default(),
            cache: None,
        });
        let instances = || wgpu::VertexBufferLayout {
            array_stride: (crate::host::gui::effects::PARTICLE_FLOATS * 4) as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 0,
                    shader_location: 0,
                },
                wgpu::VertexAttribute {
                    format: wgpu::VertexFormat::Float32x2,
                    offset: 16,
                    shader_location: 1,
                },
            ],
        };
        let draw = |label: &str, blend| {
            pipeline(
                device,
                label,
                &draw_module,
                ("vs_particle", "fs_particle"),
                &[viewport_layout, &draw_layout],
                Some(instances()),
                format,
                Some(blend),
                wgpu::PrimitiveTopology::TriangleStrip,
            )
        };
        ParticlesGpu {
            blend_pipeline: draw("particles", wgpu::BlendState::ALPHA_BLENDING),
            add_pipeline: draw("additive particles", ADD_ALPHA),
            step_pipeline,
            step_layout,
            draw_layout,
            emitters: HashMap::new(),
        }
    }

    /// Match the device's emitters to this paint's steps — a new one gets its buffers, a
    /// new capacity a new ring, a finished one is dropped — and write each step's uniform.
    fn sync(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, steps: &[EmitterStep]) {
        self.emitters
            .retain(|id, _| steps.iter().any(|step| step.id == *id));
        for step in steps {
            let capacity = step.spec.max.clamp(1, MAX_PARTICLES);
            if self
                .emitters
                .get(&step.id)
                .is_none_or(|e| e.capacity != capacity)
            {
                let emitter = self.emitter(device, capacity);
                self.emitters.insert(step.id, emitter);
            }
            let emitter = self.emitters.get_mut(&step.id).expect("inserted above");
            emitter.additive = step.spec.additive;
            queue.write_buffer(
                &emitter.params,
                0,
                as_bytes_u32(&params_words(step, capacity)),
            );
        }
    }

    fn emitter(&self, device: &wgpu::Device, capacity: u32) -> GpuEmitter {
        // zero-initialised by wgpu: every slot starts free (`life` 0)
        let particles = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("particles"),
            size: capacity as u64 * (crate::host::gui::effects::PARTICLE_FLOATS * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("particle params"),
            size: PARTICLE_PARAMS_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let step_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("particle step"),
            layout: &self.step_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: particles.as_entire_binding(),
                },
            ],
        });
        let draw_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("particle draw"),
            layout: &self.draw_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: params.as_entire_binding(),
            }],
        });
        GpuEmitter {
            capacity,
            particles,
            params,
            step_group,
            draw_group,
            additive: false,
        }
    }
}

/// A step as the shaders' `Params` uniform, word by word in its declared order.
fn params_words(step: &EmitterStep, capacity: u32) -> [u32; 28] {
    let s = &step.spec;
    let f = |x: f32| x.to_bits();
    let channel = |c: [u8; 4], i: usize| f(c[i] as f32 / 255.0);
    [
        f(step.x),
        f(step.y),
        f(s.gravity[0]),
        f(s.gravity[1]),
        f(s.speed[0]),
        f(s.speed[1]),
        f(s.life[0]),
        f(s.life[1]),
        f(s.size[0]),
        f(s.size[1]),
        f(s.angle),
        f(s.spread),
        channel(s.color, 0),
        channel(s.color, 1),
        channel(s.color, 2),
        channel(s.color, 3),
        channel(s.color_end, 0),
        channel(s.color_end, 1),
        channel(s.color_end, 2),
        channel(s.color_end, 3),
        f(step.dt),
        f(s.drag),
        step.spawn_first,
        step.spawn_count,
        capacity,
        step.seed,
        u32::from(s.square),
        0,
    ]
}

/// Where the ops being expanded land: the grid origin and cell size in effect (a
/// `CellRegion` installs its own), the scroll shift an enclosing `ScrollRegion` applies,
/// and the clip rect to restore when a region ends. The CPU painter threads the same
/// values through `render_ops`.
#[derive(Clone, Copy)]
struct Grid {
    ox: f32,
    oy: f32,
    cw: f32,
    ch: f32,
    scroll_dy: f32,
}

/// One open window's GPU state: the wgpu surface + device, the two instanced-quad
/// pipelines, the glyph atlas and the app's textures. Lives on the GUI thread. Field
/// order matters: the surface was created from the window's raw handles, so it must
/// drop before the window — Rust drops fields in declaration order.
pub struct GpuWindow {
    surface: wgpu::Surface<'static>,
    window: Rc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    configured: (u32, u32),
    viewport_buffer: wgpu::Buffer,
    viewport_bind_group: wgpu::BindGroup,
    solid_pipeline: wgpu::RenderPipeline,
    solid_instances: InstanceBuffer,
    tex_pipeline: wgpu::RenderPipeline,
    tex_layout: wgpu::BindGroupLayout,
    tex_instances: InstanceBuffer,
    sampler: wgpu::Sampler,
    atlas: Vec<AtlasPage>,
    /// Cluster+face+px key → its atlas slot; `None` for a cluster that rasterised to
    /// nothing (or was too big for a page), remembered so it is not retried each frame.
    glyphs: HashMap<u64, Option<AtlasGlyph>>,
    textures: HashMap<u32, GpuTexture>,
    viewport_layout: wgpu::BindGroupLayout,
    /// Lighting and post-processing, made the first time a frame asks for either.
    effects: Option<EffectsGpu>,
    /// The particle systems, made the first time a frame carries an emitter.
    particles: Option<ParticlesGpu>,
}

/// Whether a quad at `(x, y)` sized `w x h` could put any pixel inside a `fw x fh`
/// viewport.
///
/// The GPU would clip an off-screen quad away for free, but this path **buffers** every
/// quad before drawing — unlike the CPU painter, whose fills clip against the framebuffer
/// and cost nothing when off-screen. `Op::Cells` pushes one quad per live bit of a
/// caller-supplied bitboard, so without a cull a board far larger than the window grows
/// the instance buffer (60 bytes a quad) without bound instead of drawing nothing.
///
/// Written as a **positive** test on purpose: a frame is ordinary Brood data, so a
/// coordinate can be NaN, and every comparison against NaN is false. Phrased this way
/// that culls the quad; phrased as a negation it would buffer it.
fn quad_visible(x: f32, y: f32, w: f32, h: f32, fw: f32, fh: f32) -> bool {
    w > 0.0 && h > 0.0 && x + w > 0.0 && y + h > 0.0 && x < fw && y < fh
}

/// Pick the swapchain format. A **non-sRGB** 8-bit format is preferred so the shader's
/// raw colour bytes land on screen as given — the same bytes the CPU painter writes —
/// rather than being re-encoded as if they were linear light. Every desktop backend
/// offers one; the first supported format is the fallback when none does.
fn pick_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    formats
        .iter()
        .copied()
        .find(|f| {
            matches!(
                f,
                wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
            )
        })
        .or_else(|| formats.first().copied())
}

/// The present mode: with `vsync`, FIFO (always available, and IS vsync). Without, the
/// platform's immediate/mailbox mode when it has one, else FIFO — the only mode some
/// compositors offer, so then the choice is theirs, not ours.
fn pick_present_mode(modes: &[wgpu::PresentMode], vsync: bool) -> wgpu::PresentMode {
    if vsync {
        wgpu::PresentMode::Fifo
    } else if modes.contains(&wgpu::PresentMode::Immediate) {
        wgpu::PresentMode::Immediate
    } else if modes.contains(&wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    }
}

/// A scroll region's shift in pixels for a `dy_frac` of the cell height, capped — the
/// CPU painter's `scroll_px`.
fn scroll_px(dy_frac: f32, ch: f32) -> f32 {
    (dy_frac * ch).round().clamp(-MAX_SCROLL_PX, MAX_SCROLL_PX)
}

/// The cursor's quads for one cell: `(x, y, w, h, colour)` each. Block is a 50% white
/// overlay plus a solid rim `t` thick; Bar a caret on the left edge; Underline a rule
/// along the bottom — the CPU `cursor_cell`'s geometry, thickness scaled with the cell so
/// it stays proportional on HiDPI (never under 2 px).
fn cursor_quads(
    left: f32,
    top: f32,
    cw: f32,
    ch: f32,
    style: CursorStyle,
) -> Vec<(f32, f32, f32, f32, [u8; 4])> {
    let fg = [CURSOR_FG[0], CURSOR_FG[1], CURSOR_FG[2], 255];
    match style {
        CursorStyle::Block => {
            let t = (cw / 10.0).floor().max(2.0);
            vec![
                (left, top, cw, ch, [255, 255, 255, 128]),
                (left, top, t, ch, fg),
                (left + cw - t, top, t, ch, fg),
                (left, top, cw, t, fg),
                (left, top + ch - t, cw, t, fg),
            ]
        }
        CursorStyle::Bar => vec![(left, top, (cw / 8.0).floor().max(2.0), ch, fg)],
        CursorStyle::Underline => {
            let t = (ch / 10.0).floor().max(2.0);
            vec![(left, top + ch - t, cw, t, fg)]
        }
    }
}

impl GpuWindow {
    pub fn new(window: Rc<Window>, vsync: bool) -> Result<GpuWindow, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let raw_display_handle = window
            .display_handle()
            .map_err(|e| format!("display handle: {e}"))?
            .as_raw();
        let raw_window_handle = window
            .window_handle()
            .map_err(|e| format!("window handle: {e}"))?
            .as_raw();
        // SAFETY: the handles stay valid for as long as `window` lives, and `GpuWindow`
        // holds the `Rc<Window>` and declares `surface` before it, so the surface is
        // dropped first.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw_display_handle),
                raw_window_handle,
            })
        }
        .map_err(|e| format!("gpu surface: {e}"))?;

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
            compatible_surface: Some(&surface),
        }))
        .map_err(|e| format!("gpu adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("brood gui"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| format!("gpu device: {e}"))?;
        // The driver is loaded now: shut the GUI thread's gate before its exit handler runs.
        crate::host::gui::backend::close_at_exit();

        let caps = surface.get_capabilities(&adapter);
        let format = pick_format(&caps.formats).ok_or("gpu surface: no supported format")?;
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | if dump_path().is_some() {
                    wgpu::TextureUsages::COPY_SRC
                } else {
                    wgpu::TextureUsages::empty()
                },
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: w,
            height: h,
            present_mode: pick_present_mode(&caps.present_modes, vsync),
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("brood gui quads"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let viewport_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("viewport"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewport"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("viewport"),
            layout: &viewport_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            }],
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Nearest: a glyph is drawn at the size it was rasterised, pixel for pixel, and
        // a sprite scaled up stays crisp (pixel art) rather than blurring.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nearest"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let blend = Some(wgpu::BlendState::ALPHA_BLENDING);
        let target = [Some(wgpu::ColorTargetState {
            format,
            blend,
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let primitive = wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        };

        let solid_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("solid"),
            bind_group_layouts: &[Some(&viewport_layout)],
            immediate_size: 0,
        });
        let solid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("solid quads"),
            layout: Some(&solid_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_solid"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: (SOLID_FLOATS * 4) as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4, 1 => Float32x4, 2 => Float32, 3 => Float32,
                        4 => Float32, 5 => Float32x4
                    ],
                })],
            },
            primitive,
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_solid"),
                compilation_options: Default::default(),
                targets: &target,
            }),
            multiview_mask: None,
            cache: None,
        });

        let tex_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("textured"),
            bind_group_layouts: &[Some(&viewport_layout), Some(&tex_layout)],
            immediate_size: 0,
        });
        let tex_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("textured quads"),
            layout: Some(&tex_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_tex"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: (TEX_WORDS * 4) as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Uint32,
                        4 => Float32, 5 => Float32x4
                    ],
                })],
            },
            primitive,
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_tex"),
                compilation_options: Default::default(),
                targets: &target,
            }),
            multiview_mask: None,
            cache: None,
        });

        let solid_instances = InstanceBuffer::new(&device, "solid instances", 64 * 1024);
        let tex_instances = InstanceBuffer::new(&device, "textured instances", 64 * 1024);

        Ok(GpuWindow {
            surface,
            window,
            device,
            queue,
            config,
            configured: (w, h),
            viewport_buffer,
            viewport_bind_group,
            solid_pipeline,
            solid_instances,
            tex_pipeline,
            tex_layout,
            tex_instances,
            sampler,
            atlas: Vec::new(),
            glyphs: HashMap::new(),
            textures: HashMap::new(),
            viewport_layout,
            effects: None,
            particles: None,
        })
    }

    /// Match the swapchain to the current window size (called before each paint).
    pub fn resize(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        if self.configured != (w, h) {
            self.config.width = w;
            self.config.height = h;
            self.surface.configure(&self.device, &self.config);
            self.configured = (w, h);
        }
    }

    /// An empty `w`×`h` RGBA8 texture on the device, with the bind group the textured
    /// pipeline samples it through.
    fn create_texture(&self, w: u32, h: u32) -> (wgpu::Texture, wgpu::BindGroup) {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        (texture, bind_group)
    }

    /// Write a straight-alpha RGBA bitmap into `texture` at `(x, y)`.
    fn write_rgba(&self, texture: &wgpu::Texture, x: u32, y: u32, rgba: &[u8], w: u32, h: u32) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }

    /// `gui-texture`: keep `rgba` (`w*h*4` bytes) on the device as texture `tex`. A
    /// re-upload under a live handle replaces it; a size that does not match the bytes
    /// is dropped (the Brood side validates, this is the backstop).
    pub fn upload_texture(&mut self, tex: u32, rgba: &[u8], w: u32, h: u32) {
        if w == 0 || h == 0 || rgba.len() as u64 != w as u64 * h as u64 * 4 {
            return;
        }
        let (texture, bind_group) = self.create_texture(w, h);
        self.write_rgba(&texture, 0, 0, rgba, w, h);
        self.textures.insert(tex, GpuTexture { bind_group });
    }

    /// `gui-texture-free`: drop texture `tex` (a later `:sprite` naming it draws nothing).
    pub fn free_texture(&mut self, tex: u32) {
        self.textures.remove(&tex);
    }

    /// Pack a rasterised `w`×`h` cluster into the atlas, opening a new shelf or page as
    /// needed, and return where it landed. `None` for a glyph larger than a page.
    fn pack_glyph(&mut self, rgba: &[u8], w: u32, h: u32, mono: bool) -> Option<AtlasGlyph> {
        if w == 0 || h == 0 || w > ATLAS_SIZE || h > ATLAS_SIZE {
            return None;
        }
        let mut page_index = self.atlas.len().saturating_sub(1);
        loop {
            if page_index >= self.atlas.len() {
                let (texture, bind_group) = self.create_texture(ATLAS_SIZE, ATLAS_SIZE);
                self.atlas.push(AtlasPage {
                    texture,
                    bind_group,
                    next_x: 0,
                    next_y: 0,
                    shelf_h: 0,
                });
            }
            let page = &mut self.atlas[page_index];
            if page.next_x + w > ATLAS_SIZE {
                page.next_y += page.shelf_h;
                page.next_x = 0;
                page.shelf_h = 0;
            }
            if page.next_y + h > ATLAS_SIZE {
                page_index += 1;
                continue;
            }
            let (x, y) = (page.next_x, page.next_y);
            page.next_x += w;
            page.shelf_h = page.shelf_h.max(h);
            let texture = &self.atlas[page_index].texture;
            self.write_rgba(texture, x, y, rgba, w, h);
            return Some(AtlasGlyph {
                page: page_index,
                x,
                y,
                w,
                h,
                mono,
            });
        }
    }

    /// The atlas slot for cluster `g` under a face at the renderer's current metrics,
    /// rasterising and packing it on first sight. `None` when it has no pixels.
    fn glyph_slot(
        &mut self,
        renderer: &mut Renderer,
        g: &str,
        family: Option<u32>,
        bold: bool,
        italic: bool,
        scale: u16,
    ) -> Option<AtlasGlyph> {
        let key = glyph_key(
            g,
            family,
            bold,
            italic,
            scale,
            renderer.metrics().px,
            renderer.font_generation(),
        );
        if let Some(slot) = self.glyphs.get(&key) {
            return *slot;
        }
        let cg = renderer.cluster_glyph(g, family, bold, italic, scale);
        let (rgba, w, h, color) = (cg.rgba.clone(), cg.width as u32, cg.height as u32, cg.color);
        let packed = self.pack_glyph(&rgba, w, h, !color);
        self.glyphs.insert(key, packed);
        packed
    }

    /// Expand `ops` into quads on `fb`, in order, on grid `g` — the CPU painter's
    /// `render_ops`, arm for arm. Recursion is the two region ops: a scroll region
    /// re-enters with its shift (and clips at the grid top, as the CPU path clips every
    /// shifted op there); a cell region re-enters with its own metrics, origin and clip
    /// band, restoring the renderer's metrics after.
    fn expand(&mut self, fb: &mut FrameBuilder, renderer: &mut Renderer, ops: &[Op], g: Grid) {
        let atlas_size = ATLAS_SIZE as f32;
        let scale_factor = renderer.scale() as f32;
        for op in ops {
            match op {
                // The frame was cleared to the background before any op.
                Op::Clear => {}
                Op::ScrollRegion { dy_frac, ops } => {
                    let saved_clip = fb.clip;
                    // A shifted op is clipped at the grid origin, never painted into the
                    // inset above it — the CPU path's `clip_skip`.
                    fb.clip[1] = fb.clip[1].max(g.oy);
                    let inner = Grid {
                        scroll_dy: scroll_px(*dy_frac, g.ch),
                        ..g
                    };
                    fb.nested += 1;
                    self.expand(fb, renderer, ops, inner);
                    fb.nested -= 1;
                    fb.clip = saved_clip;
                }
                Op::CellRegion {
                    x, y, h, px, ops, ..
                } => {
                    // The rect is in the PARENT's cells (and moves with an enclosing
                    // scroll); everything inside is on the region's own grid from its
                    // top-left. The clip band is narrowed to the rect's rows, so an op that
                    // overruns the region vertically is clipped rather than painted over
                    // its neighbour. Width is not clipped (the CPU bands rows, not columns).
                    let region = renderer.metrics_at(*px);
                    if region.cell_w == 0 || region.cell_h == 0 {
                        continue;
                    }
                    let top = g.oy + *y as f32 * g.ch - g.scroll_dy;
                    let bottom = top + *h as f32 * g.ch;
                    let saved_clip = fb.clip;
                    fb.clip[1] = fb.clip[1].max(top).max(0.0);
                    fb.clip[3] = fb.clip[3].min(bottom);
                    if fb.clip[3] > fb.clip[1] {
                        let saved = renderer.metrics();
                        renderer.set_metrics(region);
                        let inner = Grid {
                            ox: g.ox + *x as f32 * g.cw,
                            oy: top.max(0.0),
                            cw: region.cell_w as f32,
                            ch: region.cell_h as f32,
                            scroll_dy: 0.0,
                        };
                        fb.nested += 1;
                        self.expand(fb, renderer, ops, inner);
                        fb.nested -= 1;
                        renderer.set_metrics(saved);
                    }
                    fb.clip = saved_clip;
                }
                // Text: the cell BACKGROUNDS as solid quads (a coloured Life cell is a
                // space + `:bg`) — all of the op's cells first, so they form one solid
                // batch — then a textured quad per non-space cluster out of the atlas,
                // which together form one textured batch, then any underline.
                Op::Text { row, col, s, face } => {
                    let (mut fg, mut bg) = (
                        face.fg.unwrap_or(DEFAULT_FG),
                        face.bg.unwrap_or(DEFAULT_BG_RGB),
                    );
                    // A face with no `:bg` is transparent: the glyph composites over
                    // whatever is under it (an hl-line band), exactly like Emacs.
                    let mut paint_bg = face.bg.is_some();
                    if face.reverse {
                        std::mem::swap(&mut fg, &mut bg);
                        paint_bg = true;
                    }
                    let scale = face.scale.max(1) as usize;
                    let ch_s = scale as f32 * g.ch;
                    let top = g.oy + *row as f32 * g.ch - g.scroll_dy;
                    let mut clusters: Vec<(f32, f32, &str)> = Vec::new();
                    let mut cx = *col as usize;
                    // Ligature units when `gui-ligatures!` is on: a punctuation run is one
                    // glyph canvas spanning its cells (`text_units`, ADR-432).
                    for cluster in text_units(s, renderer.ligatures()) {
                        // A raw tab advances to the next SCREEN stop, background only.
                        if cluster == "\t" {
                            let cells = cluster_cells_at(cluster, cx, TAB_WIDTH);
                            if paint_bg {
                                fb.fill(
                                    g.ox + cx as f32 * g.cw,
                                    top,
                                    cells as f32 * g.cw,
                                    ch_s,
                                    bg,
                                );
                            }
                            cx += cells;
                            continue;
                        }
                        let cells = unit_cells(cluster);
                        if cells == 0 {
                            continue;
                        }
                        let block_w = (cells * scale) as f32 * g.cw;
                        let left = g.ox + cx as f32 * g.cw;
                        if paint_bg {
                            fb.fill(left, top, block_w, ch_s, bg);
                        }
                        if cluster != " " {
                            clusters.push((left, block_w, cluster));
                        }
                        cx += cells * scale;
                    }
                    for (left, _, cluster) in &clusters {
                        let Some(glyph) = self.glyph_slot(
                            renderer,
                            cluster,
                            face.family,
                            face.bold,
                            face.italic,
                            face.scale,
                        ) else {
                            continue;
                        };
                        fb.textured(
                            TexKey::Atlas(glyph.page),
                            *left,
                            top,
                            glyph.w as f32,
                            glyph.h as f32,
                            [
                                glyph.x as f32 / atlas_size,
                                glyph.y as f32 / atlas_size,
                                glyph.w as f32 / atlas_size,
                                glyph.h as f32 / atlas_size,
                            ],
                            [fg[0], fg[1], fg[2], 255],
                            glyph.mono,
                            0.0,
                        );
                    }
                    if face.underline {
                        // A rule near the block bottom in the text colour, scaled with
                        // the glyph so it stays proportional.
                        let uy = top + ch_s - 2.0 * scale as f32;
                        for (left, block_w, _) in &clusters {
                            fb.fill(*left, uy, *block_w, scale as f32, fg);
                        }
                    }
                }
                Op::Rect {
                    row,
                    col,
                    w,
                    h,
                    face,
                    radius,
                } => {
                    let bg = if face.reverse { face.fg } else { face.bg };
                    if let Some(bg) = bg {
                        let (x, y, pw, ph) = (
                            g.ox + *col as f32 * g.cw,
                            g.oy + *row as f32 * g.ch - g.scroll_dy,
                            *w as f32 * g.cw,
                            *h as f32 * g.ch,
                        );
                        if *radius > 0.0 {
                            fb.solid(
                                x,
                                y,
                                pw,
                                ph,
                                [bg[0], bg[1], bg[2], 255],
                                0.0,
                                Edge::Rounded(*radius * g.cw),
                            );
                        } else {
                            fb.fill(x, y, pw, ph, bg);
                        }
                    }
                }
                Op::FRect {
                    x,
                    y,
                    w,
                    h,
                    face,
                    opacity,
                    radius,
                } => {
                    // Sub-cell rect: cell-unit floats → px via the same origin and
                    // metrics every op shares, then an AA, alpha-blended fill. A hairline
                    // snaps to whole device pixels first, as on the CPU path.
                    let bg = if face.reverse { face.fg } else { face.bg };
                    if let Some(bg) = bg {
                        let (fx, fw, fy, fh, r) = snap_hairline(
                            g.ox + *x * g.cw,
                            *w * g.cw,
                            g.oy + *y * g.ch - g.scroll_dy,
                            *h * g.ch,
                            *radius * g.cw,
                            scale_factor,
                        );
                        let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
                        fb.solid(
                            fx,
                            fy,
                            fw,
                            fh,
                            [bg[0], bg[1], bg[2], alpha],
                            0.0,
                            Edge::Rounded(r),
                        );
                    }
                }
                Op::Cursor { row, col, style } => {
                    let left = g.ox + *col as f32 * g.cw;
                    let top = g.oy + *row as f32 * g.ch - g.scroll_dy;
                    for (x, y, w, h, rgba) in cursor_quads(left, top, g.cw, g.ch, *style) {
                        fb.solid(x, y, w, h, rgba, 0.0, Edge::Plain);
                    }
                }
                // Hover metadata, hit-tested in the event loop (ADR-080); nothing to draw.
                Op::CursorZone { .. } => {}
                // Played when the frame arrived (`UserEvent::Draw`); nothing to draw.
                Op::Sound { .. } | Op::Music { .. } => {}
                // Read off the whole frame by `paint` (`FrameEffects`); nothing to expand.
                Op::Ambient { .. }
                | Op::Light { .. }
                | Op::Bloom { .. }
                | Op::Crt { .. }
                | Op::Burst { .. } => {}
                Op::Overlay => {
                    if fb.nested == 0 && fb.overlay_at.is_none() {
                        fb.overlay_at = Some(fb.batches.len());
                    }
                }
                // Drawn in op order, out of the emitter's own buffer.
                Op::Emitter { id, .. } => fb.batches.push(Batch::Particles { id: *id }),
                Op::VSpans { row0, col0, cols } => {
                    let top0 = g.oy + *row0 as f32 * g.ch - g.scroll_dy;
                    for (i, segs) in cols.iter().enumerate() {
                        let left = g.ox + (*col0 as usize + i) as f32 * g.cw;
                        let mut y = top0;
                        for (sh, color) in segs {
                            let span_h = *sh as f32 * g.ch;
                            if let Some(rgb) = color {
                                fb.fill(left, y, g.cw, span_h, *rgb);
                            }
                            y += span_h;
                        }
                    }
                }
                Op::Cells {
                    row0,
                    col0,
                    w,
                    aspect,
                    bytes,
                    color,
                } => {
                    if let Some(rgb) = color {
                        let asp = (*aspect).max(1) as usize;
                        let cell_w = (asp as f32) * g.cw;
                        let wmod = (*w).max(1) as usize;
                        for (bi, &byte) in bytes.iter().enumerate() {
                            let mut b = byte;
                            let base = bi * 8;
                            while b != 0 {
                                let bit = base + b.trailing_zeros() as usize;
                                let x = (bit % wmod) as f32;
                                let y = (bit / wmod) as f32;
                                fb.fill(
                                    g.ox + (*col0 as f32 + x * asp as f32) * g.cw,
                                    g.oy + (*row0 as f32 + y) * g.ch - g.scroll_dy,
                                    cell_w,
                                    g.ch,
                                    *rgb,
                                );
                                b &= b - 1;
                            }
                        }
                    }
                }
                Op::CellsRgb {
                    row0,
                    col0,
                    w,
                    aspect,
                    bytes,
                    colors,
                    default,
                } => {
                    let asp = (*aspect).max(1) as usize;
                    let cell_w = (asp as f32) * g.cw;
                    let wmod = (*w).max(1) as usize;
                    for (bi, &byte) in bytes.iter().enumerate() {
                        let mut b = byte;
                        let base = bi * 8;
                        while b != 0 {
                            let bit = base + b.trailing_zeros() as usize;
                            let rgb = colors.get(&(bit as u64)).copied().unwrap_or(*default);
                            let x = (bit % wmod) as f32;
                            let y = (bit / wmod) as f32;
                            fb.fill(
                                g.ox + (*col0 as f32 + x * asp as f32) * g.cw,
                                g.oy + (*row0 as f32 + y) * g.ch - g.scroll_dy,
                                cell_w,
                                g.ch,
                                rgb,
                            );
                            b &= b - 1;
                        }
                    }
                }
                // Pixel space: window coordinates as given, untouched by grid or scroll.
                Op::Sprite {
                    tex,
                    x,
                    y,
                    w,
                    h,
                    uv,
                    tint,
                    rot,
                } => {
                    if self.textures.contains_key(tex) {
                        fb.textured(TexKey::User(*tex), *x, *y, *w, *h, *uv, *tint, false, *rot);
                    }
                }
                Op::Quad {
                    x,
                    y,
                    w,
                    h,
                    color,
                    rot,
                } => {
                    fb.solid(*x, *y, *w, *h, *color, *rot, Edge::Plain);
                }
                // Pixel-space text: the `Text` walk from a pixel top-left (after the
                // alignment's shift by the run's shaped width), untouched by grid or scroll.
                Op::TextPx {
                    x,
                    y,
                    s,
                    face,
                    align,
                } => {
                    let (mut fg, mut bg) = (
                        face.fg.unwrap_or(DEFAULT_FG),
                        face.bg.unwrap_or(DEFAULT_BG_RGB),
                    );
                    let mut paint_bg = face.bg.is_some();
                    if face.reverse {
                        std::mem::swap(&mut fg, &mut bg);
                        paint_bg = true;
                    }
                    let scale = face.scale.max(1) as usize;
                    let ch_s = scale as f32 * g.ch;
                    let width = text_px_width(s, g.cw as usize, scale) as f32;
                    let left0 = match align {
                        Align::Left => *x,
                        Align::Center => *x - width / 2.0,
                        Align::Right => *x - width,
                    };
                    let top = *y;
                    let mut clusters: Vec<(f32, f32, &str)> = Vec::new();
                    let mut cx = 0.0f32;
                    for cluster in s.graphemes(true) {
                        let cells = cluster_cells(cluster);
                        if cells == 0 {
                            continue;
                        }
                        let block_w = (cells * scale) as f32 * g.cw;
                        let left = left0 + cx;
                        if paint_bg {
                            fb.fill(left, top, block_w, ch_s, bg);
                        }
                        if cluster != " " {
                            clusters.push((left, block_w, cluster));
                        }
                        cx += block_w;
                    }
                    for (left, _, cluster) in &clusters {
                        let Some(glyph) = self.glyph_slot(
                            renderer,
                            cluster,
                            face.family,
                            face.bold,
                            face.italic,
                            face.scale,
                        ) else {
                            continue;
                        };
                        fb.textured(
                            TexKey::Atlas(glyph.page),
                            *left,
                            top,
                            glyph.w as f32,
                            glyph.h as f32,
                            [
                                glyph.x as f32 / atlas_size,
                                glyph.y as f32 / atlas_size,
                                glyph.w as f32 / atlas_size,
                                glyph.h as f32 / atlas_size,
                            ],
                            [fg[0], fg[1], fg[2], 255],
                            glyph.mono,
                            0.0,
                        );
                    }
                    if face.underline {
                        let uy = top + ch_s - 2.0 * scale as f32;
                        for (left, block_w, _) in &clusters {
                            fb.fill(*left, uy, *block_w, scale as f32, fg);
                        }
                    }
                }
            }
        }
    }

    /// Draw one frame: expand the ops to quads in op order, upload, draw the batches,
    /// present. The cell pixel size and the grid origin come from the renderer — the
    /// same coordinate contract the CPU `paint` uses, so cell ops land on the same pixels
    /// whichever target paints them.
    pub(crate) fn paint(&mut self, frame: &[Op], renderer: &mut Renderer) {
        // `BROOD_GUI_TRACE=1` (or a `BROOD_STALL_MS` threshold): the same per-paint line the
        // CPU painter prints, split into this target's phases. Without it the GPU path —
        // the default in a `--with-gui-gpu` build — could not be measured at all.
        let trace = paint_stall_ms().map(|ms| (ms, std::time::Instant::now()));
        let (cw, ch) = (renderer.cell_w.max(1), renderer.cell_h.max(1));
        let size = self.window.inner_size();
        let (fw, fh) = (size.width.max(1), size.height.max(1));
        self.resize(fw, fh);

        let (fwf, fhf) = (fw as f32, fh as f32);
        let mut fb = FrameBuilder::new(fwf, fhf);
        // The grid origin, the one the CPU painter and the mouse hit-test use, so a frame
        // lands on the same pixels whichever target paints it.
        let (ox, oy) = renderer.grid_origin(fw as usize, fh as usize);
        let grid = Grid {
            ox: ox as f32,
            oy: oy as f32,
            cw: cw as f32,
            ch: ch as f32,
            scroll_dy: 0.0,
        };
        self.expand(&mut fb, renderer, frame, grid);
        // The particles step once per paint, as the CPU painter's do; an emitter the frame
        // no longer carries still draws its last particles, at the end of the scene.
        let steps = renderer.emitters.step(frame);
        let fx = FrameEffects::of(frame);
        let mut split = fb.overlay_at.unwrap_or(fb.batches.len());
        let lingering: Vec<Batch> = steps
            .iter()
            .filter(|step| !step.present)
            .map(|step| Batch::Particles { id: step.id })
            .collect();
        split += lingering.len();
        fb.batches
            .splice(split - lingering.len()..split - lingering.len(), lingering);
        let t_expand = std::time::Instant::now();

        self.queue
            .write_buffer(&self.viewport_buffer, 0, as_bytes(&[fwf, fhf, 0.0, 0.0]));
        self.solid_instances.upload(
            &self.device,
            &self.queue,
            "solid instances",
            as_bytes(&fb.solids),
        );
        self.tex_instances.upload(
            &self.device,
            &self.queue,
            "textured instances",
            as_bytes_u32(&fb.texs),
        );
        if !steps.is_empty() {
            let format = self.config.format;
            self.particles
                .get_or_insert_with(|| {
                    ParticlesGpu::new(&self.device, format, &self.viewport_layout)
                })
                .sync(&self.device, &self.queue, &steps);
        }
        if fx.any() {
            self.prepare_effects(&fx, fw, fh);
        }
        let t_upload = std::time::Instant::now();

        let frame_texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                // The swapchain went stale (a resize the OS has not told winit about yet,
                // a display change): reconfigure and skip this frame; the next paint draws.
                self.surface.configure(&self.device, &self.config);
                return;
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return,
        };
        // acquiring the next swapchain image is where a vsync'd present waits for the display
        let t_acquire = std::time::Instant::now();
        let view = frame_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let bg = renderer.bg();
        let background = wgpu::Color {
            r: bg[0] as f64 / 255.0,
            g: bg[1] as f64 / 255.0,
            b: bg[2] as f64 / 255.0,
            a: 1.0,
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        if let Some(particles) = &self.particles {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("particle step"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&particles.step_pipeline);
            for emitter in particles.emitters.values() {
                pass.set_bind_group(0, &emitter.step_group, &[]);
                pass.dispatch_workgroups(emitter.capacity.div_ceil(64), 1, 1);
            }
        }
        match self.effects.as_ref().and_then(|e| e.targets.as_ref()) {
            Some(targets) if fx.any() => {
                let effects = self.effects.as_ref().expect("targets are effects'");
                // the scene, lit, then the overlay: all into the scene texture
                self.draw_pass(
                    &mut encoder,
                    &targets.scene,
                    Some(background),
                    &fb.batches[..split],
                );
                if let Some(ambient) = fx.ambient {
                    let ambient = wgpu::Color {
                        r: ambient[0] as f64 / 255.0,
                        g: ambient[1] as f64 / 255.0,
                        b: ambient[2] as f64 / 255.0,
                        a: 1.0,
                    };
                    let mut pass =
                        render_pass(&mut encoder, "light map", &targets.light, Some(ambient));
                    pass.set_pipeline(&effects.light_pipeline);
                    pass.set_bind_group(0, &self.viewport_bind_group, &[]);
                    pass.set_vertex_buffer(0, effects.light_instances.buffer.slice(..));
                    pass.draw(0..4, 0..fx.lights.len() as u32);
                    drop(pass);
                    let mut pass = render_pass(&mut encoder, "lighting", &targets.scene, None);
                    pass.set_pipeline(&effects.multiply_pipeline);
                    pass.set_bind_group(0, &targets.multiply, &[]);
                    pass.draw(0..3, 0..1);
                }
                self.draw_pass(&mut encoder, &targets.scene, None, &fb.batches[split..]);
                if fx.bloom.is_some() {
                    for (label, target, group, pipeline) in [
                        (
                            "bloom bright pass",
                            &targets.bloom_a,
                            &targets.bright,
                            &effects.bright_pipeline,
                        ),
                        (
                            "bloom blur across",
                            &targets.bloom_b,
                            &targets.across,
                            &effects.blur_pipeline,
                        ),
                        (
                            "bloom blur down",
                            &targets.bloom_a,
                            &targets.down,
                            &effects.blur_pipeline,
                        ),
                    ] {
                        let mut pass =
                            render_pass(&mut encoder, label, target, Some(wgpu::Color::BLACK));
                        pass.set_pipeline(pipeline);
                        pass.set_bind_group(0, group, &[]);
                        pass.draw(0..3, 0..1);
                    }
                }
                let mut pass = render_pass(&mut encoder, "final", &view, Some(wgpu::Color::BLACK));
                pass.set_pipeline(&effects.final_pipeline);
                pass.set_bind_group(0, &targets.composite, &[]);
                pass.draw(0..3, 0..1);
            }
            _ => self.draw_pass(&mut encoder, &view, Some(background), &fb.batches),
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        let t_submit = std::time::Instant::now();
        if let Some(path) = dump_path() {
            dump_frame(
                &self.device,
                &self.queue,
                &frame_texture.texture,
                self.config.format,
                fw,
                fh,
                path,
            );
        }
        self.queue.present(frame_texture);
        if let Some((ms, t0)) = trace {
            let el = t0.elapsed();
            if el.as_millis() >= ms {
                let us =
                    |a: std::time::Instant, b: std::time::Instant| b.duration_since(a).as_micros();
                eprintln!(
                    "[gui-paint-gpu] {}us: fb={fw}x{fh} ops={} batches={} | expand={}us upload={}us \
                     acquire={}us submit={}us present={}us",
                    el.as_micros(),
                    frame.len(),
                    fb.batches.len(),
                    us(t0, t_expand),
                    us(t_expand, t_upload),
                    us(t_upload, t_acquire),
                    us(t_acquire, t_submit),
                    t_submit.elapsed().as_micros(),
                );
            }
        }
    }
}

impl GpuWindow {
    /// Draw `batches` in order into `view` — cleared to `clear` first, or drawn over what
    /// is there.
    fn draw_pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        clear: Option<wgpu::Color>,
        batches: &[Batch],
    ) {
        let mut pass = render_pass(encoder, "frame", view, clear);
        pass.set_bind_group(0, &self.viewport_bind_group, &[]);
        for batch in batches {
            match batch {
                Batch::Solid { first, count } => {
                    pass.set_pipeline(&self.solid_pipeline);
                    pass.set_vertex_buffer(0, self.solid_instances.buffer.slice(..));
                    pass.draw(0..4, *first..first + count);
                }
                Batch::Tex { key, first, count } => {
                    let bind_group = match key {
                        TexKey::Atlas(page) => self.atlas.get(*page).map(|p| &p.bind_group),
                        TexKey::User(tex) => self.textures.get(tex).map(|t| &t.bind_group),
                    };
                    let Some(bind_group) = bind_group else {
                        continue;
                    };
                    pass.set_pipeline(&self.tex_pipeline);
                    pass.set_vertex_buffer(0, self.tex_instances.buffer.slice(..));
                    pass.set_bind_group(1, bind_group, &[]);
                    pass.draw(0..4, *first..first + count);
                }
                Batch::Particles { id } => {
                    let Some(particles) = &self.particles else {
                        continue;
                    };
                    let Some(emitter) = particles.emitters.get(id) else {
                        continue;
                    };
                    pass.set_pipeline(if emitter.additive {
                        &particles.add_pipeline
                    } else {
                        &particles.blend_pipeline
                    });
                    pass.set_vertex_buffer(0, emitter.particles.slice(..));
                    pass.set_bind_group(1, &emitter.draw_group, &[]);
                    pass.draw(0..4, 0..emitter.capacity);
                }
            }
        }
    }

    /// Ready the effect passes for a `w`×`h` frame: the pipelines and targets, the lights'
    /// instances, and each pass's uniform.
    fn prepare_effects(&mut self, fx: &FrameEffects, w: u32, h: u32) {
        let format = self.config.format;
        let effects = self
            .effects
            .get_or_insert_with(|| EffectsGpu::new(&self.device, format, &self.viewport_layout));
        let half = effects.targets(&self.device, format, w, h).half;
        let lights: Vec<f32> = fx
            .lights
            .iter()
            .flat_map(|&(x, y, radius, color, intensity)| {
                [
                    x,
                    y,
                    radius,
                    intensity,
                    color[0] as f32 / 255.0,
                    color[1] as f32 / 255.0,
                    color[2] as f32 / 255.0,
                    1.0,
                ]
            })
            .collect();
        effects.light_instances.upload(
            &self.device,
            &self.queue,
            "light instances",
            as_bytes(&lights),
        );
        let (threshold, intensity, radius) = fx.bloom.unwrap_or((1.0, 0.0, 0.0));
        let (scanlines, curvature, vignette) = fx.crt.unwrap_or((0.0, 0.0, 0.0));
        // the blur runs at half size: a radius of r frame pixels is r/2 texels there
        let texels = radius / 2.0;
        let sigma = (texels / 2.0).max(0.5);
        let taps = texels.ceil().clamp(1.0, 32.0);
        let post = |step: [f32; 2], bloom: f32| {
            [
                threshold,
                (1.0 - threshold).max(1e-3),
                bloom,
                curvature,
                scanlines,
                vignette,
                step[0],
                step[1],
                sigma,
                taps,
                0.0,
                0.0,
            ]
        };
        let write = |buffer: &wgpu::Buffer, words: [f32; 12]| {
            self.queue.write_buffer(buffer, 0, as_bytes(&words));
        };
        write(&effects.post_bright, post([0.0, 0.0], 0.0));
        write(&effects.post_across, post([1.0 / half.0 as f32, 0.0], 0.0));
        write(&effects.post_down, post([0.0, 1.0 / half.1 as f32], 0.0));
        write(&effects.post_final, post([0.0, 0.0], intensity));
    }
}

/// A render pass over `view` with one colour attachment: cleared to `clear`, or loading
/// what is there when it is `None`.
fn render_pass<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    label: &str,
    view: &wgpu::TextureView,
    clear: Option<wgpu::Color>,
) -> wgpu::RenderPass<'e> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: clear.map_or(wgpu::LoadOp::Load, wgpu::LoadOp::Clear),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// The atlas key of a cluster under a face at a font size: what it is drawn from depends
/// on the cluster, the family, the weight/slant, the scale and the pixel size (a cell
/// region rasterises at its own px) and the font registration it was baked under (a
/// re-registered family keyword names other faces) — not the colour, which is a
/// per-instance tint.
fn glyph_key(
    g: &str,
    family: Option<u32>,
    bold: bool,
    italic: bool,
    scale: u16,
    px: f32,
    generation: u64,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    g.hash(&mut h);
    family.hash(&mut h);
    bold.hash(&mut h);
    italic.hash(&mut h);
    scale.hash(&mut h);
    px.to_bits().hash(&mut h);
    generation.hash(&mut h);
    h.finish()
}

fn as_bytes(v: &[f32]) -> &[u8] {
    // SAFETY: `f32` has no padding or invalid bit patterns; the slice covers exactly the
    // floats' storage.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn as_bytes_u32(v: &[u32]) -> &[u8] {
    // SAFETY: as `as_bytes`.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

/// `BROOD_GUI_DUMP=<path.ppm>`: the GPU path's half of the CPU painter's dump flag
/// (`paint::dump_canvas`) — read the presented frame back and write it as a binary PPM
/// after every paint, so what the GPU drew can be LOOKED at from a script (the desktop
/// forbids a screenshot to an unprivileged process). Read once; off by default. The
/// readback is a full round trip through a mapped buffer every frame, so it is a debug
/// aid, not a path anything else runs on.
fn dump_path() -> Option<&'static str> {
    static PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| std::env::var("BROOD_GUI_DUMP").ok())
        .as_deref()
}

/// Copy `texture` (the frame just drawn, `w`×`h`, `format`) into a mapped buffer and
/// write it to `path` as a PPM. Rows are padded to wgpu's 256-byte copy alignment and
/// unpadded on the way out; a BGRA surface is swizzled to RGB.
fn dump_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    format: wgpu::TextureFormat,
    w: u32,
    h: u32,
    path: &str,
) {
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_row = (4 * w).div_ceil(align) * align;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("frame dump"),
        size: padded_row as u64 * h as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("frame dump"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(std::iter::once(encoder.finish()));
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    if device.poll(wgpu::PollType::wait_indefinitely()).is_err() {
        return;
    }
    let bgra = matches!(
        format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let Ok(data) = slice.get_mapped_range() else {
        return;
    };
    let mut out = Vec::with_capacity((w * h * 3) as usize + 32);
    out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    for row in data.chunks(padded_row as usize).take(h as usize) {
        for px in row[..(4 * w) as usize].chunks(4) {
            if bgra {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            } else {
                out.extend_from_slice(&[px[0], px[1], px[2]]);
            }
        }
    }
    drop(data);
    buffer.unmap();
    let _ = std::fs::write(path, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    const FW: f32 = 800.0;
    const FH: f32 = 600.0;

    #[test]
    fn an_on_screen_quad_is_kept() {
        assert!(quad_visible(10.0, 10.0, 5.0, 5.0, FW, FH));
        // Partly off each edge still touches the viewport.
        assert!(quad_visible(-2.0, -2.0, 5.0, 5.0, FW, FH));
        assert!(quad_visible(798.0, 598.0, 5.0, 5.0, FW, FH));
    }

    #[test]
    fn a_quad_entirely_outside_the_viewport_is_culled() {
        assert!(!quad_visible(-10.0, 10.0, 5.0, 5.0, FW, FH)); // left
        assert!(!quad_visible(10.0, -10.0, 5.0, 5.0, FW, FH)); // above
        assert!(!quad_visible(FW, 10.0, 5.0, 5.0, FW, FH)); // right
        assert!(!quad_visible(10.0, FH, 5.0, 5.0, FW, FH)); // below
        // A bitboard cell far past the window: the `Op::Cells` case the cull exists for.
        assert!(!quad_visible(1_000_000.0, 1_000_000.0, 8.0, 16.0, FW, FH));
    }

    #[test]
    fn a_degenerate_quad_is_culled() {
        assert!(!quad_visible(10.0, 10.0, 0.0, 5.0, FW, FH));
        assert!(!quad_visible(10.0, 10.0, 5.0, 0.0, FW, FH));
        assert!(!quad_visible(10.0, 10.0, -5.0, 5.0, FW, FH));
    }

    #[test]
    fn a_nan_coordinate_is_culled_not_buffered() {
        // Every comparison against NaN is false, so a positively-phrased test culls; a
        // negatively-phrased one (`!(x + w <= 0.0 || …)`) would have buffered the quad.
        assert!(!quad_visible(f32::NAN, 10.0, 5.0, 5.0, FW, FH));
        assert!(!quad_visible(10.0, f32::NAN, 5.0, 5.0, FW, FH));
        assert!(!quad_visible(10.0, 10.0, f32::NAN, 5.0, FW, FH));
        assert!(!quad_visible(10.0, 10.0, 5.0, f32::NAN, FW, FH));
    }

    #[test]
    fn an_infinity_is_judged_by_where_it_puts_the_quad() {
        // A quad starting at -inf with finite width never reaches the viewport: culled.
        assert!(!quad_visible(f32::NEG_INFINITY, 10.0, 5.0, 5.0, FW, FH));
        // A quad starting at +inf is past the right edge: culled.
        assert!(!quad_visible(f32::INFINITY, 10.0, 5.0, 5.0, FW, FH));
        // An infinitely wide quad from an on-screen x covers the viewport: kept.
        assert!(quad_visible(10.0, 10.0, f32::INFINITY, 5.0, FW, FH));
    }

    #[test]
    fn a_saturating_extent_does_not_wrap_into_view() {
        // `x + w` overflowing to +inf must not read as "on screen" from the left.
        assert!(!quad_visible(f32::MAX, 10.0, f32::MAX, 5.0, FW, FH));
    }

    #[test]
    fn a_rotated_quad_is_judged_by_the_disc_it_sweeps() {
        let fb = FrameBuilder::new(FW, FH);
        // A 400-wide, 4-tall bar whose unrotated rect sits wholly below the viewport,
        // turned a quarter turn about its centre: it stands up through the bottom edge, so
        // it is kept …
        assert!(!fb.visible(100.0, FH + 10.0, 400.0, 4.0, 0.0));
        assert!(fb.visible(100.0, FH + 10.0, 400.0, 4.0, std::f32::consts::FRAC_PI_2));
        // … while a small quad far away stays culled whatever its angle.
        assert!(!fb.visible(FW + 500.0, 100.0, 4.0, 4.0, 1.0));
    }

    #[test]
    fn consecutive_quads_of_one_kind_form_one_batch_and_a_kind_change_starts_another() {
        let mut fb = FrameBuilder::new(FW, FH);
        fb.solid(0.0, 0.0, 10.0, 10.0, [255, 0, 0, 255], 0.0, Edge::Plain);
        fb.solid(20.0, 0.0, 10.0, 10.0, [255, 0, 0, 255], 0.0, Edge::Plain);
        let uv = [0.0, 0.0, 1.0, 1.0];
        fb.textured(
            TexKey::User(7),
            0.0,
            0.0,
            8.0,
            8.0,
            uv,
            [255; 4],
            false,
            0.0,
        );
        fb.textured(
            TexKey::User(7),
            8.0,
            0.0,
            8.0,
            8.0,
            uv,
            [255; 4],
            false,
            0.0,
        );
        fb.textured(
            TexKey::User(9),
            16.0,
            0.0,
            8.0,
            8.0,
            uv,
            [255; 4],
            false,
            0.0,
        );
        fb.solid(40.0, 0.0, 10.0, 10.0, [0, 255, 0, 255], 0.0, Edge::Plain);
        let shape: Vec<(bool, u32, u32)> = fb
            .batches
            .iter()
            .map(|b| match b {
                Batch::Solid { first, count } => (true, *first, *count),
                Batch::Tex { first, count, .. } => (false, *first, *count),
                Batch::Particles { .. } => unreachable!("no emitter in this frame"),
            })
            .collect();
        // Two solids → one batch; two sprites of texture 7 → one; texture 9 → its own;
        // the trailing solid a fourth, continuing the solid buffer at index 2.
        assert_eq!(
            shape,
            vec![(true, 0, 2), (false, 0, 2), (false, 2, 1), (true, 2, 1)]
        );
        assert_eq!(fb.solids.len(), 3 * SOLID_FLOATS);
        assert_eq!(fb.texs.len(), 3 * TEX_WORDS);
    }

    #[test]
    fn a_clip_band_culls_what_lies_outside_it_and_rides_on_every_quad() {
        let mut fb = FrameBuilder::new(FW, FH);
        fb.clip = [0.0, 100.0, FW, 200.0];
        // Wholly above the band: culled. Straddling it: kept, carrying the band.
        fb.solid(10.0, 10.0, 10.0, 10.0, [255; 4], 0.0, Edge::Plain);
        assert!(fb.batches.is_empty());
        fb.solid(10.0, 95.0, 10.0, 10.0, [255; 4], 0.0, Edge::Plain);
        assert_eq!(fb.solids.len(), SOLID_FLOATS);
        assert_eq!(&fb.solids[SOLID_FLOATS - 4..], &[0.0, 100.0, FW, 200.0]);
        // A rounded edge is flagged for the SDF path with its radius.
        fb.solid(10.0, 120.0, 10.0, 10.0, [255; 4], 0.0, Edge::Rounded(3.0));
        assert_eq!(&fb.solids[SOLID_FLOATS + 9..SOLID_FLOATS + 11], &[3.0, 1.0]);
    }

    #[test]
    fn cursor_quads_follow_the_cpu_geometry() {
        // Block: the overlay plus four rim bars, 2 px thick for a 10 px cell.
        let block = cursor_quads(100.0, 50.0, 10.0, 20.0, CursorStyle::Block);
        assert_eq!(block.len(), 5);
        assert_eq!(block[0], (100.0, 50.0, 10.0, 20.0, [255, 255, 255, 128]));
        assert_eq!(block[2].0, 108.0); // the right rim at cw - t
        // Bar: one caret on the left edge, at least 2 px.
        let bar = cursor_quads(100.0, 50.0, 10.0, 20.0, CursorStyle::Bar);
        assert_eq!(bar.len(), 1);
        assert_eq!((bar[0].2, bar[0].3), (2.0, 20.0));
        // Underline: one rule along the cell bottom.
        let under = cursor_quads(100.0, 50.0, 10.0, 20.0, CursorStyle::Underline);
        assert_eq!(under[0].1, 68.0);
    }

    #[test]
    fn a_scroll_shift_is_whole_pixels_and_capped() {
        assert_eq!(scroll_px(0.5, 16.0), 8.0);
        assert_eq!(scroll_px(-0.26, 16.0), -4.0);
        assert_eq!(scroll_px(1.0e9, 16.0), MAX_SCROLL_PX);
    }

    #[test]
    fn no_batch_merges_across_the_overlay() {
        let mut fb = FrameBuilder::new(100.0, 100.0);
        fb.solid(0.0, 0.0, 10.0, 10.0, [255, 0, 0, 255], 0.0, Edge::Plain);
        fb.overlay_at = Some(fb.batches.len());
        fb.solid(20.0, 0.0, 10.0, 10.0, [0, 255, 0, 255], 0.0, Edge::Plain);
        fb.solid(40.0, 0.0, 10.0, 10.0, [0, 0, 255, 255], 0.0, Edge::Plain);
        let counts: Vec<u32> = fb
            .batches
            .iter()
            .map(|b| match b {
                Batch::Solid { count, .. } => *count,
                _ => 0,
            })
            .collect();
        assert_eq!(
            counts,
            vec![1, 2],
            "the scene's run ends at the overlay; the overlay's merge"
        );
    }

    #[test]
    fn a_step_is_the_shaders_params_word_for_word() {
        let step = EmitterStep {
            id: 1,
            x: 3.0,
            y: 4.0,
            spec: crate::host::gui::EmitterSpec::default(),
            dt: 0.5,
            spawn_first: 6,
            spawn_count: 7,
            seed: 8,
            present: true,
        };
        let words = params_words(&step, 9);
        assert_eq!(words.len() as u64 * 4, PARTICLE_PARAMS_BYTES);
        assert_eq!(f32::from_bits(words[0]), 3.0);
        assert_eq!(f32::from_bits(words[20]), 0.5, "dt");
        assert_eq!(
            &words[22..26],
            &[6, 7, 9, 8],
            "spawn_first, spawn_count, capacity, seed"
        );
    }

    #[test]
    fn a_culled_quad_leaves_no_instance_and_no_batch() {
        let mut fb = FrameBuilder::new(FW, FH);
        fb.solid(-100.0, -100.0, 10.0, 10.0, [255; 4], 0.0, Edge::Plain);
        assert!(fb.batches.is_empty());
        assert!(fb.solids.is_empty());
    }

    #[test]
    fn a_non_srgb_format_is_preferred_over_an_srgb_one() {
        use wgpu::TextureFormat::{Bgra8Unorm, Bgra8UnormSrgb, Rgba16Float};
        assert_eq!(
            pick_format(&[Bgra8UnormSrgb, Rgba16Float, Bgra8Unorm]),
            Some(Bgra8Unorm)
        );
        // Only sRGB on offer: the first supported one, rather than no window at all.
        assert_eq!(pick_format(&[Bgra8UnormSrgb]), Some(Bgra8UnormSrgb));
        assert_eq!(pick_format(&[]), None);
    }

    #[test]
    fn no_vsync_takes_immediate_then_mailbox_then_fifo_and_vsync_is_fifo() {
        use wgpu::PresentMode::{Fifo, Immediate, Mailbox};
        assert_eq!(
            pick_present_mode(&[Fifo, Mailbox, Immediate], false),
            Immediate
        );
        assert_eq!(pick_present_mode(&[Fifo, Mailbox], false), Mailbox);
        assert_eq!(pick_present_mode(&[Fifo], false), Fifo);
        assert_eq!(pick_present_mode(&[Fifo, Mailbox, Immediate], true), Fifo);
    }
}
