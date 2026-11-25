//! Banco de pruebas de un cluster: varios shards en un proceso, un bus simulado entre ellos, una red
//! simulada hacia los clientes y bots que cruzan los bordes una y otra vez. Produce un informe medido y
//! permite comprobar los invariantes del reparto (ningún NPC ni jugador se pierde o se duplica, y lo
//! que ve un jugador junto al borde coincide con la verdad del conjunto).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Instant;

use tsofgw_transport::{
    Authority, ClientConfig, DeterministicEntropy, Link, ServerConfig, SimConfig, SimLink, SimNet,
};

use crate::bus::{SimBus, SimBusConfig};
use crate::client::NewConnection;
use crate::partition::{Partition, ShardId};
use crate::server::{ClusterConfig, ClusterStats};
use crate::sim::HALF_EXTENT_Q;
use crate::{Bot, GameClient, GameConfig, GameServer, NPC_BASE};
use tsofgw_net::dequantize;

#[derive(Debug, Clone)]
pub struct ClusterScenario {
    /// Rejilla de shards (columnas × filas).
    pub grid: (u16, u16),
    pub clients: usize,
    /// Cuántos de los clientes patrullan cruzando un borde (el resto deambula).
    pub patrollers: usize,
    pub patrol_ms: u64,
    pub duration_ms: u64,
    /// Tiempo extra sin pérdida y con los bots quietos, para comprobar que todo converge.
    pub settle_ms: u64,
    /// Red entre clientes y shards.
    pub net: SimConfig,
    /// Red entre shards.
    pub bus: SimBusConfig,
    /// `npc_count` es el **total** del mundo, repartido entre los shards.
    pub game: GameConfig,
    pub step_ms: u64,
    pub master: Vec<u8>,
}

impl Default for ClusterScenario {
    fn default() -> Self {
        ClusterScenario {
            grid: (2, 2),
            clients: 40,
            patrollers: 20,
            patrol_ms: 8000,
            duration_ms: 30_000,
            settle_ms: 4000,
            net: SimConfig {
                latency_ms: 30,
                jitter_ms: 10,
                loss: 0.02,
                duplicate: 0.0,
                seed: 1,
            },
            bus: SimBusConfig {
                latency_ms: 10,
                jitter_ms: 5,
                loss: 0.02,
                duplicate: 0.0,
                seed: 7,
            },
            game: GameConfig {
                npc_count: 4000,
                // Aparecen cerca del cruce de las regiones: así los bordes se cruzan de verdad.
                spawn_half_extent: 40.0,
                ..GameConfig::default()
            },
            step_ms: 10,
            master: b"cluster-master-key".to_vec(),
        }
    }
}

/// Resultado de comprobar los invariantes del reparto en un instante.
#[derive(Debug, Clone, Default)]
pub struct Invariants {
    pub npcs: usize,
    /// NPC de los que ningún shard es responsable (nunca debe ocurrir, ni siquiera con pérdida).
    pub npc_lost: usize,
    /// NPC con más de un responsable (solo es normal mientras un traspaso espera su acuse).
    pub npc_duplicated: usize,
    pub players_playing: usize,
    pub players_lost: usize,
    pub players_duplicated: usize,
    /// Mensajes aún en vuelo por el bus y traspasos sin acuse en todo el cluster.
    pub bus_in_flight: usize,
    pub pending_transfers: usize,
    pub views_checked: usize,
    /// Entidades que debería ver un jugador (verdad del cluster, bien dentro de su radio) y no estaban.
    pub view_missing: usize,
    /// Entidades que ve un jugador y no existen en ningún shard (o están muy lejos).
    pub view_phantoms: usize,
    /// Mayor diferencia (unidades) entre lo que ve un jugador y la verdad del cluster.
    pub view_error_max: f32,
}

impl Invariants {
    /// ¿El reparto es perfecto? (solo debe exigirse cuando la red está en calma y no hay traspasos en curso)
    pub fn is_exact(&self) -> bool {
        self.npc_lost == 0
            && self.npc_duplicated == 0
            && self.players_lost == 0
            && self.players_duplicated == 0
    }
}

