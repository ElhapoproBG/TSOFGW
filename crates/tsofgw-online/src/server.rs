//! Servidor de juego autoritativo.
//!
//! * El estado verdadero vive aquí; los clientes solo envían intenciones ([`Input`]).
//! * **Entradas no confiables**: se validan formato y tamaño, se descartan repetidos, la cola es acotada y
//!   un sistema de créditos limita cuántos inputs se aplican por tick (no se puede ir más rápido
//!   enviando más paquetes). Quien envía basura repetidamente es expulsado.
//! * **Apagado selectivo**: solo se simulan los NPC que están en regiones cercanas a algún jugador.
//! * **Interés**: cada jugador recibe únicamente lo que está a su alcance, como snapshots delta contra la
//!   última línea base que él mismo confirmó (con repliegue automático a snapshot completo).

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use tsofgw_net::Sim;
use tsofgw_net::{dequantize, quantize, Endpoint, EntityState, Snapshot};
use tsofgw_transport::{
    Authority, DisconnectReason, Entropy, Link, SecureServer, ServerConfig, ServerEvent, SessionId,
    TransportStats,
};
use tsofgw_world::{EntityId, FastMap, Interest, Rng, Vec3, World};

use crate::protocol::{self as proto, Welcome};
use crate::sim::{Input, MoveSim, PlayerState, HALF_EXTENT_Q};

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
    pub phase_interest_us: u64,
    pub phase_build_us: u64,
    pub phase_encode_us: u64,
    pub phase_send_us: u64,
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

struct Player {
    entity: EntityId,
    player_id: u64,
    endpoint: Endpoint,
    state: PlayerState,
    joined: bool,
    queue: VecDeque<(u32, Input)>,
    last_queued: u32,
    last_processed: u32,
    credits: u32,
    /// seq del paquete → tick del snapshot que llevaba.
    sent: HashMap<u16, u32>,
    baseline_tick: Option<u32>,
    history: VecDeque<(u32, Snapshot)>,
    malformed: u32,
}

pub struct GameServer<L: Link> {
    cfg: GameConfig,
    net: SecureServer<L>,
    world: World,
    interest: Interest,
    npcs: Vec<Npc>,
    /// Estado ya cuantizado de cada NPC (se calcula una vez al moverse, no una vez por observador).
    npc_state: Vec<EntityState>,
    players: FastMap<SessionId, Player>,
    by_entity: FastMap<EntityId, SessionId>,
    tick: u32,
    next_tick_at: Option<u64>,
    stats: GameStats,
    events: Vec<ServerEvent<L::Addr>>,
}

