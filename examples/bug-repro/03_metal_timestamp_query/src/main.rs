//! Repro for the last timestamp query before a `resolve_query_set` being lost on newer Apple GPUs.
//!
//! Renders N render passes with synthetic GPU load and times each one on the GPU with timestamp
//! queries, then checks the results are self-consistent. Before the fix in `wgpu-hal/src/metal/`,
//! the *last* timestamp written before the resolve came back as zero or as a stale value from an
//! earlier frame, so the final pass reported a garbage - often negative - duration:
//!
//! ```text
//! BAD  frame   1 | A 0.3911 ms | B 1.1186 ms | C 2.5782 ms | D    -10130375 !!
//! ```
//!
//! Passes before the last one were always fine, which is what identifies this as a race between
//! timestamp write-back and `resolveCounters` rather than anything to do with query indices.
//! Confirmed on Apple M5 Max; the workaround is gated to Apple GPU family Apple8 or newer, the
//! same gate Dawn uses for `MetalSerializeTimestampGenerationAndResolution`.
//!
//! Passing this repro needs real per-pass GPU load: empty passes finish before there is anything
//! for the resolve to race, which is why the pass workload is tunable and non-trivial by default.
//!
//! The window is created hidden ("headless") so running this does not steal focus or cover
//! anything, but a real surface is still created and configured at 1920x1080. When the hidden
//! window has no drawable (macOS reports `Occluded`), the timed passes render to an offscreen
//! target of the same size instead; the GPU work is identical either way.
//!
//! Env knobs:
//!   FRAMES        frames to render (default 60)
//!   ITERS         comma-separated fragment-shader loop count per pass (default "24,96").
//!                 The number of entries is the number of render passes.
//!   WIDTH/HEIGHT  surface size (default 1920x1080)
//!   VISIBLE=1     show the window and use the real swapchain
//!   SPLIT_SUBMIT=1  resolve the query set in a second, separately submitted command buffer
//!                   instead of at the end of the rendering one (diagnostic)

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_flag(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| v != "0" && !v.is_empty())
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    iters: u32,
    seed: f32,
    _pad0: u32,
    _pad1: u32,
}

fn main() {
    env_logger::init();

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
    let mut app = App::default();
    event_loop.run_app(&mut app).unwrap();
}

#[derive(Default)]
struct App {
    state: Option<State>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }

        let width = env_u32("WIDTH", 1920);
        let height = env_u32("HEIGHT", 1080);
        let visible = env_flag("VISIBLE");

        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("gpu-pass-timing")
                        .with_inner_size(PhysicalSize::new(width, height))
                        .with_visible(visible),
                )
                .unwrap(),
        );

        match State::new(window, width, height) {
            Some(state) => self.state = Some(state),
            None => event_loop.exit(),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = &mut self.state else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) if size.width > 0 && size.height > 0 => {
                state.surface_config.width = size.width;
                state.surface_config.height = size.height;
                let config = state.surface_config.clone();
                state.surface.configure(&state.device, &config);
            }
            WindowEvent::RedrawRequested => {
                if !state.frame() {
                    state.report();
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &self.state {
            state.window.request_redraw();
        }
    }
}

struct State {
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    /// Render target used whenever the (hidden) surface has no drawable for us.
    offscreen_view: wgpu::TextureView,
    surface_frames: u32,
    offscreen_frames: u32,

    pipeline: wgpu::RenderPipeline,
    bind_groups: Vec<wgpu::BindGroup>,
    iters: Vec<u32>,
    query_set: wgpu::QuerySet,
    resolve_buffer: wgpu::Buffer,
    readback_buffer: wgpu::Buffer,
    query_count: u32,
    timestamp_period: f32,
    split_submit: bool,

    frames_target: u32,
    frame_index: u32,
    /// Per frame, the raw resolved ticks: `[p0_begin, p0_end, p1_begin, p1_end, ...]`.
    samples: Vec<Vec<u64>>,
}

