//! Cluster de shards: **un mismo mundo repartido entre varios servidores**.
//!
//! Cada shard es dueño de una región rectangular ([`Partition`]) y simula solo lo que hay en ella. Para que
//! el borde no se note:
//!
//! * **Fantasmas**: cada tick, un shard copia a sus vecinos las entidades que están a menos de
//!   `ghost_margin` de su región (todos los jugadores cercanos, y los NPC que podría ver algún jugador del
//!   vecino). Son de solo lectura: se ven, empujan a los NPC del vecino a despertar, pero no se simulan.
//!   Si una copia deja de refrescarse, caduca sola (`ghost_ttl_ticks`): así se toleran paquetes perdidos,
//!   traspasos y jugadores que se alejan, sin mensajes de «borrar».
//! * **Traspaso de NPC**: cuando un NPC se adentra `npc_depth` unidades en la región vecina, se congela y
//!   se envía su estado completo (posición exacta, dirección y generador aleatorio). El mensaje se
//!   reenvía hasta recibir acuse; quien lo recibe recuerda `(origen, id)` para no adoptarlo dos veces.
//!   Mientras tanto el emisor lo sigue mostrando, así que no hay parpadeo.
//! * **Traspaso de jugador** (dirigido por el cliente, que es quien tiene la conexión):
//!   `PREPARE` (el destino se prepara) → `PREP_ACK` → `REDIRECT` al cliente → el cliente se conecta al
//!   destino con su propia clave de cuenta y envía `JOIN` → el destino pide `ACTIVATE` → el origen **se
//!   congela** y responde `HANDOVER` con el estado final → el destino lo adopta, responde `HAND_ACK` y
//!   da la bienvenida al cliente. La autoridad pasa en un único instante (el `HANDOVER`); todo lo demás
//!   se reintenta y es idempotente. Si el cliente nunca llega, el origen cancela con `ABORT` y el jugador
//!   sigue donde estaba.
//!
//! Garantías que comprueban las pruebas: ningún NPC se pierde ni se duplica (una vez asentada la red),
//! ningún jugador queda sin dueño ni con dos, y lo que ve un jugador junto al borde coincide con la
//! verdad del conjunto del cluster.

use tsofgw_net::{quantize, EntityState};
use tsofgw_transport::{Authority, Entropy, Link, ServerConfig, SessionId};
use tsofgw_world::{EntityId, FastMap, Rng, Vec3};

use super::{vec3_from_q, GameConfig, GameServer, Npc, KIND_NPC, KIND_PLAYER, NPC_BASE};
use crate::bus::{ghost_chunks, NpcXfer, PeerBus, PeerMsg, MAX_PEER_MSG};
use crate::partition::{Partition, Rect, ShardId};
use crate::protocol::{self as proto, Welcome};
use crate::sim::PlayerState;
use crate::states::Own;

/// Margen extra (unidades) sobre el radio que cubre un fantasma: movimiento durante el retraso del bus.
const SLACK: f32 = 8.0;

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub shard: ShardId,
    pub partition: Partition,
    /// Distancia al borde dentro de la cual las entidades se copian a la región vecina.
    pub ghost_margin: f32,
    /// Ticks sin refresco tras los cuales caduca un fantasma.
    pub ghost_ttl_ticks: u32,
    /// Cuánto debe adentrarse un NPC en otra región para cambiar de dueño.
    pub npc_depth: f32,
    /// Lo mismo para un jugador (más grande: un jugador que va y viene sobre el borde no debe saltar).
    pub player_depth: f32,
    /// Cada cuántos ticks se reenvía lo que no ha recibido acuse.
    pub retry_ticks: u32,
    /// Ticks tras un traspaso (o cancelación) en los que ese jugador no vuelve a traspasarse.
    pub handoff_cooldown_ticks: u32,
    /// Ticks tras los cuales se abandona un traspaso de jugador que no avanza.
    pub handoff_timeout_ticks: u32,
    /// Ticks durante los que se recuerdan los traspasos de NPC ya adoptados (para detectar repeticiones).
    pub xfer_memory_ticks: u32,
}

impl ClusterConfig {
    /// Valores por defecto razonables para la configuración de juego `game`.
    pub fn new(
        shard: ShardId,
        partition: Partition,
        game: &GameConfig,
    ) -> Result<ClusterConfig, String> {
        let reach = game.interest_radius * game.hysteresis;
        let c = ClusterConfig {
            shard,
            partition,
            ghost_margin: game.sim_radius.max(reach) + SLACK + 2.0,
            ghost_ttl_ticks: 6,
            npc_depth: 1.5,
            player_depth: 6.0,
            retry_ticks: 4,
            handoff_cooldown_ticks: 40,
            handoff_timeout_ticks: 200,
            xfer_memory_ticks: 1200,
        };
        c.validate(game)?;
        Ok(c)
    }

