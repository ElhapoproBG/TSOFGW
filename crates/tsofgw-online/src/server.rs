//! Servidor de juego autoritativo.
//!
//! * El estado verdadero vive aquí; los clientes solo envían intenciones ([`Input`]).
//! * **Entradas no confiables**: se validan formato y tamaño, se descartan repetidos, la cola es acotada y
//!   un sistema de créditos limita cuántos inputs se aplican por tick (no se puede ir más rápido
//!   enviando más paquetes). Quien envía basura repetidamente es expulsado.
//! * **Apagado selectivo**: solo se simulan los NPC que están en regiones cercanas a algún jugador.
//! * **Interés**: cada jugador recibe únicamente lo que está a su alcance, como snapshots delta contra la
//!   última línea base que él mismo confirmó (con repliegue automático a snapshot completo).
//! * **Varios hilos**: la replicación (interés, snapshot y codificación de cada jugador) es independiente
//!   entre jugadores y se reparte entre hilos; el resultado es idéntico con 1 o con N hilos.
//! * **Niveles de detalle y presupuesto**: lo lejano se refresca menos y cada snapshot puede limitarse a
//!   un número de bytes (ver el módulo `replicate`).

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use tsofgw_net::Sim;
use tsofgw_net::{dequantize, quantize, Endpoint, EntityState, Snapshot};
use tsofgw_transport::{
    Authority, DisconnectReason, Entropy, Link, SecureServer, ServerConfig, ServerEvent, SessionId,
    TransportStats,
};
use tsofgw_world::{Cell, EntityId, FastMap, Neighborhood, Rng, Vec3, World};

mod cluster;

use crate::par;
use crate::protocol::{self as proto, Welcome};
use crate::replicate::{self, LodBand, ReplCtx};
use crate::sim::{Input, MoveSim, PlayerState, HALF_EXTENT_Q};
use crate::states::{Entities, Own};
pub use cluster::{ClusterConfig, ClusterStats};
use cluster::{ClusterNode, Handoff};

/// Los NPC usan identificadores desde aquí; las cuentas de jugador deben ser menores.
pub const NPC_BASE: u64 = 1 << 48;
pub const KIND_NPC: u16 = 0;
pub const KIND_PLAYER: u16 = 1;

#[derive(Debug, Clone)]
pub struct GameConfig {
    pub tick_ms: u64,
    /// Radio (unidades) dentro del cual un jugador recibe entidades.
    pub interest_radius: f32,
    pub hysteresis: f32,
    pub cell_size: f32,
    /// Radio alrededor de cada jugador en el que se simulan los NPC.
    pub sim_radius: f32,
    pub npc_count: usize,
    /// Velocidad de los NPC en unidades por segundo.
    pub npc_speed: f32,
    pub seed: u64,
    pub max_players: usize,
    /// Máximo de inputs en cola por jugador; el exceso se descarta.
    pub input_queue_max: usize,
    /// Máximo de créditos de input acumulables (ráfaga tolerada).
    pub credit_cap: u32,
    /// Snapshots enviados que se recuerdan por jugador.
    pub history_max: usize,
    /// Si la línea base confirmada es más vieja que esto (en ticks), se envía un snapshot completo.
    pub max_baseline_age: u32,
    /// Máximo de entidades por snapshot (las más cercanas).
    pub max_visible: usize,
    /// Un jugador que envía más mensajes malformados que esto es expulsado.
    pub malformed_limit: u32,
    /// Mitad del lado (unidades) del área donde aparecen los jugadores.
    pub spawn_half_extent: f32,
    /// Hilos para la replicación: 1 = un solo hilo, 0 = los núcleos disponibles (con tope 16).
    /// No cambia el resultado, solo el tiempo.
    pub threads: usize,
    /// Niveles de detalle por distancia (vacío = todo se actualiza en cada tick).
    pub lod: Vec<LodBand>,
    /// Máximo de bytes por snapshot codificado (0 = sin límite). Lo que no cabe se retrasa, lo más
    /// lejano primero.
    pub snapshot_budget: usize,
    /// Si en una misma celda hay al menos tantos jugadores, su vecindad se reúne una sola vez y se
    /// comparte (0 = nunca). No cambia el resultado, solo el tiempo en multitudes.
    pub interest_share_min: usize,
}

