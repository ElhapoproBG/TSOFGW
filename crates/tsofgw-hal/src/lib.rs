//! Capa de abstracción de hardware (HAL) de TSOFGW.
//!
//! Define las interfaces que cada plataforma implementa (gráficos, XR, entrada) y los perfiles
//! de ejecución (`vr`, `pc`, `consola`) con sus presupuestos. Los backends reales (wgpu, OpenXR,
//! SDK de consolas) viven en crates aparte y solo necesitan implementar estos `trait`s.
//!
//! Los backends incluidos aquí son *headless*: no dibujan nada, sirven para pruebas, servidores
//! dedicados y para verificar la lógica del motor sin hardware.

pub mod math;
pub mod scene;

pub use math::Mat4;
pub use scene::{MeshData, MeshId, Scene, Vertex};
use std::collections::VecDeque;
use std::sync::Arc;
use tsofgw_core::{fn_node, Contract, Cost, DataType, Node, Transform, Value};

// ------------------------------------------------------------- perfiles

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Vr,
    Pc,
    Console,
}

/// Perfil de ejecución: frecuencia objetivo y presupuesto por frame.
///
/// Los valores por defecto son puntos de partida razonables, **no mediciones**: hay que calibrarlos
/// con hardware real de cada plataforma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub platform: Platform,
    pub name: &'static str,
    pub target_hz: u32,
    pub mem_kb: u32,
}

impl Profile {
    pub const VR: Profile = Profile {
        platform: Platform::Vr,
        name: "vr",
        target_hz: 90,
        mem_kb: 3_000_000,
    };
    pub const VR_HIGH: Profile = Profile {
        platform: Platform::Vr,
        name: "vr-120",
        target_hz: 120,
        mem_kb: 6_000_000,
    };
    pub const PC: Profile = Profile {
        platform: Platform::Pc,
        name: "pc",
        target_hz: 60,
        mem_kb: 8_000_000,
    };
    pub const CONSOLE: Profile = Profile {
        platform: Platform::Console,
        name: "consola",
        target_hz: 60,
        mem_kb: 6_000_000,
    };

    pub fn by_name(name: &str) -> Option<Profile> {
        [Self::VR, Self::VR_HIGH, Self::PC, Self::CONSOLE]
            .into_iter()
            .find(|p| p.name == name)
    }

    /// Duración de un frame en microsegundos.
    pub fn frame_us(&self) -> u32 {
        1_000_000 / self.target_hz
    }

    /// Presupuesto del grafo por tick. Se deja un margen del 20 % para el sistema operativo y el compositor.
    pub fn budget(&self) -> Cost {
        let us = self.frame_us() * 80 / 100;
        Cost {
            cpu_us: us,
            gpu_us: us,
            mem_kb: self.mem_kb,
        }
    }
}

