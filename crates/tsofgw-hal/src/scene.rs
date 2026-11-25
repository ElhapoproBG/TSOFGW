//! Mallas, escenas de demostración y funciones para dibujarlas con cualquier backend.

use crate::math::{self, Mat4};
use crate::{DrawCmd, FrameStats, GraphicsBackend, XrBackend};
use tsofgw_core::Transform;

pub type MeshId = u32;

/// Vértice con posición y color (lineal, sin gamma).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub color: [f32; 3],
}

#[derive(Debug, Clone, PartialEq)]
pub struct MeshData {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

pub mod shapes {
    use super::{MeshData, Vertex};

    /// Triángulo con un vértice rojo (arriba), uno verde (abajo izquierda) y uno azul (abajo derecha).
    pub fn triangle() -> MeshData {
        MeshData {
            vertices: vec![
                Vertex {
                    pos: [0.0, 0.5, 0.0],
                    color: [1.0, 0.0, 0.0],
                },
                Vertex {
                    pos: [-0.5, -0.5, 0.0],
                    color: [0.0, 1.0, 0.0],
                },
                Vertex {
                    pos: [0.5, -0.5, 0.0],
                    color: [0.0, 0.0, 1.0],
                },
            ],
            indices: vec![0, 1, 2],
        }
    }

    /// Triángulo de un solo color; sirve para pruebas de profundidad.
    pub fn flat_triangle(color: [f32; 3]) -> MeshData {
        let mut m = triangle();
        for v in &mut m.vertices {
            v.color = color;
        }
        m
    }