impl Default for GameConfig {
    fn default() -> Self {
        GameConfig {
            tick_ms: 50,
            interest_radius: 60.0,
            hysteresis: 1.25,
            cell_size: 32.0,
            sim_radius: 90.0,
            npc_count: 20_000,
            npc_speed: 3.0,
            seed: 1,
            max_players: 2048,
            input_queue_max: 12,
            credit_cap: 3,
            history_max: 16,
            max_baseline_age: 32,
            max_visible: 256,
            malformed_limit: 20,
            spawn_half_extent: 150.0,
            threads: 1,
            lod: Vec::new(),
            snapshot_budget: 0,
            interest_share_min: 8,
        }
    }
}

/// Contadores del servidor de juego (acumulados desde que arrancó).
#[derive(Debug, Clone, Default)]
pub struct GameStats {
    pub ticks: u64,
    pub tick_time_us: u64,
    pub tick_time_max_us: u64,
    /// Reparto del tiempo de tick por fase (µs acumulados).
    pub phase_inputs_us: u64,
    pub phase_npc_us: u64,
    /// Interés, construcción y codificación: tiempo de CPU **sumado** entre todos los hilos.
    pub phase_interest_us: u64,
    pub phase_build_us: u64,
    pub phase_encode_us: u64,
    /// Tiempo real (pared) que ocupó la replicación completa, con todos los hilos a la vez.
    pub phase_repl_wall_us: u64,
    pub phase_send_us: u64,
    /// Entidades cuya actualización se sujetó por nivel de detalle (suma sobre todos los snapshots).
    pub lod_held: u64,
    /// Snapshots que el presupuesto de bytes obligó a recortar.
    pub budget_trimmed: u64,
    /// Mayor snapshot codificado (sin cabeceras) enviado nunca.
    pub snapshot_payload_max: u64,
    /// Suma, sobre los ticks, de los NPC simulados.
    pub npcs_simulated: u64,
    /// Suma, sobre los ticks, de los NPC totales (para sacar la fracción simulada).
    pub npcs_total: u64,
    pub snapshots_full: u64,
    pub snapshots_delta: u64,
    pub bytes_full: u64,
    pub bytes_delta: u64,
    pub inputs_applied: u64,
    pub inputs_stale: u64,
    pub inputs_overflow: u64,
    pub malformed: u64,
    pub joins: u64,
    pub kicks: u64,
    pub refused: u64,
}

struct Npc {
    dir: [f32; 2],
    turn_in: u32,
    rng: Rng,
}

pub(crate) struct Player {
    pub(crate) entity: EntityId,
    pub(crate) player_id: u64,
    pub(crate) endpoint: Endpoint,
    pub(crate) state: PlayerState,
    pub(crate) joined: bool,
    pub(crate) queue: VecDeque<(u32, Input)>,
    pub(crate) last_queued: u32,
    pub(crate) last_processed: u32,
    pub(crate) credits: u32,
    /// seq del paquete → tick del snapshot que llevaba.
    pub(crate) sent: HashMap<u16, u32>,
    pub(crate) baseline_tick: Option<u32>,
    pub(crate) history: VecDeque<(u32, Snapshot)>,
    pub(crate) malformed: u32,
    /// Lo que ve ahora mismo (ordenado por id); sirve de «pasado» para la histéresis del interés.
    pub(crate) visible: Vec<EntityId>,
    /// Cluster: estado del traspaso de este jugador a otro shard.
    pub(crate) handoff: Handoff,
    /// Cluster: hasta este tick no se inicia otro traspaso.
    pub(crate) cooldown_until: u32,
    /// Cluster: inputs extra que se permiten aplicar tras un traspaso (el tiempo que el jugador estuvo
    /// congelado entre shards no se le descuenta). Se agotan al vaciarse la cola.
    pub(crate) catchup: u32,
}

