//! Backend gráfico por software (CPU) de TSOFGW.
//!
//! Sirve para tres cosas: dibujar sin GPU (servidores, CI, equipos sin controlador), tener una
//! **imagen de referencia** contra la que comparar los backends de GPU, y desarrollar la lógica de
//! escena sin depender de ninguna API gráfica.
//!
//! Qué hace: transformación con las matrices de `tsofgw_hal::math`, recorte contra el plano cercano,
//! rasterización con coordenadas baricéntricas, color interpolado con corrección de perspectiva y
//! z-buffer de 32 bits. No hace: iluminación, texturas, recorte lateral (se recorta por rectángulo de
//! pantalla), antialiasing ni conversión sRGB (los colores se escriben tal cual, como `Rgba8Unorm`).

pub mod compare;
pub mod png;

use tsofgw_hal::math::model_matrix;
use tsofgw_hal::{DrawCmd, FrameStats, GraphicsBackend, Mat4, MeshData, MeshId};

#[derive(Clone, Copy)]
struct ClipVertex {
    clip: [f32; 4],
    color: [f32; 3],
}

pub struct SoftGraphics {
    width: u32,
    height: u32,
    color: Vec<u8>,
    depth: Vec<f32>,
    meshes: Vec<MeshData>,
    view: Mat4,
    proj: Mat4,
    frame: u64,
    in_frame: bool,
    draw_calls: u32,
}

impl Default for SoftGraphics {
    fn default() -> Self {
        SoftGraphics::new(1, 1)
    }
}

impl SoftGraphics {
    pub fn new(width: u32, height: u32) -> Self {
        let n = width.max(1) as usize * height.max(1) as usize;
        SoftGraphics {
            width: width.max(1),
            height: height.max(1),
            color: vec![0; n * 4],
            depth: vec![1.0; n],
            meshes: vec![],
            view: Mat4::IDENTITY,
            proj: Mat4::IDENTITY,
            frame: 0,
            in_frame: false,
            draw_calls: 0,
        }
    }

    /// Píxeles RGBA8 del framebuffer, filas de arriba abajo.
    pub fn pixels(&self) -> &[u8] {
        &self.color
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        [
            self.color[i],
            self.color[i + 1],
            self.color[i + 2],
            self.color[i + 3],
        ]
    }

    pub fn to_png(&self) -> Vec<u8> {
        png::encode_rgba(self.width, self.height, &self.color)
    }

    fn clear(&mut self, c: [f32; 4]) {
        let px = [to_u8(c[0]), to_u8(c[1]), to_u8(c[2]), to_u8(c[3])];
        for p in self.color.chunks_exact_mut(4) {
            p.copy_from_slice(&px);
        }
        self.depth.fill(1.0);
    }

    fn draw_mesh(&mut self, mesh: usize, model: &Mat4) {
        let mvp = self.proj.mul(&self.view).mul(model);
        let mut triangles: Vec<[ClipVertex; 3]> = Vec::new();
        {
            let m = &self.meshes[mesh];
            let verts: Vec<ClipVertex> = m
                .vertices
                .iter()
                .map(|v| ClipVertex {
                    clip: mvp.mul_vec4([v.pos[0], v.pos[1], v.pos[2], 1.0]),
                    color: v.color,
                })
                .collect();
            for tri in m.indices.chunks_exact(3) {
                let poly = vec![
                    verts[tri[0] as usize],
                    verts[tri[1] as usize],
                    verts[tri[2] as usize],
                ];
                // plano cercano (z >= 0) y w positivo
                let poly = clip_polygon(poly, |v| v.clip[2]);
                let poly = clip_polygon(poly, |v| v.clip[3] - 1e-6);
                for i in 1..poly.len().saturating_sub(1) {
                    triangles.push([poly[0], poly[i], poly[i + 1]]);
                }
            }
        }
        for [a, b, c] in triangles {
            self.raster(a, b, c);
        }
    }

