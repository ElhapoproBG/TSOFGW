//! Demo de MMO mínimo: une grafo, mundo, red y HAL en un solo escenario medible.

use std::time::Instant;
use tsofgw_core::{fn_node, Contract, Cost, DataType, Graph, Link, Node, NodeId, NodeIo, Value};
use tsofgw_hal::{xr_pose_node, HeadlessXr, Profile};
use tsofgw_net::{quantize, EntityState, Snapshot};
use tsofgw_world::{EntityId, Interest, Rng, Vec3, World};

pub struct DemoConfig {
    pub profile: Profile,
    pub entities: u64,
    pub players: u64,
    pub ticks: u32,
    pub seed: u64,
}

impl Default for DemoConfig {
    fn default() -> Self {
        DemoConfig {
            profile: Profile::VR,
            entities: 50_000,
            players: 100,
            ticks: 60,
            seed: 1,
        }
    }
}

const AREA: f32 = 5000.0; // el mundo va de -AREA a +AREA en x y z
const RADIUS: f32 = 150.0; // radio de interés de cada jugador

/// Servidor de mundo como un nodo del grafo: simula solo donde hay jugadores, calcula interés
/// y mide lo que costaría enviar el estado al jugador 0 (completo vs delta).
struct MmoServer {
    world: World,
    interest: Interest,
    rng: Rng,
    players: u64,
    baseline: Snapshot,
    tick: u32,
}

impl MmoServer {
    fn new(cfg: &DemoConfig) -> Self {
        let mut world = World::new(50.0);
        let mut rng = Rng::new(cfg.seed);
        for id in 0..cfg.entities {
            let p = Vec3::new(
                rng.range(-AREA, AREA),
                rng.range(0.0, 10.0),
                rng.range(-AREA, AREA),
            );
            world.spawn(id, p);
        }
        MmoServer {
            world,
            interest: Interest::new(1.2),
            rng,
            players: cfg.players,
            baseline: Snapshot::default(),
            tick: 0,
        }
    }
}

impl Node for MmoServer {
    fn contract(&self) -> Contract {
        Contract::new("mmo_server", (1, 0))
            .output("simulated", DataType::Int)
            .output("total", DataType::Int)
            .output("visible", DataType::Int)
            .output("enters", DataType::Int)
            .output("leaves", DataType::Int)
            .output("delta_bytes", DataType::Int)
            .output("full_bytes", DataType::Int)
            .cap("net")
            .cost(Cost {
                cpu_us: 3000,
                gpu_us: 0,
                mem_kb: 200_000,
            })
    }

    fn tick(&mut self, io: &mut NodeIo) {
        self.tick += 1;
        let observers: Vec<(EntityId, f32)> = (0..self.players).map(|id| (id, RADIUS)).collect();

        // 1. Simulación selectiva: solo las celdas cercanas a algún jugador.
        let spheres: Vec<(Vec3, f32)> = observers
            .iter()
            .filter_map(|&(id, r)| self.world.position(id).map(|p| (p, r * 1.5)))
            .collect();
        let cells = self.world.active_cells(&spheres);
        let sim_ids = self.world.entities_in_cells(&cells);
        for &id in &sim_ids {
            if let Some(p) = self.world.position(id) {
                let q = Vec3::new(
                    (p.x + self.rng.range(-2.0, 2.0)).clamp(-AREA, AREA),
                    p.y,
                    (p.z + self.rng.range(-2.0, 2.0)).clamp(-AREA, AREA),
                );
                self.world.move_to(id, q);
            }
        }

        // 2. Interés.
        let deltas = self.interest.update(&self.world, &observers);
        let (enters, leaves) = deltas
            .values()
            .fold((0, 0), |(e, l), d| (e + d.enter.len(), l + d.leave.len()));

        // 3. Red: snapshot del jugador 0 (se asume la línea base anterior confirmada).
        let mut snap = Snapshot {
            tick: self.tick,
            ..Default::default()
        };
        for id in self.interest.visible(0) {
            if let Some(p) = self.world.position(id) {
                snap.entities.insert(
                    id,
                    EntityState {
                        pos: [quantize(p.x), quantize(p.y), quantize(p.z)],
                        data: 0,
                    },
                );
            }
        }
        let full = snap.encode_full().len();
        let delta = if self.tick == 1 {
            full
        } else {
            snap.encode_delta(&self.baseline).len()
        };
        self.baseline = snap;

        let visible = self.baseline.entities.len();
        for (name, v) in [
            ("simulated", sim_ids.len()),
            ("total", self.world.len()),
            ("visible", visible),
            ("enters", enters),
            ("leaves", leaves),
            ("delta_bytes", delta),
            ("full_bytes", full),
        ] {
            io.set_output(name, Value::Int(v as i64));
        }
    }
}