impl Player {
    /// Un jugador recién conectado (aún sin `JOIN`).
    pub(crate) fn new(entity: EntityId, player_id: u64) -> Player {
        Player {
            entity,
            player_id,
            endpoint: Endpoint::default(),
            state: PlayerState::default(),
            joined: false,
            queue: VecDeque::new(),
            last_queued: 0,
            last_processed: 0,
            credits: 0,
            sent: HashMap::new(),
            baseline_tick: None,
            history: VecDeque::new(),
            malformed: 0,
            visible: Vec::new(),
            handoff: Handoff::None,
            cooldown_until: 0,
            catchup: 0,
        }
    }

    /// La autoridad sobre este jugador ya pasó a otro shard: aquí no se mueve ni se le replica.
    pub(crate) fn frozen(&self) -> bool {
        matches!(self.handoff, Handoff::HandedOver { .. })
    }
}

pub struct GameServer<L: Link> {
    cfg: GameConfig,
    net: SecureServer<L>,
    world: World,
    npcs: Vec<Npc>,
    /// Estado ya cuantizado de cada entidad (se calcula una vez al moverse, no una vez por observador).
    ents: Entities,
    players: FastMap<SessionId, Player>,
    by_entity: FastMap<EntityId, SessionId>,
    tick: u32,
    next_tick_at: Option<u64>,
    stats: GameStats,
    events: Vec<ServerEvent<L::Addr>>,
    /// Último instante (ms) con el que se llamó a `update`; el bus entre shards lo necesita.
    now_ms: u64,
    cluster: Option<ClusterNode>,
}

pub(crate) fn vec3_from_q(p: [i32; 3]) -> Vec3 {
    Vec3::new(dequantize(p[0]), dequantize(p[1]), dequantize(p[2]))
}

impl<L: Link> GameServer<L> {
    pub fn new(
        link: L,
        authority: Authority,
        cfg: GameConfig,
        transport: ServerConfig,
        entropy: Box<dyn Entropy>,
    ) -> Self {
        Self::build(link, authority, cfg, transport, entropy, None)
    }

    /// Dónde aparece una cuenta que entra por primera vez (solo depende de la semilla y de la cuenta, así
    /// que un servicio de entrada puede decidir a qué shard mandarla antes de que se conecte).
    pub fn spawn_position(cfg: &GameConfig, player_id: u64) -> [i32; 3] {
        let h = cfg.spawn_half_extent;
        let mut rng = Rng::new(cfg.seed ^ player_id.wrapping_mul(0xd6e8_feb8_6659_fd93));
        [quantize(rng.range(-h, h)), 0, quantize(rng.range(-h, h))]
    }