/// Un cluster en marcha. Las pruebas pueden avanzarlo paso a paso e inspeccionarlo.
pub struct ClusterRun {
    pub net: SimNet,
    pub bus: SimBus,
    pub partition: Partition,
    pub shards: Vec<GameServer<SimLink>>,
    pub bots: Vec<Bot<SimLink>>,
    pub now: u64,
    pub step_ms: u64,
    scenario: ClusterScenario,
    frozen: bool,
    wall: Instant,
}

impl ClusterRun {
    pub fn new(s: &ClusterScenario) -> Result<ClusterRun, String> {
        let partition = Partition::grid(s.grid.0, s.grid.1, dequantize(HALF_EXTENT_Q))?;
        let n = partition.shard_count();
        let net = SimNet::new(s.net.clone());
        let bus = SimBus::new(n, s.bus.clone());
        let authority = Authority::new(&s.master);
        let mut shards = Vec::with_capacity(n);
        for sh in 0..n as ShardId {
            let cluster = ClusterConfig::new(sh, partition.clone(), &s.game)?;
            shards.push(GameServer::new_shard(
                net.endpoint(),
                authority.clone(),
                s.game.clone(),
                ServerConfig::default(),
                Box::new(DeterministicEntropy(
                    s.net.seed ^ 0xabcd ^ ((sh as u64) << 32),
                )),
                cluster,
                Box::new(bus.end(sh)),
            )?);
        }
        let addrs: Vec<_> = shards.iter().map(|g| g.link().addr()).collect();

        let mut bots = Vec::with_capacity(s.clients);
        for i in 0..s.clients {
            let id = 1000 + i as u64;
            // Un servicio de entrada elegiría el shard según dónde aparece la cuenta.
            let spawn = GameServer::<SimLink>::spawn_position(&s.game, id);
            let home = partition.owner_of(tsofgw_world::Vec3::new(
                dequantize(spawn[0]),
                0.0,
                dequantize(spawn[2]),
            ));
            let (net2, addrs2, seed) = (net.clone(), addrs.clone(), s.net.seed);
            let mut attempts = 0u64;
            let client = GameClient::new(
                net.endpoint(),
                addrs[home as usize],
                authority.issue(id),
                ClientConfig::default(),
                Box::new(DeterministicEntropy(s.net.seed.wrapping_mul(31) + i as u64)),
            )
            .with_connector(Box::new(move |shard| {
                attempts += 1;
                addrs2.get(shard as usize).map(|&server| NewConnection {
                    link: net2.endpoint(),
                    server,
                    entropy: Box::new(DeterministicEntropy(
                        seed ^ ((i as u64) << 24) ^ attempts.wrapping_mul(0x9e37),
                    )),
                })
            }));
            let mut bot = Bot::new(client, s.net.seed.wrapping_add(i as u64 * 7919));
            if i < s.patrollers {
                bot = bot.with_patrol(patrol_dir(i), s.patrol_ms);
            }
            bots.push(bot);
        }
        Ok(ClusterRun {
            net,
            bus,
            partition,
            shards,
            bots,
            now: 0,
            step_ms: s.step_ms,
            scenario: s.clone(),
            frozen: false,
            wall: Instant::now(),
        })
    }

    /// Avanza un paso de reloj: cada shard y cada cliente se actualizan una vez.
    pub fn step(&mut self) {
        self.now += self.step_ms;
        self.net.set_now(self.now);
        for sh in &mut self.shards {
            sh.update(self.now);
        }
        for b in &mut self.bots {
            b.update(self.now);
        }
    }

    pub fn run_for(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.step();
        }
    }

    /// Detiene a los bots y quita la pérdida de las dos redes: a partir de aquí todo debe converger.
    pub fn settle(&mut self) {
        if self.frozen {
            return;
        }
        self.frozen = true;
        for b in &mut self.bots {
            b.freeze();
        }
        let mut quiet = self.scenario.net.clone();
        quiet.loss = 0.0;
        quiet.duplicate = 0.0;
        self.net.set_config(quiet);
        let mut qb = self.scenario.bus.clone();
        qb.loss = 0.0;
        qb.duplicate = 0.0;
        self.bus.set_config(qb);
    }

    /// Comprueba el reparto ahora mismo. Con `view` también compara lo que ve cada jugador con la verdad.
    pub fn check(&self, view: bool) -> Invariants {
        check_cluster(
            &self.shards,
            &self.bots,
            &self.scenario.game,
            self.bus.in_flight(),
            view,
        )
    }

    pub fn report(&self) -> ClusterReport {
        let mut r = ClusterReport {
            grid: self.scenario.grid,
            clients: self.scenario.clients,
            npcs: self.scenario.game.npc_count,
            sim_seconds: self.now as f64 / 1000.0,
            wall_seconds: self.wall.elapsed().as_secs_f64(),
            tick_budget_us: self.scenario.game.tick_ms * 1000,
            bus: self.bus.stats(),
            shards: self.shards.iter().map(shard_report).collect(),
            invariants: self.check(false),
            ..Default::default()
        };
        fold_clients(&mut r, &self.bots);
        r
    }
}