    /// Resumen de todo lo que debe ser **igual en todos los shards** para que el mundo sea coherente:
    /// la rejilla, el tick, los radios, los NPC (cuántos, qué velocidad, qué semilla) y el margen de
    /// fantasmas. Lo que es política local (hilos, límites de jugadores, reintentos...) no cuenta.
    pub fn digest(&self, game: &GameConfig) -> u64 {
        let (nx, nz, half) = self.partition.shape();
        let mut b: Vec<u8> = b"tsofgw/cluster-config/v1".to_vec();
        b.extend_from_slice(&nx.to_le_bytes());
        b.extend_from_slice(&nz.to_le_bytes());
        for f in [
            half,
            game.interest_radius,
            game.hysteresis,
            game.cell_size,
            game.sim_radius,
            game.npc_speed,
            self.ghost_margin,
        ] {
            b.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        b.extend_from_slice(&game.tick_ms.to_le_bytes());
        b.extend_from_slice(&(game.npc_count as u64).to_le_bytes());
        b.extend_from_slice(&game.seed.to_le_bytes());
        let h = tsofgw_pack::sha256(&b);
        u64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]])
    }

    pub fn validate(&self, game: &GameConfig) -> Result<(), String> {
        if !self.partition.is_valid(self.shard) {
            return Err(format!(
                "el shard {} no existe en una rejilla de {} regiones",
                self.shard,
                self.partition.shard_count()
            ));
        }
        if self.partition.shard_count() > 1 {
            let need = game.sim_radius.max(game.interest_radius * game.hysteresis) + SLACK;
            if self.ghost_margin < need {
                return Err(format!(
                    "ghost_margin ({}) no cubre el radio de simulación e interés: hace falta al menos {need}",
                    self.ghost_margin
                ));
            }
            if self.ghost_margin * 2.0 > self.partition.tile_size() {
                return Err(format!(
                    "las regiones ({} u de lado) son demasiado pequeñas para un margen de {}: \
                     deben medir al menos el doble",
                    self.partition.tile_size(),
                    self.ghost_margin
                ));
            }
        }
        if self.ghost_ttl_ticks < 2 || self.retry_ticks == 0 {
            return Err("ghost_ttl_ticks debe ser ≥ 2 y retry_ticks ≥ 1".into());
        }
        if self.handoff_timeout_ticks < self.retry_ticks.saturating_mul(4) {
            return Err("handoff_timeout_ticks debe ser al menos 4 × retry_ticks".into());
        }
        if !(self.npc_depth >= 0.0 && self.player_depth >= 0.0) {
            return Err("las profundidades de traspaso no pueden ser negativas".into());
        }
        Ok(())
    }
}

/// Contadores del cluster (acumulados desde que arrancó el shard).
#[derive(Debug, Clone, Default)]
pub struct ClusterStats {
    pub bus_msgs_out: u64,
    pub bus_bytes_out: u64,
    pub bus_msgs_in: u64,
    pub bus_bytes_in: u64,
    /// Mensajes del bus que no se pudieron decodificar o venían de un origen imposible.
    pub bus_malformed: u64,
    /// Datagramas que el propio bus descartó (autenticación, repetición, reloj, formato...).
    pub bus_rejected: u64,
    /// De ellos, los de un shard auténtico pero con otra configuración de mundo.
    pub bus_config_mismatch: u64,
    pub ghost_msgs_out: u64,
    pub ghost_players_out: u64,
    pub ghost_npcs_out: u64,
    pub ghost_applied: u64,
    pub ghost_stale: u64,
    pub ghost_expired: u64,
    pub npc_xfers_out: u64,
    pub npc_xfer_retries: u64,
    pub npc_xfers_in: u64,
    pub npc_xfer_dups: u64,
    pub npc_xfer_conflicts: u64,
    pub handoffs_started: u64,
    pub handoffs_completed: u64,
    pub handoffs_aborted: u64,
    pub redirect_retries: u64,
    /// Traspasos que el origen dio por hechos sin recibir el acuse final.
    pub handovers_unacked: u64,
    pub arrivals_completed: u64,
    pub arrivals_expired: u64,
}

/// Estado de traspaso de un jugador conectado a este shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handoff {
    None,
    /// Se avisó al destino; se espera su acuse.
    Preparing {
        to: ShardId,
        xfer: u32,
        since: u32,
        last_sent: u32,
    },
    /// El destino está listo; se ha dicho al cliente que se conecte allí.
    Redirecting {
        to: ShardId,
        xfer: u32,
        since: u32,
        last_sent: u32,
    },
    /// La autoridad ya se entregó: este shard no simula ni replica a este jugador.
    HandedOver {
        to: ShardId,
        xfer: u32,
        pos: [i32; 3],
        last_processed: u32,
        since: u32,
        last_sent: u32,
    },
}

struct Ghost {
    /// Tick local en que se refrescó por última vez.
    seen: u32,
    /// Tick del emisor del último estado aplicado (descarta mensajes viejos que llegan tarde).
    sent_tick: u32,
    from: ShardId,
}

struct OutXfer {
    to: ShardId,
    sent: u32,
    msg: NpcXfer,
}

/// Un jugador que otro shard va a pasarnos.
struct Arrival {
    from: ShardId,
    xfer: u32,
    created: u32,
    /// Sesión del cliente ya conectada aquí (llega cuando envía su `JOIN`).
    session: Option<SessionId>,
    last_activate: Option<u32>,
    /// Tick del primer `ACTIVATE`: el tiempo hasta recibir el `HANDOVER` es el que el jugador estuvo
    /// congelado, y se le devuelve como inputs extra.
    first_activate: Option<u32>,
}

pub(crate) struct ClusterNode {
    pub cfg: ClusterConfig,
    bus: Box<dyn PeerBus>,
    neighbors: Vec<(ShardId, Rect)>,
    ghosts: FastMap<EntityId, Ghost>,
    npc_out: FastMap<EntityId, OutXfer>,
    seen_xfers: FastMap<(ShardId, u32), u32>,
    arrivals: FastMap<u64, Arrival>,
    next_xfer: u32,
    pub stats: ClusterStats,
    inbox: Vec<(ShardId, Vec<u8>)>,
}