impl State {
    fn new(window: Arc<Window>, width: u32, height: u32) -> Option<Self> {
        let instance_desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        let instance = wgpu::Instance::new(instance_desc);
        let surface = instance.create_surface(window.clone()).unwrap();

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .expect("no suitable adapter");

        let info = adapter.get_info();
        println!("== adapter =====================================================");
        println!("  name      : {}", info.name);
        println!("  backend   : {:?}", info.backend);
        println!("  device ty : {:?}", info.device_type);

        if !adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            println!("\nERROR: adapter does not advertise TIMESTAMP_QUERY; cannot time passes.");
            return None;
        }

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("timing device"),
            required_features: wgpu::Features::TIMESTAMP_QUERY,
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }))
        .expect("failed to create device");

        device.on_uncaptured_error(Arc::new(|e| panic!("wgpu error: {e}")));

        let timestamp_period = queue.get_timestamp_period();
        println!("  ts period : {timestamp_period} ns/tick");

        let mut surface_config = surface
            .get_default_config(&adapter, width, height)
            .expect("surface not supported by this adapter");
        surface_config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        // No vsync, so frame pacing doesn't muddy what we're measuring.
        let caps = surface.get_capabilities(&adapter);
        for mode in [wgpu::PresentMode::Immediate, wgpu::PresentMode::Mailbox] {
            if caps.present_modes.contains(&mode) {
                surface_config.present_mode = mode;
                break;
            }
        }
        let format = surface_config.format;
        surface.configure(&device, &surface_config);
        println!(
            "  surface   : {width}x{height} {format:?} {:?}",
            surface_config.present_mode
        );

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("params bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("load pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(format.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let iters: Vec<u32> = std::env::var("ITERS")
            .unwrap_or_else(|_| "24,96".to_string())
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        assert!(!iters.is_empty(), "ITERS must list at least one pass");

        let bind_groups: Vec<_> = iters
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let label = format!("params {}", pass_name(i));
                let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(&label),
                    size: size_of::<Params>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                queue.write_buffer(
                    &buffer,
                    0,
                    bytemuck::bytes_of(&Params {
                        iters: n,
                        seed: 0.1 + i as f32 * 0.13,
                        _pad0: 0,
                        _pad1: 0,
                    }),
                );
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some(&label),
                    layout: &bgl,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    }],
                })
            })
            .collect();

        let load_desc: Vec<String> = iters
            .iter()
            .enumerate()
            .map(|(i, n)| format!("{} = {n}", pass_name(i)))
            .collect();
        println!("  passes    : {}", load_desc.join(", "));

        let query_count = iters.len() as u32 * 2;
        let query_bytes = query_count as u64 * 8;
        let query_set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("pass timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: query_count,
        });
        let resolve_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("resolve"),
            size: query_bytes,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: query_bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let offscreen_view = device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("offscreen target"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());

        let split_submit = env_flag("SPLIT_SUBMIT");
        if split_submit {
            println!("  resolve   : separate command buffer, separate submit (SPLIT_SUBMIT)");
        }
        let frames_target = env_u32("FRAMES", 60);
        println!("  frames    : {frames_target}");
        println!("================================================================\n");

        Some(State {
            window,
            device,
            queue,
            surface,
            surface_config,
            offscreen_view,
            surface_frames: 0,
            offscreen_frames: 0,
            pipeline,
            bind_groups,
            iters,
            query_set,
            resolve_buffer,
            readback_buffer,
            query_count,
            timestamp_period,
            split_submit,
            frames_target,
            frame_index: 0,
            samples: Vec::new(),
        })
    }

    /// Renders and times one frame. Returns false when the run is done.
    fn frame(&mut self) -> bool {
        if self.frame_index >= self.frames_target {
            return false;
        }

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => Some(f),
            other => {
                if self.frame_index == 0 && self.offscreen_frames == 0 {
                    println!(
                        "note: surface has no drawable ({other:?}); rendering offscreen at the \
                         same size.\n      (run with VISIBLE=1 to use the real swapchain)\n"
                    );
                }
                None
            }
        };
        let owned_view = frame.as_ref().map(|f| {
            f.texture
                .create_view(&wgpu::TextureViewDescriptor::default())
        });
        let view = match &owned_view {
            Some(v) => {
                self.surface_frames += 1;
                v
            }
            None => {
                self.offscreen_frames += 1;
                &self.offscreen_view
            }
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        for (i, bind_group) in self.bind_groups.iter().enumerate() {
            let label = format!("pass {}", pass_name(i));
            let load = if i == 0 {
                wgpu::LoadOp::Clear(wgpu::Color::BLACK)
            } else {
                wgpu::LoadOp::Load
            };
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(&label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: Some(wgpu::RenderPassTimestampWrites {
                    query_set: &self.query_set,
                    beginning_of_pass_write_index: Some(i as u32 * 2),
                    end_of_pass_write_index: Some(i as u32 * 2 + 1),
                }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        let query_bytes = self.query_count as u64 * 8;
        if self.split_submit {
            self.queue.submit(Some(encoder.finish()));
            let mut enc2 = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("resolve"),
                });
            enc2.resolve_query_set(
                &self.query_set,
                0..self.query_count,
                &self.resolve_buffer,
                0,
            );
            enc2.copy_buffer_to_buffer(
                &self.resolve_buffer,
                0,
                &self.readback_buffer,
                0,
                query_bytes,
            );
            self.queue.submit(Some(enc2.finish()));
        } else {
            encoder.resolve_query_set(
                &self.query_set,
                0..self.query_count,
                &self.resolve_buffer,
                0,
            );
            encoder.copy_buffer_to_buffer(
                &self.resolve_buffer,
                0,
                &self.readback_buffer,
                0,
                query_bytes,
            );
            self.queue.submit(Some(encoder.finish()));
        }

        if let Some(frame) = frame {
            self.queue.present(frame);
        }

        // Wait for the GPU, then read the resolved timestamps back.
        self.readback_buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, |r| r.expect("map failed"));
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();

        let ticks: Vec<u64> = {
            let mapped = self
                .readback_buffer
                .slice(..)
                .get_mapped_range()
                .expect("mapped range");
            bytemuck::cast_slice::<u8, u64>(&mapped).to_vec()
        };
        self.readback_buffer.unmap();

        self.log_frame(&ticks);
        self.samples.push(ticks);
        self.frame_index += 1;
        true
    }

    fn ms(&self, ticks: u64) -> f64 {
        ticks as f64 * self.timestamp_period as f64 / 1.0e6
    }

    fn log_frame(&self, ticks: &[u64]) {
        let mut cells = Vec::new();
        let mut bad = false;
        for i in 0..self.iters.len() {
            let (begin, end) = (ticks[i * 2], ticks[i * 2 + 1]);
            let ok = begin != 0 && end != 0 && end > begin;
            bad |= !ok;
            if ok {
                cells.push(format!("{} {:>8.4} ms", pass_name(i), self.ms(end - begin)));
            } else {
                // Show the signed delta; a stale end timestamp usually reads as negative.
                let delta = end as i128 - begin as i128;
                cells.push(format!("{} {:>11} !!", pass_name(i), delta));
            }
        }
        let raw: Vec<String> = ticks.iter().map(|t| t.to_string()).collect();
        println!(
            "{} frame {:>3} | {} | raw {}",
            if bad { "BAD " } else { "ok  " },
            self.frame_index,
            cells.join(" | "),
            raw.join(" ")
        );
    }

    fn report(&self) {
        println!("\n== summary =====================================================");
        let n = self.samples.len();
        if n == 0 {
            println!("  no frames recorded");
            return;
        }
        let passes = self.iters.len();

        println!("  frames                 : {n}");
        println!(
            "  target                 : {} via swapchain, {} offscreen",
            self.surface_frames, self.offscreen_frames
        );

        let mut problems: Vec<String> = Vec::new();

        for i in 0..passes {
            let mut zero = 0usize;
            let mut nonmono = 0usize;
            let mut stale = 0usize;
            let mut durs: Vec<u64> = Vec::new();
            for (f, s) in self.samples.iter().enumerate() {
                let (begin, end) = (s[i * 2], s[i * 2 + 1]);
                if begin == 0 || end == 0 {
                    zero += 1;
                }
                if end <= begin {
                    nonmono += 1;
                }
                // Byte-identical to the previous frame's value for the same slot: the resolve
                // read a slot the GPU had not written yet.
                if f > 0 && end == self.samples[f - 1][i * 2 + 1] {
                    stale += 1;
                }
                durs.push(end.wrapping_sub(begin));
            }
            let good: Vec<u64> = durs
                .iter()
                .copied()
                .filter(|&d| d != 0 && (d as i64) > 0)
                .collect();
            let mean = if good.is_empty() {
                0.0
            } else {
                good.iter().map(|&x| x as f64).sum::<f64>() / good.len() as f64
            };
            println!(
                "  pass {:<3} ({:>4} iters)  : mean {:>8.4} ms over {}/{} usable frames | \
                 zero {zero} | non-monotonic {nonmono} | stale-repeat {stale}",
                pass_name(i),
                self.iters[i],
                mean * self.timestamp_period as f64 / 1.0e6,
                good.len(),
                n
            );
            if zero > 0 {
                problems.push(format!(
                    "pass {}: {zero}/{n} frames had a zero timestamp",
                    pass_name(i)
                ));
            }
            if nonmono > 0 {
                problems.push(format!(
                    "pass {}: {nonmono}/{n} frames ended before they began",
                    pass_name(i)
                ));
            }
            if stale > 0 {
                problems.push(format!(
                    "pass {}: {stale}/{n} frames repeated the previous frame's end timestamp verbatim",
                    pass_name(i)
                ));
            }
        }

        // With strictly increasing shader load, pass durations should be ordered the same way.
        let means: Vec<f64> = (0..passes)
            .map(|i| {
                let good: Vec<f64> = self
                    .samples
                    .iter()
                    .map(|s| s[i * 2 + 1].wrapping_sub(s[i * 2]))
                    .filter(|&d| d != 0 && (d as i64) > 0)
                    .map(|d| d as f64)
                    .collect();
                if good.is_empty() {
                    0.0
                } else {
                    good.iter().sum::<f64>() / good.len() as f64
                }
            })
            .collect();
        if passes >= 2 {
            for i in 1..passes {
                if self.iters[i] > self.iters[i - 1] * 2 && means[i] < means[i - 1] * 1.5 {
                    problems.push(format!(
                        "pass {} does {:.1}x the work of pass {} but measures only {:.2}x as long",
                        pass_name(i),
                        self.iters[i] as f64 / self.iters[i - 1] as f64,
                        pass_name(i - 1),
                        means[i] / means[i - 1].max(1.0),
                    ));
                }
            }
        }

        println!("\n  verdict:");
        if problems.is_empty() {
            println!("    PASS - GPU pass timings look correct.");
        } else {
            for p in &problems {
                println!("    FAIL - {p}");
            }
        }
        println!("================================================================");
    }
}

fn pass_name(i: usize) -> char {
    (b'A' + i as u8) as char
}