/// Un consumidor que declara su costo y no hace nada más: representa render, HUD, analítica…
fn consumidor(name: &'static str, input: &'static str, cost: Cost) -> Box<dyn Node> {
    fn_node(
        Contract::new(name, (1, 0))
            .input(input, DataType::Int)
            .cost(cost),
        |_| {},
    )
}

pub struct DemoGraph {
    pub graph: Graph,
    pub server: NodeId,
}

pub fn build_graph(cfg: &DemoConfig) -> DemoGraph {
    let mut g = Graph::new();
    let link = |from, output, input| Link {
        from,
        output,
        input,
    };
    let server = g.add(Box::new(MmoServer::new(cfg)), &[]).unwrap();
    let xr = g
        .add(xr_pose_node(Box::new(HeadlessXr::new())), &[])
        .unwrap();

    let render = g
        .add(
            fn_node(
                Contract::new("render_vr", (1, 0))
                    .input("head", DataType::Transform)
                    .input("visible", DataType::Int)
                    .cost(Cost {
                        cpu_us: 1500,
                        gpu_us: 6000,
                        mem_kb: 800_000,
                    }),
                |_| {},
            ),
            &[link(xr, "head", "head"), link(server, "visible", "visible")],
        )
        .unwrap();
    let net = g
        .add(
            consumidor(
                "net_send",
                "delta_bytes",
                Cost {
                    cpu_us: 800,
                    gpu_us: 0,
                    mem_kb: 20_000,
                },
            ),
            &[link(server, "delta_bytes", "delta_bytes")],
        )
        .unwrap();
    let hud = g
        .add(
            consumidor(
                "hud",
                "visible",
                Cost {
                    cpu_us: 2500,
                    gpu_us: 500,
                    mem_kb: 50_000,
                },
            ),
            &[link(server, "visible", "visible")],
        )
        .unwrap();
    let analytics = g
        .add(
            consumidor(
                "analytics",
                "simulated",
                Cost {
                    cpu_us: 4000,
                    gpu_us: 0,
                    mem_kb: 100_000,
                },
            ),
            &[link(server, "simulated", "simulated")],
        )
        .unwrap();

    for (id, prio) in [(render, 10), (net, 9), (hud, 1), (analytics, 0)] {
        g.set_demanded(id, true);
        g.set_priority(id, prio);
    }
    g.set_budget(Some(cfg.profile.budget()));
    DemoGraph { graph: g, server }
}

pub struct DemoReport {
    pub profile: Profile,
    pub budget: Cost,
    pub active: Vec<String>,
    pub shed: Vec<String>,
    pub active_cost: Cost,
    pub total_entities: f64,
    pub simulated_avg: f64,
    pub visible_avg: f64,
    pub enters_per_tick: f64,
    pub leaves_per_tick: f64,
    pub delta_bytes_avg: f64,
    pub full_bytes_avg: f64,
    pub real_us_per_tick: f64,
}

pub fn run_demo(cfg: &DemoConfig) -> DemoReport {
    let DemoGraph { mut graph, server } = build_graph(cfg);
    let (mut sim, mut vis, mut delta, mut full) = (0.0, 0.0, 0.0, 0.0);
    let (mut enters, mut leaves) = (0i64, 0i64);
    let mut total = 0.0;
    let start = Instant::now();
    for t in 0..cfg.ticks {
        graph.tick();
        let get = |g: &Graph, n: &str| match g.output(server, n) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        sim += get(&graph, "simulated") as f64;
        total = get(&graph, "total") as f64;
        vis += get(&graph, "visible") as f64;
        if t > 0 {
            // el primer tick solo registra la aparición inicial de todo
            enters += get(&graph, "enters");
            leaves += get(&graph, "leaves");
        }
        delta += get(&graph, "delta_bytes") as f64;
        full += get(&graph, "full_bytes") as f64;
    }
    let real_us_per_tick = start.elapsed().as_micros() as f64 / cfg.ticks.max(1) as f64;
    let n = cfg.ticks.max(1) as f64;

    let names: Vec<(NodeId, String)> = (0..graph.len() as NodeId)
        .map(|i| (i, graph.contract(i).name.to_string()))
        .collect();
    let active = names
        .iter()
        .filter(|(i, _)| graph.state(*i) == tsofgw_core::State::Active)
        .map(|(_, n)| n.clone())
        .collect();
    let shed = graph
        .shed()
        .iter()
        .map(|i| graph.contract(*i).name.to_string())
        .collect();
    DemoReport {
        profile: cfg.profile,
        budget: cfg.profile.budget(),
        active,
        shed,
        active_cost: graph.active_cost(),
        total_entities: total,
        simulated_avg: sim / n,
        visible_avg: vis / n,
        enters_per_tick: enters as f64 / (cfg.ticks.max(2) - 1) as f64,
        leaves_per_tick: leaves as f64 / (cfg.ticks.max(2) - 1) as f64,
        delta_bytes_avg: delta / n,
        full_bytes_avg: full / n,
        real_us_per_tick,
    }
}

