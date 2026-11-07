//! wgpu surface: NV12 uploaded as an R8 (Y) and an RG8 (UV) texture, converted to RGB in WGSL
//! with the stream's matrix and range (see [`colour_params`]), drawn into the letterbox rect. Mailbox when available, else Fifo.

use std::sync::Arc;

use anyhow::{Context, Result};
use winit::window::Window;

use super::decode::{Colour, Matrix, Nv12Frame};
use super::geometry::{video_rect, Rect};

/// The shader's conversion for a [`Colour`]: `y' = (Y - y_off)·y_scale`,
/// `(u', v') = (UV - 128/255)·c_scale`, `R = y' + coef[0]·v'`,
/// `G = y' + coef[1]·u' + coef[2]·v'`, `B = y' + coef[3]·u'`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColourParams {
    pub y_off: f32,
    pub y_scale: f32,
    pub c_scale: f32,
    pub coef: [f32; 4],
}

pub fn colour_params(c: Colour) -> ColourParams {
    let (kr, kb) = match c.matrix {
        Matrix::Bt601 => (0.299, 0.114),
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let (y_off, y_scale, c_scale) = if c.full_range {
        (0.0, 1.0, 1.0)
    } else {
        (16.0 / 255.0, 255.0 / 219.0, 255.0 / 224.0)
    };
    ColourParams {
        y_off,
        y_scale,
        c_scale,
        coef: [
            2.0 * (1.0 - kr),
            -2.0 * kb * (1.0 - kb) / kg,
            -2.0 * kr * (1.0 - kr) / kg,
            2.0 * (1.0 - kb),
        ],
    }
}

/// The `Params` uniform: `srgb_out: u32, y_off, y_scale, c_scale: f32, coef: vec4<f32>`.
fn params_bytes(srgb_out: bool, c: Colour) -> [u8; 32] {
    let p = colour_params(c);
    let mut b = [0u8; 32];
    b[0..4].copy_from_slice(&(srgb_out as u32).to_ne_bytes());
    let floats = [p.y_off, p.y_scale, p.c_scale];
    for (i, v) in floats.into_iter().chain(p.coef).enumerate() {
        b[4 + 4 * i..8 + 4 * i].copy_from_slice(&v.to_ne_bytes());
    }
    b
}

/// CPU reference of the shader's conversion (8-bit YUV in, clamped 0..1 RGB out).
#[cfg(test)]
fn yuv_to_rgb(p: ColourParams, y: u8, u: u8, v: u8) -> [f32; 3] {
    let n = |x: u8| x as f32 / 255.0;
    let yy = (n(y) - p.y_off) * p.y_scale;
    let cu = (n(u) - 128.0 / 255.0) * p.c_scale;
    let cv = (n(v) - 128.0 / 255.0) * p.c_scale;
    [
        yy + p.coef[0] * cv,
        yy + p.coef[1] * cu + p.coef[2] * cv,
        yy + p.coef[3] * cu,
    ]
    .map(|x| x.clamp(0.0, 1.0))
}

const SHADER: &str = r#"
struct Params {
    // 1 when the surface is sRGB: the shader must output linear light.
    srgb_out: u32,
    // The stream's matrix and range (see `ColourParams`).
    y_off: f32,
    y_scale: f32,
    c_scale: f32,
    coef: vec4<f32>,
};
@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;
@group(0) @binding(3) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // One triangle covering the viewport.
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    out.uv = vec2<f32>(p.x, 1.0 - p.y);
    return out;
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let y = (textureSample(y_tex, samp, in.uv).r - params.y_off) * params.y_scale;
    let c = (textureSample(uv_tex, samp, in.uv).rg - vec2<f32>(128.0 / 255.0)) * params.c_scale;
    let k = params.coef;
    var rgb = vec3<f32>(
        y + k.x * c.y,
        y + k.y * c.x + k.z * c.y,
        y + k.w * c.x,
    );
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    if (params.srgb_out == 1u) {
        rgb = to_linear(rgb);
    }
    return vec4<f32>(rgb, 1.0);
}
"#;

struct Planes {
    w: u32,
    h: u32,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind: wgpu::BindGroup,
}

pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    /// The colour the `params` uniform currently holds.
    colour: Colour,
    planes: Option<Planes>,
}

