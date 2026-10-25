//! wgpu renderer. Draws each decoded GX frame into an offscreen framebuffer (the EFB) and
//! copies the displayed part of it to the window, as the console's copy-to-display step does.
//!
//! Materials are fragment shaders generated from the game's texture-environment state (see
//! tev.rs). Every draw names its shader, up to eight textures, and a block of parameters.

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use bytemuck::{Pod, Zeroable};
use winit::window::Window;

/// Largest framebuffer the console supports, in guest pixels.
pub const EFB_WIDTH: u32 = 640;
pub const EFB_HEIGHT: u32 = 528;
/// Smallest internal resolution multiplier; a tall window raises it (`Renderer::wanted_scale`).
const MIN_SCALE: u32 = 2;
const MAX_SCALE: u32 = 6;

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// Texture maps a material can address.
pub const TEXTURE_SLOTS: usize = 8;
/// Per-draw shader parameters: four color registers, four constants, the alpha-test
/// references, the fog color and the fog terms, as 44 floats. Blocks are spaced at the uniform-buffer offset alignment.
pub const PARAMS_FLOATS: usize = 44;
const PARAMS_SIZE: u64 = PARAMS_FLOATS as u64 * 4;
const PARAMS_STRIDE: usize = 256;

/// A range of a frame's vertices to be placed again for a wide picture (see `Frame::flat`).
#[derive(Clone, Copy)]
pub struct Flat {
    pub from: u32,
    pub to: u32,
    /// For a model: its origin in clip coordinates. `None` for flat drawing.
    pub origin: Option<[f32; 4]>,
    /// Which widget of the heads-up display this belongs to, counted from 1; 0 for none.
    pub hud: u32,
    /// How far left and right it reaches across the 4:3 picture, -1 to 1.
    pub extent: (f32, f32),
}

/// A vertex already transformed to clip space on the CPU.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 4],
    /// The two rasterized color channels.
    pub colors: [[f32; 4]; 2],
    /// Eight texture coordinates, two per vector.
    pub uvs: [[f32; 4]; 4],
}

/// A vertex as the game sent it, for the vertex shader to transform and light (`vs_gx`):
/// position and normal in the model's space, the two vertex colors, and the first four
/// texture coordinate attributes.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct RawVertex {
    pub pos: [f32; 3],
    pub pad0: f32,
    pub normal: [f32; 3],
    pub pad1: f32,
    pub colors: [[f32; 4]; 2],
    pub tex: [[f32; 2]; 4],
}

/// What the vertex shader needs for one draw besides the vertices: 352 words (see
/// `gx::vparams_block` and `VParams` in the shader). Blocks are spaced at `VPARAMS_STRIDE`.
pub const VPARAMS_WORDS: usize = 352;
const VPARAMS_SIZE: u64 = VPARAMS_WORDS as u64 * 4;
const VPARAMS_STRIDE: usize = 1536;

/// Room in the buffers that keep the vertices and indices of display lists from one frame
/// to the next (`Renderer::cache_alloc`): 115 MB and 24 MB. A large level fills less than
/// that with everything in it that does not move.
const CACHE_VERTICES: u64 = 1_200_000;
const CACHE_INDICES: u64 = 6_000_000;

const TRANSFORMED_ATTRIBUTES: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
    0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4
];
/// `RawVertex`: position, normal, two colors, and the texture coordinates two to a vector.
const RAW_ATTRIBUTES: [wgpu::VertexAttribute; 6] = [
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 0, shader_location: 0 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 16, shader_location: 1 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 32, shader_location: 2 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 48, shader_location: 3 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 64, shader_location: 4 },
    wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 80, shader_location: 5 },
];

/// Where a draw's vertices are and who transforms them.
#[derive(Clone, Copy, PartialEq)]
pub enum Geometry {
    /// In `Frame::vertices`, already in clip space.
    Transformed,
    /// In `Frame::raw`, for the vertex shader.
    Raw,
    /// In the display list cache, for the vertex shader; indices are relative to this vertex.
    Cached(i32),
}

/// A vertex of the on-screen controls (touch.rs), drawn over the finished picture: position
/// in the window from -1 to 1, y up, and a color blended over what is there.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct OverlayVertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
}

/// Room for the controls' triangles, in vertices.
const OVERLAY_VERTICES: usize = 8192;

/// Identifies decoded texture contents: address, width, height, format, hash of the source.
pub type TexKey = (u32, u32, u32, u32, u64);
/// Wrap mode for s and t (0 clamp, 1 repeat, 2 mirror) and whether to filter linearly.
pub type SamplerKey = (u8, u8, bool);
/// The texture bound to unused slots: a single white texel.
pub const WHITE_TEXTURE: usize = 0;

/// Fixed-function state that selects a pipeline. Values are the raw GX register encodings.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DrawState {
    pub depth_test: bool,
    pub depth_write: bool,
    pub depth_func: u8,
    pub blend: bool,
    pub subtract: bool,
    pub src_factor: u8,
    pub dst_factor: u8,
    pub color_write: bool,
    pub alpha_write: bool,
    /// Which faces are dropped (BP 0x00 bits 14-15): 0 none, 1 or 2 one winding or the other.
    /// (3, both, never reaches the renderer.)
    pub cull: u8,
}

pub struct Draw {
    /// Range of the frame's indices.
    pub first: u32,
    pub count: u32,
    pub state: DrawState,
    /// Key of the generated fragment shader, registered with `Renderer::ensure_shader`.
    pub shader: u64,
    pub textures: [usize; TEXTURE_SLOTS],
    pub samplers: [SamplerKey; TEXTURE_SLOTS],
    /// Byte offset of this draw's block in the frame's parameters.
    pub params: u32,
    pub geometry: Geometry,
    /// Byte offset of its block of vertex shader parameters (not for `Transformed`).
    pub vparams: u32,
    /// x, y, width, height in guest pixels.
    pub viewport: [f32; 4],
    /// left, top, right, bottom in guest pixels.
    pub scissor: [u32; 4],
}

#[derive(Default)]
pub struct Frame {
    pub vertices: Vec<Vertex>,
    /// Triangle list, three indices into `vertices` each.
    pub indices: Vec<u32>,
    pub draws: Vec<Draw>,
    /// Vertex ranges to pull toward the centre of a wide picture when the frame is shown (see
    /// aspect.rs). Flat screen-space drawing is squeezed horizontally; a model drawn over it
    /// keeps its shape, is brought back to its 4:3 size and is moved as a whole. A widget of
    /// the heads-up display in the left or right half of the 4:3 picture goes to that edge
    /// of the wide one instead, as far from it as it was.
    pub flat: Vec<Flat>,
    params: Vec<u8>,
    pub raw: Vec<RawVertex>,
    vparams: Vec<u8>,
    /// Blocks of vertex shader parameters whose draws are models drawn over flat drawing,
    /// to be placed again for a wide picture as `flat` says: the block's offset and the
    /// model's origin in clip coordinates.
    pub raw_flat: Vec<(u32, [f32; 4])>,
}

impl Frame {
    /// Empties the frame for reuse. Its buffers run to megabytes; keeping them allocated
    /// avoids growing them from nothing every frame.
    pub fn clear(&mut self) {
        self.vertices.clear();
        self.indices.clear();
        self.draws.clear();
        self.flat.clear();
        self.params.clear();
        self.raw.clear();
        self.vparams.clear();
        self.raw_flat.clear();
    }

    /// Tells the vertex shader to place the draws of the block at `offset` again: brought
    /// to `scale` times their size about their origin, then moved sideways by `shift`.
    pub fn place_again(&mut self, offset: u32, scale: f32, shift: f32) {
        let at = offset as usize + (VPARAMS_WORDS - 4) * 4;
        self.vparams[at..at + 16].copy_from_slice(bytemuck::cast_slice(&[scale, shift, 1.0f32, 1.0]));
    }

    /// Stores one draw's vertex shader parameters and returns their offset for
    /// `Draw::vparams`. A block equal to the previous one is shared.
    pub fn push_vparams(&mut self, block: &[u32; VPARAMS_WORDS]) -> u32 {
        let bytes: &[u8] = bytemuck::cast_slice(block);
        let offset = self.vparams.len();
        if offset >= VPARAMS_STRIDE && self.vparams[offset - VPARAMS_STRIDE..][..bytes.len()] == *bytes {
            return (offset - VPARAMS_STRIDE) as u32;
        }
        self.vparams.extend_from_slice(bytes);
        self.vparams.resize(offset + VPARAMS_STRIDE, 0);
        offset as u32
    }