    fn raster(&mut self, a: ClipVertex, b: ClipVertex, c: ClipVertex) {
        let (w, h) = (self.width as f32, self.height as f32);
        // posición en pantalla y 1/w
        let sv = |v: &ClipVertex| {
            let iw = 1.0 / v.clip[3];
            (
                (v.clip[0] * iw * 0.5 + 0.5) * w,
                (0.5 - v.clip[1] * iw * 0.5) * h,
                v.clip[2] * iw,
                iw,
            )
        };
        let (pa, pb, pc) = (sv(&a), sv(&b), sv(&c));
        let edge = |p: (f32, f32, f32, f32), q: (f32, f32, f32, f32), x: f32, y: f32| {
            (x - p.0) * (q.1 - p.1) - (y - p.1) * (q.0 - p.0)
        };
        let area = edge(pa, pb, pc.0, pc.1);
        if area == 0.0 || !area.is_finite() {
            return;
        }
        let min_x = pa.0.min(pb.0).min(pc.0).floor().max(0.0) as i64;
        let max_x = (pa.0.max(pb.0).max(pc.0).ceil() as i64).min(self.width as i64 - 1);
        let min_y = pa.1.min(pb.1).min(pc.1).floor().max(0.0) as i64;
        let max_y = (pa.1.max(pb.1).max(pc.1).ceil() as i64).min(self.height as i64 - 1);
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                // pesos baricéntricos (dividir por el área normaliza el signo según el sentido del triángulo)
                let w0 = edge(pb, pc, px, py) / area;
                let w1 = edge(pc, pa, px, py) / area;
                let w2 = edge(pa, pb, px, py) / area;
                if w0 < -1e-6 || w1 < -1e-6 || w2 < -1e-6 {
                    continue;
                }
                let z = w0 * pa.2 + w1 * pb.2 + w2 * pc.2;
                let idx = y as usize * self.width as usize + x as usize;
                if !(0.0..=1.0).contains(&z) || z >= self.depth[idx] {
                    continue;
                }
                self.depth[idx] = z;
                // color con corrección de perspectiva
                let (u0, u1, u2) = (w0 * pa.3, w1 * pb.3, w2 * pc.3);
                let s = u0 + u1 + u2;
                for k in 0..3 {
                    let v = (u0 * a.color[k] + u1 * b.color[k] + u2 * c.color[k]) / s;
                    self.color[idx * 4 + k] = to_u8(v);
                }
                self.color[idx * 4 + 3] = 255;
            }
        }
    }
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

/// Recorta un polígono convexo contra el semiespacio `dist(v) >= 0` (Sutherland–Hodgman).
fn clip_polygon(poly: Vec<ClipVertex>, dist: impl Fn(&ClipVertex) -> f32) -> Vec<ClipVertex> {
    let mut out = Vec::with_capacity(poly.len() + 1);
    for i in 0..poly.len() {
        let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
        let (dp, dq) = (dist(&p), dist(&q));
        if dp >= 0.0 {
            out.push(p);
        }
        if (dp >= 0.0) != (dq >= 0.0) {
            let t = dp / (dp - dq);
            let lerp = |a: f32, b: f32| a + (b - a) * t;
            out.push(ClipVertex {
                clip: [
                    lerp(p.clip[0], q.clip[0]),
                    lerp(p.clip[1], q.clip[1]),
                    lerp(p.clip[2], q.clip[2]),
                    lerp(p.clip[3], q.clip[3]),
                ],
                color: [
                    lerp(p.color[0], q.color[0]),
                    lerp(p.color[1], q.color[1]),
                    lerp(p.color[2], q.color[2]),
                ],
            });
        }
    }
    out
}