    fn build(
        link: L,
        authority: Authority,
        cfg: GameConfig,
        transport: ServerConfig,
        entropy: Box<dyn Entropy>,
        cluster: Option<ClusterNode>,
    ) -> Self {
        let mut world = World::new(cfg.cell_size);
        let half = dequantize(HALF_EXTENT_Q);
        let mut npcs = Vec::with_capacity(cfg.npc_count);
        let mut ents = Entities::new(cfg.npc_count);
        for i in 0..cfg.npc_count {
            // La posición inicial de cada NPC depende solo de su número y de la semilla (no de los demás),
            // así cualquier servidor puede calcular dónde nació cualquier NPC.
            let mut r = Rng::new(cfg.seed ^ (i as u64 + 1).wrapping_mul(0xa24b_aed4_963e_e407));
            let pos = Vec3::new(r.range(-half, half), 0.0, r.range(-half, half));
            // En un cluster, cada shard solo se queda con los NPC que nacen en su región.
            let mine = cluster
                .as_ref()
                .is_none_or(|c| c.cfg.partition.owner_of(pos) == c.cfg.shard);
            if mine {
                world.spawn(NPC_BASE + i as u64, pos);
            }
            ents.push_npc(
                EntityState {
                    pos: [quantize(pos.x), quantize(pos.y), quantize(pos.z)],
                    data: KIND_NPC,
                },
                if mine { Own::Owned } else { Own::Absent },
            );
            npcs.push(Npc {
                dir: [0.0, 0.0],
                turn_in: 0,
                rng: Rng::new(cfg.seed ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            });
        }
        GameServer {
            net: SecureServer::new(link, authority, transport, entropy),
            cfg,
            world,
            npcs,
            ents,
            players: FastMap::default(),
            by_entity: FastMap::default(),
            tick: 0,
            next_tick_at: None,
            stats: GameStats::default(),
            events: Vec::new(),
            now_ms: 0,
            cluster,
        }
    }

    pub fn config(&self) -> &GameConfig {
        &self.cfg
    }
    pub fn stats(&self) -> &GameStats {
        &self.stats
    }
    pub fn transport_stats(&self) -> &TransportStats {
        self.net.stats()
    }
    pub fn link(&self) -> &L {
        self.net.link()
    }
    pub fn tick(&self) -> u32 {
        self.tick
    }
    pub fn player_count(&self) -> usize {
        self.players.values().filter(|p| p.joined).count()
    }
    pub fn entity_count(&self) -> usize {
        self.world.len()
    }

    fn player_by_id(&self, player_id: u64) -> Option<&Player> {
        self.by_entity
            .get(&player_id)
            .and_then(|s| self.players.get(s))
    }

    /// Inputs en cola de un jugador (aún sin aplicar).
    pub fn queued_inputs(&self, player_id: u64) -> Option<usize> {
        self.player_by_id(player_id).map(|p| p.queue.len())
    }

    /// Posición autoritativa (1/64 de unidad) de un jugador conectado.
    pub fn player_position(&self, player_id: u64) -> Option<[i32; 3]> {
        self.player_by_id(player_id).map(|p| p.state.pos)
    }

    /// Snapshot enviado a ese jugador en un tick concreto, si aún se recuerda.
    pub fn snapshot_at(&self, player_id: u64, tick: u32) -> Option<&Snapshot> {
        self.player_by_id(player_id)?
            .history
            .iter()
            .find(|(t, _)| *t == tick)
            .map(|(_, s)| s)
    }

    /// Último snapshot enviado a ese jugador.
    pub fn last_snapshot(&self, player_id: u64) -> Option<&Snapshot> {
        self.player_by_id(player_id)
            .and_then(|p| p.history.back().map(|(_, s)| s))
    }

    /// Avanza el servidor hasta `now_ms`: lee la red y ejecuta los ticks que toquen.
    pub fn update(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        let mut events = std::mem::take(&mut self.events);
        self.net.poll(now_ms, &mut events);
        for e in events.drain(..) {
            self.on_event(e);
        }
        self.events = events;
        self.cluster_poll(now_ms);

        let next = *self.next_tick_at.get_or_insert(now_ms + self.cfg.tick_ms);
        let mut next = next;
        let mut ran = 0;
        while now_ms >= next {
            if ran == 5 {
                // Va muy atrasado: se salta lo perdido en lugar de intentar recuperarlo todo de golpe.
                next = now_ms + self.cfg.tick_ms;
                break;
            }
            let t0 = Instant::now();
            self.run_tick();
            let us = t0.elapsed().as_micros() as u64;
            self.stats.tick_time_us += us;
            self.stats.tick_time_max_us = self.stats.tick_time_max_us.max(us);
            next += self.cfg.tick_ms;
            ran += 1;
        }
        self.next_tick_at = Some(next);
    }

    fn on_event(&mut self, e: ServerEvent<L::Addr>) {
        match e {
            ServerEvent::Connected {
                session, player_id, ..
            } => {
                if player_id >= NPC_BASE || self.players.len() >= self.cfg.max_players {
                    self.stats.refused += 1;
                    self.net.kick(session);
                    return;
                }
                self.players
                    .insert(session, Player::new(player_id, player_id));
            }
            ServerEvent::Message { session, data } => self.on_message(session, &data),
            ServerEvent::Disconnected { session, .. } => self.drop_player(session),
        }
    }

    fn drop_player(&mut self, session: SessionId) {
        let Some(p) = self.players.remove(&session) else {
            return;
        };
        if !p.joined {
            return;
        }
        if self.by_entity.get(&p.entity) == Some(&session) {
            self.by_entity.remove(&p.entity);
        }
        match p.handoff {
            // Ya era del otro shard: aquí solo queda como copia hasta que caduque.
            Handoff::HandedOver { to, .. } => self.cluster_handed_over_gone(&p, to),
            other => {
                if let Handoff::Preparing { to, xfer, .. } | Handoff::Redirecting { to, xfer, .. } =
                    other
                {
                    self.cluster_abort_for_gone(to, xfer, p.player_id);
                }
                self.world.despawn(p.entity);
                self.ents.remove_player(p.entity);
            }
        }
    }

    fn kick(&mut self, session: SessionId) {
        self.stats.kicks += 1;
        self.net.kick(session);
        self.drop_player(session);
    }

    fn on_message(&mut self, session: SessionId, data: &[u8]) {
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&session) else {
            return;
        };
        let Some((header, kind, body)) = proto::unframe(data) else {
            p.malformed += 1;
            self.stats.malformed += 1;
            if p.malformed > self.cfg.malformed_limit {
                self.kick(session);
            }
            return;
        };
        let mut kick = false;
        // Los acks que trae este mensaje dicen qué snapshots recibió el cliente.
        for seq in p.endpoint.on_receive(&header) {
            if let Some(t) = p.sent.remove(&seq) {
                if p.baseline_tick.is_none_or(|b| t > b) {
                    p.baseline_tick = Some(t);
                }
            }
        }
        if let Some(b) = p.baseline_tick {
            p.history.retain(|(t, _)| *t >= b);
        }
        p.sent.retain(|_, t| tick.wrapping_sub(*t) < 256);

        let mut bad = false;
        match kind {
            proto::JOIN => match proto::parse_join(body) {
                Some(proto::PROTOCOL_VERSION) => {
                    if !p.joined {
                        // ¿Es un jugador que otro shard nos está traspasando? Entonces no aparece de cero:
                        // se reclama la autoridad (en el siguiente tick) y la bienvenida llega con su estado.
                        let arriving = self
                            .cluster
                            .as_mut()
                            .is_some_and(|c| c.register_arrival(p.player_id, session));
                        if !arriving {
                            let mut pos = Self::spawn_position(&self.cfg, p.player_id);
                            if let Some(c) = self.cluster.as_mut() {
                                let v = c.region().clamp_inside(vec3_from_q(pos), 1.0);
                                pos = [quantize(v.x), pos[1], quantize(v.z)];
                                // Una copia con ese id solo puede ser de una sesión anterior: se sustituye.
                                if c.take_ghost(p.entity) {
                                    self.world.despawn(p.entity);
                                    self.ents.remove_player(p.entity);
                                }
                            }
                            p.state = PlayerState { pos };
                            if self.world.spawn(p.entity, vec3_from_q(pos)) {
                                self.ents.set_player(
                                    p.entity,
                                    EntityState {
                                        pos,
                                        data: KIND_PLAYER,
                                    },
                                );
                                p.joined = true;
                                self.by_entity.insert(p.entity, session);
                                self.stats.joins += 1;
                            } else {
                                // La entidad ya existe: no debería pasar; se rechaza esta sesión.
                                self.stats.refused += 1;
                                kick = true;
                            }
                        }
                    }
                    let welcome = Welcome {
                        entity: p.entity,
                        tick,
                        tick_ms: self.cfg.tick_ms as u16,
                        pos: p.state.pos,
                    };
                    if p.joined {
                        let hdr = p.endpoint.next_header();
                        let msg = proto::frame(hdr, proto::WELCOME, &proto::welcome_body(&welcome));
                        let _ = self.net.send(session, &msg);
                    }
                }
                _ => bad = true,
            },
            proto::INPUT => match proto::parse_input(body) {
                Some((first, inputs)) if p.joined => {
                    for (i, input) in inputs.into_iter().enumerate() {
                        let seq = first.saturating_add(i as u32);
                        if seq <= p.last_queued {
                            self.stats.inputs_stale += 1;
                            continue;
                        }
                        if p.queue.len() >= self.cfg.input_queue_max {
                            self.stats.inputs_overflow += 1;
                            break;
                        }
                        p.queue.push_back((seq, input));
                        p.last_queued = seq;
                    }
                }
                Some(_) => {} // input antes de JOIN: se ignora
                None => bad = true,
            },
            _ => bad = true,
        }
        if bad {
            p.malformed += 1;
            self.stats.malformed += 1;
            kick |= p.malformed > self.cfg.malformed_limit;
        }
        if kick {
            self.kick(session);
        }
    }