impl ClusterNode {
    pub fn new(cfg: ClusterConfig, bus: Box<dyn PeerBus>) -> ClusterNode {
        let neighbors = cfg
            .partition
            .neighbors(cfg.shard)
            .into_iter()
            .map(|s| (s, cfg.partition.rect(s)))
            .collect();
        ClusterNode {
            cfg,
            bus,
            neighbors,
            ghosts: FastMap::default(),
            npc_out: FastMap::default(),
            seen_xfers: FastMap::default(),
            arrivals: FastMap::default(),
            next_xfer: 0,
            stats: ClusterStats::default(),
            inbox: Vec::new(),
        }
    }

    fn next_xfer(&mut self) -> u32 {
        self.next_xfer = self.next_xfer.wrapping_add(1).max(1);
        self.next_xfer
    }

    pub fn region(&self) -> Rect {
        self.cfg.partition.rect(self.cfg.shard)
    }

    /// Un cliente esperado acaba de conectarse a este shard. `false` si no se esperaba a ese jugador.
    pub(super) fn register_arrival(&mut self, player: u64, session: SessionId) -> bool {
        match self.arrivals.get_mut(&player) {
            Some(a) => {
                a.session = Some(session);
                true
            }
            None => false,
        }
    }

    /// Quita una copia (si la había) para que el jugador real ocupe su sitio.
    pub(super) fn take_ghost(&mut self, id: EntityId) -> bool {
        self.ghosts.remove(&id).is_some()
    }
}

enum Act {
    Prepare(SessionId),
    Redirect(SessionId, ShardId),
    Handover(SessionId),
    Abort(SessionId, ShardId, u32),
    GiveUp(SessionId),
}

impl Act {
    fn session(&self) -> SessionId {
        match self {
            Act::Prepare(s)
            | Act::Redirect(s, _)
            | Act::Handover(s)
            | Act::Abort(s, _, _)
            | Act::GiveUp(s) => *s,
        }
    }
}

impl<L: Link> GameServer<L> {
    /// Un shard de un cluster: simula solo su región y habla con los demás por `bus`.
    pub fn new_shard(
        link: L,
        authority: Authority,
        cfg: GameConfig,
        transport: ServerConfig,
        entropy: Box<dyn Entropy>,
        cluster: ClusterConfig,
        bus: Box<dyn PeerBus>,
    ) -> Result<Self, String> {
        cluster.validate(&cfg)?;
        let mut bus = bus;
        bus.set_config_digest(cluster.digest(&cfg));
        Ok(Self::build(
            link,
            authority,
            cfg,
            transport,
            entropy,
            Some(ClusterNode::new(cluster, bus)),
        ))
    }

    // ------------------------------------------------------------------ consulta

    pub fn shard_id(&self) -> Option<ShardId> {
        self.cluster.as_ref().map(|c| c.cfg.shard)
    }

    pub fn cluster_config(&self) -> Option<&ClusterConfig> {
        self.cluster.as_ref().map(|c| &c.cfg)
    }

    pub fn cluster_stats(&self) -> Option<&ClusterStats> {
        self.cluster.as_ref().map(|c| &c.stats)
    }

    /// Copias de solo lectura que este shard tiene ahora mismo (NPC y jugadores de otros shards).
    pub fn ghost_count(&self) -> usize {
        self.cluster.as_ref().map_or(0, |c| c.ghosts.len())
    }

    /// NPC que este shard simula.
    pub fn owned_npc_count(&self) -> usize {
        self.ents.owned_npcs()
    }

    /// ¿Este shard es responsable de ese NPC (lo simula o lo tiene congelado en un traspaso)?
    pub fn holds_npc(&self, id: EntityId) -> bool {
        id >= NPC_BASE
            && ((id - NPC_BASE) as usize) < self.npcs.len()
            && matches!(
                self.ents.npc_own((id - NPC_BASE) as usize),
                Own::Owned | Own::Transferring
            )
    }

    /// Identificadores de los NPC de los que este shard es responsable (para inspección y pruebas).
    pub fn held_npc_ids(&self) -> Vec<EntityId> {
        (0..self.npcs.len())
            .filter(|&i| matches!(self.ents.npc_own(i), Own::Owned | Own::Transferring))
            .map(|i| NPC_BASE + i as u64)
            .collect()
    }

    /// Cuentas de los jugadores sobre los que este shard manda (incluye un jugador recién entregado
    /// hasta que el destino acusa recibo).
    pub fn owned_player_ids(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self.by_entity.keys().copied().collect();
        v.sort_unstable();
        v
    }

    /// ¿Lo tiene como copia de solo lectura?
    pub fn has_ghost(&self, id: EntityId) -> bool {
        self.cluster
            .as_ref()
            .is_some_and(|c| c.ghosts.contains_key(&id))
    }

    /// Traspasos de NPC enviados que aún no han recibido acuse.
    pub fn pending_npc_transfers(&self) -> usize {
        self.cluster.as_ref().map_or(0, |c| c.npc_out.len())
    }

    /// Jugadores con un traspaso en curso (en cualquiera de sus fases) o esperados aquí.
    pub fn pending_player_handoffs(&self) -> usize {
        self.players
            .values()
            .filter(|p| !matches!(p.handoff, Handoff::None))
            .count()
            + self.cluster.as_ref().map_or(0, |c| c.arrivals.len())
    }

    /// ¿Este shard manda sobre ese jugador?
    pub fn owns_player(&self, player_id: u64) -> bool {
        self.by_entity.contains_key(&player_id)
    }