    /// Cubo de lado 1 centrado en el origen, un color por cara.
    pub fn cube() -> MeshData {
        // (normal, color) de cada cara; las dos tangentes salen de la normal
        let faces: [([f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.9, 0.3, 0.3]),
            ([-1.0, 0.0, 0.0], [0.5, 0.1, 0.1]),
            ([0.0, 1.0, 0.0], [0.3, 0.9, 0.3]),
            ([0.0, -1.0, 0.0], [0.1, 0.5, 0.1]),
            ([0.0, 0.0, 1.0], [0.3, 0.3, 0.9]),
            ([0.0, 0.0, -1.0], [0.1, 0.1, 0.5]),
        ];
        let mut vertices = vec![];
        let mut indices = vec![];
        for (n, color) in faces {
            let u = if n[0] != 0.0 {
                [0.0, 1.0, 0.0]
            } else if n[1] != 0.0 {
                [0.0, 0.0, 1.0]
            } else {
                [1.0, 0.0, 0.0]
            };
            let v = [
                n[1] * u[2] - n[2] * u[1],
                n[2] * u[0] - n[0] * u[2],
                n[0] * u[1] - n[1] * u[0],
            ];
            let base = vertices.len() as u32;
            for (su, sv) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                let pos = [
                    0.5 * (n[0] + su * u[0] + sv * v[0]),
                    0.5 * (n[1] + su * u[1] + sv * v[1]),
                    0.5 * (n[2] + su * u[2] + sv * v[2]),
                ];
                vertices.push(Vertex { pos, color });
            }
            indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        MeshData { vertices, indices }
    }
}

/// Una escena sencilla: mallas y los objetos que las colocan en el mundo.
#[derive(Debug, Clone)]
pub struct Scene {
    pub meshes: Vec<MeshData>,
    /// `(índice de malla, transformación)`
    pub objects: Vec<(usize, Transform)>,
    pub clear: [f32; 4],
}

impl Scene {
    /// Escenas de demostración: `triangulo`, `cubo` y `mixta` (cubo girado con un triángulo delante, para ver la profundidad).
    pub fn demo(name: &str) -> Option<Scene> {
        let clear = [0.05, 0.06, 0.09, 1.0];
        match name {
            "triangulo" => Some(Scene {
                meshes: vec![shapes::triangle()],
                objects: vec![(
                    0,
                    Transform {
                        pos: [0.0, 0.0, -2.0],
                        ..Transform::default()
                    },
                )],
                clear,
            }),
            "cubo" | "mixta" => {
                let rot = math::quat_mul(
                    math::quat_from_axis_angle([1.0, 0.0, 0.0], 0.5),
                    math::quat_from_axis_angle([0.0, 1.0, 0.0], 0.7),
                );
                let mut s = Scene {
                    meshes: vec![shapes::cube(), shapes::triangle()],
                    objects: vec![(
                        0,
                        Transform {
                            pos: [0.0, 0.0, -3.0],
                            rot,
                        },
                    )],
                    clear,
                };
                if name == "mixta" {
                    s.objects.push((
                        1,
                        Transform {
                            pos: [-0.6, -0.2, -1.8],
                            ..Transform::default()
                        },
                    ));
                }
                Some(s)
            }
            _ => None,
        }
    }
}

/// Mallas ya subidas a un backend.
pub struct UploadedScene {
    pub ids: Vec<MeshId>,
}

pub fn upload_scene<G: GraphicsBackend + ?Sized>(g: &mut G, scene: &Scene) -> UploadedScene {
    UploadedScene {
        ids: scene.meshes.iter().map(|m| g.create_mesh(m)).collect(),
    }
}

/// Dibuja un frame completo de la escena con la cámara dada.
pub fn draw_scene<G: GraphicsBackend + ?Sized>(
    g: &mut G,
    scene: &Scene,
    up: &UploadedScene,
    view: &Mat4,
    proj: &Mat4,
) -> FrameStats {
    g.begin_frame();
    g.set_camera(view, proj);
    let mut cmds = vec![DrawCmd::Clear(scene.clear)];
    cmds.extend(scene.objects.iter().map(|(m, t)| DrawCmd::Mesh {
        mesh: up.ids[*m],
        transform: *t,
    }));
    g.submit(&cmds);
    g.end_frame()
}

const NEAR: f32 = 0.05;
const FAR: f32 = 100.0;

/// Renderiza una vista simple (cámara simétrica de 60° vertical) y devuelve los píxeles RGBA8.
/// Devuelve `None` si el backend no permite leer la imagen.
pub fn render_mono<G: GraphicsBackend + ?Sized>(
    g: &mut G,
    scene: &Scene,
    camera: &Transform,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    g.resize(width, height);
    let up = upload_scene(g, scene);
    let fov = math::symmetric_fov(60f32.to_radians(), width as f32 / height as f32);
    draw_scene(
        g,
        scene,
        &up,
        &math::view_matrix(camera),
        &math::perspective(fov, NEAR, FAR),
    );
    g.read_back()
}

/// Renderiza los dos ojos de un visor lado a lado en una sola imagen de `2 * eye_width` × `height`.
/// Es el camino que seguirá un backend OpenXR: pose y campo de visión salen del visor.
pub fn render_stereo<G: GraphicsBackend + ?Sized>(
    g: &mut G,
    scene: &Scene,
    xr: &dyn XrBackend,
    eye_width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    g.resize(eye_width, height);
    let up = upload_scene(g, scene);
    let row = eye_width as usize * 4;
    let mut out = vec![0u8; row * 2 * height as usize];
    for (i, eye) in xr.eyes().iter().enumerate() {
        let (view, proj) = math::eye_matrices(eye, NEAR, FAR);
        draw_scene(g, scene, &up, &view, &proj);
        let px = g.read_back()?;
        for y in 0..height as usize {
            let dst = y * row * 2 + i * row;
            out[dst..dst + row].copy_from_slice(&px[y * row..(y + 1) * row]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn el_cubo_tiene_24_vertices_y_36_indices_validos() {
        let c = shapes::cube();
        assert_eq!((c.vertices.len(), c.indices.len()), (24, 36));
        assert!(c.indices.iter().all(|&i| (i as usize) < c.vertices.len()));
        // todas las posiciones están en la superficie del cubo de lado 1
        for v in &c.vertices {
            let m = v.pos.iter().fold(0.0f32, |a, b| a.max(b.abs()));
            assert!((m - 0.5).abs() < 1e-6, "{:?}", v.pos);
            assert!(v.pos.iter().all(|c| c.abs() <= 0.5 + 1e-6));
        }
        // cada cara es un cuadrado de lado 1: 4 vértices distintos por cara
        for f in 0..6 {
            let cara = &c.vertices[f * 4..f * 4 + 4];
            for i in 0..4 {
                for j in i + 1..4 {
                    assert_ne!(cara[i].pos, cara[j].pos);
                }
            }
        }
    }

    #[test]
    fn escenas_de_demo() {
        assert!(Scene::demo("triangulo").is_some());
        assert_eq!(Scene::demo("cubo").unwrap().objects.len(), 1);
        assert_eq!(Scene::demo("mixta").unwrap().objects.len(), 2);
        assert!(Scene::demo("nada").is_none());
    }
}