    fn run_tick(&mut self) {
        self.tick += 1;
        self.stats.ticks += 1;
        let t = Instant::now();

        // 0. Cluster: caducidad de copias, reenvíos y plazos de los traspasos.
        self.cluster_begin_tick();

        // 1. Inputs de jugadores, con créditos: como mucho 1 por tick de media, con ráfagas acotadas.
        let mut moved: Vec<(EntityId, [i32; 3])> = Vec::new();
        for p in self
            .players
            .values_mut()
            .filter(|p| p.joined && !p.frozen())
        {
            p.credits = (p.credits + 1).min(self.cfg.credit_cap);
            let before = p.state.pos;
            while p.credits > 0 || p.catchup > 0 {
                let Some((seq, input)) = p.queue.pop_front() else {
                    break;
                };
                p.state = MoveSim::step(&p.state, &input);
                p.last_processed = seq;
                if p.credits > 0 {
                    p.credits -= 1;
                } else {
                    p.catchup -= 1;
                }
                self.stats.inputs_applied += 1;
            }
            if p.queue.is_empty() {
                p.catchup = 0;
            }
            if p.state.pos != before {
                moved.push((p.entity, p.state.pos));
            }
        }
        for (id, pos) in moved {
            self.world.move_to(id, vec3_from_q(pos));
            self.ents.set_player(
                id,
                EntityState {
                    pos,
                    data: KIND_PLAYER,
                },
            );
        }
        self.cluster_players_begin();
        self.stats.phase_inputs_us += t.elapsed().as_micros() as u64;
        let t = Instant::now();

        // 2. Apagado selectivo: solo se simulan los NPC de regiones cercanas a algún jugador (en un
        //    cluster, también a los de otros shards que están junto al borde: sus copias despiertan NPC).
        let mut spheres: Vec<(Vec3, f32)> = self
            .players
            .values()
            .filter(|p| p.joined && !p.frozen())
            .map(|p| (vec3_from_q(p.state.pos), self.cfg.sim_radius))
            .collect();
        spheres.extend(
            self.cluster_ghost_player_positions()
                .into_iter()
                .map(|pos| (pos, self.cfg.sim_radius)),
        );
        let active = self.world.active_cells(&spheres);
        let ids = self.world.entities_in_cells(&active);
        let step = self.cfg.npc_speed * self.cfg.tick_ms as f32 / 1000.0;
        let half = dequantize(HALF_EXTENT_Q);
        let mut simulated = 0u64;
        let mut leaving: Vec<(EntityId, usize, Vec3, u16)> = Vec::new();
        for id in ids.into_iter().filter(|&id| id >= NPC_BASE) {
            let idx = (id - NPC_BASE) as usize;
            // Solo se simula lo que es mío (no las copias de otros shards ni lo que está en traspaso).
            if self.ents.npc_own(idx) != Own::Owned {
                continue;
            }
            let Some(pos) = self.world.position(id) else {
                continue;
            };
            let npc = &mut self.npcs[idx];
            if npc.turn_in == 0 {
                npc.turn_in = 20 + (npc.rng.next_u64() % 60) as u32;
                npc.dir = if npc.rng.next_u64().is_multiple_of(4) {
                    [0.0, 0.0]
                } else {
                    let a = npc.rng.range(0.0, std::f32::consts::TAU);
                    [a.cos(), a.sin()]
                };
            }
            npc.turn_in -= 1;
            let nx = (pos.x + npc.dir[0] * step).clamp(-half, half);
            let nz = (pos.z + npc.dir[1] * step).clamp(-half, half);
            if (nx - pos.x - npc.dir[0] * step).abs() > 1e-6
                || (nz - pos.z - npc.dir[1] * step).abs() > 1e-6
            {
                npc.dir = [-npc.dir[0], -npc.dir[1]]; // rebota en el borde
            }
            self.world.move_to(id, Vec3::new(nx, pos.y, nz));
            self.ents.npc_mut(idx).pos = [quantize(nx), quantize(pos.y), quantize(nz)];
            simulated += 1;
            if let Some(c) = self.cluster.as_ref() {
                if let Some(to) = c.cfg.partition.deep_owner(
                    c.cfg.shard,
                    Vec3::new(nx, pos.y, nz),
                    c.cfg.npc_depth,
                ) {
                    leaving.push((id, idx, Vec3::new(nx, pos.y, nz), to));
                }
            }
        }
        for (id, idx, pos, to) in leaving {
            self.cluster_start_npc_xfer(id, idx, pos, to);
        }
        self.cluster_feed();
        self.stats.npcs_simulated += simulated;
        self.stats.npcs_total += self.ents.owned_npcs() as u64;
        self.stats.phase_npc_us += t.elapsed().as_micros() as u64;
        let t = Instant::now();

        // 3. Replicación: interés, snapshot y codificación de cada jugador. Es independiente entre
        //    jugadores, así que se reparte entre hilos; el envío (firma + red) sigue siendo secuencial y
        //    en orden de sesión, de modo que lo que sale por la red no depende del número de hilos.
        let threads = par::resolve_threads(self.cfg.threads);
        let mut work: Vec<(SessionId, &mut Player)> = self
            .players
            .iter_mut()
            .filter(|(_, p)| p.joined && !p.frozen())
            .map(|(&s, p)| (s, p))
            .collect();
        work.sort_unstable_by_key(|(s, _)| *s);
        // Multitudes: los jugadores de una misma celda comparten la vecindad (se reúne una vez, en paralelo).
        let mut hoods: FastMap<Cell, Neighborhood> = FastMap::default();
        if self.cfg.interest_share_min > 0 {
            let mut counts: FastMap<Cell, usize> = FastMap::default();
            for (_, p) in work.iter() {
                *counts
                    .entry(self.world.cell_of(vec3_from_q(p.state.pos)))
                    .or_default() += 1;
            }
            let mut cells: Vec<Cell> = counts
                .into_iter()
                .filter(|&(_, n)| n >= self.cfg.interest_share_min)
                .map(|(c, _)| c)
                .collect();
            cells.sort_unstable();
            let reach = self.cfg.interest_radius * self.cfg.hysteresis;
            let world = &self.world;
            let built = par::map_mut(&mut cells, threads, 2, |c| world.neighborhood(*c, reach));
            hoods.extend(built.into_iter().map(|h| (h.cell(), h)));
        }
        let ctx = ReplCtx::new(&self.world, &self.ents, &self.cfg, self.tick, &hoods);
        let outs = par::map_mut(&mut work, threads, 8, |(s, p)| {
            replicate::replicate(&ctx, *s, p)
        });
        drop(work);
        self.stats.phase_repl_wall_us += t.elapsed().as_micros() as u64;

        for o in outs {
            self.stats.phase_interest_us += o.interest_us;
            self.stats.phase_build_us += o.build_us;
            self.stats.phase_encode_us += o.encode_us;
            self.stats.lod_held += o.lod_held as u64;
            self.stats.budget_trimmed += o.trimmed as u64;
            self.stats.snapshot_payload_max = self.stats.snapshot_payload_max.max(o.payload as u64);
            if o.full {
                self.stats.snapshots_full += 1;
                self.stats.bytes_full += o.msg.len() as u64;
            } else {
                self.stats.snapshots_delta += 1;
                self.stats.bytes_delta += o.msg.len() as u64;
            }
            let t = Instant::now();
            let _ = self.net.send(o.session, &o.msg);
            self.stats.phase_send_us += t.elapsed().as_micros() as u64;
        }
    }

    /// Expulsa a un jugador por cuenta (por ejemplo, desde una consola de administración).
    pub fn kick_player(&mut self, player_id: u64) -> bool {
        match self.by_entity.get(&player_id).copied() {
            Some(s) => {
                self.kick(s);
                true
            }
            None => false,
        }
    }

    /// Motivo legible de una desconexión (para registros).
    pub fn describe(reason: DisconnectReason) -> &'static str {
        match reason {
            DisconnectReason::Timeout => "sin respuesta",
            DisconnectReason::Requested => "pedida por el cliente",
            DisconnectReason::Replaced => "reemplazada por otra conexión",
            DisconnectReason::Kicked => "expulsado",
            DisconnectReason::ConnectFailed => "no completó el handshake",
        }
    }
}