// ------------------------------------------------------------ interfaces

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Eye {
    pub transform: Transform,
    /// Ángulos del campo de visión en radianes: izquierda, derecha, arriba, abajo.
    pub fov: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DrawCmd {
    Clear([f32; 4]),
    Mesh { mesh: u32, transform: Transform },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameStats {
    pub frame: u64,
    pub draw_calls: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    Button { id: u16, pressed: bool },
    Axis { id: u16, value: f32 },
}

pub trait GraphicsBackend: Send {
    fn name(&self) -> &str;
    fn resize(&mut self, width: u32, height: u32);
    fn size(&self) -> (u32, u32);
    /// Sube una malla al backend y devuelve su identificador.
    fn create_mesh(&mut self, mesh: &MeshData) -> MeshId;
    fn begin_frame(&mut self);
    /// Cámara del frame en curso: del mundo al ojo y del ojo al clip (profundidad `[0, 1]`).
    fn set_camera(&mut self, view: &Mat4, proj: &Mat4);
    fn submit(&mut self, cmds: &[DrawCmd]);
    fn end_frame(&mut self) -> FrameStats;
    /// Píxeles RGBA8 (filas de arriba abajo) del último frame terminado, si el backend puede entregarlos.
    fn read_back(&mut self) -> Option<Vec<u8>> {
        None
    }
}

pub trait XrBackend: Send {
    fn name(&self) -> &str;
    /// Pose actual de la cabeza (ya predicha para el momento de mostrar el frame).
    fn head_pose(&mut self) -> Transform;
    fn eyes(&self) -> [Eye; 2];
    /// Entrega los dos ojos al compositor del visor.
    fn submit_stereo(&mut self, frame: u64);
}

pub trait InputBackend: Send {
    fn name(&self) -> &str;
    fn poll(&mut self) -> Vec<InputEvent>;
}

// --------------------------------------------------------------- headless

#[derive(Default)]
pub struct HeadlessGraphics {
    pub size: (u32, u32),
    pub last_frame: Vec<DrawCmd>,
    pub meshes: Vec<MeshData>,
    frame: u64,
    in_frame: bool,
}

impl GraphicsBackend for HeadlessGraphics {
    fn name(&self) -> &str {
        "headless"
    }
    fn resize(&mut self, width: u32, height: u32) {
        self.size = (width, height);
    }
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn create_mesh(&mut self, mesh: &MeshData) -> MeshId {
        self.meshes.push(mesh.clone());
        (self.meshes.len() - 1) as MeshId
    }
    fn set_camera(&mut self, _view: &Mat4, _proj: &Mat4) {
        assert!(self.in_frame, "set_camera fuera de un frame");
    }
    fn begin_frame(&mut self) {
        assert!(!self.in_frame, "begin_frame sin end_frame");
        self.in_frame = true;
        self.last_frame.clear();
    }
    fn submit(&mut self, cmds: &[DrawCmd]) {
        assert!(self.in_frame, "submit fuera de un frame");
        self.last_frame.extend_from_slice(cmds);
    }
    fn end_frame(&mut self) -> FrameStats {
        assert!(self.in_frame, "end_frame sin begin_frame");
        self.in_frame = false;
        self.frame += 1;
        FrameStats {
            frame: self.frame,
            draw_calls: self
                .last_frame
                .iter()
                .filter(|c| matches!(c, DrawCmd::Mesh { .. }))
                .count() as u32,
        }
    }
}

/// Visor simulado: la pose se fija desde fuera.
pub struct HeadlessXr {
    pose: Transform,
    pub submitted: Vec<u64>,
}

impl HeadlessXr {
    pub fn new() -> Self {
        HeadlessXr {
            pose: Transform::default(),
            submitted: vec![],
        }
    }
    pub fn set_pose(&mut self, pose: Transform) {
        self.pose = pose;
    }
}

impl Default for HeadlessXr {
    fn default() -> Self {
        Self::new()
    }
}

impl XrBackend for HeadlessXr {
    fn name(&self) -> &str {
        "headless-xr"
    }
    fn head_pose(&mut self) -> Transform {
        self.pose
    }
    fn eyes(&self) -> [Eye; 2] {
        // separación interpupilar de 64 mm
        let mut l = self.pose;
        let mut r = self.pose;
        l.pos[0] -= 0.032;
        r.pos[0] += 0.032;
        let fov = [-0.9, 0.9, 0.9, -0.9];
        [Eye { transform: l, fov }, Eye { transform: r, fov }]
    }
    fn submit_stereo(&mut self, frame: u64) {
        self.submitted.push(frame);
    }
}

/// Entrada con eventos programados.
#[derive(Default)]
pub struct ScriptedInput {
    queue: VecDeque<InputEvent>,
}

impl ScriptedInput {
    pub fn push(&mut self, e: InputEvent) {
        self.queue.push_back(e);
    }
}

impl InputBackend for ScriptedInput {
    fn name(&self) -> &str {
        "scripted-input"
    }
    fn poll(&mut self) -> Vec<InputEvent> {
        self.queue.drain(..).collect()
    }
}

// -------------------------------------------------------- nodos del grafo

/// Nodo que publica la pose de la cabeza del visor en el puerto `head`.
pub fn xr_pose_node(mut xr: Box<dyn XrBackend>) -> Box<dyn Node> {
    fn_node(
        Contract::new("xr_pose", (1, 0))
            .output("head", DataType::Transform)
            .cap("xr")
            .cost(Cost {
                cpu_us: 50,
                gpu_us: 0,
                mem_kb: 64,
            }),
        move |io| io.set_output("head", Value::Transform(xr.head_pose())),
    )
}

/// Nodo que publica los identificadores de botones pulsados en este tick en el puerto `pressed` (`Events`).
pub fn input_node(mut input: Box<dyn InputBackend>) -> Box<dyn Node> {
    fn_node(
        Contract::new("input", (1, 0))
            .output("pressed", DataType::Events)
            .cap("input")
            .cost(Cost {
                cpu_us: 20,
                gpu_us: 0,
                mem_kb: 16,
            }),
        move |io| {
            let ids: Vec<u64> = input
                .poll()
                .into_iter()
                .filter_map(|e| match e {
                    InputEvent::Button { id, pressed: true } => Some(id as u64),
                    _ => None,
                })
                .collect();
            io.set_output("pressed", Value::Events(Arc::from(ids)));
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tsofgw_core::{Graph, Link};

    #[test]
    fn perfiles_y_presupuestos() {
        assert_eq!(Profile::VR.frame_us(), 11_111);
        assert_eq!(Profile::VR_HIGH.frame_us(), 8_333);
        assert_eq!(Profile::PC.frame_us(), 16_666);
        assert_eq!(Profile::VR.budget().cpu_us, 8_888);
        assert!(Profile::VR_HIGH.budget().cpu_us < Profile::VR.budget().cpu_us);
        assert_eq!(Profile::by_name("consola"), Some(Profile::CONSOLE));
        assert_eq!(Profile::by_name("nada"), None);
        assert_eq!(Profile::CONSOLE.platform, Platform::Console);
    }

    #[test]
    fn grafico_headless_cuenta_frames_y_dibujos() {
        let mut g = HeadlessGraphics::default();
        g.resize(1920, 1080);
        assert_eq!(g.size, (1920, 1080));
        g.begin_frame();
        g.submit(&[
            DrawCmd::Clear([0.0; 4]),
            DrawCmd::Mesh {
                mesh: 1,
                transform: Transform::default(),
            },
        ]);
        g.submit(&[DrawCmd::Mesh {
            mesh: 2,
            transform: Transform::default(),
        }]);
        let s = g.end_frame();
        assert_eq!(
            s,
            FrameStats {
                frame: 1,
                draw_calls: 2
            }
        );
        g.begin_frame();
        assert!(g.last_frame.is_empty());
        assert_eq!(g.end_frame().frame, 2);
    }

    #[test]
    #[should_panic(expected = "fuera de un frame")]
    fn submit_fuera_de_frame_es_error() {
        HeadlessGraphics::default().submit(&[]);
    }

    #[test]
    fn xr_ojos_separados_y_entrega() {
        let mut xr = HeadlessXr::new();
        let p = Transform {
            pos: [1.0, 1.7, 2.0],
            ..Transform::default()
        };
        xr.set_pose(p);
        assert_eq!(xr.head_pose(), p);
        let [l, r] = xr.eyes();
        assert!((r.transform.pos[0] - l.transform.pos[0] - 0.064).abs() < 1e-6);
        xr.submit_stereo(7);
        assert_eq!(xr.submitted, vec![7]);
    }

    #[test]
    fn nodos_de_hal_en_el_grafo() {
        let mut xr = HeadlessXr::new();
        let p = Transform {
            pos: [0.0, 1.6, 0.0],
            ..Transform::default()
        };
        xr.set_pose(p);
        let mut input = ScriptedInput::default();
        input.push(InputEvent::Button {
            id: 3,
            pressed: true,
        });
        input.push(InputEvent::Button {
            id: 4,
            pressed: false,
        });
        input.push(InputEvent::Axis { id: 0, value: 0.5 });

        let mut g = Graph::new();
        let pose = g.add(xr_pose_node(Box::new(xr)), &[]).unwrap();
        let inp = g.add(input_node(Box::new(input)), &[]).unwrap();
        let consumidor = g
            .add(
                fn_node(
                    Contract::new("consumidor", (1, 0)).input("head", DataType::Transform),
                    |_| {},
                ),
                &[Link {
                    from: pose,
                    output: "head",
                    input: "head",
                }],
            )
            .unwrap();
        g.set_demanded(consumidor, true);
        g.set_demanded(inp, true);
        g.set_budget(Some(Profile::VR.budget()));
        g.tick();
        assert_eq!(g.output(pose, "head"), Some(&Value::Transform(p)));
        assert_eq!(
            g.output(inp, "pressed"),
            Some(&Value::Events(Arc::from(vec![3u64])))
        );
        assert!(g.shed().is_empty());
        assert_eq!(g.capability_report()["xr_pose"], vec!["xr"]);
    }
}
