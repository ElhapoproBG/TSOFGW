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
use tsofgw_store::Batch;
use tsofgw_transport::{Authority, Entropy, Link, ServerConfig, SessionId};
use tsofgw_world::{EntityId, FastMap, Rng, Vec3};

use super::{vec3_from_q, GameConfig, GameServer, Npc, KIND_NPC, KIND_PLAYER, NPC_BASE};
use crate::bus::{ghost_chunks, DirStatus, NpcXfer, PeerBus, PeerMsg, MAX_PEER_MSG};
use crate::partition::{Partition, Rect, ShardId};
use crate::persist::{
    k_ho, k_npc, k_player, k_tomb, k_xi, k_xo, HoRec, HoStage, NpcRec, PlayerRec, TombRec, XoRec,
};
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
    /// Hay un **directorio de sesiones** en el bus (un par más, con identificador `shard_count`) y los
    /// shards le preguntan dónde vive cada cuenta antes de dejarla entrar. Debe valer lo mismo en todos
    /// los shards y en el directorio; exige activar la persistencia en cada shard.
    pub directory: bool,
    /// Réplicas del directorio (1, 3, 5 o 7; con una sola no hay consenso, como en la 0.5). Sus identificadores en
    /// el bus son `shard_count .. shard_count + dir_replicas`. Debe valer lo mismo en todo el cluster.
    pub dir_replicas: usize,
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
            directory: false,
            dir_replicas: 1,
        };
        c.validate(game)?;
        Ok(c)
    }

    /// Resumen de todo lo que debe ser **igual en todos los shards** para que el mundo sea coherente:
    /// la rejilla, el tick, los radios, los NPC (cuántos, qué velocidad, qué semilla) y el margen de
    /// fantasmas. Lo que es política local (hilos, límites de jugadores, reintentos...) no cuenta.
    pub fn digest(&self, game: &GameConfig) -> u64 {
        let (nx, nz, half) = self.partition.shape();
        let mut b: Vec<u8> = b"tsofgw/cluster-config/v2".to_vec();
        b.extend_from_slice(&nx.to_le_bytes());
        b.extend_from_slice(&nz.to_le_bytes());
        b.push(self.directory as u8);
        b.push(self.dir_replicas as u8);
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

    /// Identificador de la primera réplica del directorio de sesiones en el bus, si lo hay: el siguiente al
    /// último shard.
    pub fn dir_peer(&self) -> Option<ShardId> {
        self.directory
            .then(|| self.partition.shard_count() as ShardId)
    }

    /// ¿Es `id` una réplica del directorio?
    pub fn is_dir_peer(&self, id: ShardId) -> bool {
        self.dir_peer().is_some_and(|base| {
            id >= base && (id as usize) < base as usize + self.dir_replicas.max(1)
        })
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
        if self.directory
            && self.partition.shard_count() + self.dir_replicas >= ShardId::MAX as usize
        {
            return Err("con directorio no caben más de 65 534 pares (shards y réplicas)".into());
        }
        if self.directory && !matches!(self.dir_replicas, 1 | 3 | 5 | 7) {
            return Err("las réplicas del directorio deben ser 1, 3, 5 o 7".into());
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
    /// Traspasos de jugador que el destino acusó, y cuántos ticks pasaron en cada uno desde que la
    /// autoridad salió de este shard hasta ese acuse (suma y máximo). Es lo que cuesta el camino
    /// seguro: escribir a disco antes de entregar, anotar el cambio en el directorio y esperar al
    /// destino, que también escribe antes de acusar.
    pub handover_acked: u64,
    pub handover_ticks_sum: u64,
    pub handover_ticks_max: u32,
    pub arrivals_completed: u64,
    pub arrivals_expired: u64,
    /// Traspasos de jugador que se repitieron y se reenviaron porque seguían sin acuse.
    pub handover_retries: u64,
    /// `HANDOVER` repetidos o de una época que este shard ya conocía (se acusan, no se adoptan).
    pub handovers_stale: u64,
    /// `HANDOVER` de una cuenta que ya está conectada aquí con otra época (no debería ocurrir).
    pub handover_conflicts: u64,
    /// `HANDOVER` que llegaron mientras este shard aún esperaba el acuse de un traspaso de esa misma
    /// cuenta hacia fuera: se ignoran y el origen los reenvía.
    pub handovers_deferred: u64,
    /// Jugadores adoptados sin que su cliente estuviera aquí (quedan residentes hasta que entren).
    pub offline_adoptions: u64,
    /// Preguntas al directorio al entrar, respuestas recibidas y su resultado.
    pub dir_logins: u64,
    pub dir_rotations: u64,
    pub dir_redirects: u64,
    pub dir_routes_in: u64,
    pub dir_transfers_out: u64,
    pub dir_results_in: u64,
    /// Cambios de dueño que el directorio rechazó (la cuenta no estaba donde creíamos) o no conocía.
    pub dir_rejected: u64,
    pub dir_unknown: u64,
    /// Clientes que pidieron entrar en un shard que no era el de su cuenta (se les redirigió).
    pub login_redirects: u64,
    /// Entradas que tuvieron que esperar (a que el directorio contestara o llegara el estado).
    pub login_waits: u64,
}

impl ClusterStats {
    /// Suma los contadores de `other` a los de `self` (por ejemplo, de varias vidas de un shard que se
    /// reinició).
    pub fn merge(&mut self, other: &ClusterStats) {
        self.bus_msgs_out += other.bus_msgs_out;
        self.bus_bytes_out += other.bus_bytes_out;
        self.bus_msgs_in += other.bus_msgs_in;
        self.bus_bytes_in += other.bus_bytes_in;
        self.bus_malformed += other.bus_malformed;
        self.bus_rejected += other.bus_rejected;
        self.bus_config_mismatch += other.bus_config_mismatch;
        self.ghost_msgs_out += other.ghost_msgs_out;
        self.ghost_players_out += other.ghost_players_out;
        self.ghost_npcs_out += other.ghost_npcs_out;
        self.ghost_applied += other.ghost_applied;
        self.ghost_stale += other.ghost_stale;
        self.ghost_expired += other.ghost_expired;
        self.npc_xfers_out += other.npc_xfers_out;
        self.npc_xfer_retries += other.npc_xfer_retries;
        self.npc_xfers_in += other.npc_xfers_in;
        self.npc_xfer_dups += other.npc_xfer_dups;
        self.npc_xfer_conflicts += other.npc_xfer_conflicts;
        self.handoffs_started += other.handoffs_started;
        self.handoffs_completed += other.handoffs_completed;
        self.handoffs_aborted += other.handoffs_aborted;
        self.redirect_retries += other.redirect_retries;
        self.handovers_unacked += other.handovers_unacked;
        self.handover_acked += other.handover_acked;
        self.handover_ticks_sum += other.handover_ticks_sum;
        self.handover_ticks_max = self.handover_ticks_max.max(other.handover_ticks_max);
        self.arrivals_completed += other.arrivals_completed;
        self.arrivals_expired += other.arrivals_expired;
        self.handover_retries += other.handover_retries;
        self.handovers_stale += other.handovers_stale;
        self.handover_conflicts += other.handover_conflicts;
        self.handovers_deferred += other.handovers_deferred;
        self.offline_adoptions += other.offline_adoptions;
        self.dir_logins += other.dir_logins;
        self.dir_rotations += other.dir_rotations;
        self.dir_redirects += other.dir_redirects;
        self.dir_routes_in += other.dir_routes_in;
        self.dir_transfers_out += other.dir_transfers_out;
        self.dir_results_in += other.dir_results_in;
        self.dir_rejected += other.dir_rejected;
        self.dir_unknown += other.dir_unknown;
        self.login_redirects += other.login_redirects;
        self.login_waits += other.login_waits;
    }
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
    /// La autoridad ya se entregó: este shard no simula ni replica a este jugador. El estado que se
    /// lleva el destino y los reintentos viven en el buzón de salida, que no depende de la sesión.
    HandedOver {
        to: ShardId,
        xfer: u32,
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

/// Un traspaso de jugador que este shard ha entregado y aún no ha recibido acuse. Es **independiente de
/// la sesión**: sigue vivo aunque el cliente se desconecte, y con persistencia está también en disco, de
/// modo que sobrevive a un reinicio y se reintenta sin límite hasta que el destino lo acuse.
struct HoOut {
    to: ShardId,
    xfer: u32,
    /// Época que tendrá la cuenta en el destino (la de aquí más uno).
    epoch: u64,
    pos: [i32; 3],
    last_processed: u32,
    /// ¿El directorio ya anotó el cambio de dueño? (sin directorio, siempre). Hasta entonces el estado
    /// de la cuenta no se envía al destino: dos shards no pueden quedarse con ella.
    committed: bool,
    since: u32,
    last_sent: u32,
}

/// Un traspaso de jugador entregado y sin acuse, tal como lo ve quien inspecciona un shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxEntry {
    pub account: u64,
    /// Shard al que se entrega.
    pub to: ShardId,
    /// Época que tendrá la cuenta allí.
    pub epoch: u64,
    /// El directorio ya anotó el cambio de dueño (sin directorio, siempre). Hasta entonces la entrega
    /// puede cancelarse; después ya no.
    pub committed: bool,
}

/// Lo que respondió el directorio la última vez que se le preguntó por una cuenta.
#[derive(Debug, Clone, Copy)]
pub(super) struct Route {
    pub shard: ShardId,
    pub epoch: u64,
    at: u32,
}

/// Cuánto vale una respuesta del directorio que nadie ha usado (ticks).
const ROUTE_TTL_TICKS: u32 = 400;

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
    /// Traspasos de jugador entregados y sin acuse, por cuenta.
    outbox: FastMap<u64, HoOut>,
    /// Respuestas del directorio aún sin usar, y cuándo se preguntó por última vez cada cuenta.
    routes: FastMap<u64, Route>,
    asked: FastMap<u64, u32>,
    /// A qué réplica del directorio se le pregunta ahora (la última que se creyó líder) y cuántas peticiones
    /// seguidas llevan sin respuesta de ninguna.
    dir_hint: ShardId,
    dir_misses: u32,
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
        // Cada shard empieza por una réplica distinta, para no cargar a una sola mientras no se sabe quién lidera.
        let dir_hint = cfg.dir_peer().map_or(0, |base| {
            base + (cfg.shard as usize % cfg.dir_replicas.max(1)) as ShardId
        });
        ClusterNode {
            cfg,
            bus,
            neighbors,
            ghosts: FastMap::default(),
            npc_out: FastMap::default(),
            seen_xfers: FastMap::default(),
            arrivals: FastMap::default(),
            outbox: FastMap::default(),
            routes: FastMap::default(),
            asked: FastMap::default(),
            dir_hint,
            dir_misses: 0,
            next_xfer: 0,
            stats: ClusterStats::default(),
            inbox: Vec::new(),
        }
    }

    pub(super) fn next_xfer(&mut self) -> u32 {
        self.next_xfer = self.next_xfer.wrapping_add(1).max(1);
        self.next_xfer
    }

    /// Tras un reinicio: los números ya reservados (y quizá usados) no se repiten.
    pub(super) fn set_next_xfer(&mut self, v: u32) {
        self.next_xfer = v;
    }

    /// Identificador del directorio en el bus, si lo hay.
    /// La réplica a la que toca preguntar (la que se cree líder). `None` si no hay directorio.
    pub(super) fn dir_peer(&self) -> Option<ShardId> {
        self.cfg.directory.then_some(self.dir_hint)
    }

    /// Se va a enviar una petición al directorio: si varias seguidas no han recibido ni una respuesta (ni
    /// siquiera una redirección), se prueba con la siguiente réplica.
    pub(super) fn note_dir_send(&mut self) {
        self.dir_misses += 1;
        if self.dir_misses > 3 {
            self.dir_misses = 0;
            if let Some(base) = self.cfg.dir_peer() {
                let n = self.cfg.dir_replicas.max(1) as u32;
                let next = (self.dir_hint as u32 - base as u32 + 1) % n;
                self.dir_hint = base + next as ShardId;
                self.stats.dir_rotations += 1;
            }
        }
    }

    /// Llegó algo del directorio: hay alguien al otro lado.
    pub(super) fn note_dir_reply(&mut self) {
        self.dir_misses = 0;
    }

    /// Una réplica que no lidera nos dice quién cree que sí.
    pub(super) fn on_dir_redirect(&mut self, leader: ShardId) {
        self.stats.dir_redirects += 1;
        self.dir_misses = 0;
        if self.cfg.is_dir_peer(leader) {
            self.dir_hint = leader;
        }
    }

    pub(super) fn outbox_has(&self, account: u64) -> bool {
        self.outbox.contains_key(&account)
    }

    /// Los traspasos de jugador sin acuse, por cuenta.
    pub(super) fn outbox_summary(&self) -> Vec<OutboxEntry> {
        let mut v: Vec<_> = self
            .outbox
            .iter()
            .map(|(&account, h)| OutboxEntry {
                account,
                to: h.to,
                epoch: h.epoch,
                committed: h.committed,
            })
            .collect();
        v.sort_unstable_by_key(|e| e.account);
        v
    }

    /// Respuesta del directorio para una cuenta, si hay una vigente.
    pub(super) fn route(&self, account: u64, tick: u32) -> Option<Route> {
        self.routes
            .get(&account)
            .copied()
            .filter(|r| tick.wrapping_sub(r.at) <= ROUTE_TTL_TICKS)
    }

    /// La respuesta ya se usó: la próxima entrada de esa cuenta vuelve a preguntar.
    pub(super) fn consume_route(&mut self, account: u64) {
        self.routes.remove(&account);
    }

    /// ¿Toca (re)preguntar al directorio por esta cuenta? Como mucho una vez por `retry_ticks`.
    pub(super) fn should_ask_dir(&mut self, account: u64, tick: u32) -> bool {
        let retry = self.cfg.retry_ticks;
        match self.asked.get(&account) {
            Some(&t) if tick.wrapping_sub(t) < retry => false,
            _ => {
                self.asked.insert(account, tick);
                true
            }
        }
    }

    /// Restaura un traspaso de NPC sin acuse tras un reinicio (se reenvía enseguida).
    pub(super) fn restore_npc_out(&mut self, id: EntityId, to: ShardId, msg: NpcXfer, tick: u32) {
        let sent = tick.wrapping_sub(self.cfg.retry_ticks);
        self.npc_out.insert(id, OutXfer { to, sent, msg });
    }

    /// Restaura el recuerdo de un traspaso de NPC ya adoptado.
    pub(super) fn restore_seen(&mut self, from: ShardId, xfer: u32, tick: u32) {
        self.seen_xfers.insert((from, xfer), tick);
    }

    /// Restaura un traspaso de jugador sin acuse tras un reinicio (se reenvía enseguida).
    pub(super) fn restore_outbox(&mut self, account: u64, r: HoRec, tick: u32) {
        let committed = r.stage == HoStage::Committed || self.dir_peer().is_none();
        self.outbox.insert(
            account,
            HoOut {
                to: r.to,
                xfer: r.xfer,
                epoch: r.epoch,
                pos: r.pos,
                last_processed: r.last_processed,
                committed,
                since: tick,
                last_sent: tick.wrapping_sub(self.cfg.retry_ticks),
            },
        );
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
    Abort(SessionId, ShardId, u32),
}

impl Act {
    fn session(&self) -> SessionId {
        match self {
            Act::Prepare(s) | Act::Redirect(s, _) | Act::Abort(s, _, _) => *s,
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
        if let Some(c) = self.cluster.as_ref() {
            v.extend(c.outbox.keys().copied());
        }
        v.sort_unstable();
        v.dedup();
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

    /// En qué fase está el traspaso de un jugador conectado a este shard: `"preparing"` (se avisó al
    /// destino), `"redirecting"` (se mandó al cliente al destino), `"handed_over"` (la autoridad ya se
    /// entregó) o `"none"`. `None` si la cuenta no está conectada aquí.
    pub fn player_handoff_state(&self, account: u64) -> Option<&'static str> {
        let p = self.player_by_id(account)?;
        Some(match p.handoff {
            Handoff::None => "none",
            Handoff::Preparing { .. } => "preparing",
            Handoff::Redirecting { .. } => "redirecting",
            Handoff::HandedOver { .. } => "handed_over",
        })
    }

    /// Traspasos de NPC enviados y sin acuse, como `(NPC, destino)`. Orden por NPC.
    pub fn npc_transfers_out(&self) -> Vec<(EntityId, ShardId)> {
        let mut v: Vec<_> = self
            .cluster
            .as_ref()
            .map(|c| c.npc_out.iter().map(|(&id, o)| (id, o.to)).collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    }

    /// Jugadores con un traspaso en curso (en cualquiera de sus fases) o esperados aquí.
    pub fn pending_player_handoffs(&self) -> usize {
        self.players
            .values()
            .filter(|p| !matches!(p.handoff, Handoff::None) && !p.frozen())
            .count()
            + self
                .cluster
                .as_ref()
                .map_or(0, |c| c.arrivals.len() + c.outbox.len())
    }

    /// Cuentas que este shard **está simulando ahora mismo** (conectadas y no congeladas). Una cuenta no
    /// puede estar en esta lista de dos shards a la vez, ni siquiera a mitad de un traspaso.
    pub fn simulated_player_ids(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .players
            .values()
            .filter(|p| p.joined && !p.frozen())
            .map(|p| p.entity)
            .collect();
        v.sort_unstable();
        v
    }

    /// ¿Este shard manda sobre ese jugador? (También mientras lo tiene entregado sin acuse.)
    pub fn owns_player(&self, player_id: u64) -> bool {
        self.by_entity.contains_key(&player_id)
            || self
                .cluster
                .as_ref()
                .is_some_and(|c| c.outbox.contains_key(&player_id))
    }

    /// Estado replicado (cuantizado) de cualquier entidad que este shard conozca, propia o copia.
    pub fn entity_state(&self, id: EntityId) -> Option<EntityState> {
        self.ents.get(id).cloned()
    }

    // ------------------------------------------------------------------ bus

    pub(super) fn peer_send(&mut self, to: ShardId, msg: &PeerMsg) {
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
            let from_dir = c.cfg.is_dir_peer(from);
            if from == c.cfg.shard || (!c.cfg.partition.is_valid(from) && !from_dir) {
                c.stats.bus_malformed += 1;
                continue;
            }
            match PeerMsg::decode(&data) {
                Ok(m) => self.on_peer(from, from_dir, m),
                Err(_) => c.stats.bus_malformed += 1,
            }
        }
        if let Some(c) = self.cluster.as_mut() {
            c.inbox = inbox;
        }
    }

    fn on_peer(&mut self, from: ShardId, from_dir: bool, m: PeerMsg) {
        // El directorio solo habla con DirRoute y DirResult; ningún shard puede enviarlos; y DirLogin y
        // DirTransfer (que van al directorio) no tienen sentido en un shard.
        let dir_reply = matches!(
            m,
            PeerMsg::DirRoute { .. } | PeerMsg::DirResult { .. } | PeerMsg::DirRedirect { .. }
        );
        let dir_request = matches!(
            m,
            PeerMsg::DirLogin { .. } | PeerMsg::DirTransfer { .. } | PeerMsg::Raft(_)
        );
        if dir_request || dir_reply != from_dir {
            if let Some(c) = self.cluster.as_mut() {
                c.stats.bus_malformed += 1;
            }
            return;
        }
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
                epoch,
            } => self.on_handover(from, xfer, player, pos, last_processed, epoch),
            PeerMsg::HandoverAck { xfer, player } => self.on_handover_ack(from, xfer, player),
            PeerMsg::Abort { xfer, player } => self.on_abort(from, xfer, player),
            PeerMsg::DirRoute {
                account,
                shard,
                epoch,
            } => self.on_dir_route(account, shard, epoch),
            PeerMsg::DirResult {
                xfer,
                account,
                status,
                shard,
                epoch,
            } => self.on_dir_result(xfer, account, status, shard, epoch),
            // Ya filtrados arriba.
            PeerMsg::DirRedirect { leader } => {
                if let Some(c) = self.cluster.as_mut() {
                    c.on_dir_redirect(leader);
                }
            }
            PeerMsg::DirLogin { .. } | PeerMsg::DirTransfer { .. } | PeerMsg::Raft(_) => {}
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
        let mut forgotten: Vec<(ShardId, u32)> = Vec::new();
        c.seen_xfers.retain(|k, t| {
            let keep = tick.wrapping_sub(*t) <= memory;
            if !keep {
                forgotten.push(*k);
            }
            keep
        });
        c.routes
            .retain(|_, r| tick.wrapping_sub(r.at) <= ROUTE_TTL_TICKS);
        c.asked
            .retain(|_, t| tick.wrapping_sub(*t) <= ROUTE_TTL_TICKS);

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
        if !forgotten.is_empty() {
            if let Some(pe) = self.persist.as_mut() {
                // Lo que ya no se recuerda en memoria tampoco se guarda (no es crítico: si el borrado
                // se pierde en una caída, el recuerdo caduca de nuevo tras el reinicio).
                let mut b = Batch::new();
                for (from, xfer) in forgotten {
                    b.delete(k_xi(from, xfer));
                }
                pe.apply(&b);
            }
        }
        for (to, m) in resend {
            self.peer_send_durable(to, PeerMsg::NpcXfer(m));
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
        self.cluster_outbox_timers();
    }

    /// Reenvíos y plazos de los traspasos de jugadores que este shard está preparando.
    fn cluster_player_timers(&mut self) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_ref() else {
            return;
        };
        let (retry, timeout) = (c.cfg.retry_ticks, c.cfg.handoff_timeout_ticks);
        let mut acts: Vec<Act> = Vec::new();
        for (&s, p) in self.players.iter_mut() {
            match &mut p.handoff {
                Handoff::None | Handoff::HandedOver { .. } => {}
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
                            self.peer_send_durable(to, m);
                        }
                    }
                }
                Act::Redirect(s, to) => {
                    if let Some(c) = self.cluster.as_mut() {
                        c.stats.redirect_retries += 1;
                    }
                    self.send_redirect(s, to);
                }
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
        if self.cluster.is_none() {
            return;
        }
        let xfer = self.alloc_xfer();
        let npc = &self.npcs[idx];
        let msg = NpcXfer {
            xfer,
            id,
            pos: [pos.x, pos.y, pos.z],
            dir: npc.dir,
            turn_in: npc.turn_in,
            rng: npc.rng.state(),
        };
        // A disco antes que a ningún vecino: el NPC deja de ser «mío» y pasa a ser «en traspaso hacia
        // `to`» en un único lote, así que tras cualquier caída consta bajo una sola de las dos claves.
        if let Some(pe) = self.persist.as_mut() {
            pe.clear_npc(idx);
            let mut b = Batch::new();
            b.delete(k_npc(idx));
            b.put(
                k_xo(idx),
                XoRec {
                    to,
                    msg: msg.clone(),
                }
                .encode(),
            );
            pe.apply_critical(&b);
        }
        if let Some(c) = self.cluster.as_mut() {
            c.npc_out.insert(
                id,
                OutXfer {
                    to,
                    sent: tick,
                    msg: msg.clone(),
                },
            );
            c.stats.npc_xfers_out += 1;
        }
        self.ents.set_npc_own(idx, Own::Transferring);
        self.peer_send_durable(to, PeerMsg::NpcXfer(msg));
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
            let first_time = c.seen_xfers.insert((from, x.xfer), tick).is_none();
            if first_time {
                if let Some(pe) = self.persist.as_mut() {
                    let mut b = Batch::new();
                    b.put(k_xi(from, x.xfer), vec![1]);
                    pe.apply(&b);
                }
            }
            self.peer_send_durable(from, ack);
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
        // A disco antes de acusar: si el acuse llega al origen, este shard ya es dueño de verdad.
        if let Some(pe) = self.persist.as_mut() {
            let mut b = Batch::new();
            b.put(k_npc(idx), NpcRec::from_xfer(&x).encode());
            b.put(k_xi(from, x.xfer), vec![1]);
            pe.apply_critical(&b);
        }
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
        self.peer_send_durable(from, ack);
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
        let idx = (id - NPC_BASE) as usize;
        self.ents.set_npc_own(idx, Own::Ghost);
        // Ya no hace falta recordar el traspaso. Se sincroniza enseguida: si el borrado se perdiera en una
        // caída, el NPC se reenviaría tras el reinicio, y el destino solo lo reconoce como repetido
        // mientras recuerde el traspaso (`xfer_memory_ticks`).
        if let Some(pe) = self.persist.as_mut() {
            let mut b = Batch::new();
            b.delete(k_xo(idx));
            pe.apply_critical(&b);
        }
    }

    // ------------------------------------------------------------------ jugadores: origen

    /// ¿Hay directorio de sesiones y almacén para usarlo?
    fn dir_active(&self) -> bool {
        self.persist.is_some()
            && self
                .cluster
                .as_ref()
                .is_some_and(|c| c.dir_peer().is_some())
    }

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
            let xfer = self.alloc_xfer();
            if let Some(c) = self.cluster.as_mut() {
                c.stats.handoffs_started += 1;
            }
            if let Some(p) = self.players.get_mut(&s) {
                p.handoff = Handoff::Preparing {
                    to,
                    xfer,
                    since: tick,
                    last_sent: tick,
                };
            }
            if let Some(m) = self.prepare_msg(s) {
                self.peer_send_durable(to, m);
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

    pub(super) fn send_redirect(&mut self, session: SessionId, to: ShardId) {
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

    /// El destino dice que el cliente ya está allí y pide la autoridad. Es el instante en que el jugador
    /// deja de ser de este shard: se congela, su estado final pasa al buzón de salida (y a disco) y se
    /// pide al directorio, si lo hay, que anote el cambio de dueño.
    fn on_activate(&mut self, from: ShardId, xfer: u32, player: u64) {
        let tick = self.tick;
        // ¿Ya está entregado? Entonces es un ACTIVATE repetido: se reenvía lo que toque.
        let known = self
            .cluster
            .as_ref()
            .and_then(|c| c.outbox.get(&player).map(|h| (h.to, h.xfer)));
        if let Some((to, x)) = known {
            if to == from && x == xfer {
                self.send_outbox_msg(player);
            }
            return;
        }
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
            Handoff::HandedOver { .. } | Handoff::None => return,
        };
        if to != from || x != xfer {
            return; // de un intento anterior
        }
        // Aquí cambia la autoridad: desde este instante el jugador queda congelado en este shard.
        p.queue.clear();
        p.handoff = Handoff::HandedOver { to, xfer };
        let (account, pos, last_processed, epoch) =
            (p.player_id, p.state.pos, p.last_processed, p.epoch + 1);
        let dir_active = self.dir_active();
        if let Some(c) = self.cluster.as_mut() {
            c.outbox.insert(
                account,
                HoOut {
                    to,
                    xfer,
                    epoch,
                    pos,
                    last_processed,
                    committed: !dir_active,
                    since: tick,
                    last_sent: tick,
                },
            );
        }
        // A disco antes de decir nada a nadie: la cuenta deja de ser residente y pasa a estar «en
        // traspaso» en un solo lote.
        if let Some(pe) = self.persist.as_mut() {
            let mut b = Batch::new();
            b.delete(k_player(account));
            b.put(
                k_ho(account),
                HoRec {
                    to,
                    xfer,
                    epoch,
                    pos,
                    last_processed,
                    stage: if dir_active {
                        HoStage::Committing
                    } else {
                        HoStage::Committed
                    },
                }
                .encode(),
            );
            pe.saved_pos.remove(&account);
            pe.apply_critical(&b);
        }
        self.send_outbox_msg(account);
    }

    /// Envía lo que toca de un traspaso entregado: la petición al directorio mientras no esté anotado,
    /// y el estado de la cuenta al destino cuando ya lo está.
    fn send_outbox_msg(&mut self, account: u64) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let dir = c.dir_peer();
        let Some(ho) = c.outbox.get_mut(&account) else {
            return;
        };
        ho.last_sent = tick;
        let (to, xfer, epoch, pos, last_processed, committed) = (
            ho.to,
            ho.xfer,
            ho.epoch,
            ho.pos,
            ho.last_processed,
            ho.committed,
        );
        if committed {
            self.peer_send_durable(
                to,
                PeerMsg::Handover {
                    xfer,
                    player: account,
                    pos,
                    last_processed,
                    epoch,
                },
            );
        } else if dir.is_some() {
            c.note_dir_send();
            let dir = c.dir_peer().unwrap_or(0);
            c.stats.dir_transfers_out += 1;
            self.peer_send_durable(
                dir,
                PeerMsg::DirTransfer {
                    xfer,
                    account,
                    epoch: epoch - 1,
                    to,
                },
            );
        }
    }

    /// Reenvíos de los traspasos de jugador entregados. Con persistencia no se rinden nunca (tras el
    /// plazo reintentan despacio): la cuenta ya no es de este shard y su estado solo vive aquí hasta que
    /// el destino lo acuse. Sin persistencia, pasado el plazo se abandonan (como en la 0.4).
    fn cluster_outbox_timers(&mut self) {
        let tick = self.tick;
        let persistent = self.persist.is_some();
        let slow = self.persist.as_ref().map(|p| p.cfg.slow_retry_ticks);
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let (retry, timeout) = (c.cfg.retry_ticks, c.cfg.handoff_timeout_ticks);
        let slow = slow.unwrap_or(retry).max(retry);
        let mut due: Vec<u64> = Vec::new();
        let mut give_up: Vec<u64> = Vec::new();
        for (&a, ho) in c.outbox.iter() {
            let age = tick.wrapping_sub(ho.since);
            if !persistent && age > timeout {
                give_up.push(a);
                continue;
            }
            let every = if persistent && age > timeout {
                slow
            } else {
                retry
            };
            if tick.wrapping_sub(ho.last_sent) >= every {
                due.push(a);
            }
        }
        due.sort_unstable();
        give_up.sort_unstable();
        c.stats.handover_retries += due.len() as u64;
        for a in give_up {
            self.finish_outbox(a, false);
        }
        for a in due {
            self.send_outbox_msg(a);
            // Si el cliente sigue conectado aquí (por ejemplo su conexión al destino se perdió), se le
            // vuelve a indicar adónde ir.
            let to = self
                .cluster
                .as_ref()
                .and_then(|c| c.outbox.get(&a).map(|h| h.to));
            if let (Some(to), Some(&s)) = (to, self.by_entity.get(&a)) {
                self.send_redirect(s, to);
            }
        }
    }

    fn on_handover_ack(&mut self, from: ShardId, xfer: u32, player: u64) {
        let matches = self.cluster.as_ref().is_some_and(|c| {
            c.outbox
                .get(&player)
                .is_some_and(|h| h.to == from && h.xfer == xfer && h.committed)
        });
        if matches {
            self.finish_outbox(player, true);
        }
    }

    /// El traspaso terminó (el destino lo acusó, o se abandonó): la cuenta ya no es de este shard.
    fn finish_outbox(&mut self, account: u64, acked: bool) {
        let tick = self.tick;
        let persistent = self.persist.is_some();
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let Some(ho) = c.outbox.remove(&account) else {
            return;
        };
        if acked {
            let ticks = tick.wrapping_sub(ho.since);
            c.stats.handover_acked += 1;
            c.stats.handover_ticks_sum += u64::from(ticks);
            c.stats.handover_ticks_max = c.stats.handover_ticks_max.max(ticks);
        } else {
            c.stats.handovers_unacked += 1;
        }
        c.stats.handoffs_completed += 1;
        c.ghosts.insert(
            account,
            Ghost {
                seen: tick,
                sent_tick: 0,
                from: ho.to,
            },
        );
        // Queda una lápida con la época: un `HANDOVER` repetido que llegara tarde no resucita nada.
        if let Some(pe) = self.persist.as_mut() {
            let mut b = Batch::new();
            b.delete(k_ho(account));
            b.put(
                k_tomb(account),
                TombRec {
                    epoch: ho.epoch,
                    to: ho.to,
                }
                .encode(),
            );
            pe.apply(&b);
        }
        // Si el cliente sigue conectado aquí con el jugador congelado, esa entrada sobra.
        if let Some(s) = self.by_entity.get(&account).copied() {
            if self.players.get(&s).is_some_and(|p| p.frozen()) {
                self.players.remove(&s);
                self.by_entity.remove(&account);
                if persistent {
                    self.net.kick(s);
                }
            }
        }
    }

    /// El cliente del jugador entregado se desconectó de este shard (lo normal tras pasar al nuevo). El
    /// traspaso sigue adelante sin él: su estado está en el buzón de salida.
    pub(super) fn cluster_handed_over_gone(&mut self, p: &super::Player, to: ShardId) {
        let tick = self.tick;
        if let Some(c) = self.cluster.as_mut() {
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

    /// Respuesta del directorio a una petición de cambio de dueño.
    fn on_dir_result(
        &mut self,
        xfer: u32,
        account: u64,
        status: DirStatus,
        shard: ShardId,
        epoch: u64,
    ) {
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        c.stats.dir_results_in += 1;
        c.note_dir_reply();
        let Some(ho) = c.outbox.get_mut(&account) else {
            return; // de un intento anterior, o ya resuelto
        };
        if ho.xfer != xfer || ho.committed {
            return;
        }
        match status {
            DirStatus::Committed => {
                ho.committed = true;
                let rec = HoRec {
                    to: ho.to,
                    xfer,
                    epoch: ho.epoch,
                    pos: ho.pos,
                    last_processed: ho.last_processed,
                    stage: HoStage::Committed,
                };
                // El cambio de dueño está decidido y ya consta en el directorio: se anota aquí también
                // antes de enviar el estado al destino.
                if let Some(pe) = self.persist.as_mut() {
                    let mut b = Batch::new();
                    b.put(k_ho(account), rec.encode());
                    pe.apply_critical(&b);
                }
                self.send_outbox_msg(account);
            }
            DirStatus::Rejected | DirStatus::Unknown => {
                self.abort_outbox(account, status, shard, epoch)
            }
        }
    }

    /// El directorio no acepta el cambio: la cuenta no estaba donde creíamos. No debería ocurrir salvo
    /// que alguien haya tocado el directorio o haya dos copias de la cuenta; se deja como está lo que el
    /// directorio dice y se cuenta.
    fn abort_outbox(
        &mut self,
        account: u64,
        status: DirStatus,
        dir_shard: ShardId,
        dir_epoch: u64,
    ) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        let me = c.cfg.shard;
        let cooldown = c.cfg.handoff_cooldown_ticks;
        let Some(ho) = c.outbox.remove(&account) else {
            return;
        };
        match status {
            DirStatus::Unknown => c.stats.dir_unknown += 1,
            _ => c.stats.dir_rejected += 1,
        }
        c.stats.handoffs_aborted += 1;
        // Que el destino olvide la llegada.
        self.peer_send(
            ho.to,
            &PeerMsg::Abort {
                xfer: ho.xfer,
                player: account,
            },
        );
        // Si el directorio no conoce la cuenta, o dice que vive aquí, la cuenta sigue siendo de este
        // shard (con la época del directorio si la dio); si dice que vive en otro, deja de serlo.
        let keep = status == DirStatus::Unknown || dir_shard == me;
        let epoch = if status == DirStatus::Unknown {
            ho.epoch - 1
        } else {
            dir_epoch
        };
        if let Some(pe) = self.persist.as_mut() {
            let mut b = Batch::new();
            b.delete(k_ho(account));
            if keep {
                b.put(k_player(account), PlayerRec { pos: ho.pos, epoch }.encode());
            } else {
                b.put(
                    k_tomb(account),
                    TombRec {
                        epoch: dir_epoch,
                        to: dir_shard,
                    }
                    .encode(),
                );
            }
            pe.apply_critical(&b);
        }
        let session = self.by_entity.get(&account).copied();
        match (keep, session) {
            (true, Some(s)) => {
                // El jugador vuelve a la vida donde estaba.
                if let Some(p) = self.players.get_mut(&s) {
                    p.handoff = Handoff::None;
                    p.epoch = epoch;
                    p.cooldown_until = tick.wrapping_add(cooldown);
                }
                if let Some(pe) = self.persist.as_mut() {
                    pe.saved_pos.insert(account, ho.pos);
                }
            }
            (false, Some(s)) => self.kick(s),
            _ => {}
        }
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
        epoch: u64,
    ) {
        if self.persist.is_some() {
            self.on_handover_durable(from, xfer, player, pos, last_processed, epoch);
        } else {
            self.on_handover_volatile(from, xfer, player, pos, last_processed, epoch);
        }
    }

    /// Sin almacén (como en la 0.4): hace falta que el cliente ya esté aquí para adoptar al jugador.
    fn on_handover_volatile(
        &mut self,
        from: ShardId,
        xfer: u32,
        player: u64,
        pos: [i32; 3],
        last_processed: u32,
        epoch: u64,
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
        let owed = tick.wrapping_sub(a.first_activate.unwrap_or(tick));
        c.arrivals.remove(&player);
        c.ghosts.remove(&player);
        c.stats.arrivals_completed += 1;
        if !self.activate_arrival(session, pos, last_processed, epoch, owed) {
            return; // su sesión ya no existe: el jugador se pierde (el origen lo da por entregado)
        }
        self.peer_send(from, &PeerMsg::HandoverAck { xfer, player });
    }

    /// Con almacén: el estado de la cuenta se anota **antes** de acusar recibo, y no hace falta que el
    /// cliente esté aquí (si no está, la cuenta queda residente y entra cuando se conecte).
    fn on_handover_durable(
        &mut self,
        from: ShardId,
        xfer: u32,
        player: u64,
        pos: [i32; 3],
        last_processed: u32,
        epoch: u64,
    ) {
        let tick = self.tick;
        if epoch <= self.known_epoch(player) {
            // Una repetición (ya adoptado) o de una época superada: se acusa para que el origen pare.
            if let Some(c) = self.cluster.as_mut() {
                c.stats.handovers_stale += 1;
            }
            self.peer_send_durable(from, PeerMsg::HandoverAck { xfer, player });
            return;
        }
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        if c.outbox.contains_key(&player) {
            // Esta cuenta salió de aquí y su traspaso aún no tiene acuse: no se puede volver a adoptar
            // hasta saber cómo terminó (el origen reenvía; mientras tanto el destino acusará el nuestro,
            // porque ya conoce una época igual o mayor).
            c.stats.handovers_deferred += 1;
            return;
        }
        let arrival = c
            .arrivals
            .get(&player)
            .filter(|a| a.from == from && a.xfer == xfer)
            .map(|a| (a.session, a.first_activate));
        if arrival.is_none() && self.by_entity.contains_key(&player) {
            // La cuenta está conectada aquí con otra época: dos copias. No se adopta.
            c.stats.handover_conflicts += 1;
            return;
        }
        if let Some(pe) = self.persist.as_mut() {
            let mut b = Batch::new();
            b.put(k_player(player), PlayerRec { pos, epoch }.encode());
            b.delete(k_tomb(player));
            pe.apply_critical(&b);
        }
        let session = arrival
            .and_then(|(s, first)| s.map(|s| (s, first)))
            .filter(|(s, _)| self.players.contains_key(s));
        match session {
            Some((session, first)) => {
                let owed = tick.wrapping_sub(first.unwrap_or(tick));
                if let Some(c) = self.cluster.as_mut() {
                    c.arrivals.remove(&player);
                    c.ghosts.remove(&player);
                    c.stats.arrivals_completed += 1;
                }
                self.activate_arrival(session, pos, last_processed, epoch, owed);
                if let Some(pe) = self.persist.as_mut() {
                    pe.saved_pos.insert(player, pos);
                }
            }
            None => {
                // El cliente no está aquí (se cayó, o este shard acaba de reiniciarse): la cuenta queda
                // residente y entrará con este estado cuando se conecte.
                if let Some(c) = self.cluster.as_mut() {
                    if arrival.is_some() {
                        c.arrivals.remove(&player);
                    }
                    c.stats.offline_adoptions += 1;
                }
            }
        }
        self.peer_send_durable(from, PeerMsg::HandoverAck { xfer, player });
    }

    /// Pone al jugador de una sesión ya conectada en el estado que trae el traspaso y le da la bienvenida.
    /// `false` si su sesión ya no existe.
    fn activate_arrival(
        &mut self,
        session: SessionId,
        pos: [i32; 3],
        last_processed: u32,
        epoch: u64,
        owed: u32,
    ) -> bool {
        let tick = self.tick;
        let cooldown = self
            .cluster
            .as_ref()
            .map_or(0, |c| c.cfg.handoff_cooldown_ticks);
        let Some(p) = self.players.get_mut(&session) else {
            return false;
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
        p.epoch = epoch;
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
        true
    }

    /// El directorio dice dónde vive una cuenta. Se guarda hasta que su cliente (que vuelve a pedir
    /// entrar cada pocos cientos de milisegundos) la use, y se atiende ya a quien estuviera esperando.
    fn on_dir_route(&mut self, account: u64, shard: ShardId, epoch: u64) {
        let tick = self.tick;
        let Some(c) = self.cluster.as_mut() else {
            return;
        };
        c.stats.dir_routes_in += 1;
        c.note_dir_reply();
        if !c.cfg.partition.is_valid(shard) {
            c.stats.bus_malformed += 1;
            return;
        }
        c.routes.insert(
            account,
            Route {
                shard,
                epoch,
                at: tick,
            },
        );
        let mut waiting: Vec<SessionId> = self
            .players
            .iter()
            .filter(|(_, p)| p.player_id == account && !p.joined && p.join_requested)
            .map(|(&s, _)| s)
            .collect();
        waiting.sort_unstable();
        for s in waiting {
            self.on_join(s);
        }
    }
}