/// Informe de un shard a partir de sus propios contadores.
pub(crate) fn shard_report<L: Link>(sh: &GameServer<L>) -> ShardReport {
    let g = sh.stats();
    ShardReport {
        shard: sh.shard_id().unwrap_or(0),
        players: sh.owned_player_ids().len(),
        npcs_owned: sh.owned_npc_count(),
        ghosts: sh.ghost_count(),
        ticks: g.ticks,
        tick_avg_us: if g.ticks > 0 {
            g.tick_time_us as f64 / g.ticks as f64
        } else {
            0.0
        },
        tick_max_us: g.tick_time_max_us,
        simulated_avg: if g.ticks > 0 {
            g.npcs_simulated as f64 / g.ticks as f64
        } else {
            0.0
        },
        cluster: sh.cluster_stats().cloned().unwrap_or_default(),
    }
}

/// Suma lo que cuentan los clientes (cambios de shard, redirecciones, correcciones).
pub(crate) fn fold_clients<C: Link>(r: &mut ClusterReport, bots: &[Bot<C>]) {
    r.playing = bots.iter().filter(|b| b.client.is_playing()).count();
    for b in bots {
        let st = b.client.stats();
        r.switches += st.switches;
        r.switch_failures += st.switch_failures;
        r.redirects += st.redirects;
        r.corrections += st.corrections;
        r.correction_max_q = r.correction_max_q.max(st.correction_max_q);
    }
}

/// Sentido en que patrulla el bot `i`: sobre x, sobre z y en diagonal, para cruzar bordes y esquinas.
pub(crate) fn patrol_dir(i: usize) -> [i8; 3] {
    match i % 3 {
        0 => [127, 0, 0],
        1 => [0, 0, 127],
        _ => [90, 0, 90],
    }
}

/// Comprueba el reparto de un cluster, sea cual sea el enlace de los shards y de los clientes.
/// `in_flight` son los mensajes que el bus sabe que siguen en camino (0 si no lo puede saber).
pub fn check_cluster<L: Link, C: Link>(
    shards: &[GameServer<L>],
    bots: &[Bot<C>],
    game: &GameConfig,
    in_flight: usize,
    view: bool,
) -> Invariants {
    let mut inv = Invariants {
        npcs: game.npc_count,
        bus_in_flight: in_flight,
        pending_transfers: shards
            .iter()
            .map(|s| s.pending_npc_transfers() + s.pending_player_handoffs())
            .sum(),
        ..Default::default()
    };
    // NPC: cuántos shards son responsables de cada uno.
    let mut holders = vec![0u8; game.npc_count];
    for sh in shards {
        for id in sh.held_npc_ids() {
            holders[(id - NPC_BASE) as usize] += 1;
        }
    }
    inv.npc_lost = holders.iter().filter(|&&h| h == 0).count();
    inv.npc_duplicated = holders.iter().filter(|&&h| h > 1).count();
    // Jugadores.
    let playing: Vec<u64> = bots
        .iter()
        .filter(|b| b.client.is_playing())
        .map(|b| b.client.entity())
        .collect();
    inv.players_playing = playing.len();
    for &id in &playing {
        match shards.iter().filter(|s| s.owns_player(id)).count() {
            0 => inv.players_lost += 1,
            1 => {}
            _ => inv.players_duplicated += 1,
        }
    }
    if view {
        check_views(shards, game, &playing, &mut inv);
    }
    inv
}

/// Verdad del cluster: posición de cada entidad según su dueño.
fn truth<L: Link>(shards: &[GameServer<L>]) -> HashMap<u64, [i32; 3]> {
    let mut t = HashMap::new();
    for sh in shards {
        for id in sh.held_npc_ids().into_iter().chain(sh.owned_player_ids()) {
            if let Some(st) = sh.entity_state(id) {
                t.insert(id, st.pos);
            }
        }
    }
    t
}