fn vec3_from_q(p: [i32; 3]) -> Vec3 {
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
        let mut world = World::new(cfg.cell_size);
        let mut rng = Rng::new(cfg.seed);
        let half = dequantize(HALF_EXTENT_Q);
        let mut npcs = Vec::with_capacity(cfg.npc_count);
        let mut npc_state = Vec::with_capacity(cfg.npc_count);
        for i in 0..cfg.npc_count {
            let pos = Vec3::new(rng.range(-half, half), 0.0, rng.range(-half, half));
            world.spawn(NPC_BASE + i as u64, pos);
            npc_state.push(EntityState {
                pos: [quantize(pos.x), quantize(pos.y), quantize(pos.z)],
                data: KIND_NPC,
            });
            npcs.push(Npc {
                dir: [0.0, 0.0],
                turn_in: 0,
                rng: Rng::new(cfg.seed ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)),
            });
        }
        let interest = Interest::new(cfg.hysteresis);
        GameServer {
            net: SecureServer::new(link, authority, transport, entropy),
            cfg,
            world,
            interest,
            npcs,
            npc_state,
            players: FastMap::default(),
            by_entity: FastMap::default(),
            tick: 0,
            next_tick_at: None,
            stats: GameStats::default(),
            events: Vec::new(),
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
        let mut events = std::mem::take(&mut self.events);
        self.net.poll(now_ms, &mut events);
        for e in events.drain(..) {
            self.on_event(e);
        }
        self.events = events;

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
                self.players.insert(
                    session,
                    Player {
                        entity: player_id,
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
                    },
                );
            }
            ServerEvent::Message { session, data } => self.on_message(session, &data),
            ServerEvent::Disconnected { session, .. } => self.drop_player(session),
        }
    }

    fn drop_player(&mut self, session: SessionId) {
        if let Some(p) = self.players.remove(&session) {
            if p.joined {
                self.world.despawn(p.entity);
                if self.by_entity.get(&p.entity) == Some(&session) {
                    self.by_entity.remove(&p.entity);
                }
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
                        let pos = {
                            let h = self.cfg.spawn_half_extent;
                            let mut rng = Rng::new(
                                self.cfg.seed ^ p.player_id.wrapping_mul(0xd6e8_feb8_6659_fd93),
                            );
                            [quantize(rng.range(-h, h)), 0, quantize(rng.range(-h, h))]
                        };
                        p.state = PlayerState { pos };
                        if self.world.spawn(p.entity, vec3_from_q(pos)) {
                            p.joined = true;
                            self.by_entity.insert(p.entity, session);
                            self.stats.joins += 1;
                        } else {
                            // La entidad ya existe: no debería pasar; se rechaza esta sesión.
                            self.stats.refused += 1;
                            kick = true;
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

    fn state_of(&self, id: EntityId) -> Option<EntityState> {
        if id >= NPC_BASE {
            self.npc_state.get((id - NPC_BASE) as usize).cloned()
        } else {
            let p = self.players.get(self.by_entity.get(&id)?)?;
            Some(EntityState {
                pos: p.state.pos,
                data: KIND_PLAYER,
            })
        }
    }

    fn build_snapshot(&self, session: SessionId) -> Option<Snapshot> {
        let p = self.players.get(&session)?;
        let ids = self.interest.visible_ids(p.entity);
        let mut list: Vec<(EntityId, EntityState)> = Vec::with_capacity(ids.len() + 1);
        for &id in ids {
            if let Some(st) = self.state_of(id) {
                list.push((id, st));
            }
        }
        let cap = self.cfg.max_visible;
        if list.len() > cap {
            // Demasiada gente a la vista: se quedan las `cap` entidades más cercanas.
            let me = p.state.pos;
            let d2 = |st: &EntityState| -> i64 {
                (0..3)
                    .map(|k| (st.pos[k] as i64 - me[k] as i64).pow(2))
                    .sum()
            };
            list.select_nth_unstable_by_key(cap, |(_, st)| d2(st));
            list.truncate(cap);
            list.sort_unstable_by_key(|(id, _)| *id);
        }
        // El propio jugador siempre va (Interest no incluye al observador); se inserta en su sitio.
        let at = list.partition_point(|(id, _)| *id < p.entity);
        list.insert(
            at,
            (
                p.entity,
                EntityState {
                    pos: p.state.pos,
                    data: KIND_PLAYER,
                },
            ),
        );
        Some(Snapshot {
            tick: self.tick,
            entities: list.into_iter().collect(),
        })
    }

    fn run_tick(&mut self) {
        self.tick += 1;
        self.stats.ticks += 1;
        let t = Instant::now();

        // 1. Inputs de jugadores, con créditos: como mucho 1 por tick de media, con ráfagas acotadas.
        let mut moved: Vec<(EntityId, [i32; 3])> = Vec::new();
        for p in self.players.values_mut().filter(|p| p.joined) {
            p.credits = (p.credits + 1).min(self.cfg.credit_cap);
            let before = p.state.pos;
            while p.credits > 0 {
                let Some((seq, input)) = p.queue.pop_front() else {
                    break;
                };
                p.state = MoveSim::step(&p.state, &input);
                p.last_processed = seq;
                p.credits -= 1;
                self.stats.inputs_applied += 1;
            }
            if p.state.pos != before {
                moved.push((p.entity, p.state.pos));
            }
        }
        for (id, pos) in moved {
            self.world.move_to(id, vec3_from_q(pos));
        }
        self.stats.phase_inputs_us += t.elapsed().as_micros() as u64;
        let t = Instant::now();

        // 2. Apagado selectivo: solo se simulan los NPC de regiones cercanas a algún jugador.
        let spheres: Vec<(Vec3, f32)> = self
            .players
            .values()
            .filter(|p| p.joined)
            .map(|p| (vec3_from_q(p.state.pos), self.cfg.sim_radius))
            .collect();
        let active = self.world.active_cells(&spheres);
        let ids = self.world.entities_in_cells(&active);
        let step = self.cfg.npc_speed * self.cfg.tick_ms as f32 / 1000.0;
        let half = dequantize(HALF_EXTENT_Q);
        let mut simulated = 0u64;
        for id in ids.into_iter().filter(|&id| id >= NPC_BASE) {
            let Some(pos) = self.world.position(id) else {
                continue;
            };
            let npc = &mut self.npcs[(id - NPC_BASE) as usize];
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
            self.npc_state[(id - NPC_BASE) as usize].pos =
                [quantize(nx), quantize(pos.y), quantize(nz)];
            simulated += 1;
        }
        self.stats.npcs_simulated += simulated;
        self.stats.npcs_total += self.npcs.len() as u64;
        self.stats.phase_npc_us += t.elapsed().as_micros() as u64;
        let t = Instant::now();

        // 3. Interés y snapshots.
        let observers: Vec<(EntityId, f32)> = self
            .players
            .values()
            .filter(|p| p.joined)
            .map(|p| (p.entity, self.cfg.interest_radius))
            .collect();
        self.interest.update(&self.world, &observers);
        self.stats.phase_interest_us += t.elapsed().as_micros() as u64;

        let mut sessions: Vec<SessionId> = self
            .players
            .iter()
            .filter(|(_, p)| p.joined)
            .map(|(&s, _)| s)
            .collect();
        sessions.sort_unstable();
        for session in sessions {
            self.send_snapshot(session);
        }
    }

    fn send_snapshot(&mut self, session: SessionId) {
        let t = Instant::now();
        let Some(snap) = self.build_snapshot(session) else {
            return;
        };
        self.stats.phase_build_us += t.elapsed().as_micros() as u64;
        let t = Instant::now();
        let tick = self.tick;
        let max_age = self.cfg.max_baseline_age;
        let history_max = self.cfg.history_max;
        let Some(p) = self.players.get_mut(&session) else {
            return;
        };
        let baseline = p.baseline_tick.and_then(|bt| {
            p.history
                .iter()
                .find(|(t, _)| *t == bt)
                .map(|(_, s)| s)
                .filter(|s| tick.wrapping_sub(s.tick) <= max_age)
        });
        let (kind, encoded) = match baseline {
            Some(b) => (proto::SNAP_DELTA, snap.encode_delta(b)),
            None => (proto::SNAP_FULL, snap.encode_full()),
        };
        let hdr = p.endpoint.next_header();
        let msg = proto::frame(hdr, kind, &proto::snapshot_body(p.last_processed, &encoded));
        p.sent.insert(hdr.seq, tick);
        p.history.push_back((tick, snap));
        while p.history.len() > history_max {
            p.history.pop_front();
        }
        if kind == proto::SNAP_DELTA {
            self.stats.snapshots_delta += 1;
            self.stats.bytes_delta += msg.len() as u64;
        } else {
            self.stats.snapshots_full += 1;
            self.stats.bytes_full += msg.len() as u64;
        }
        self.stats.phase_encode_us += t.elapsed().as_micros() as u64;
        let t = Instant::now();
        let _ = self.net.send(session, &msg);
        self.stats.phase_send_us += t.elapsed().as_micros() as u64;
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