impl GraphicsBackend for SoftGraphics {
    fn name(&self) -> &str {
        "software"
    }
    fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        self.width = w;
        self.height = h;
        self.color = vec![0; w as usize * h as usize * 4];
        self.depth = vec![1.0; w as usize * h as usize];
    }
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
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
        self.meshes.push(mesh.clone());
        (self.meshes.len() - 1) as MeshId
    }
    fn begin_frame(&mut self) {
        assert!(!self.in_frame, "begin_frame sin end_frame");
        self.in_frame = true;
        self.draw_calls = 0;
        self.color.fill(0);
        self.depth.fill(1.0);
    }
    fn set_camera(&mut self, view: &Mat4, proj: &Mat4) {
        assert!(self.in_frame, "set_camera fuera de un frame");
        self.view = *view;
        self.proj = *proj;
    }
    fn submit(&mut self, cmds: &[DrawCmd]) {
        assert!(self.in_frame, "submit fuera de un frame");
        for cmd in cmds {
            match cmd {
                DrawCmd::Clear(c) => self.clear(*c),
                DrawCmd::Mesh { mesh, transform } => {
                    assert!(
                        (*mesh as usize) < self.meshes.len(),
                        "malla desconocida: {mesh}"
                    );
                    self.draw_mesh(*mesh as usize, &model_matrix(transform));
                    self.draw_calls += 1;
                }
            }
        }
    }
    fn end_frame(&mut self) -> FrameStats {
        assert!(self.in_frame, "end_frame sin begin_frame");
        self.in_frame = false;
        self.frame += 1;
        FrameStats {
            frame: self.frame,
            draw_calls: self.draw_calls,
        }
    }
    fn read_back(&mut self) -> Option<Vec<u8>> {
        Some(self.color.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsofgw_core::Transform;
    use tsofgw_hal::math::{perspective, symmetric_fov, view_matrix};
    use tsofgw_hal::scene::{render_mono, render_stereo, shapes, Scene};
    use tsofgw_hal::{DrawCmd, HeadlessXr};

    const CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

    fn camara() -> (Mat4, Mat4) {
        (
            view_matrix(&Transform::default()),
            perspective(symmetric_fov(60f32.to_radians(), 1.0), 0.05, 100.0),
        )
    }

    fn en(pos: [f32; 3]) -> Transform {
        Transform {
            pos,
            ..Transform::default()
        }
    }

    fn dibujar(g: &mut SoftGraphics, mallas: &[MeshData], objetos: &[(usize, Transform)]) {
        let ids: Vec<MeshId> = mallas.iter().map(|m| g.create_mesh(m)).collect();
        let (v, p) = camara();
        g.begin_frame();
        g.set_camera(&v, &p);
        let mut cmds = vec![DrawCmd::Clear(CLEAR)];
        cmds.extend(objetos.iter().map(|(m, t)| DrawCmd::Mesh {
            mesh: ids[*m],
            transform: *t,
        }));
        g.submit(&cmds);
        g.end_frame();
    }

    fn cerca(a: u8, b: u8, tol: u8) -> bool {
        a.abs_diff(b) <= tol
    }

    #[test]
    fn el_triangulo_aparece_en_el_centro_con_los_colores_interpolados() {
        let mut g = SoftGraphics::new(64, 64);
        dibujar(&mut g, &[shapes::triangle()], &[(0, en([0.0, 0.0, -2.0]))]);
        // esquina: color de fondo
        assert_eq!(g.pixel(0, 0), [0, 0, 0, 255]);
        assert_eq!(g.pixel(63, 63), [0, 0, 0, 255]);
        // centro del triángulo: mitad rojo (vértice de arriba), un cuarto verde y un cuarto azul
        let [r, gr, b, a] = g.pixel(32, 32);
        assert_eq!(a, 255);
        assert!(
            cerca(r, 128, 14) && cerca(gr, 64, 14) && cerca(b, 64, 14),
            "centro = {:?}",
            [r, gr, b]
        );
        // cerca del vértice de abajo a la izquierda domina el verde; abajo a la derecha, el azul
        let abajo_izq = g.pixel(22, 41);
        let abajo_der = g.pixel(41, 41);
        assert!(
            abajo_izq[1] > abajo_izq[0] && abajo_izq[1] > abajo_izq[2],
            "{abajo_izq:?}"
        );
        assert!(
            abajo_der[2] > abajo_der[0] && abajo_der[2] > abajo_der[1],
            "{abajo_der:?}"
        );
        // y la imagen no es solo fondo
        let cubiertos = g
            .pixels()
            .chunks_exact(4)
            .filter(|p| p[..3] != [0, 0, 0])
            .count();
        assert!(
            cubiertos > 300 && cubiertos < 64 * 64 / 2,
            "{cubiertos} píxeles cubiertos"
        );
    }

    #[test]
    fn el_triangulo_se_ve_arriba_a_la_izquierda_de_donde_estaba_al_trasladarlo() {
        let mut g = SoftGraphics::new(64, 64);
        dibujar(
            &mut g,
            &[shapes::flat_triangle([1.0, 1.0, 1.0])],
            &[(0, en([-0.6, 0.4, -2.0]))],
        );
        assert_eq!(g.pixel(14, 20), [255, 255, 255, 255]); // dentro, a la izquierda y arriba
        assert_eq!(g.pixel(50, 50), [0, 0, 0, 255]);
        // hacia +X aparece a la derecha
        let mut h = SoftGraphics::new(64, 64);
        dibujar(
            &mut h,
            &[shapes::flat_triangle([1.0, 1.0, 1.0])],
            &[(0, en([0.6, 0.4, -2.0]))],
        );
        assert_eq!(h.pixel(50, 20), [255, 255, 255, 255]);
        assert_eq!(h.pixel(14, 20), [0, 0, 0, 255]);
    }

    #[test]
    fn el_z_buffer_hace_que_lo_cercano_tape_a_lo_lejano_sin_importar_el_orden() {
        let rojo = shapes::flat_triangle([1.0, 0.0, 0.0]);
        let verde = shapes::flat_triangle([0.0, 1.0, 0.0]);
        for orden in [[(0, -2.0), (1, -4.0)], [(1, -4.0), (0, -2.0)]] {
            let mut g = SoftGraphics::new(64, 64);
            let objs: Vec<(usize, Transform)> =
                orden.iter().map(|&(m, z)| (m, en([0.0, 0.0, z]))).collect();
            dibujar(&mut g, &[rojo.clone(), verde.clone()], &objs);
            assert_eq!(
                g.pixel(32, 36),
                [255, 0, 0, 255],
                "el rojo está más cerca y debe verse, orden {orden:?}"
            );
        }
    }

    #[test]
    fn recorte_cercano_un_triangulo_a_medias_detras_de_la_camara_no_revienta() {
        let mut g = SoftGraphics::new(64, 64);
        // un triángulo grande en el plano z = 0 de la cámara: cruza el plano cercano
        let enorme = MeshData {
            vertices: vec![
                tsofgw_hal::Vertex {
                    pos: [-1.0, -1.0, 5.0],
                    color: [1.0, 1.0, 1.0],
                },
                tsofgw_hal::Vertex {
                    pos: [1.0, -1.0, 5.0],
                    color: [1.0, 1.0, 1.0],
                },
                tsofgw_hal::Vertex {
                    pos: [0.0, -1.0, -20.0],
                    color: [1.0, 1.0, 1.0],
                },
            ],
            indices: vec![0, 1, 2],
        };
        // el suelo está un poco por debajo de la cámara: se ve la mitad inferior de la imagen
        dibujar(&mut g, &[enorme], &[(0, en([0.0, 0.0, 0.0]))]);
        let abajo = (0..64)
            .filter(|&x| g.pixel(x, 60) == [255, 255, 255, 255])
            .count();
        let arriba = (0..64)
            .filter(|&x| g.pixel(x, 2) == [255, 255, 255, 255])
            .count();
        assert!(abajo > 0, "debería verse el suelo abajo");
        assert_eq!(arriba, 0);
        // todo detrás de la cámara: no se dibuja nada
        let mut g = SoftGraphics::new(32, 32);
        dibujar(
            &mut g,
            &[shapes::flat_triangle([1.0, 1.0, 1.0])],
            &[(0, en([0.0, 0.0, 3.0]))],
        );
        assert!(g.pixels().chunks_exact(4).all(|p| p[..3] == [0, 0, 0]));
    }

    #[test]
    fn el_cubo_se_ve_con_varias_caras_de_colores_distintos() {
        let mut g = SoftGraphics::new(96, 96);
        let escena = Scene::demo("cubo").unwrap();
        let px = render_mono(&mut g, &escena, &Transform::default(), 96, 96).unwrap();
        let mut colores: Vec<[u8; 3]> = px
            .chunks_exact(4)
            .map(|p| [p[0], p[1], p[2]])
            .filter(|c| *c != [13, 15, 23])
            .collect();
        colores.sort_unstable();
        colores.dedup();
        assert!(
            colores.len() >= 3,
            "se esperaban al menos 3 caras de color, hay {}",
            colores.len()
        );
        // el cubo no llena la imagen: las esquinas son fondo (0.05, 0.06, 0.09 → 13, 15, 23)
        assert_eq!(g.pixel(0, 0), [13, 15, 23, 255]);
    }

    #[test]
    fn la_escena_mixta_deja_el_triangulo_delante_del_cubo() {
        let mut g = SoftGraphics::new(128, 128);
        let escena = Scene::demo("mixta").unwrap();
        let px = render_mono(&mut g, &escena, &Transform::default(), 128, 128).unwrap();
        // el triángulo (z = -1.8) está más cerca que el cubo (z = -3): su vértice rojo domina arriba
        // y, con el color mezclado, hay píxeles muy rojos que el cubo solo (rojo 0.9, 0.3, 0.3) no tiene
        let muy_rojos = px
            .chunks_exact(4)
            .filter(|p| p[0] > 240 && p[1] < 40 && p[2] < 40)
            .count();
        assert!(muy_rojos > 0);
    }

    #[test]
    fn estereo_dos_ojos_lado_a_lado_con_paralaje() {
        let mut g = SoftGraphics::new(1, 1);
        let escena = Scene::demo("triangulo").unwrap();
        let xr = HeadlessXr::new();
        let (w, h) = (80u32, 80u32);
        let px = render_stereo(&mut g, &escena, &xr, w, h).unwrap();
        assert_eq!(px.len(), (w * 2 * h * 4) as usize);
        // en cada mitad el triángulo cubre píxeles; y no es la misma imagen (paralaje entre ojos)
        let mitad = |ojo: usize| -> Vec<u8> {
            (0..h as usize)
                .flat_map(|y| {
                    px[(y * 2 * w as usize + ojo * w as usize) * 4
                        ..(y * 2 * w as usize + ojo * w as usize + w as usize) * 4]
                        .to_vec()
                })
                .collect()
        };
        let (izq, der) = (mitad(0), mitad(1));
        let cubiertos = |m: &Vec<u8>| m.chunks_exact(4).filter(|p| p[..3] != [13, 15, 23]).count();
        assert!(cubiertos(&izq) > 100 && cubiertos(&der) > 100);
        assert_ne!(izq, der, "los ojos deben ver imágenes distintas");
        // el ojo izquierdo está 3,2 cm a la izquierda: el objeto cercano se desplaza hacia la derecha en su imagen
        let centro_x = |m: &Vec<u8>| {
            let (suma, n) = m
                .chunks_exact(4)
                .enumerate()
                .filter(|(_, p)| p[..3] != [13, 15, 23])
                .fold((0usize, 0usize), |(s, n), (i, _)| {
                    (s + i % w as usize, n + 1)
                });
            suma as f32 / n as f32
        };
        assert!(
            centro_x(&izq) > centro_x(&der),
            "izq {} der {}",
            centro_x(&izq),
            centro_x(&der)
        );
    }

    #[test]
    fn el_png_exportado_se_vuelve_a_leer_igual() {
        let mut g = SoftGraphics::new(48, 32);
        let escena = Scene::demo("triangulo").unwrap();
        render_mono(&mut g, &escena, &Transform::default(), 48, 32).unwrap();
        let (w, h, px) = png::decode_rgba(&g.to_png()).unwrap();
        assert_eq!((w, h), (48, 32));
        assert_eq!(px, g.pixels());
    }

    #[test]
    fn el_contrato_de_frames_se_respeta() {
        let mut g = SoftGraphics::new(8, 8);
        let id = g.create_mesh(&shapes::triangle());
        let (v, p) = camara();
        g.begin_frame();
        g.set_camera(&v, &p);
        g.submit(&[
            DrawCmd::Mesh {
                mesh: id,
                transform: en([0.0, 0.0, -2.0]),
            },
            DrawCmd::Mesh {
                mesh: id,
                transform: en([0.0, 0.0, -3.0]),
            },
        ]);
        assert_eq!(
            g.end_frame(),
            FrameStats {
                frame: 1,
                draw_calls: 2
            }
        );
        assert_eq!(g.size(), (8, 8));
        assert_eq!(g.name(), "software");
    }

    #[test]
    #[should_panic(expected = "malla desconocida")]
    fn dibujar_una_malla_inexistente_es_un_error() {
        let mut g = SoftGraphics::new(8, 8);
        g.begin_frame();
        g.submit(&[DrawCmd::Mesh {
            mesh: 5,
            transform: Transform::default(),
        }]);
    }

    #[test]
    #[should_panic(expected = "índice fuera de rango")]
    fn una_malla_con_indices_invalidos_se_rechaza() {
        let mut g = SoftGraphics::new(8, 8);
        let mut m = shapes::triangle();
        m.indices = vec![0, 1, 9];
        g.create_mesh(&m);
    }
}