    /// Estado replicado (cuantizado) de cualquier entidad que este shard conozca, propia o copia.
    pub fn entity_state(&self, id: EntityId) -> Option<EntityState> {
        self.ents.get(id).cloned()
    }

    // ------------------------------------------------------------------ bus

    fn peer_send(&mut self, to: ShardId, msg: &PeerMsg) {
        let now = self.now_ms;
        if let Some(c) = self.cluster.as_mut() {
            let bytes = msg.encode();
            c.stats.bus_msgs_out += 1;
            c.stats.bus_bytes_out += bytes.len() as u64;
            c.bus.send(now, to, bytes);
        }
    }

    /// Lee del bus todo lo llegado hasta `now` y lo aplica.
    pub(super) fn cluster_poll(&mut self, now: u64) {
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let mut inbox = std::mem::take(&mut c.inbox);
        c.bus.recv(now, &mut inbox);
        c.stats.bus_rejected = c.bus.rejected();
        c.stats.bus_config_mismatch = c.bus.config_mismatches();
        for (from, data) in inbox.drain(..) {
            let Some(c) = self.cluster.as_mut() else {
                return;
            };
            c.stats.bus_msgs_in += 1;
            c.stats.bus_bytes_in += data.len() as u64;
            if from == c.cfg.shard || !c.cfg.partition.is_valid(from) {
                c.stats.bus_malformed += 1;
                continue;
            }
            match PeerMsg::decode(&data) {
                Ok(m) => self.on_peer(from, m),
                Err(_) => c.stats.bus_malformed += 1,
            }
        }
        if let Some(c) = self.cluster.as_mut() {
            c.inbox = inbox;
        }
    }

    fn on_peer(&mut self, from: ShardId, m: PeerMsg) {
        match m {
            PeerMsg::Ghosts {
                tick,
                players,
                npcs,
            } => self.on_ghosts(from, tick, players, npcs),
            PeerMsg::NpcXfer(x) => self.on_npc_xfer(from, x),
            PeerMsg::NpcXferAck { xfer, id } => self.on_npc_xfer_ack(from, xfer, id),
            PeerMsg::Prepare { xfer, player, .. } => self.on_prepare(from, xfer, player),
            PeerMsg::PrepareAck { xfer, player } => self.on_prepare_ack(from, xfer, player),
            PeerMsg::Activate { xfer, player } => self.on_activate(from, xfer, player),
            PeerMsg::Handover {
                xfer,
                player,
                pos,
                last_processed,
            } => self.on_handover(from, xfer, player, pos, last_processed),
            PeerMsg::HandoverAck { xfer, player } => self.on_handover_ack(from, xfer, player),
            PeerMsg::Abort { xfer, player } => self.on_abort(from, xfer, player),
        }
    }

    // ------------------------------------------------------------------ fantasmas

    fn on_ghosts(
        &mut self,
        from: ShardId,
        sent_tick: u32,
        players: Vec<(EntityId, EntityState)>,
        npcs: Vec<(EntityId, EntityState)>,
    ) {
        let now = self.tick;
        let npc_total = self.npcs.len();
        for (id, st) in players {
            // Lo que es mío manda sobre cualquier copia (también un jugador ya entregado, hasta su acuse).
            if self.by_entity.contains_key(&id) {
                continue;
            }
            let Some(c) = self.cluster.as_mut() else {
                return;
            };
            if c.ghosts
                .get(&id)
                .is_some_and(|g| g.from == from && sent_tick < g.sent_tick)
            {
                c.stats.ghost_stale += 1;
                continue;
            }
            c.ghosts.insert(
                id,
                Ghost {
                    seen: now,
                    sent_tick,
                    from,
                },
            );
            c.stats.ghost_applied += 1;
            let pos = vec3_from_q(st.pos);
            if self.world.position(id).is_some() {
                self.world.move_to(id, pos);
            } else {
                self.world.spawn(id, pos);
            }
            self.ents.set_player(
                id,
                EntityState {
                    pos: st.pos,
                    data: KIND_PLAYER,
                },
            );
        }
        for (id, st) in npcs {
            let idx = (id - NPC_BASE) as usize;
            if idx >= npc_total {
                continue; // un NPC que no existe: se ignora
            }
            let own = self.ents.npc_own(idx);
            if matches!(own, Own::Owned | Own::Transferring) {
                continue;
            }
            let Some(c) = self.cluster.as_mut() else {
                return;
            };
            if c.ghosts
                .get(&id)
                .is_some_and(|g| g.from == from && sent_tick < g.sent_tick)
            {
                c.stats.ghost_stale += 1;
                continue;
            }
            c.ghosts.insert(
                id,
                Ghost {
                    seen: now,
                    sent_tick,
                    from,
                },
            );
            c.stats.ghost_applied += 1;
            let pos = vec3_from_q(st.pos);
            if own == Own::Absent {
                self.world.spawn(id, pos);
            } else {
                self.world.move_to(id, pos);
            }
            *self.ents.npc_mut(idx) = EntityState {
                pos: st.pos,
                data: KIND_NPC,
            };
            self.ents.set_npc_own(idx, Own::Ghost);
        }
    }

    fn remove_ghost_entity(&mut self, id: EntityId) {
        self.world.despawn(id);
        if id >= NPC_BASE {
            self.ents.set_npc_own((id - NPC_BASE) as usize, Own::Absent);
        } else {
            self.ents.remove_player(id);
        }
    }