fn check_views<L: Link>(
    shards: &[GameServer<L>],
    cfg: &GameConfig,
    playing: &[u64],
    inv: &mut Invariants,
) {
    let truth = truth(shards);
    let d = |a: [i32; 3], b: [i32; 3]| -> f32 {
        let sq: f64 = (0..3)
            .map(|i| (dequantize(a[i]) as f64 - dequantize(b[i]) as f64).powi(2))
            .sum();
        sq.sqrt() as f32
    };
    for &id in playing {
        let Some(owner) = shards.iter().find(|s| s.owns_player(id)) else {
            continue;
        };
        let (Some(snap), Some(me)) = (owner.last_snapshot(id), owner.player_position(id)) else {
            continue;
        };
        inv.views_checked += 1;
        // Lo que está bien dentro del radio debe estar; lo que se ve no puede estar mucho más lejos
        // de lo que permite la histéresis. Los márgenes absorben el retraso de un tick del bus.
        let mut inner = cfg.interest_radius - 3.0;
        // Si el tope de entidades por snapshot muerde, solo se exige lo que está más cerca que lo
        // más lejano que se incluyó (el tope se queda con las más cercanas).
        if cfg.max_visible > 0 && snap.entities.len() > cfg.max_visible {
            let far = snap
                .entities
                .iter()
                .filter(|(&eid, _)| eid != id)
                .map(|(_, st)| d(st.pos, me))
                .fold(0.0f32, f32::max);
            inner = inner.min(far - 1.0);
        }
        let outer = cfg.interest_radius * cfg.hysteresis + 3.0;
        for (&eid, &pos) in &truth {
            if eid != id && d(pos, me) <= inner && !snap.entities.contains_key(&eid) {
                inv.view_missing += 1;
            }
        }
        for (&eid, st) in &snap.entities {
            if eid == id {
                continue;
            }
            match truth.get(&eid) {
                Some(&pos) => {
                    inv.view_error_max = inv.view_error_max.max(d(pos, st.pos));
                    if d(pos, me) > outer {
                        inv.view_phantoms += 1;
                    }
                }
                None => inv.view_phantoms += 1,
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ShardReport {
    pub shard: ShardId,
    pub players: usize,
    pub npcs_owned: usize,
    pub ghosts: usize,
    pub ticks: u64,
    pub tick_avg_us: f64,
    pub tick_max_us: u64,
    pub simulated_avg: f64,
    pub cluster: ClusterStats,
}

#[derive(Debug, Clone, Default)]
pub struct ClusterReport {
    pub grid: (u16, u16),
    pub clients: usize,
    pub playing: usize,
    pub npcs: usize,
    pub sim_seconds: f64,
    pub wall_seconds: f64,
    pub tick_budget_us: u64,
    pub shards: Vec<ShardReport>,
    pub bus: crate::bus::BusStats,
    /// El bus entre shards era UDP real (y no la red simulada).
    pub udp: bool,
    pub switches: u64,
    pub switch_failures: u64,
    pub redirects: u64,
    pub corrections: u64,
    pub correction_max_q: u32,
    pub invariants: Invariants,
}

impl ClusterReport {
    pub fn render(&self) -> String {
        let mut o = String::new();
        let n = self.shards.len();
        let _ = writeln!(
            o,
            "== Cluster TSOFGW: {}×{} = {n} shards ==",
            self.grid.0, self.grid.1
        );
        let _ = writeln!(
            o,
            "Clientes: {}/{} jugando | NPC en el mundo: {} | tiempo simulado: {:.1} s (CPU real: {:.1} s)",
            self.playing, self.clients, self.npcs, self.sim_seconds, self.wall_seconds
        );
        let _ = writeln!(o, "\nShards (cada uno solo simula su región)");
        let _ = writeln!(
            o,
            "  shard  jugadores  NPC propios  copias  NPC simulados  tick medio / máx (µs)  presupuesto"
        );
        for s in &self.shards {
            let _ = writeln!(
                o,
                "  {:>5}  {:>9}  {:>11}  {:>6}  {:>13.0}  {:>10.0} / {:<8}  {:>5.1} %",
                s.shard,
                s.players,
                s.npcs_owned,
                s.ghosts,
                s.simulated_avg,
                s.tick_avg_us,
                s.tick_max_us,
                s.tick_avg_us / self.tick_budget_us.max(1) as f64 * 100.0
            );
        }
        let sum = |f: fn(&ClusterStats) -> u64| -> u64 {
            self.shards.iter().map(|s| f(&s.cluster)).sum()
        };
        let max_tick = self
            .shards
            .iter()
            .map(|s| s.tick_avg_us)
            .fold(0.0, f64::max);
        let tot_tick: f64 = self.shards.iter().map(|s| s.tick_avg_us).sum();
        let _ = writeln!(
            o,
            "  el shard más cargado usa {:.0} µs de media por tick; la suma de todos es {:.0} µs \
             (en máquinas separadas, el límite lo marca el más cargado)",
            max_tick, tot_tick
        );
        let _ = writeln!(o, "\nBordes");
        let _ = writeln!(
            o,
            "  traspasos de jugador: {} iniciados, {} completados, {} cancelados | redirecciones {} → cambios de shard en clientes: {} ({} fallidos)",
            sum(|c| c.handoffs_started),
            sum(|c| c.handoffs_completed),
            sum(|c| c.handoffs_aborted),
            self.redirects,
            self.switches,
            self.switch_failures
        );
        let _ = writeln!(
            o,
            "  traspasos de NPC: {} enviados, {} reenvíos, {} recibidos, {} repeticiones detectadas, {} conflictos",
            sum(|c| c.npc_xfers_out),
            sum(|c| c.npc_xfer_retries),
            sum(|c| c.npc_xfers_in),
            sum(|c| c.npc_xfer_dups),
            sum(|c| c.npc_xfer_conflicts)
        );
        let ticks = self.shards.first().map_or(1, |s| s.ticks.max(1)) as f64;
        let _ = writeln!(
            o,
            "  fantasmas enviados por tick (todo el cluster): {:.1} jugadores + {:.1} NPC | caducados {}",
            sum(|c| c.ghost_players_out) as f64 / ticks,
            sum(|c| c.ghost_npcs_out) as f64 / ticks,
            sum(|c| c.ghost_expired)
        );
        if self.udp {
            let _ = writeln!(
                o,
                "  bus entre shards (UDP real, HMAC): {} mensajes, {:.1} KB/s en total | datagramas rechazados: {} (por otra configuración de mundo: {}) | mensajes mal formados: {}",
                sum(|c| c.bus_msgs_out),
                sum(|c| c.bus_bytes_out) as f64 / 1000.0 / self.sim_seconds.max(0.001),
                sum(|c| c.bus_rejected),
                sum(|c| c.bus_config_mismatch),
                sum(|c| c.bus_malformed)
            );
        } else {
            let _ = writeln!(
                o,
                "  bus entre shards: {} mensajes, {:.1} KB/s en total ({} perdidos en la red simulada, {} duplicados, {} demasiado grandes)",
                self.bus.sent,
                self.bus.bytes as f64 / 1000.0 / self.sim_seconds.max(0.001),
                self.bus.dropped,
                self.bus.duplicated,
                self.bus.oversize
            );
        }
        let _ = writeln!(
            o,
            "  correcciones de predicción: {} (máxima {:.2} u)",
            self.corrections,
            self.correction_max_q as f32 / 64.0
        );
        let i = &self.invariants;
        let _ = writeln!(o, "\nReparto al final (red en calma)");
        let _ = writeln!(
            o,
            "  NPC: {} perdidos, {} duplicados de {} | jugadores: {} perdidos, {} duplicados de {}",
            i.npc_lost,
            i.npc_duplicated,
            i.npcs,
            i.players_lost,
            i.players_duplicated,
            i.players_playing
        );
        let _ = writeln!(
            o,
            "  vistas comprobadas: {} | entidades que faltaban: {} | entidades inexistentes: {} | error máximo de posición: {:.2} u",
            i.views_checked, i.view_missing, i.view_phantoms, i.view_error_max
        );
        o
    }
}

/// Ejecuta el escenario completo (con tramo final en calma) y devuelve el informe.
pub fn run_cluster(s: &ClusterScenario) -> Result<ClusterReport, String> {
    let mut run = ClusterRun::new(s)?;
    run.run_for(s.duration_ms);
    run.settle();
    run.run_for(s.settle_ms);
    let mut r = run.report();
    r.invariants = run.check(true);
    Ok(r)
}