impl DemoReport {
    pub fn render(&self, cfg: &DemoConfig) -> String {
        let pct = |a: f64, b: f64| if b > 0.0 { 100.0 * a / b } else { 0.0 };
        format!(
            "TSOFGW demo — perfil {} ({} Hz, presupuesto cpu {} µs / gpu {} µs por tick)\n\
             Mundo: {} entidades, {} jugadores, {} ticks\n\
             Nodos activos: {}\n\
             Descartados por presupuesto: {}\n\
             Costo declarado activo: cpu {} µs, gpu {} µs\n\
             Simulación selectiva: {:.0} de {:.0} entidades por tick ({:.1} %)\n\
             Interés: {:.1} entidades visibles para el jugador 0; {:.1} entradas y {:.1} salidas por tick (suma de todos los jugadores)\n\
             Red (jugador 0): snapshot completo {:.0} B vs delta {:.0} B por tick ({:.1} %)\n\
             Tiempo real en esta máquina: {:.0} µs por tick\n\
             (peor caso: cada entidad simulada se mueve en cada tick)\n",
            self.profile.name,
            self.profile.target_hz,
            self.budget.cpu_us,
            self.budget.gpu_us,
            cfg.entities,
            cfg.players,
            cfg.ticks,
            self.active.join(", "),
            if self.shed.is_empty() { "ninguno".to_string() } else { self.shed.join(", ") },
            self.active_cost.cpu_us,
            self.active_cost.gpu_us,
            self.simulated_avg,
            self.total_entities,
            pct(self.simulated_avg, self.total_entities),
            self.visible_avg,
            self.enters_per_tick,
            self.leaves_per_tick,
            self.full_bytes_avg,
            self.delta_bytes_avg,
            pct(self.delta_bytes_avg, self.full_bytes_avg),
            self.real_us_per_tick,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(profile: Profile) -> DemoConfig {
        DemoConfig {
            profile,
            entities: 20_000,
            players: 20,
            ticks: 12,
            seed: 7,
        }
    }

    #[test]
    fn el_perfil_vr_descarta_analitica_y_pc_no() {
        let vr = run_demo(&cfg(Profile::VR));
        assert_eq!(vr.shed, vec!["analytics"]);
        assert!(vr.active.contains(&"render_vr".to_string()));
        assert!(!vr.active.contains(&"analytics".to_string()));
        assert!(vr.active_cost.cpu_us <= vr.budget.cpu_us);

        let pc = run_demo(&cfg(Profile::PC));
        assert!(pc.shed.is_empty());
        assert!(pc.active.contains(&"analytics".to_string()));
    }

    #[test]
    fn simula_una_fraccion_y_el_delta_ahorra_red() {
        let r = run_demo(&cfg(Profile::PC));
        assert!(
            r.simulated_avg < r.total_entities * 0.5,
            "simuló {} de {}",
            r.simulated_avg,
            r.total_entities
        );
        assert!(r.visible_avg > 0.0);
        assert!(
            r.delta_bytes_avg < r.full_bytes_avg,
            "delta {} vs completo {}",
            r.delta_bytes_avg,
            r.full_bytes_avg
        );
        assert!(r.real_us_per_tick > 0.0);
    }

    #[test]
    fn es_determinista_con_la_misma_semilla() {
        let a = run_demo(&cfg(Profile::PC));
        let b = run_demo(&cfg(Profile::PC));
        assert_eq!(a.simulated_avg, b.simulated_avg);
        assert_eq!(a.visible_avg, b.visible_avg);
        assert_eq!(
            (a.enters_per_tick, a.leaves_per_tick),
            (b.enters_per_tick, b.leaves_per_tick)
        );
        assert_eq!(a.delta_bytes_avg, b.delta_bytes_avg);
    }

    #[test]
    fn el_informe_se_imprime() {
        let c = cfg(Profile::VR);
        let t = run_demo(&c).render(&c);
        assert!(t.contains("perfil vr (90 Hz"));
        assert!(t.contains("Descartados por presupuesto: analytics"));
    }
}
