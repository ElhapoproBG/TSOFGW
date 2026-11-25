//! Backend gráfico de GPU para TSOFGW, sobre wgpu 30.
//!
//! Dibuja **fuera de pantalla** (sin ventana) en una textura `Rgba8Unorm` con profundidad de 32 bits y
//! devuelve los píxeles con [`GraphicsBackend::read_back`]. Eso permite probarlo en cualquier equipo con
//! Vulkan, DirectX 12 o Metal (incluidos los controladores por software como lavapipe o WARP) y compararlo
//! píxel a píxel con el backend por software de `tsofgw-soft`.
//!
//! # Estado
//! **Este crate todavía no se ha compilado contra la biblioteca real de wgpu en el entorno donde se escribió**
//! (sin acceso a descargar paquetes). La primera integración continua lo comprobó y falló por dos nombres que
//! habían cambiado en wgpu 30: `PollType::wait()` ya no existe (hoy es `wait_indefinitely()`) y
//! `BufferSlice::get_mapped_range()` devuelve un `Result`. Ambos se corrigieron leyendo las firmas en docs.rs
//! (wgpu 30.0.2) y el ejemplo `hello_triangle` de esa versión, y se comprobaron contra una maqueta de la API
//! actualizada; **la prueba definitiva sigue siendo la CI** (trabajo `gpu`, con Vulkan por software).
//!
//! # Qué hace y qué no
//! Hace: mallas con búfer de vértices e índices, una matriz MVP por objeto, prueba de profundidad, lectura de
//! píxeles. No hace todavía: ventana y presentación (`winit` + `Surface`), uniformes dinámicos (hoy se crea un
//! búfer pequeño por objeto y por frame), instancias, texturas ni iluminación.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tsofgw_hal::math::{model_matrix, Mat4};
use tsofgw_hal::{DrawCmd, FrameStats, GraphicsBackend, MeshData, MeshId};
use wgpu::util::DeviceExt;

const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SHADER: &str = include_str!("shader.wgsl");

/// Posición (3 × f32) y color (3 × f32) por vértice: 24 bytes.
const VERTEX_STRIDE: u64 = 24;
const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 2] = [
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 0,
        shader_location: 0,
    },
    wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x3,
        offset: 12,
        shader_location: 1,
    },
];

struct GpuMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

/// Textura de color, vista de profundidad y búfer de lectura, del tamaño actual.
struct Target {
    color: wgpu::Texture,
    depth: wgpu::TextureView,
    readback: wgpu::Buffer,
    /// Bytes por fila de la copia, redondeados al alineamiento que exige wgpu (256).
    padded_row: u32,
}

pub struct WgpuGraphics {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    meshes: Vec<GpuMesh>,
    target: Option<Target>,
    size: (u32, u32),
    view: Mat4,
    proj: Mat4,
    pending: Vec<DrawCmd>,
    frame: u64,
    in_frame: bool,
    adapter_name: String,
}

/// Tiempo máximo que se espera a que la GPU entregue los píxeles antes de rendirse (un controlador roto no
/// debe dejar el proceso colgado para siempre).
const READBACK_TIMEOUT: Duration = Duration::from_secs(30);

fn f32s_to_bytes(values: impl Iterator<Item = f32>) -> Vec<u8> {
    values.flat_map(|f| f.to_le_bytes()).collect()
}

impl WgpuGraphics {
    /// Crea el backend con el mejor adaptador disponible. Falla con un mensaje si no hay ninguno.
    pub fn new() -> Result<Self, String> {
        // Sin ventana no hace falta un manejador de pantalla.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .map_err(|e| format!("no hay un adaptador gráfico disponible: {e:?}"))?;
        let adapter_name = adapter.get_info().name;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| format!("no se pudo abrir el dispositivo gráfico: {e:?}"))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tsofgw shader"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(SHADER)),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tsofgw pipeline"),
            // Diseño automático a partir del shader: el grupo 0 es el uniforme MVP.
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: VERTEX_STRIDE,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &VERTEX_ATTRIBUTES,
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(COLOR_FORMAT.into())],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let mut g = WgpuGraphics {
            device,
            queue,
            pipeline,
            meshes: Vec::new(),
            target: None,
            size: (1, 1),
            view: Mat4::IDENTITY,
            proj: Mat4::IDENTITY,
            pending: Vec::new(),
            frame: 0,
            in_frame: false,
            adapter_name,
        };
        g.resize(1, 1);
        Ok(g)
    }

    /// Nombre del adaptador elegido (por ejemplo, el de la tarjeta gráfica).
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }
}