    /// Stores one draw's shader parameters and returns their offset for `Draw::params`. A
    /// block equal to the previous one is shared.
    pub fn push_params(&mut self, block: &[f32; PARAMS_FLOATS]) -> u32 {
        let bytes: &[u8] = bytemuck::cast_slice(block);
        let offset = self.params.len();
        if offset >= PARAMS_STRIDE && self.params[offset - PARAMS_STRIDE..][..bytes.len()] == *bytes {
            return (offset - PARAMS_STRIDE) as u32;
        }
        self.params.extend_from_slice(bytes);
        self.params.resize(offset + PARAMS_STRIDE, 0);
        offset as u32
    }

    /// Queues a draw. The game sends its geometry in thousands of small pieces per frame; one
    /// that continues the previous draw with the same material and state is merged into it.
    pub fn push_draw(&mut self, draw: Draw) {
        if let Some(last) = self.draws.last_mut() {
            if last.first + last.count == draw.first
                && last.state == draw.state
                && last.shader == draw.shader
                && last.textures == draw.textures
                && last.samplers == draw.samplers
                && last.params == draw.params
                && last.geometry == draw.geometry
                && !matches!(draw.geometry, Geometry::Cached(_))
                && last.vparams == draw.vparams
                && last.viewport == draw.viewport
                && last.scissor == draw.scissor
            {
                last.count += draw.count;
                return;
            }
        }
        self.draws.push(draw);
    }
}

struct Target {
    size: WindowSize,
    /// `None` while the system has the window's surface (see `surface_lost`).
    surface: Option<wgpu::Surface<'static>>,
    config: wgpu::SurfaceConfiguration,
    blit: wgpu::RenderPipeline,
    overlay: wgpu::RenderPipeline,
}

/// Android destroys a window's surface whenever the app leaves the screen and makes a new
/// one when it comes back. The window thread hears of both and leaves word here; the game
/// thread, which owns the renderer, acts on it at its next frame. While the surface is gone
/// that frame waits, which is also what pauses the game in the background.
struct Handover {
    lost: bool,
    replacement: Option<wgpu::Surface<'static>>,
}

static HANDOVER: Mutex<Handover> = Mutex::new(Handover { lost: false, replacement: None });
static HANDOVER_CHANGED: Condvar = Condvar::new();

/// Called from the window thread when the system is about to destroy the window's surface.
/// Returns once no frame is being drawn to it; none will be until `surface_replaced`.
pub fn surface_lost() {
    // The presenter takes the lock for every picture it shows; it stands back for this.
    SURFACE_WANTED.store(true, Ordering::Relaxed);
    let mut handover = HANDOVER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    handover.lost = true;
    handover.replacement = None;
    SURFACE_ABSENT.store(true, Ordering::Relaxed);
    SURFACE_WANTED.store(false, Ordering::Relaxed);
}

/// Called from the window thread with the surface of the window's new incarnation.
pub fn surface_replaced(surface: wgpu::Surface<'static>) {
    HANDOVER.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).replacement = Some(surface);
    HANDOVER_CHANGED.notify_all();
}

/// Set while the window thread waits to take the surface away.
static SURFACE_WANTED: AtomicBool = AtomicBool::new(false);
/// Set from `surface_lost` until the presenter has the window's new surface.
static SURFACE_ABSENT: AtomicBool = AtomicBool::new(false);

/// The picture the presenter puts in the window: a texture nothing draws into while it is
/// shown, the part of it to show (uv offset and size), and the picture's shape.
struct Shown {
    view: wgpu::TextureView,
    rect: [f32; 4],
    picture: f32,
    serial: u64,
}

static SHOWN: Mutex<Option<Shown>> = Mutex::new(None);
static SHOW_WANTED: Mutex<bool> = Mutex::new(false);
static SHOW_CHANGED: Condvar = Condvar::new();

/// The least time between two pictures put in the window.
const SHOW_GAP: std::time::Duration = std::time::Duration::from_millis(2);

/// Asks the presenter, if there is one, to put a picture in the window now: the game has
/// a new frame, or the pointer has moved.
pub fn show_soon() {
    *SHOW_WANTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
    SHOW_CHANGED.notify_one();
}

/// Whether the window gets its pictures from a thread of its own rather than from the game
/// as it finishes each frame. The game's frame then waits for nothing, and the pointer,
/// which is drawn over it, moves the moment the mouse does instead of at the game's next
/// frame. On by default on Android, where the pointer is the game's own; TSSM_PRESENTER=1
/// or 0 says otherwise.
fn presenter_wanted() -> bool {
    match std::env::var("TSSM_PRESENTER") {
        Ok(value) => value != "0",
        Err(_) => cfg!(target_os = "android"),
    }
}

/// The presenter: shows the newest picture with the touch controls and the pointer over it
/// whenever `show_soon` asks, and at least ten times a second.
fn run_presenter(device: wgpu::Device, queue: wgpu::Queue, mut target: Target, sampler: wgpu::Sampler) {
    let blit_rect = buffer(&device, "blit rect", 16, wgpu::BufferUsages::UNIFORM);
    let overlay_size = (OVERLAY_VERTICES * std::mem::size_of::<OverlayVertex>()) as u64;
    let overlay_buffer = buffer(&device, "overlay", overlay_size, wgpu::BufferUsages::VERTEX);
    let mut overlay_vertices: Vec<OverlayVertex> = Vec::new();
    let mut group: Option<(u64, wgpu::BindGroup)> = None;
    let mut last = std::time::Instant::now();
    loop {
        {
            let mut wanted = SHOW_WANTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            if !*wanted {
                let wait = std::time::Duration::from_millis(100);
                wanted = SHOW_CHANGED.wait_timeout(wanted, wait).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
            }
            *wanted = false;
        }
        let since = last.elapsed();
        if since < SHOW_GAP {
            std::thread::sleep(SHOW_GAP - since);
        }
        last = std::time::Instant::now();
        if SURFACE_WANTED.load(Ordering::Relaxed) {
            continue;
        }
        // Held until the picture is on screen, so the window thread cannot give the surface
        // up in the middle of it.
        let mut handover = HANDOVER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if std::mem::take(&mut handover.lost) {
            target.surface = None;
        }
        while target.surface.is_none() && handover.replacement.is_none() {
            handover = HANDOVER_CHANGED.wait(handover).unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if let Some(surface) = handover.replacement.take() {
            let (width, height) = (target.size)();
            (target.config.width, target.config.height) = (width.max(1), height.max(1));
            surface.configure(&device, &target.config);
            target.surface = Some(surface);
            // The game, which waits while there is no surface, goes on.
            SURFACE_ABSENT.store(false, Ordering::Relaxed);
            HANDOVER_CHANGED.notify_all();
        }
        let Some(surface) = target.surface.as_ref() else { continue };
        let (width, height) = (target.size)();
        if width == 0 || height == 0 {
            continue;
        }
        if (width, height) != (target.config.width, target.config.height) {
            target.config.width = width;
            target.config.height = height;
            surface.configure(&device, &target.config);
        }
        let texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                surface.configure(&device, &target.config);
                continue;
            }
            _ => continue,
        };
        let shown = {
            let shown = SHOWN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            shown.as_ref().map(|shown| (shown.view.clone(), shown.rect, shown.picture, shown.serial))
        };
        if let Some((source, rect, _, serial)) = &shown {
            queue.write_buffer(&blit_rect, 0, bytemuck::cast_slice(rect));
            if group.as_ref().is_none_or(|(made_for, _)| made_for != serial) {
                let made = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &target.blit.get_bind_group_layout(0),
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(source) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
                        wgpu::BindGroupEntry { binding: 2, resource: blit_rect.as_entire_binding() },
                    ],
                });
                group = Some((*serial, made));
            }
        }
        let (sw, sh) = (target.config.width as f32, target.config.height as f32);
        overlay_vertices.clear();
        crate::touch::overlay((sw, sh), &mut overlay_vertices);
        overlay_vertices.truncate(OVERLAY_VERTICES);
        if !overlay_vertices.is_empty() {
            queue.write_buffer(&overlay_buffer, 0, bytemuck::cast_slice(&overlay_vertices));
        }
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let view = texture.texture.create_view(&Default::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            if let (Some((_, _, picture, _)), Some((_, group))) = (&shown, &group) {
                // Fit the picture inside the window with bars, not stretched.
                let (vw, vh) = if sw > sh * picture { (sh * picture, sh) } else { (sw, sw / picture) };
                pass.set_viewport((sw - vw) / 2.0, (sh - vh) / 2.0, vw.max(1.0), vh.max(1.0), 0.0, 1.0);
                pass.set_pipeline(&target.blit);
                pass.set_bind_group(0, group, &[]);
                pass.draw(0..3, 0..1);
            }
            if !overlay_vertices.is_empty() {
                pass.set_viewport(0.0, 0.0, sw, sh, 0.0, 1.0);
                pass.set_pipeline(&target.overlay);
                pass.set_vertex_buffer(0, overlay_buffer.slice(..));
                pass.draw(0..overlay_vertices.len() as u32, 0..1);
            }
        }
        queue.submit([encoder.finish()]);
        queue.present(texture);
        drop(handover);
    }
}

