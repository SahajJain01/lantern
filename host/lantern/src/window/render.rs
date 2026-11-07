//! wgpu surface: NV12 uploaded as an R8 (Y) and an RG8 (UV) texture, converted to RGB in WGSL
//! (BT.709, limited range), drawn into the letterbox rect. Mailbox when available, else Fifo.

use std::sync::Arc;

use anyhow::{Context, Result};
use winit::window::Window;

use super::decode::Nv12Frame;
use super::geometry::{video_rect, Rect};

const SHADER: &str = r#"
struct Params {
    // 1 when the surface is sRGB: the shader must output linear light.
    srgb_out: u32,
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
    // BT.709 limited range.
    let y = (textureSample(y_tex, samp, in.uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let c = (textureSample(uv_tex, samp, in.uv).rg - vec2<f32>(128.0 / 255.0)) * (255.0 / 224.0);
    var rgb = vec3<f32>(
        y + 1.5748 * c.y,
        y - 0.1873 * c.x - 0.4681 * c.y,
        y + 1.8556 * c.x,
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
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut p = [0u8; 16];
        p[0..4].copy_from_slice(&(format.is_srgb() as u32).to_ne_bytes());
        queue.write_buffer(&params, 0, &p);
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
        },
    };
    let text = text.to_uppercase();
    let cols = text.chars().count() as u32 * 6;
    let scale = ((f.w * 6 / 10) / cols.max(1)).max(1);
    let (x0, y0) = ((f.w - (cols * scale).min(f.w)) / 2, (f.h - 7 * scale) / 2);
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
}