impl GraphicsBackend for WgpuGraphics {
    fn name(&self) -> &str {
        "wgpu"
    }

    fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        let extent = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let color = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tsofgw color"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: COLOR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = self
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("tsofgw depth"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_row = (w * 4).div_ceil(align) * align;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tsofgw readback"),
            size: padded_row as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.size = (w, h);
        self.target = Some(Target {
            color,
            depth,
            readback,
            padded_row,
        });
    }

    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn create_mesh(&mut self, mesh: &MeshData) -> MeshId {
        assert!(
            mesh.indices.len().is_multiple_of(3),
            "los índices deben formar triángulos"
        );
        assert!(
            mesh.indices
                .iter()
                .all(|&i| (i as usize) < mesh.vertices.len()),
            "índice fuera de rango"
        );
        let vbytes = f32s_to_bytes(
            mesh.vertices
                .iter()
                .flat_map(|v| v.pos.into_iter().chain(v.color)),
        );
        let ibytes: Vec<u8> = mesh.indices.iter().flat_map(|i| i.to_le_bytes()).collect();
        let vertices = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tsofgw vertices"),
                contents: &vbytes,
                usage: wgpu::BufferUsages::VERTEX,
            });
        let indices = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("tsofgw indices"),
                contents: &ibytes,
                usage: wgpu::BufferUsages::INDEX,
            });
        self.meshes.push(GpuMesh {
            vertices,
            indices,
            index_count: mesh.indices.len() as u32,
        });
        (self.meshes.len() - 1) as MeshId
    }

    fn begin_frame(&mut self) {
        assert!(!self.in_frame, "begin_frame sin end_frame");
        self.in_frame = true;
        self.pending.clear();
    }

    fn set_camera(&mut self, view: &Mat4, proj: &Mat4) {
        assert!(self.in_frame, "set_camera fuera de un frame");
        self.view = *view;
        self.proj = *proj;
    }

    fn submit(&mut self, cmds: &[DrawCmd]) {
        assert!(self.in_frame, "submit fuera de un frame");
        for cmd in cmds {
            if let DrawCmd::Mesh { mesh, .. } = cmd {
                assert!(
                    (*mesh as usize) < self.meshes.len(),
                    "malla desconocida: {mesh}"
                );
            }
        }
        self.pending.extend_from_slice(cmds);
    }

    fn end_frame(&mut self) -> FrameStats {
        assert!(self.in_frame, "end_frame sin begin_frame");
        self.in_frame = false;
        self.frame += 1;

        let target = self
            .target
            .as_ref()
            .expect("siempre hay un destino tras new()");
        let (w, h) = self.size;
        let view = target
            .color
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Solo se respeta el primer `Clear` del frame (es la operación de carga del pase de render).
        let clear = self
            .pending
            .iter()
            .find_map(|c| {
                if let DrawCmd::Clear(c) = c {
                    Some(*c)
                } else {
                    None
                }
            })
            .unwrap_or([0.0; 4]);

        // Un uniforme (MVP) y su grupo de enlace por objeto.
        let bind_layout = self.pipeline.get_bind_group_layout(0);
        let view_proj = self.proj.mul(&self.view);
        let mut draws: Vec<(usize, wgpu::BindGroup)> = Vec::new();
        for cmd in &self.pending {
            if let DrawCmd::Mesh { mesh, transform } = cmd {
                // wgpu 30 no admite búferes vacíos en `set_vertex_buffer` / `set_index_buffer`.
                if self.meshes[*mesh as usize].index_count == 0 {
                    continue;
                }
                let mvp = view_proj.mul(&model_matrix(transform));
                let bytes = f32s_to_bytes(mvp.to_cols_array().into_iter());
                let buffer = self
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("tsofgw mvp"),
                        contents: &bytes,
                        usage: wgpu::BufferUsages::UNIFORM,
                    });
                let group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("tsofgw mvp group"),
                    layout: &bind_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    }],
                });
                draws.push((*mesh as usize, group));
            }
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tsofgw frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tsofgw pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0] as f64,
                            g: clear[1] as f64,
                            b: clear[2] as f64,
                            a: clear[3] as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &target.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            for (mesh, group) in &draws {
                let m = &self.meshes[*mesh];
                pass.set_bind_group(0, group, &[]);
                pass.set_vertex_buffer(0, m.vertices.slice(..));
                pass.set_index_buffer(m.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..m.index_count, 0, 0..1);
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target.color,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &target.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(target.padded_row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        FrameStats {
            frame: self.frame,
            draw_calls: draws.len() as u32,
        }
    }

    fn read_back(&mut self) -> Option<Vec<u8>> {
        let target = self.target.as_ref()?;
        let slice = target.readback.slice(..);
        let (tx, rx) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        // `poll` con espera ejecuta la devolución de llamada cuando la GPU termina. Se repite hasta que llegue
        // el resultado, con un plazo: si el controlador falla, se devuelve `None` en vez de colgarse.
        let deadline = Instant::now() + READBACK_TIMEOUT;
        let mapped = loop {
            self.device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
            match rx.try_recv() {
                Ok(result) => break result,
                Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Err(_) => return None,
            }
        };
        mapped.ok()?;

        let (w, h) = self.size;
        let (row, padded) = (w as usize * 4, target.padded_row as usize);
        let mut out = Vec::with_capacity(row * h as usize);
        // Desde wgpu 30 `get_mapped_range` devuelve un `Result`. Pase lo que pase, el búfer se desmapea para
        // que el siguiente frame pueda copiar sobre él.
        let copied = match slice.get_mapped_range() {
            Ok(data) => {
                for y in 0..h as usize {
                    out.extend_from_slice(&data[y * padded..y * padded + row]);
                }
                true
            }
            Err(_) => false,
        };
        target.readback.unmap();
        copied.then_some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsofgw_core::Transform;
    use tsofgw_hal::scene::{render_mono, Scene};
    use tsofgw_soft::{compare, SoftGraphics};

    /// Dibuja la misma escena con la GPU y con el rasterizador por software y exige que coincidan.
    /// Si el equipo no tiene ningún adaptador gráfico, la prueba se omite con un aviso.
    #[test]
    fn coincide_con_el_rasterizador_por_software() {
        let mut gpu = match WgpuGraphics::new() {
            Ok(g) => g,
            Err(e) => {
                // La CI define TSOFGW_REQUIRE_GPU para que la falta de adaptador cuente como fallo.
                assert!(
                    std::env::var_os("TSOFGW_REQUIRE_GPU").is_none(),
                    "se exigía GPU y no hay adaptador: {e}"
                );
                eprintln!("OMITIDA: {e}");
                return;
            }
        };
        eprintln!("adaptador: {}", gpu.adapter_name());
        for name in ["triangulo", "cubo", "mixta"] {
            let scene = Scene::demo(name).unwrap();
            let a = render_mono(&mut gpu, &scene, &Transform::default(), 160, 120)
                .expect("la GPU no devolvió la imagen");
            let b = render_mono(
                &mut SoftGraphics::new(1, 1),
                &scene,
                &Transform::default(),
                160,
                120,
            )
            .unwrap();
            let d = compare::diff(&a, &b, 16).unwrap();
            assert!(d.is_close(), "escena {name}: {d:?}");
        }
    }
}