type TextureSet = ([usize; TEXTURE_SLOTS], [SamplerKey; TEXTURE_SLOTS]);

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    shaders: HashMap<u64, wgpu::ShaderModule>,
    /// By state, shader, and whether the vertex shader transforms the vertices.
    pipelines: HashMap<(DrawState, u64, bool), wgpu::RenderPipeline>,
    vertex_module: wgpu::ShaderModule,
    /// The driver's compiled pipelines and the file they are kept in between runs, how
    /// many pipelines have been made since the file was written, and when it last was.
    pipeline_cache: Option<(wgpu::PipelineCache, String)>,
    unsaved_pipelines: u32,
    saved_at: std::time::Instant,
    pipeline_layout: wgpu::PipelineLayout,
    raw_pipeline_layout: wgpu::PipelineLayout,
    vparams_layout: wgpu::BindGroupLayout,
    vparams_buffer: wgpu::Buffer,
    vparams_group: wgpu::BindGroup,
    vparams_capacity: u64,
    raw_buffer: wgpu::Buffer,
    raw_capacity: u64,
    /// The display list cache's vertices and indices, and how much of each is taken.
    cache_vertices: wgpu::Buffer,
    cache_indices: wgpu::Buffer,
    cache_used: (u64, u64),
    params_layout: wgpu::BindGroupLayout,
    texture_layout: wgpu::BindGroupLayout,
    params_buffer: wgpu::Buffer,
    params_group: wgpu::BindGroup,
    params_capacity: u64,
    textures: Vec<wgpu::TextureView>,
    texture_ids: HashMap<TexKey, usize>,
    samplers: HashMap<SamplerKey, wgpu::Sampler>,
    texture_groups: HashMap<TextureSet, wgpu::BindGroup>,
    /// Textures the game made by copying the EFB, keyed by the guest address it copied to:
    /// texture id, the texture itself, and its size.
    efb_copies: HashMap<u32, (usize, wgpu::Texture, u32, u32)>,
    efb: wgpu::Texture,
    efb_view: wgpu::TextureView,
    depth_view: wgpu::TextureView,
    /// Host pixels per guest pixel, horizontally and vertically. A wide picture has more
    /// columns: the game draws its wider view into the same guest framebuffer.
    scale: (u32, u32),
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: u64,
    index_buffer: wgpu::Buffer,
    index_capacity: u64,
    sampler: wgpu::Sampler,
    blit_rect: wgpu::Buffer,
    overlay_buffer: wgpu::Buffer,
    overlay_vertices: Vec<OverlayVertex>,
    target: Option<Target>,
    /// Whether a presenter shows the frames (see `presenter_wanted`); it has the target then.
    presenting: bool,
    /// How to ask for the window's size, if there is a window.
    size: Option<WindowSize>,
    /// With a presenter: the copy of the framebuffer it shows while the game draws the
    /// next frame, and how many pictures it has been given.
    front: Option<(wgpu::Texture, wgpu::TextureView)>,
    shown: u64,
    /// Frame numbers to save as PNG (TSSM_SCREENSHOT=60,600).
    screenshots: Vec<u64>,
    /// Made since the gx decoder last asked (for its report of slow frames): shaders,
    /// pipelines, textures, and the time the pipelines took.
    pub made: (u32, u32, u32, std::time::Duration),
}