    /// Posiciones de los jugadores de otros shards que hay aquí como copia: también despiertan a los NPC.
    pub(super) fn cluster_ghost_player_positions(&self) -> Vec<Vec3> {
        let Some(c) = self.cluster.as_ref() else {
            return Vec::new();
        };
        let mut ids: Vec<EntityId> = c
            .ghosts
            .keys()
            .copied()
            .filter(|&id| id < NPC_BASE)
            .collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| self.ents.get(id))
            .map(|st| vec3_from_q(st.pos))
            .collect()
    }

    /// Envía a cada vecino las entidades propias que le conciernen.
    pub(super) fn cluster_feed(&mut self) {
        let Some(c) = self.cluster.as_ref() else {
            return;
        };
        let margin = c.cfg.ghost_margin;
        let reach = self.cfg.interest_radius * self.cfg.hysteresis + SLACK;
        let owned: Vec<(EntityId, Vec3, [i32; 3])> = {
            let mut v: Vec<_> = self
                .players
                .values()
                .filter(|p| p.joined && !p.frozen())
                .map(|p| (p.entity, vec3_from_q(p.state.pos), p.state.pos))
                .collect();
            v.sort_unstable_by_key(|e| e.0);
            v
        };
        let mut out: Vec<(ShardId, PeerMsg)> = Vec::new();
        for &(to, rect) in &c.neighbors {
            let players: Vec<(EntityId, EntityState)> = owned
                .iter()
                .filter(|(_, pos, _)| rect.distance(*pos) <= margin)
                .map(|&(id, _, q)| {
                    (
                        id,
                        EntityState {
                            pos: q,
                            data: KIND_PLAYER,
                        },
                    )
                })
                .collect();
            // Los NPC solo interesan si hay jugadores del vecino cerca: los conocemos por sus fantasmas.
            let mut watchers: Vec<EntityId> = c
                .ghosts
                .iter()
                .filter(|(&id, g)| g.from == to && id < NPC_BASE)
                .map(|(&id, _)| id)
                .collect();
            watchers.sort_unstable();
            let spheres: Vec<(Vec3, f32)> = watchers
                .iter()
                .filter_map(|&id| self.ents.get(id))
                .map(|st| (vec3_from_q(st.pos), reach))
                .collect();
            let mut npcs: Vec<(EntityId, EntityState)> = Vec::new();
            if !spheres.is_empty() {
                let cells = self.world.active_cells(&spheres);
                for id in self.world.entities_in_cells(&cells) {
                    if id < NPC_BASE {
                        continue;
                    }
                    let idx = (id - NPC_BASE) as usize;
                    if self.ents.npc_own(idx) != Own::Owned {
                        continue;
                    }
                    let st = self.ents.npc_ref(idx);
                    if rect.distance(vec3_from_q(st.pos)) <= margin {
                        npcs.push((id, st.clone()));
                    }
                }
                npcs.sort_unstable_by_key(|e| e.0);
            }
            // Un mensaje por datagrama: lo que no cabe en uno se reparte en varios del mismo tick.
            for msg in ghost_chunks(self.tick, players, npcs, MAX_PEER_MSG) {
                out.push((to, msg));
            }
        }
        for (to, msg) in out {
            if let (Some(c), PeerMsg::Ghosts { players, npcs, .. }) = (self.cluster.as_mut(), &msg)
            {
                c.stats.ghost_msgs_out += 1;
                c.stats.ghost_players_out += players.len() as u64;
                c.stats.ghost_npcs_out += npcs.len() as u64;
            }
            self.peer_send(to, &msg);
        }
    }

    // ------------------------------------------------------------------ temporizadores

    /// Al principio de cada tick: caducidad de fantasmas, reenvíos y plazos.
    pub(super) fn cluster_begin_tick(&mut self) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let ttl = c.cfg.ghost_ttl_ticks;
        let mut expired: Vec<EntityId> = c
            .ghosts
            .iter()
            .filter(|(_, g)| tick.wrapping_sub(g.seen) > ttl)
            .map(|(&id, _)| id)
            .collect();
        expired.sort_unstable();
        for id in &expired {
            c.ghosts.remove(id);
        }
        c.stats.ghost_expired += expired.len() as u64;

        let retry = c.cfg.retry_ticks;
        let mut resend: Vec<(ShardId, NpcXfer)> = Vec::new();
        for out in c.npc_out.values_mut() {
            if tick.wrapping_sub(out.sent) >= retry {
                out.sent = tick;
                resend.push((out.to, out.msg.clone()));
            }
        }
        resend.sort_unstable_by_key(|(_, m)| m.id);
        c.stats.npc_xfer_retries += resend.len() as u64;

        let memory = c.cfg.xfer_memory_ticks;
        c.seen_xfers.retain(|_, t| tick.wrapping_sub(*t) <= memory);

        // Llegadas: reclamar la autoridad cuando el cliente ya está aquí; descartar las que no avanzan.
        let timeout = c.cfg.handoff_timeout_ticks;
        let mut activate: Vec<(ShardId, u32, u64)> = Vec::new();
        let mut dead: Vec<(u64, Option<SessionId>)> = Vec::new();
        for (&player, a) in c.arrivals.iter_mut() {
            if tick.wrapping_sub(a.created) > timeout {
                dead.push((player, a.session));
            } else if a.session.is_some()
                && a.last_activate
                    .is_none_or(|t| tick.wrapping_sub(t) >= retry)
            {
                a.last_activate = Some(tick);
                a.first_activate.get_or_insert(tick);
                activate.push((a.from, a.xfer, player));
            }
        }
        dead.sort_unstable_by_key(|d| d.0);
        activate.sort_unstable_by_key(|a| a.2);
        for (player, _) in &dead {
            c.arrivals.remove(player);
        }
        c.stats.arrivals_expired += dead.len() as u64;

        for id in expired {
            self.remove_ghost_entity(id);
        }
        for (to, m) in resend {
            self.peer_send(to, &PeerMsg::NpcXfer(m));
        }
        for (from, xfer, player) in activate {
            self.peer_send(from, &PeerMsg::Activate { xfer, player });
        }
        for (_, session) in dead {
            if let Some(s) = session {
                // Su cliente se conectó aquí pero el traspaso no se completó: que vuelva al origen.
                self.net.kick(s);
                self.drop_player(s);
            }
        }
        self.cluster_player_timers();
    }

    /// Reenvíos y plazos de los traspasos de jugadores que este shard está entregando.
    fn cluster_player_timers(&mut self) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_ref() else {
            return;
        };
        let (retry, timeout) = (c.cfg.retry_ticks, c.cfg.handoff_timeout_ticks);
        let mut acts: Vec<Act> = Vec::new();
        for (&s, p) in self.players.iter_mut() {
            match &mut p.handoff {
                Handoff::None => {}
                Handoff::Preparing {
                    to,
                    xfer,
                    since,
                    last_sent,
                } => {
                    if tick.wrapping_sub(*since) > timeout {
                        acts.push(Act::Abort(s, *to, *xfer));
                    } else if tick.wrapping_sub(*last_sent) >= retry {
                        *last_sent = tick;
                        acts.push(Act::Prepare(s));
                    }
                }
                Handoff::Redirecting {
                    to,
                    xfer,
                    since,
                    last_sent,
                } => {
                    if tick.wrapping_sub(*since) > timeout {
                        acts.push(Act::Abort(s, *to, *xfer));
                    } else if tick.wrapping_sub(*last_sent) >= retry * 2 {
                        *last_sent = tick;
                        acts.push(Act::Redirect(s, *to));
                    }
                }
                Handoff::HandedOver {
                    since, last_sent, ..
                } => {
                    if tick.wrapping_sub(*since) > timeout {
                        acts.push(Act::GiveUp(s));
                    } else if tick.wrapping_sub(*last_sent) >= retry {
                        *last_sent = tick;
                        acts.push(Act::Handover(s));
                    }
                }
            }
        }
        acts.sort_unstable_by_key(|a| a.session());
        for a in acts {
            match a {
                Act::Prepare(s) => {
                    if let Some(m) = self.prepare_msg(s) {
                        if let Some(Handoff::Preparing { to, .. }) =
                            self.players.get(&s).map(|p| p.handoff)
                        {
                            self.peer_send(to, &m);
                        }
                    }
                }
                Act::Redirect(s, to) => {
                    if let Some(c) = self.cluster.as_mut() {
                        c.stats.redirect_retries += 1;
                    }
                    self.send_redirect(s, to);
                }
                Act::Handover(s) => self.resend_handover(s),
                Act::Abort(s, to, xfer) => {
                    let Some(p) = self.players.get_mut(&s) else {
                        continue;
                    };
                    p.handoff = Handoff::None;
                    p.cooldown_until = tick.wrapping_add(
                        self.cluster
                            .as_ref()
                            .map_or(0, |c| c.cfg.handoff_cooldown_ticks),
                    );
                    let player = p.player_id;
                    if let Some(c) = self.cluster.as_mut() {
                        c.stats.handoffs_aborted += 1;
                    }
                    self.peer_send(to, &PeerMsg::Abort { xfer, player });
                }
                Act::GiveUp(s) => {
                    if let Some(Handoff::HandedOver { to, .. }) =
                        self.players.get(&s).map(|p| p.handoff)
                    {
                        if let Some(c) = self.cluster.as_mut() {
                            c.stats.handovers_unacked += 1;
                        }
                        self.finish_handover(s, to);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ NPC

    /// Un NPC propio ya está dentro de otra región: se congela y se envía.
    pub(super) fn cluster_start_npc_xfer(
        &mut self,
        id: EntityId,
        idx: usize,
        pos: Vec3,
        to: ShardId,
    ) {
        let tick = self.tick;
        let npc = &self.npcs[idx];
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let xfer = c.next_xfer();
        let msg = NpcXfer {
            xfer,
            id,
            pos: [pos.x, pos.y, pos.z],
            dir: npc.dir,
            turn_in: npc.turn_in,
            rng: npc.rng.state(),
        };
        c.npc_out.insert(
            id,
            OutXfer {
                to,
                sent: tick,
                msg: msg.clone(),
            },
        );
        c.stats.npc_xfers_out += 1;
        self.ents.set_npc_own(idx, Own::Transferring);
        self.peer_send(to, &PeerMsg::NpcXfer(msg));
    }

    fn on_npc_xfer(&mut self, from: ShardId, x: NpcXfer) {
        let idx = (x.id - NPC_BASE) as usize;
        if idx >= self.npcs.len() {
            return;
        }
        let tick = self.tick;
        let own = self.ents.npc_own(idx);
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let ack = PeerMsg::NpcXferAck {
            xfer: x.xfer,
            id: x.id,
        };
        if c.seen_xfers.contains_key(&(from, x.xfer)) || own == Own::Owned {
            // Una repetición (el acuse se perdió) o ya es mío: basta con acusar de nuevo.
            c.stats.npc_xfer_dups += 1;
            c.seen_xfers.insert((from, x.xfer), tick);
            self.peer_send(from, &ack);
            return;
        }
        if own == Own::Transferring {
            // Yo mismo lo estoy entregando: dos dueños a la vez no deberían ocurrir; no se acusa.
            c.stats.npc_xfer_conflicts += 1;
            return;
        }
        c.ghosts.remove(&x.id);
        c.seen_xfers.insert((from, x.xfer), tick);
        c.stats.npc_xfers_in += 1;
        let pos = Vec3::new(x.pos[0], x.pos[1], x.pos[2]);
        if own == Own::Absent {
            self.world.spawn(x.id, pos);
        } else {
            self.world.move_to(x.id, pos);
        }
        self.npcs[idx] = Npc {
            dir: x.dir,
            turn_in: x.turn_in,
            rng: Rng::from_state(x.rng),
        };
        *self.ents.npc_mut(idx) = EntityState {
            pos: [quantize(pos.x), quantize(pos.y), quantize(pos.z)],
            data: KIND_NPC,
        };
        self.ents.set_npc_own(idx, Own::Owned);
        self.peer_send(from, &ack);
    }

    fn on_npc_xfer_ack(&mut self, from: ShardId, xfer: u32, id: EntityId) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let Some(out) = c.npc_out.get(&id) else {
            return;
        };
        if out.to != from || out.msg.xfer != xfer {
            return;
        }
        c.npc_out.remove(&id);
        // Sigue visible aquí como copia mientras el nuevo dueño nos la refresque.
        c.ghosts.insert(
            id,
            Ghost {
                seen: tick,
                sent_tick: 0,
                from,
            },
        );
        self.ents.set_npc_own((id - NPC_BASE) as usize, Own::Ghost);
    }

    // ------------------------------------------------------------------ jugadores: origen

    /// Tras mover a los jugadores: ¿alguno se ha adentrado en otra región?
    pub(super) fn cluster_players_begin(&mut self) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_ref() else {
            return;
        };
        let (shard, depth) = (c.cfg.shard, c.cfg.player_depth);
        let mut starts: Vec<(SessionId, ShardId)> = Vec::new();
        for (&s, p) in self.players.iter() {
            if !p.joined
                || p.handoff != Handoff::None
                || tick.wrapping_sub(p.cooldown_until) > u32::MAX / 2
            {
                continue;
            }
            if let Some(to) = c
                .cfg
                .partition
                .deep_owner(shard, vec3_from_q(p.state.pos), depth)
            {
                starts.push((s, to));
            }
        }
        starts.sort_unstable_by_key(|s| s.0);
        for (s, to) in starts {
            let Some(c) = self.cluster.as_mut() else {
                return;
            };
            let xfer = c.next_xfer();
            c.stats.handoffs_started += 1;
            if let Some(p) = self.players.get_mut(&s) {
                p.handoff = Handoff::Preparing {
                    to,
                    xfer,
                    since: tick,
                    last_sent: tick,
                };
            }
            if let Some(m) = self.prepare_msg(s) {
                self.peer_send(to, &m);
            }
        }
    }

    fn prepare_msg(&self, session: SessionId) -> Option<PeerMsg> {
        let p = self.players.get(&session)?;
        let Handoff::Preparing { xfer, .. } = p.handoff else {
            return None;
        };
        Some(PeerMsg::Prepare {
            xfer,
            player: p.player_id,
            pos: p.state.pos,
            last_processed: p.last_processed,
        })
    }

    fn send_redirect(&mut self, session: SessionId, to: ShardId) {
        if let Some(p) = self.players.get_mut(&session) {
            let hdr = p.endpoint.next_header();
            let msg = proto::frame(hdr, proto::REDIRECT, &proto::redirect_body(to));
            let _ = self.net.send(session, &msg);
        }
    }

    fn on_prepare_ack(&mut self, from: ShardId, xfer: u32, player: u64) {
        let tick = self.tick;
        let Some(&session) = self.by_entity.get(&player) else {
            return;
        };
        let Some(p) = self.players.get_mut(&session) else {
            return;
        };
        if let Handoff::Preparing {
            to, xfer: x, since, ..
        } = p.handoff
        {
            if to == from && x == xfer {
                p.handoff = Handoff::Redirecting {
                    to,
                    xfer,
                    since,
                    last_sent: tick,
                };
                self.send_redirect(session, to);
            }
        }
    }

    fn on_activate(&mut self, from: ShardId, xfer: u32, player: u64) {
        let tick = self.tick;
        let Some(&session) = self.by_entity.get(&player) else {
            return;
        };
        let Some(p) = self.players.get_mut(&session) else {
            return;
        };
        let (to, x) = match p.handoff {
            Handoff::Preparing { to, xfer, .. } | Handoff::Redirecting { to, xfer, .. } => {
                (to, xfer)
            }
            Handoff::HandedOver { to, xfer, .. } => (to, xfer),
            Handoff::None => return,
        };
        if to != from || x != xfer {
            return; // de un intento anterior
        }
        if !matches!(p.handoff, Handoff::HandedOver { .. }) {
            // Aquí cambia la autoridad: desde este instante el jugador queda congelado en este shard.
            p.queue.clear();
            p.handoff = Handoff::HandedOver {
                to,
                xfer,
                pos: p.state.pos,
                last_processed: p.last_processed,
                since: tick,
                last_sent: tick,
            };
        }
        self.resend_handover(session);
    }

    fn resend_handover(&mut self, session: SessionId) {
        let Some(p) = self.players.get(&session) else {
            return;
        };
        if let Handoff::HandedOver {
            to,
            xfer,
            pos,
            last_processed,
            ..
        } = p.handoff
        {
            let player = p.player_id;
            self.peer_send(
                to,
                &PeerMsg::Handover {
                    xfer,
                    player,
                    pos,
                    last_processed,
                },
            );
        }
    }

    fn on_handover_ack(&mut self, from: ShardId, xfer: u32, player: u64) {
        let Some(&session) = self.by_entity.get(&player) else {
            return;
        };
        if let Some(Handoff::HandedOver { to, xfer: x, .. }) =
            self.players.get(&session).map(|p| p.handoff)
        {
            if to == from && x == xfer {
                self.finish_handover(session, to);
            }
        }
    }

    /// El jugador ya es del destino: aquí queda solo como copia hasta que caduque o se la refresquen.
    fn finish_handover(&mut self, session: SessionId, to: ShardId) {
        let tick = self.tick;
        let Some(p) = self.players.remove(&session) else {
            return;
        };
        if self.by_entity.get(&p.entity) == Some(&session) {
            self.by_entity.remove(&p.entity);
        }
        if let Some(c) = self.cluster.as_mut() {
            c.stats.handoffs_completed += 1;
            c.ghosts.insert(
                p.entity,
                Ghost {
                    seen: tick,
                    sent_tick: 0,
                    from: to,
                },
            );
        }
    }

    /// El cliente del jugador entregado se desconectó de este shard (lo normal tras pasar al nuevo).
    pub(super) fn cluster_handed_over_gone(&mut self, p: &super::Player, to: ShardId) {
        let tick = self.tick;
        if let Some(c) = self.cluster.as_mut() {
            c.stats.handoffs_completed += 1;
            c.ghosts.insert(
                p.entity,
                Ghost {
                    seen: tick,
                    sent_tick: 0,
                    from: to,
                },
            );
        }
    }

    /// Un jugador con traspaso a medias se fue: se avisa al destino para que no espere en vano.
    pub(super) fn cluster_abort_for_gone(&mut self, to: ShardId, xfer: u32, player: u64) {
        if let Some(c) = self.cluster.as_mut() {
            c.stats.handoffs_aborted += 1;
        }
        self.peer_send(to, &PeerMsg::Abort { xfer, player });
    }

    // ------------------------------------------------------------------ jugadores: destino

    fn on_prepare(&mut self, from: ShardId, xfer: u32, player: u64) {
        if player >= NPC_BASE || self.by_entity.contains_key(&player) {
            return;
        }
        let tick = self.tick;
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let same = c
            .arrivals
            .get(&player)
            .is_some_and(|a| a.from == from && a.xfer == xfer);
        if !same {
            c.arrivals.insert(
                player,
                Arrival {
                    from,
                    xfer,
                    created: tick,
                    session: None,
                    last_activate: None,
                    first_activate: None,
                },
            );
        }
        self.peer_send(from, &PeerMsg::PrepareAck { xfer, player });
    }

    fn on_abort(&mut self, from: ShardId, xfer: u32, player: u64) {
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        if c.arrivals
            .get(&player)
            .is_some_and(|a| a.from == from && a.xfer == xfer)
        {
            if let Some(a) = c.arrivals.remove(&player) {
                if let Some(s) = a.session {
                    self.net.kick(s);
                    self.drop_player(s);
                }
            }
        }
    }

    fn on_handover(
        &mut self,
        from: ShardId,
        xfer: u32,
        player: u64,
        pos: [i32; 3],
        last_processed: u32,
    ) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let Some(a) = c.arrivals.get(&player) else {
            // Una repetición tras completarse: ya es mío, solo falta volver a acusar.
            if self.by_entity.contains_key(&player) {
                self.peer_send(from, &PeerMsg::HandoverAck { xfer, player });
            }
            return;
        };
        if a.from != from || a.xfer != xfer {
            return;
        }
        let Some(session) = a.session else {
            return; // el cliente aún no ha llegado: no se puede completar
        };
        let cooldown = c.cfg.handoff_cooldown_ticks;
        let owed = tick.wrapping_sub(a.first_activate.unwrap_or(tick));
        c.arrivals.remove(&player);
        c.ghosts.remove(&player);
        c.stats.arrivals_completed += 1;

        let Some(p) = self.players.get_mut(&session) else {
            return; // su sesión ya no existe: el jugador se pierde (el origen lo da por entregado)
        };
        p.state = PlayerState { pos };
        p.last_queued = last_processed;
        p.last_processed = last_processed;
        p.credits = 0;
        p.queue.clear();
        p.joined = true;
        p.handoff = Handoff::None;
        p.cooldown_until = tick.wrapping_add(cooldown);
        p.catchup = owed.min(self.cfg.input_queue_max as u32);
        let p_entity = p.entity;
        let position = vec3_from_q(pos);
        if self.world.position(p_entity).is_some() {
            self.world.move_to(p_entity, position);
        } else {
            self.world.spawn(p_entity, position);
        }
        self.ents.set_player(
            p_entity,
            EntityState {
                pos,
                data: KIND_PLAYER,
            },
        );
        self.by_entity.insert(p_entity, session);
        let welcome = Welcome {
            entity: p_entity,
            tick,
            tick_ms: self.cfg.tick_ms as u16,
            pos,
        };
        if let Some(p) = self.players.get_mut(&session) {
            let hdr = p.endpoint.next_header();
            let msg = proto::frame(hdr, proto::WELCOME, &proto::welcome_body(&welcome));
            let _ = self.net.send(session, &msg);
        }
        self.peer_send(from, &PeerMsg::HandoverAck { xfer, player });
    }
}