impl Renderer {
    /// `display` is the event loop's display (needed by the GL fallback on Wayland).
    pub fn new(
        window: Arc<Window>,
        display: winit::event_loop::OwnedDisplayHandle,
    ) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            ..wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display))
        });
        let surface = instance
            .create_surface(window.clone())
            .context("create wgpu surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("no GPU adapter for the window surface")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("lantern"),
            ..Default::default()
        }))
        .context("request GPU device")?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&device, &config);
        tracing::info!(adapter = %adapter.get_info().name, ?format, ?present_mode, "renderer ready");

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let colour = Colour::default();
        queue.write_buffer(&params, 0, &params_bytes(format.is_srgb(), colour));
        Ok(Self {
            window,
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            sampler,
            params,
            colour,
            planes: None,
        })
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 || (w, h) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    /// Size of the current video, if a frame was uploaded.
    pub fn video_size(&self) -> Option<(u32, u32)> {
        self.planes.as_ref().map(|p| (p.w, p.h))
    }

    /// The video's rect in the window, for pointer mapping.
    pub fn video_rect(&self) -> Option<Rect> {
        let v = self.video_size()?;
        Some(video_rect((self.config.width, self.config.height), v))
    }

    fn make_planes(&self, w: u32, h: u32) -> Planes {
        let tex = |label, w, h, format| {
            self.device.create_texture(&wgpu::TextureDescriptor {
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
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let y = tex("y", w, h, wgpu::TextureFormat::R8Unorm);
        let uv = tex(
            "uv",
            w.div_ceil(2),
            h.div_ceil(2),
            wgpu::TextureFormat::Rg8Unorm,
        );
        let yv = y.create_view(&Default::default());
        let uvv = uv.create_view(&Default::default());
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&yv),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uvv),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.params.as_entire_binding(),
                },
            ],
        });
        Planes { w, h, y, uv, bind }
    }

    /// Uploads a frame, recreating the textures when the video size changed.
    pub fn upload(&mut self, f: &Nv12Frame) {
        if self.video_size() != Some((f.w, f.h)) {
            tracing::debug!(w = f.w, h = f.h, "video size changed; new textures");
            self.planes = Some(self.make_planes(f.w, f.h));
        }
        if f.colour != self.colour {
            tracing::debug!(colour = ?f.colour, "stream colour");
            self.colour = f.colour;
            let bytes = params_bytes(self.config.format.is_srgb(), f.colour);
            self.queue.write_buffer(&self.params, 0, &bytes);
        }
        let p = self.planes.as_ref().expect("just made");
        let write = |tex: &wgpu::Texture, data: &[u8], stride: u32, w: u32, h: u32| {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        write(&p.y, &f.y, f.y_stride, f.w, f.h);
        write(&p.uv, &f.uv, f.uv_stride, f.w.div_ceil(2), f.h.div_ceil(2));
    }

    /// Draws the current video letterboxed on black (just black before the first frame).
    pub fn draw(&mut self) {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            other => {
                tracing::debug!(?other, "no surface texture this time");
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("video"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let (Some(p), Some(r)) = (self.planes.as_ref(), self.video_rect()) {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &p.bind, &[]);
                pass.set_viewport(r.x, r.y, r.w.max(1.0), r.h.max(1.0), 0.0, 1.0);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
        self.window.pre_present_notify();
        self.queue.present(frame);
    }
}

/// 5×7 glyphs for the status messages (uppercase only).
fn glyph(c: char) -> [u8; 7] {
    match c {
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        _ => [0; 7],
    }
}

/// A status picture: `base` dimmed (or black 640×400), with `text` centered in white.
pub fn message_frame(base: Option<&Nv12Frame>, text: &str) -> Nv12Frame {
    let mut f = match base {
        Some(b) => {
            let mut f = b.clone();
            for y in f.y.iter_mut() {
                *y = 16 + (y.saturating_sub(16)) / 4;
            }
            f
        }
        None => Nv12Frame {
            w: 640,
            h: 400,
            y: vec![16; 640 * 400],
            uv: vec![128; 640 * 200],
            y_stride: 640,
            uv_stride: 640,
            colour: Colour::default(),
        },
    };
    let text = text.to_uppercase();
    let cols = text.chars().count() as u32 * 6;
    let scale = ((f.w * 6 / 10) / cols.max(1)).max(1);
    let (x0, y0) = (
        (f.w - (cols * scale).min(f.w)) / 2,
        f.h.saturating_sub(7 * scale) / 2,
    );
    for (i, c) in text.chars().enumerate() {
        let g = glyph(c);
        for (row, bits) in g.iter().enumerate() {
            for col in 0..5u32 {
                if bits & (1 << (4 - col)) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = x0 + (i as u32 * 6 + col) * scale + dx;
                        let y = y0 + row as u32 * scale + dy;
                        if x < f.w && y < f.h {
                            f.y[(y * f.y_stride + x) as usize] = 235;
                            let ci = ((y / 2) * f.uv_stride + (x / 2) * 2) as usize;
                            f.uv[ci] = 128;
                            f.uv[ci + 1] = 128;
                        }
                    }
                }
            }
        }
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_frame_draws_white_text_on_black() {
        let f = message_frame(None, "Phone disconnected");
        assert_eq!((f.w, f.h), (640, 400));
        let white = f.y.iter().filter(|&&v| v == 235).count();
        assert!(white > 1000, "{white} text pixels");
        assert_eq!(f.y[0], 16);
    }

    #[test]
    fn message_frame_on_a_tiny_frame_does_not_underflow() {
        let base = Nv12Frame {
            w: 64,
            h: 4,
            y: vec![100; 64 * 4],
            uv: vec![128; 64 * 2],
            y_stride: 64,
            uv_stride: 64,
            colour: Colour::default(),
        };
        let f = message_frame(Some(&base), "Phone disconnected");
        assert_eq!((f.w, f.h), (64, 4));
    }

    #[test]
    fn params_uniform_layout_matches_the_wgsl_struct() {
        let c = Colour {
            matrix: Matrix::Bt601,
            full_range: false,
        };
        let b = params_bytes(true, c);
        let f = |o: usize| f32::from_ne_bytes(b[o..o + 4].try_into().unwrap());
        assert_eq!(u32::from_ne_bytes(b[0..4].try_into().unwrap()), 1);
        let p = colour_params(c);
        assert_eq!((f(4), f(8), f(12)), (p.y_off, p.y_scale, p.c_scale));
        // vec4 is 16-byte aligned in WGSL uniforms.
        assert_eq!([f(16), f(20), f(24), f(28)], p.coef);
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.02)
    }

    fn params(matrix: Matrix, full_range: bool) -> ColourParams {
        colour_params(Colour { matrix, full_range })
    }

    #[test]
    fn colour_params_limited_range_levels() {
        for m in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020] {
            let p = params(m, false);
            assert!(close(yuv_to_rgb(p, 16, 128, 128), [0.0; 3]), "{m:?}");
            assert!(close(yuv_to_rgb(p, 235, 128, 128), [1.0; 3]), "{m:?}");
        }
    }

    #[test]
    fn colour_params_full_range_levels() {
        let p = params(Matrix::Bt601, true);
        assert!(close(yuv_to_rgb(p, 0, 128, 128), [0.0; 3]));
        assert!(close(yuv_to_rgb(p, 255, 128, 128), [1.0; 3]));
        assert!(close(yuv_to_rgb(p, 128, 128, 128), [0.502; 3]));
        // Limited-range expansion would clip full-range near-white to 1.0.
        let lim = yuv_to_rgb(params(Matrix::Bt601, false), 245, 128, 128);
        assert!(close(lim, [1.0; 3]));
        assert!(close(yuv_to_rgb(p, 245, 128, 128), [0.961; 3]));
    }

    #[test]
    fn colour_params_primaries_per_matrix() {
        // Pure red/blue encoded with each standard's matrix decode back to pure.
        // BT.601 limited: red = (81, 90, 240), blue = (41, 240, 110).
        let p = params(Matrix::Bt601, false);
        let red = yuv_to_rgb(p, 81, 90, 240);
        assert!(close(red, [1.0, 0.0, 0.0]), "{red:?}");
        assert!(close(yuv_to_rgb(p, 41, 240, 110), [0.0, 0.0, 1.0]));
        // BT.709 limited: red = (63, 102, 240).
        let p = params(Matrix::Bt709, false);
        assert!(close(yuv_to_rgb(p, 63, 102, 240), [1.0, 0.0, 0.0]));
        // BT.601 full: red = (76, 85, 255).
        let p = params(Matrix::Bt601, true);
        assert!(close(yuv_to_rgb(p, 76, 85, 255), [1.0, 0.0, 0.0]));
        // Decoding BT.601 red with BT.709 is visibly off (the bug being fixed).
        let wrong = yuv_to_rgb(params(Matrix::Bt709, false), 81, 90, 240);
        assert!(!close(wrong, [1.0, 0.0, 0.0]), "{wrong:?}");
    }
}