/// Declarations every generated fragment shader builds on, plus the pass-through vertex stage.
fn shader_header() -> String {
    let mut s = String::from(
        "struct Params { reg: array<vec4<f32>, 4>, konst: array<vec4<f32>, 4>, alpha_ref: vec4<f32>, fog_color: vec4<f32>, fog: vec4<f32> };\n\
         @group(0) @binding(0) var<uniform> p: Params;\n",
    );
    for i in 0..TEXTURE_SLOTS {
        s += &format!("@group(1) @binding({i}) var t{i}: texture_2d<f32>;\n");
        s += &format!("@group(1) @binding({}) var s{i}: sampler;\n", i + TEXTURE_SLOTS);
    }
    s += "
// The position is invariant: the game draws some surfaces twice, the second time with a
// light or a shadow over the first, and both must land on exactly the same depth whatever
// else differs between the two pipelines. Without it the two passes fight for the pixels.
struct VOut {
    @invariant @builtin(position) pos: vec4<f32>,
    @location(0) c0: vec4<f32>,
    @location(1) c1: vec4<f32>,
    @location(2) uv01: vec4<f32>,
    @location(3) uv23: vec4<f32>,
    @location(4) uv45: vec4<f32>,
    @location(5) uv67: vec4<f32>,
};
@vertex fn vs(
    @location(0) pos: vec4<f32>, @location(1) c0: vec4<f32>, @location(2) c1: vec4<f32>,
    @location(3) uv01: vec4<f32>, @location(4) uv23: vec4<f32>, @location(5) uv45: vec4<f32>,
    @location(6) uv67: vec4<f32>,
) -> VOut {
    return VOut(pos, c0, c1, uv01, uv23, uv45, uv67);
}

// The console's transform and lighting, for vertices sent as the game sent them. It follows
// `VertexJob::run` in gx.rs step by step; the two must give the same picture.
struct Gen { r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>, info: vec4<f32> };
struct Chan { material: vec4<f32>, ambient: vec4<f32>, ctrl: vec4<u32> };
struct Light { color: vec4<f32>, cos_att: vec4<f32>, dist_att: vec4<f32>, pos: vec4<f32>, dir: vec4<f32> };
struct VParams {
    proj0: vec4<f32>, proj1: vec4<f32>,
    m0: vec4<f32>, m1: vec4<f32>, m2: vec4<f32>,
    n0: vec4<f32>, n1: vec4<f32>, n2: vec4<f32>,
    gens: array<Gen, 8>,
    chans: array<Chan, 2>,
    lights: array<Light, 8>,
    // A model drawn over flat drawing, on a wide picture: its origin in clip coordinates,
    // then the scale about it, the shift sideways, and whether to apply them.
    origin: vec4<f32>,
    place: vec4<f32>,
};
@group(2) @binding(0) var<uniform> v: VParams;

fn unit(a: vec3<f32>) -> vec3<f32> {
    let squared = dot(a, a);
    if (squared > 1e-30 && squared < 3.0e38) { return a * (1.0 / sqrt(squared)); }
    let largest = max(abs(a.x), max(abs(a.y), abs(a.z)));
    if (!(largest > 0.0) || !(largest < 3.0e38)) { return vec3<f32>(0.0); }
    let b = a / largest;
    return b / sqrt(dot(b, b));
}
fn ratio(above: f32, below: f32) -> f32 {
    if (below == 0.0) { return 0.0; }
    return max(above, 0.0) / below;
}
fn light_amount(i: u32, ctrl: u32, pos: vec3<f32>, normal: vec3<f32>) -> f32 {
    let to_light = v.lights[i].pos.xyz - pos;
    var direction = unit(to_light);
    let diffuse = (ctrl >> 7u) & 3u;
    let kind = (ctrl >> 9u) & 3u;
    var attenuation = 1.0;
    if (kind == 1u) {
        var c = 0.0;
        if (dot(direction, normal) >= 0.0) { c = max(dot(v.lights[i].dir.xyz, normal), 0.0); }
        let powers = vec3<f32>(1.0, c, c * c);
        var dist_att = v.lights[i].dist_att.xyz;
        if (diffuse != 0u) { dist_att = unit(dist_att); }
        attenuation = ratio(dot(powers, v.lights[i].cos_att.xyz), dot(powers, dist_att));
    } else if (kind == 3u) {
        let distance = sqrt(dot(to_light, to_light));
        let c = max(dot(direction, v.lights[i].dir.xyz), 0.0);
        attenuation = ratio(dot(vec3<f32>(1.0, c, c * c), v.lights[i].cos_att.xyz), dot(vec3<f32>(1.0, distance, distance * distance), v.lights[i].dist_att.xyz));
    } else if (direction.x == 0.0 && direction.y == 0.0 && direction.z == 0.0) {
        direction = normal;
    }
    if (diffuse == 0u) { return attenuation; }
    if (diffuse == 1u) { return attenuation * dot(direction, normal); }
    return attenuation * max(dot(direction, normal), 0.0);
}
fn shade(n: u32, ctrl: u32, vertex: vec4<f32>, pos: vec3<f32>, normal: vec3<f32>) -> vec4<f32> {
    let flags = v.chans[n].ctrl.z;
    let has_color = (flags & 1u) != 0u;
    var material = v.chans[n].material;
    if (has_color && (ctrl & 1u) != 0u) { material = vertex; }
    if ((ctrl & 2u) == 0u || (flags & 2u) == 0u) { return material; }
    var sum = v.chans[n].ambient;
    if (has_color && (ctrl & 0x40u) != 0u) { sum = vertex; }
    let lights = ((ctrl >> 2u) & 0xFu) | (((ctrl >> 11u) & 0xFu) << 4u);
    for (var i = 0u; i < 8u; i++) {
        if (((lights >> i) & 1u) != 0u) { sum += v.lights[i].color * light_amount(i, ctrl, pos, normal); }
    }
    return material * clamp(sum, vec4<f32>(0.0), vec4<f32>(1.0));
}
fn channel(n: u32, vertex: vec4<f32>, pos: vec3<f32>, normal: vec3<f32>) -> vec4<f32> {
    let ctrl = v.chans[n].ctrl;
    let color = shade(n, ctrl.x, vertex, pos, normal);
    if (ctrl.x == ctrl.y) { return color; }
    return vec4<f32>(color.rgb, shade(n, ctrl.y, vertex, pos, normal).a);
}
fn coordinate(n: u32, pos: vec3<f32>, t0: vec2<f32>, t1: vec2<f32>, t2: vec2<f32>, t3: vec2<f32>) -> vec2<f32> {
    let info = v.gens[n].info;
    if (info.x < 0.0) { return vec2<f32>(0.0); }
    var input = vec4<f32>(0.0);
    if (info.x == 0.0) { input = vec4<f32>(pos, 1.0); }
    else if (info.x == 1.0) { input = vec4<f32>(t0, 1.0, 1.0); }
    else if (info.x == 2.0) { input = vec4<f32>(t1, 1.0, 1.0); }
    else if (info.x == 3.0) { input = vec4<f32>(t2, 1.0, 1.0); }
    else if (info.x == 4.0) { input = vec4<f32>(t3, 1.0, 1.0); }
    var st = vec2<f32>(dot(v.gens[n].r0, input), dot(v.gens[n].r1, input));
    if (info.y != 0.0) {
        let q = dot(v.gens[n].r2, input);
        if (q != 0.0) { st = st / q; }
    }
    return st - info.zw;
}
@vertex fn vs_gx(
    @location(0) pos: vec3<f32>, @location(1) nrm: vec3<f32>, @location(2) vc0: vec4<f32>,
    @location(3) vc1: vec4<f32>, @location(4) t01: vec4<f32>, @location(5) t23: vec4<f32>,
) -> VOut {
    let p = vec4<f32>(pos, 1.0);
    let at = vec3<f32>(dot(v.m0, p), dot(v.m1, p), dot(v.m2, p));
    var normal = nrm;
    if (v.proj1.z != 0.0) {
        normal = unit(vec3<f32>(dot(v.n0.xyz, nrm), dot(v.n1.xyz, nrm), dot(v.n2.xyz, nrm)));
    }
    var o: VOut;
    o.pos = vec4<f32>(v.proj0.x * at.x + v.proj0.y * at.z, v.proj0.z * at.y + v.proj0.w * at.z, v.proj1.x * at.z + v.proj1.y - at.z, -at.z);
    if (v.place.z != 0.0) {
        o.pos = v.origin + (o.pos - v.origin) * v.place.x;
        o.pos.x += v.place.y * o.pos.w;
    }
    o.c0 = channel(0u, vc0, at, normal);
    o.c1 = channel(1u, vc1, at, normal);
    o.uv01 = vec4<f32>(coordinate(0u, pos, t01.xy, t01.zw, t23.xy, t23.zw), coordinate(1u, pos, t01.xy, t01.zw, t23.xy, t23.zw));
    o.uv23 = vec4<f32>(coordinate(2u, pos, t01.xy, t01.zw, t23.xy, t23.zw), coordinate(3u, pos, t01.xy, t01.zw, t23.xy, t23.zw));
    o.uv45 = vec4<f32>(coordinate(4u, pos, t01.xy, t01.zw, t23.xy, t23.zw), coordinate(5u, pos, t01.xy, t01.zw, t23.xy, t23.zw));
    o.uv67 = vec4<f32>(coordinate(6u, pos, t01.xy, t01.zw, t23.xy, t23.zw), coordinate(7u, pos, t01.xy, t01.zw, t23.xy, t23.zw));
    return o;
}
";
    s
}

const BLIT_SHADER: &str = "
@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
// uv offset in xy, uv size in zw
@group(0) @binding(2) var<uniform> rect: vec4<f32>;
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var o: VOut;
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = rect.xy + vec2<f32>(p.x, 1.0 - p.y) * rect.zw;
    return o;
}
@fragment fn fs(i: VOut) -> @location(0) vec4<f32> { return vec4<f32>(textureSample(t, s, i.uv).rgb, 1.0); }
";

const OVERLAY_SHADER: &str = "
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };
@vertex fn vs(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> VOut {
    return VOut(vec4<f32>(pos, 0.0, 1.0), color);
}
@fragment fn fs(i: VOut) -> @location(0) vec4<f32> { return i.color; }
";

fn upload_texture(device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32, rgba: &[u8]) -> wgpu::TextureView {
    let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: None },
        size,
    );
    texture.create_view(&Default::default())
}

fn compare(func: u8) -> wgpu::CompareFunction {
    use wgpu::CompareFunction::*;
    [Never, Less, Equal, LessEqual, Greater, NotEqual, GreaterEqual, Always][(func & 7) as usize]
}

/// GX blend factors; code 2/3 means "the other side's color".
fn factor(code: u8, is_source: bool) -> wgpu::BlendFactor {
    use wgpu::BlendFactor::*;
    match (code & 7, is_source) {
        (0, _) => Zero,
        (1, _) => One,
        (2, true) => Dst,
        (3, true) => OneMinusDst,
        (2, false) => Src,
        (3, false) => OneMinusSrc,
        (4, _) => SrcAlpha,
        (5, _) => OneMinusSrcAlpha,
        (6, _) => DstAlpha,
        _ => OneMinusDstAlpha,
    }
}

/// Asks a window for its size in pixels, from any thread.
type WindowSize = Arc<dyn Fn() -> (u32, u32) + Send + Sync>;

/// A window together with the GPU surface that draws into it.
///
/// Must be built on the thread that owns the window: the platform only hands out a window's
/// native handle there. The finished surface can then move to the thread that renders.
pub struct WindowSurface {
    size: WindowSize,
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
}

/// WGPU_BACKEND (dx12, vulkan, gl) overrides the backend choice.
pub fn new_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env())
}

impl WindowSurface {
    pub fn new(window: Arc<Window>) -> WindowSurface {
        let instance = new_instance();
        let surface = instance.create_surface(window.clone()).expect("cannot create surface");
        let size = move || {
            let size = window.inner_size();
            (size.width, size.height)
        };
        WindowSurface { size: Arc::new(size), instance, surface }
    }

    /// For a window that is not winit's (Android): a surface made from it with `instance`,
    /// and how to ask for its size.
    #[cfg(target_os = "android")]
    pub fn from_parts(
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        size: impl Fn() -> (u32, u32) + Send + Sync + 'static,
    ) -> WindowSurface {
        WindowSurface { size: Arc::new(size), instance, surface }
    }

    /// The instance the surface came from, which can make the window another one.
    pub fn instance(&self) -> wgpu::Instance {
        self.instance.clone()
    }
}

fn buffer(device: &wgpu::Device, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn params_group(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, buffer: &wgpu::Buffer, size: u64) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("params"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer, offset: 0, size: NonZeroU64::new(size) }),
        }],
    })
}

impl Renderer {
    /// With a window, frames are shown in it; without one, rendering is offscreen only
    /// (useful with screenshots).
    pub fn new(window: Option<WindowSurface>) -> Renderer {
        pollster::block_on(Self::create(window))
    }

    async fn create(window: Option<WindowSurface>) -> Renderer {
        let (instance, window, surface) = match window {
            Some(w) => (w.instance, Some(w.size), Some(w.surface)),
            None => (new_instance(), None, None),
        };
        eprintln!("[render] requesting adapter");
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { compatible_surface: surface.as_ref(), ..Default::default() })
            .await
            .expect("no suitable graphics adapter");
        eprintln!("[render] {} ({:?}); opening device", adapter.get_info().name, adapter.get_info().backend);
        // Where the driver lets its compiled pipelines be kept (Vulkan), they are, between
        // runs too: a pipeline met again is then made in a fraction of the time.
        let required_features = adapter.features() & wgpu::Features::PIPELINE_CACHE;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor { required_features, ..Default::default() })
            .await
            .expect("cannot open graphics device");
        eprintln!("[render] device ready");
        let pipeline_cache = wgpu::util::pipeline_cache_key(&adapter.get_info()).filter(|_| !required_features.is_empty()).map(|key| {
            let path = format!("cache/{key}");
            let data = std::fs::read(&path).ok();
            eprintln!("[render] pipeline cache {path}: {} bytes from the last run", data.as_ref().map_or(0, |data| data.len()));
            // SAFETY: the data is only ever what this driver gave out for this cache key, and
            // with `fallback` anything it does not accept is dropped for an empty cache.
            let cache = unsafe {
                device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor { label: None, data: data.as_deref(), fallback: true })
            };
            (cache, path)
        });
        // The vertex stage is the same for every material: kept in a module of its own, it
        // is the same to the driver too, which then compiles it once and not per material.
        let vertex_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vertex"),
            source: wgpu::ShaderSource::Wgsl(shader_header().into()),
        });

        let scale = Self::wanted_scale(window.as_ref().map_or(0, |size| size().1));
        let (efb, efb_view, depth_view) = Self::framebuffer(&device, scale);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let blit_rect = buffer(&device, "blit rect", 16, wgpu::BufferUsages::UNIFORM);

        let target = window.zip(surface).map(|(size, surface)| {
            let (width, height) = size();
            let mut config = surface.get_default_config(&adapter, width.max(1), height.max(1)).expect("surface unsupported");
            // The game's colors are already display-referred; avoid a second gamma encode.
            let capabilities = surface.get_capabilities(&adapter);
            config.format = capabilities.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(config.format);
            // The game paces itself (one frame a retrace), so the display need not hold it
            // back. On a phone a queue of frames waiting for the display is what the pointer
            // and the camera are felt to lag by: there the newest frame replaces the one
            // waiting (no tearing), where the display can do that. The same goes for a game
            // that runs at the display's own rate or above it: a queue would fill and stall it.
            let mailbox = (cfg!(target_os = "android") || presenter_wanted() || crate::host::frame_rate_unlocked())
                && capabilities.present_modes.contains(&wgpu::PresentMode::Mailbox);
            config.present_mode = if mailbox { wgpu::PresentMode::Mailbox } else { wgpu::PresentMode::AutoVsync };
            if cfg!(target_os = "android") || presenter_wanted() {
                config.desired_maximum_frame_latency = 1;
            }
            eprintln!(
                "[render] present mode {:?} of {:?}, presenter: {}",
                config.present_mode,
                capabilities.present_modes,
                presenter_wanted()
            );
            surface.configure(&device, &config);

            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("blit"),
                source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
            });
            let blit = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("blit"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(config.format.into())],
                }),
                multiview_mask: None,
                cache: None,
            });
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("overlay"),
                source: wgpu::ShaderSource::Wgsl(OVERLAY_SHADER.into()),
            });
            let overlay = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("overlay"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<OverlayVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                    })],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: config.format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
            Target { size, surface: Some(surface), config, blit, overlay }
        });
        let overlay_size = (OVERLAY_VERTICES * std::mem::size_of::<OverlayVertex>()) as u64;
        let overlay_buffer = buffer(&device, "overlay", overlay_size, wgpu::BufferUsages::VERTEX);

        let size = target.as_ref().map(|target| target.size.clone());
        let presenting = target.is_some() && presenter_wanted();
        let target = match target {
            Some(target) if presenting => {
                let (device, queue, sampler) = (device.clone(), queue.clone(), sampler.clone());
                std::thread::Builder::new()
                    .name("presenter".into())
                    .spawn(move || run_presenter(device, queue, target, sampler))
                    .expect("cannot start the presenter thread");
                None
            }
            target => target,
        };

        let screenshots = std::env::var("TSSM_SCREENSHOT")
            .map(|s| s.split(',').filter_map(|n| n.trim().parse().ok()).collect())
            .unwrap_or_default();

        // Group 0: the draw's parameter block, selected by a dynamic offset.
        let params_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("params"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: NonZeroU64::new(PARAMS_SIZE),
                },
                count: None,
            }],
        });
        // Group 1: eight textures followed by their eight samplers.
        let slot = |binding: usize, ty| wgpu::BindGroupLayoutEntry {
            binding: binding as u32,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty,
            count: None,
        };
        let texture_entries: Vec<_> = (0..TEXTURE_SLOTS)
            .map(|i| {
                slot(
                    i,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                )
            })
            .chain((0..TEXTURE_SLOTS).map(|i| {
                slot(i + TEXTURE_SLOTS, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering))
            }))
            .collect();
        let texture_layout = device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("textures"), entries: &texture_entries });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("draw"),
            bind_group_layouts: &[Some(&params_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        // Group 2, for draws whose vertices the shader transforms: the vertex parameters.
        let vparams_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vertex params"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: NonZeroU64::new(VPARAMS_SIZE),
                },
                count: None,
            }],
        });
        let raw_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("draw raw"),
            bind_group_layouts: &[Some(&params_layout), Some(&texture_layout), Some(&vparams_layout)],
            immediate_size: 0,
        });
        let vparams_capacity = 1 << 20;
        let vparams_buffer = buffer(&device, "vertex params", vparams_capacity, wgpu::BufferUsages::UNIFORM);
        let vparams_group = params_group(&device, &vparams_layout, &vparams_buffer, VPARAMS_SIZE);
        let raw_capacity = 1 << 22;
        let raw_buffer = buffer(&device, "raw vertices", raw_capacity, wgpu::BufferUsages::VERTEX);
        let raw_size = std::mem::size_of::<RawVertex>() as u64;
        let cache_vertices = buffer(&device, "cached vertices", CACHE_VERTICES * raw_size, wgpu::BufferUsages::VERTEX);
        let cache_indices = buffer(&device, "cached indices", CACHE_INDICES * 4, wgpu::BufferUsages::INDEX);

        let params_capacity = 1 << 20;
        let params_buffer = buffer(&device, "params", params_capacity, wgpu::BufferUsages::UNIFORM);
        let params_group = params_group(&device, &params_layout, &params_buffer, PARAMS_SIZE);
        let textures = vec![upload_texture(&device, &queue, 1, 1, &[255; 4])];
        let vertex_capacity = 1 << 22;
        let vertex_buffer = buffer(&device, "vertices", vertex_capacity, wgpu::BufferUsages::VERTEX);
        let index_capacity = 1 << 20;
        let index_buffer = buffer(&device, "indices", index_capacity, wgpu::BufferUsages::INDEX);

        Renderer {
            device,
            queue,
            shaders: HashMap::new(),
            pipelines: HashMap::new(),
            vertex_module,
            pipeline_cache,
            unsaved_pipelines: 0,
            saved_at: std::time::Instant::now(),
            pipeline_layout,
            raw_pipeline_layout,
            vparams_layout,
            vparams_buffer,
            vparams_group,
            vparams_capacity,
            raw_buffer,
            raw_capacity,
            cache_vertices,
            cache_indices,
            cache_used: (0, 0),
            params_layout,
            texture_layout,
            params_buffer,
            params_group,
            params_capacity,
            textures,
            texture_ids: HashMap::new(),
            samplers: HashMap::new(),
            texture_groups: HashMap::new(),
            efb_copies: HashMap::new(),
            efb,
            efb_view,
            depth_view,
            scale,
            vertex_buffer,
            vertex_capacity,
            index_buffer,
            index_capacity,
            sampler,
            blit_rect,
            overlay_buffer,
            overlay_vertices: Vec::new(),
            target,
            presenting,
            size,
            front: None,
            shown: 0,
            screenshots,
            made: (0, 0, 0, std::time::Duration::ZERO),
        }
    }

    /// The internal resolution for the window as it is now: enough rows to fill its height,
    /// and columns in proportion to how much wider than 4:3 the picture is.
    fn wanted_scale(height: u32) -> (u32, u32) {
        let rows = height.div_ceil(480).clamp(MIN_SCALE, MAX_SCALE);
        (((rows as f32 * crate::aspect::widen()).ceil() as u32).max(rows), rows)
    }

    /// The framebuffer at an internal resolution: color texture, its view, and the depth view.
    fn framebuffer(device: &wgpu::Device, scale: (u32, u32)) -> (wgpu::Texture, wgpu::TextureView, wgpu::TextureView) {
        let size = wgpu::Extent3d { width: EFB_WIDTH * scale.0, height: EFB_HEIGHT * scale.1, depth_or_array_layers: 1 };
        let texture = |format, usage| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        use wgpu::TextureUsages as U;
        let efb = texture(COLOR_FORMAT, U::RENDER_ATTACHMENT | U::TEXTURE_BINDING | U::COPY_SRC);
        let depth = texture(DEPTH_FORMAT, U::RENDER_ATTACHMENT);
        let (efb_view, depth_view) = (efb.create_view(&Default::default()), depth.create_view(&Default::default()));
        (efb, efb_view, depth_view)
    }

    /// Follows the window: after it changes size or shape, the next frame is drawn at the
    /// resolution that suits it. Called between frames; the old framebuffer's contents go.
    fn follow_window(&mut self) {
        let wanted = Self::wanted_scale(self.size.as_ref().map_or(0, |size| size().1));
        if wanted != self.scale {
            self.scale = wanted;
            self.front = None;
            (self.efb, self.efb_view, self.depth_view) = Self::framebuffer(&self.device, wanted);
        }
    }

    /// Makes the fragment shader for `key` available, generating its source on first use.
    pub fn ensure_shader(&mut self, key: u64, fragment: impl FnOnce() -> String) {
        if !self.shaders.contains_key(&key) {
            let source = shader_header() + &fragment();
            let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("material"),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            self.shaders.insert(key, module);
            self.made.0 += 1;
        }
    }

    /// Keeps a display list's vertices and its triangles' indices (counted from the first of
    /// those vertices) on the GPU, and returns where: the first vertex and the first index.
    /// `None` when there is no room left; `cache_reset` then makes room.
    pub fn cache_alloc(&mut self, vertices: &[RawVertex], indices: &[u32]) -> Option<(i32, u32)> {
        let (vertex, index) = self.cache_used;
        if vertex + vertices.len() as u64 > CACHE_VERTICES || index + indices.len() as u64 > CACHE_INDICES {
            return None;
        }
        let size = std::mem::size_of::<RawVertex>() as u64;
        self.queue.write_buffer(&self.cache_vertices, vertex * size, bytemuck::cast_slice(vertices));
        self.queue.write_buffer(&self.cache_indices, index * 4, bytemuck::cast_slice(indices));
        self.cache_used = (vertex + vertices.len() as u64, index + indices.len() as u64);
        Some((vertex as i32, index as u32))
    }

    /// How many vertices `cache_alloc` holds.
    pub fn cache_used(&self) -> u64 {
        self.cache_used.0
    }

    /// Forgets everything `cache_alloc` holds. Only between frames: draws name its contents.
    pub fn cache_reset(&mut self) {
        self.cache_used = (0, 0);
    }

    fn make_pipeline(&self, state: DrawState, shader: &wgpu::ShaderModule, raw: bool) -> wgpu::RenderPipeline {
        let blend = state.blend.then(|| {
            let component = if state.subtract {
                wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::ReverseSubtract,
                }
            } else {
                wgpu::BlendComponent {
                    src_factor: factor(state.src_factor, true),
                    dst_factor: factor(state.dst_factor, false),
                    operation: wgpu::BlendOperation::Add,
                }
            };
            wgpu::BlendState { color: component, alpha: component }
        });
        let mut write_mask = wgpu::ColorWrites::empty();
        write_mask.set(wgpu::ColorWrites::COLOR, state.color_write);
        write_mask.set(wgpu::ColorWrites::ALPHA, state.alpha_write);

        self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("draw"),
            layout: Some(if raw { &self.raw_pipeline_layout } else { &self.pipeline_layout }),
            vertex: wgpu::VertexState {
                module: &self.vertex_module,
                entry_point: Some(if raw { "vs_gx" } else { "vs" }),
                compilation_options: Default::default(),
                buffers: &[Some(if raw {
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<RawVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &RAW_ATTRIBUTES,
                    }
                } else {
                    wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Vertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &TRANSFORMED_ATTRIBUTES,
                    }
                })],
            },
            primitive: wgpu::PrimitiveState {
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: match state.cull {
                    // Found by trying: the other way round the ground disappears.
                    1 => Some(wgpu::Face::Front),
                    2 => Some(wgpu::Face::Back),
                    _ => None,
                },
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(state.depth_test && state.depth_write),
                depth_compare: Some(if state.depth_test { compare(state.depth_func) } else { wgpu::CompareFunction::Always }),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: COLOR_FORMAT, blend, write_mask })],
            }),
            multiview_mask: None,
            cache: self.pipeline_cache.as_ref().map(|cache| &cache.0),
        })
    }

    /// The id of an already uploaded texture with these contents.
    pub fn find_texture(&self, key: &TexKey) -> Option<usize> {
        self.texture_ids.get(key).copied()
    }

    /// Uploads decoded RGBA8 texels and returns the texture's id.
    pub fn add_texture(&mut self, key: TexKey, w: u32, h: u32, rgba: &[u8]) -> usize {
        self.textures.push(upload_texture(&self.device, &self.queue, w, h, rgba));
        self.made.2 += 1;
        self.texture_ids.insert(key, self.textures.len() - 1);
        self.textures.len() - 1
    }

    fn make_texture_group(&mut self, set: &TextureSet) -> wgpu::BindGroup {
        let device = &self.device;
        for &key in &set.1 {
            self.samplers.entry(key).or_insert_with(|| {
                let wrap = |mode: u8| match mode {
                    1 => wgpu::AddressMode::Repeat,
                    2 => wgpu::AddressMode::MirrorRepeat,
                    _ => wgpu::AddressMode::ClampToEdge,
                };
                let filter = if key.2 { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest };
                device.create_sampler(&wgpu::SamplerDescriptor {
                    address_mode_u: wrap(key.0),
                    address_mode_v: wrap(key.1),
                    mag_filter: filter,
                    min_filter: filter,
                    ..Default::default()
                })
            });
        }
        let entries: Vec<_> = (0..TEXTURE_SLOTS)
            .map(|i| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: wgpu::BindingResource::TextureView(&self.textures[set.0[i]]),
            })
            .chain((0..TEXTURE_SLOTS).map(|i| wgpu::BindGroupEntry {
                binding: (i + TEXTURE_SLOTS) as u32,
                resource: wgpu::BindingResource::Sampler(&self.samplers[&set.1[i]]),
            }))
            .collect();
        device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &self.texture_layout, entries: &entries })
    }

    /// Encodes the drawing of `frame` into the EFB. `clear` is the color to clear to first, or
    /// `None` to draw over the existing contents.
    fn draw_frame(&mut self, frame: &Frame, clear: Option<[f64; 4]>) -> wgpu::CommandEncoder {
        let vertices: &[u8] = bytemuck::cast_slice(&frame.vertices);
        if vertices.len() as u64 > self.vertex_capacity {
            self.vertex_capacity = (vertices.len() as u64).next_power_of_two();
            self.vertex_buffer = buffer(&self.device, "vertices", self.vertex_capacity, wgpu::BufferUsages::VERTEX);
        }
        self.queue.write_buffer(&self.vertex_buffer, 0, vertices);
        let indices: &[u8] = bytemuck::cast_slice(&frame.indices);
        if indices.len() as u64 > self.index_capacity {
            self.index_capacity = (indices.len() as u64).next_power_of_two();
            self.index_buffer = buffer(&self.device, "indices", self.index_capacity, wgpu::BufferUsages::INDEX);
        }
        self.queue.write_buffer(&self.index_buffer, 0, indices);
        if frame.params.len() as u64 > self.params_capacity {
            self.params_capacity = (frame.params.len() as u64).next_power_of_two();
            self.params_buffer = buffer(&self.device, "params", self.params_capacity, wgpu::BufferUsages::UNIFORM);
            self.params_group = params_group(&self.device, &self.params_layout, &self.params_buffer, PARAMS_SIZE);
        }
        self.queue.write_buffer(&self.params_buffer, 0, &frame.params);
        let raw: &[u8] = bytemuck::cast_slice(&frame.raw);
        if raw.len() as u64 > self.raw_capacity {
            self.raw_capacity = (raw.len() as u64).next_power_of_two();
            self.raw_buffer = buffer(&self.device, "raw vertices", self.raw_capacity, wgpu::BufferUsages::VERTEX);
        }
        self.queue.write_buffer(&self.raw_buffer, 0, raw);
        if frame.vparams.len() as u64 > self.vparams_capacity {
            self.vparams_capacity = (frame.vparams.len() as u64).next_power_of_two();
            self.vparams_buffer = buffer(&self.device, "vertex params", self.vparams_capacity, wgpu::BufferUsages::UNIFORM);
            self.vparams_group = params_group(&self.device, &self.vparams_layout, &self.vparams_buffer, VPARAMS_SIZE);
        }
        self.queue.write_buffer(&self.vparams_buffer, 0, &frame.vparams);

        crate::gx::PROF[8].fetch_add(frame.draws.len() as u64, Ordering::Relaxed);
        // Most draws use the pipeline and the textures of the draw before them: those are
        // looked up only when they change, here and in the pass below.
        let mut last_key = None;
        let mut last_set = None;
        for draw in &frame.draws {
            let key = (draw.state, draw.shader, draw.geometry != Geometry::Transformed);
            if last_key != Some(key) {
                last_key = Some(key);
                if !self.pipelines.contains_key(&key) {
                    if let Some(shader) = self.shaders.get(&draw.shader) {
                        let started = std::time::Instant::now();
                        let pipeline = self.make_pipeline(draw.state, shader, key.2);
                        self.pipelines.insert(key, pipeline);
                        self.made.1 += 1;
                        self.made.3 += started.elapsed();
                        self.unsaved_pipelines += 1;
                    }
                }
            }
            let set = (draw.textures, draw.samplers);
            if last_set != Some(set) {
                last_set = Some(set);
                if !self.texture_groups.contains_key(&set) {
                    let group = self.make_texture_group(&set);
                    self.texture_groups.insert(set, group);
                }
            }
        }

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let load = |c: Option<[f64; 4]>| match c {
                Some([r, g, b, a]) => wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a }),
                None => wgpu::LoadOp::Load,
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("efb"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.efb_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: load(clear), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: if clear.is_some() { wgpu::LoadOp::Clear(1.0) } else { wgpu::LoadOp::Load },
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            });
            // Which of the three kinds of geometry the buffers are set for: 0 transformed,
            // 1 raw, 2 cached.
            let mut bound = usize::MAX;
            // What the pass is set to, so that only what differs from the draw before is set.
            let mut set_key = None;
            let mut set_textures = None;
            let mut set_params = None;
            let mut set_vparams = None;
            let mut set_viewport = None;
            let mut set_scissor = None;
            let mut pipeline = None;
            let (sx, sy) = self.scale;
            let (max_w, max_h) = (EFB_WIDTH * sx, EFB_HEIGHT * sy);
            for draw in &frame.draws {
                let raw = draw.geometry != Geometry::Transformed;
                let key = (draw.state, draw.shader, raw);
                let new_pipeline = set_key != Some(key);
                if new_pipeline {
                    pipeline = self.pipelines.get(&key);
                    // Looked up again after a draw whose pipeline could not be made.
                    set_key = pipeline.map(|_| key);
                }
                let Some(pipeline) = pipeline else { continue };
                // Clip both rectangles to the framebuffer; skip draws that end up empty.
                let [vx, vy, vw, vh] = draw.viewport;
                let (vx, vy, vw, vh) = (vx * sx as f32, vy * sy as f32, vw * sx as f32, vh * sy as f32);
                let (x0, y0) = (vx.max(0.0), vy.max(0.0));
                let (x1, y1) = ((vx + vw).min(max_w as f32), (vy + vh).min(max_h as f32));
                let [l, t, r, b] = draw.scissor;
                let (l, t, r, b) = (l * sx, t * sy, r * sx, b * sy);
                let (r, b) = (r.min(max_w), b.min(max_h));
                if x1 <= x0 || y1 <= y0 || r <= l || b <= t {
                    continue;
                }
                if new_pipeline {
                    pass.set_pipeline(pipeline);
                }
                if set_params != Some(draw.params) {
                    set_params = Some(draw.params);
                    pass.set_bind_group(0, &self.params_group, &[draw.params]);
                }
                let textures = (draw.textures, draw.samplers);
                if set_textures != Some(textures) {
                    set_textures = Some(textures);
                    pass.set_bind_group(1, &self.texture_groups[&textures], &[]);
                }
                let viewport = [x0, y0, x1 - x0, y1 - y0];
                if set_viewport != Some(viewport) {
                    set_viewport = Some(viewport);
                    pass.set_viewport(x0, y0, x1 - x0, y1 - y0, 0.0, 1.0);
                }
                let scissor = [l, t, r - l, b - t];
                if set_scissor != Some(scissor) {
                    set_scissor = Some(scissor);
                    pass.set_scissor_rect(l, t, r - l, b - t);
                }
                let (kind, base) = match draw.geometry {
                    Geometry::Transformed => (0, 0),
                    Geometry::Raw => (1, 0),
                    Geometry::Cached(base) => (2, base),
                };
                if bound != kind {
                    bound = kind;
                    let vertices = [&self.vertex_buffer, &self.raw_buffer, &self.cache_vertices][kind];
                    let indices = if kind == 2 { &self.cache_indices } else { &self.index_buffer };
                    pass.set_vertex_buffer(0, vertices.slice(..));
                    pass.set_index_buffer(indices.slice(..), wgpu::IndexFormat::Uint32);
                }
                if raw && set_vparams != Some(draw.vparams) {
                    set_vparams = Some(draw.vparams);
                    pass.set_bind_group(2, &self.vparams_group, &[draw.vparams]);
                }
                pass.draw_indexed(draw.first..draw.first + draw.count, base, 0..1);
            }
        }
        encoder
    }

    /// Renders `frame` into the EFB and shows `shown` (x, y, width, height in guest pixels).
    pub fn present(&mut self, frame: &Frame, clear: Option<[f64; 4]>, shown: [u32; 4], number: u64) {
        let mut encoder = self.draw_frame(frame, clear);
        let (w, h) = (EFB_WIDTH as f32, EFB_HEIGHT as f32);
        let rect = [shown[0] as f32 / w, shown[1] as f32 / h, shown[2] as f32 / w, shown[3] as f32 / h];
        let view = if self.presenting { self.copy_to_front(&mut encoder) } else { self.efb_view.clone() };
        self.show(encoder, &view, rect, crate::aspect::picture());
        if self.screenshots.contains(&number) {
            self.screenshot(shown, number);
        }
        self.save_pipelines();
        self.follow_window();
    }

    /// Writes the driver's compiled pipelines to their file when new ones have been made,
    /// at most every ten seconds; the writing itself is done by a thread of its own.
    fn save_pipelines(&mut self) {
        if self.unsaved_pipelines == 0 || self.saved_at.elapsed() < std::time::Duration::from_secs(10) {
            return;
        }
        (self.unsaved_pipelines, self.saved_at) = (0, std::time::Instant::now());
        let Some((cache, path)) = &self.pipeline_cache else { return };
        let Some(data) = cache.get_data() else { return };
        let path = path.clone();
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all("cache");
            // Written whole under another name first, so a half-written file is never read.
            let partial = format!("{path}.new");
            if std::fs::write(&partial, &data).and_then(|()| std::fs::rename(&partial, &path)).is_err() {
                eprintln!("[render] cannot write {path}");
            }
        });
    }

    /// Renders `frame` into the EFB without showing it, so the EFB can be copied from mid-frame.
    pub fn flush(&mut self, frame: &Frame, clear: Option<[f64; 4]>) {
        let encoder = self.draw_frame(frame, clear);
        self.queue.submit([encoder.finish()]);
    }

    /// The texture standing in for the EFB copy the game made to guest address `addr`, if any.
    pub fn efb_copy(&self, addr: u32) -> Option<usize> {
        self.efb_copies.get(&addr).map(|copy| copy.0)
    }

    /// Whether texture `id` stands in for an EFB copy.
    pub fn is_efb_copy(&self, id: usize) -> bool {
        id != WHITE_TEXTURE && self.efb_copies.values().any(|copy| copy.0 == id)
    }

    /// Copies part of the EFB (x, y, width, height in guest pixels) into the texture that
    /// stands in for guest address `addr`. The copy stays at the internal resolution.
    pub fn copy_efb(&mut self, addr: u32, rect: [u32; 4]) {
        let (sx, sy) = self.scale;
        let (x, y, w, h) = (rect[0] * sx, rect[1] * sy, rect[2] * sx, rect[3] * sy);
        let (max_w, max_h) = (EFB_WIDTH * sx, EFB_HEIGHT * sy);
        if x >= max_w || y >= max_h {
            return;
        }
        let (w, h) = (w.min(max_w - x), h.min(max_h - y));
        if w == 0 || h == 0 {
            return;
        }
        let reusable = self.efb_copies.get(&addr).is_some_and(|copy| (copy.2, copy.3) == (w, h));
        if !reusable {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("efb copy"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: COLOR_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            // Keep the id stable across resizes; bind groups made for the old view are stale.
            let id = match self.efb_copies.get(&addr) {
                Some(old) => {
                    let id = old.0;
                    self.textures[id] = view;
                    self.texture_groups.retain(|set, _| !set.0.contains(&id));
                    id
                }
                None => {
                    self.textures.push(view);
                    self.textures.len() - 1
                }
            };
            self.efb_copies.insert(addr, (id, texture, w, h));
        }
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.efb,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture: &self.efb_copies[&addr].1,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
    }

    /// Adds to `encoder` a copy of the framebuffer into the texture the presenter shows, and
    /// returns that texture's view. The game draws its next frame into the framebuffer
    /// while this one is on show.
    fn copy_to_front(&mut self, encoder: &mut wgpu::CommandEncoder) -> wgpu::TextureView {
        let size = wgpu::Extent3d {
            width: EFB_WIDTH * self.scale.0,
            height: EFB_HEIGHT * self.scale.1,
            depth_or_array_layers: 1,
        };
        let (front, view) = self.front.get_or_insert_with(|| {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("front"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: COLOR_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            (texture, view)
        });
        let whole = |texture| wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        };
        encoder.copy_texture_to_texture(whole(&self.efb), whole(front), size);
        view.clone()
    }

    /// Copies part of `source` (uv offset and size in `rect`) to the window as a picture of
    /// shape `picture` (width over height) and presents it, after running whatever `encoder`
    /// already holds.
    fn show(&mut self, mut encoder: wgpu::CommandEncoder, source: &wgpu::TextureView, rect: [f32; 4], picture: f32) {
        if self.presenting {
            self.queue.submit([encoder.finish()]);
            // While the surface is gone the frame waits, which pauses the game.
            if SURFACE_ABSENT.load(Ordering::Relaxed) {
                let mut handover = HANDOVER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                while SURFACE_ABSENT.load(Ordering::Relaxed) {
                    handover = HANDOVER_CHANGED.wait(handover).unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            }
            self.shown += 1;
            let shown = Shown { view: source.clone(), rect, picture, serial: self.shown };
            *SHOWN.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(shown);
            show_soon();
            return;
        }
        // Held until the frame is on screen, so the window thread cannot give the surface up
        // in the middle of it.
        let mut handover = HANDOVER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(target) = &mut self.target {
            if std::mem::take(&mut handover.lost) {
                target.surface = None;
            }
            while target.surface.is_none() && handover.replacement.is_none() {
                handover = HANDOVER_CHANGED.wait(handover).unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            if let Some(surface) = handover.replacement.take() {
                let (width, height) = (target.size)();
                (target.config.width, target.config.height) = (width.max(1), height.max(1));
                surface.configure(&self.device, &target.config);
                target.surface = Some(surface);
            }
        }
        let surface_texture = self.target.as_mut().and_then(|target| {
            let surface = target.surface.as_ref()?;
            let (width, height) = (target.size)();
            if width == 0 || height == 0 {
                return None;
            }
            if (width, height) != (target.config.width, target.config.height) {
                target.config.width = width;
                target.config.height = height;
                surface.configure(&self.device, &target.config);
            }
            match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
                wgpu::CurrentSurfaceTexture::Outdated => {
                    surface.configure(&self.device, &target.config);
                    None
                }
                _ => None,
            }
        });
        if let (Some(texture), Some(target)) = (&surface_texture, &self.target) {
            self.queue.write_buffer(&self.blit_rect, 0, bytemuck::cast_slice(&rect));
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &target.blit.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(source) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 2, resource: self.blit_rect.as_entire_binding() },
                ],
            });
            let view = texture.texture.create_view(&Default::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            // Fit the picture inside the window with bars, not stretched.
            let (sw, sh) = (target.config.width as f32, target.config.height as f32);
            let (vw, vh) = if sw > sh * picture { (sh * picture, sh) } else { (sw, sw / picture) };
            pass.set_viewport((sw - vw) / 2.0, (sh - vh) / 2.0, vw.max(1.0), vh.max(1.0), 0.0, 1.0);
            pass.set_pipeline(&target.blit);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);

            // The on-screen controls go over the whole window, bars included.
            self.overlay_vertices.clear();
            crate::touch::overlay((sw, sh), &mut self.overlay_vertices);
            self.overlay_vertices.truncate(OVERLAY_VERTICES);
            if !self.overlay_vertices.is_empty() {
                self.queue.write_buffer(&self.overlay_buffer, 0, bytemuck::cast_slice(&self.overlay_vertices));
                pass.set_viewport(0.0, 0.0, sw, sh, 0.0, 1.0);
                pass.set_pipeline(&target.overlay);
                pass.set_vertex_buffer(0, self.overlay_buffer.slice(..));
                pass.draw(0..self.overlay_vertices.len() as u32, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        if let Some(texture) = surface_texture {
            self.queue.present(texture);
        }
        drop(handover);
    }

    /// Shows a finished RGBA8 image in the window, bypassing the EFB. Used when the game
    /// writes pixels straight into the display buffer, as its movie player does.
    pub fn present_image(&mut self, w: u32, h: u32, rgba: &[u8]) {
        if self.target.is_none() && !self.presenting {
            return;
        }
        let view = upload_texture(&self.device, &self.queue, w, h, rgba);
        let encoder = self.device.create_command_encoder(&Default::default());
        // Movies and other finished images are 4:3.
        self.show(encoder, &view, [0.0, 0.0, 1.0, 1.0], 4.0 / 3.0);
    }

    /// Debugging aid: writes the texture standing in for the EFB copy at `addr` to a PNG.
    pub fn save_copy(&self, addr: u32, path: &str) {
        let Some((_, texture, w, h)) = self.efb_copies.get(&addr) else { return };
        let rgb = self.read_rgb(texture, 0, 0, *w, *h);
        let _ = std::fs::create_dir_all("screenshots");
        match std::fs::write(path, crate::png::encode_rgb(*w, *h, &rgb)) {
            Ok(()) => eprintln!("[render] wrote {path} ({w}x{h})"),
            Err(e) => eprintln!("[render] cannot write {path}: {e}"),
        }
    }

    /// Reads the shown part of the EFB back and writes it to screenshots/frame_N.png.
    fn screenshot(&self, shown: [u32; 4], number: u64) {
        let (sx, sy) = self.scale;
        let (x, y, w, h) = (shown[0] * sx, shown[1] * sy, shown[2] * sx, shown[3] * sy);
        let (w, h) = (w.min(EFB_WIDTH * sx - x), h.min(EFB_HEIGHT * sy - y));
        let rgb = self.read_rgb(&self.efb, x, y, w, h);
        let _ = std::fs::create_dir_all("screenshots");
        let path = format!("screenshots/frame_{number}.png");
        match std::fs::write(&path, crate::png::encode_rgb(w, h, &rgb)) {
            Ok(()) => eprintln!("[render] wrote {path} ({w}x{h})"),
            Err(e) => eprintln!("[render] cannot write {path}: {e}"),
        }
    }

    /// Reads a rectangle of an RGBA8 texture back from the GPU as tightly packed RGB.
    fn read_rgb(&self, texture: &wgpu::Texture, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
        let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = buffer(&self.device, "screenshot", (row * h) as u64, wgpu::BufferUsages::MAP_READ);
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: None },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);
        readback.map_async(wgpu::MapMode::Read, .., |r| r.expect("cannot map screenshot buffer"));
        self.device.poll(wgpu::PollType::wait_indefinitely()).expect("device poll failed");
        let data = readback.get_mapped_range(..).expect("screenshot buffer not mapped");
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for line in data.chunks_exact(row as usize) {
            for px in line[..(w * 4) as usize].chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
            }
        }
        rgb
    }
}
