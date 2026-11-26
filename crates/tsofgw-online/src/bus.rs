//! Bus entre shards: los mensajes que se cruzan los servidores de un mismo mundo, su codificación
//! binaria **estricta** y un bus simulado (latencia, jitter, pérdida y duplicados) para probar fallos.
//!
//! El bus es una abstracción ([`PeerBus`]): hoy hay una implementación en memoria para pruebas y para
//! ejecutar un cluster entero en un proceso; la de red (UDP autenticado con una clave de cluster) se
//! enchufa por el mismo trait sin tocar la lógica del servidor.
//!
//! ```text
//! GHOSTS    1 | tick | jugadores | NPC          copia de las entidades cercanas al borde (sin acuse)
//! NPC_XFER  2 | xfer id pos dir turn_in rng      un NPC cambia de dueño (se reenvía hasta el acuse)
//! XFER_ACK  3 | xfer id
//! PREPARE   4 | xfer jugador pos último_input    «voy a pasarte a este jugador»
//! PREP_ACK  5 | xfer jugador
//! ACTIVATE  6 | xfer jugador                     «su cliente ya está conectado aquí: cédeme la autoridad»
//! HANDOVER  7 | xfer jugador pos último_input época   estado final: desde aquí manda el destino
//! HAND_ACK  8 | xfer jugador
//! ABORT     9 | xfer jugador                     el traspaso se cancela
//! DIR_LOGIN 10| cuenta casa                      shard → directorio: «esta cuenta quiere entrar; ¿dónde vive?»
//! DIR_ROUTE 11| cuenta shard época               directorio → shard: dónde vive la cuenta
//! DIR_XFER  12| xfer cuenta época destino        shard → directorio: «la cuenta pasa de mí a `destino`»
//! DIR_RESULT 13| xfer cuenta estado shard época  directorio → shard: aceptado, rechazado o desconocida
//! ```
//! Los mensajes 10 a 13 solo viajan entre un shard y el **directorio de sesiones** (un par más del bus,
//! con identificador `shard_count`); el directorio nunca recibe ni envía el resto.
//! Todos los enteros van como varint (los con signo, en zigzag). El decodificador rechaza versiones
//! desconocidas, mensajes truncados o con bytes sobrantes, ids repetidos o desordenados, flotantes no
//! finitos y listas imposibles, sin reservar memoria en función de lo que diga el mensaje.

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::sync::{Arc, Mutex};

use tsofgw_net::{varint_len, zigzag_len, DecodeError, EntityState, Reader, Writer};
use tsofgw_world::{EntityId, Rng};

use crate::partition::ShardId;
use crate::server::NPC_BASE;

pub const PEER_VERSION: u8 = 2;
/// Tope de entidades por lista en un mensaje de fantasmas.
pub const MAX_GHOSTS_PER_LIST: usize = 50_000;
/// Tamaño máximo de un mensaje del bus. Cabe en un único datagrama UDP de 1200 bytes con la cabecera y
/// la etiqueta del bus real, así que no hay fragmentación ni reensamblado entre shards:
/// perder un datagrama pierde solo lo que llevaba. Los dos buses (real y simulado) lo hacen cumplir.
pub const MAX_PEER_MSG: usize = 1150;

/// Transporte de mensajes entre shards. No garantiza entrega ni orden: la lógica que lo usa
/// reenvía lo que importa y tolera duplicados.
pub trait PeerBus {
    /// Envía un mensaje de, como máximo, [`MAX_PEER_MSG`] bytes (uno mayor se descarta y se cuenta).
    fn send(&mut self, now_ms: u64, to: ShardId, data: Vec<u8>);
    /// Añade a `out` los mensajes llegados hasta `now_ms`, con el shard que los envió.
    fn recv(&mut self, now_ms: u64, out: &mut Vec<(ShardId, Vec<u8>)>);
    /// Datagramas descartados antes de llegar a la lógica (autenticación, repetición, formato...).
    fn rejected(&self) -> u64 {
        0
    }
    /// Resumen de la configuración de mundo de este shard (véase `ClusterConfig::digest`). Un bus de
    /// red lo adjunta a todo lo que envía y descarta lo que traiga otro: dos shards con mundos
    /// distintos no deben hablarse.
    fn set_config_digest(&mut self, _digest: u64) {}
    /// Cuántos datagramas auténticos se descartaron por traer otra configuración de mundo.
    fn config_mismatches(&self) -> u64 {
        0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NpcXfer {
    pub xfer: u32,
    pub id: EntityId,
    pub pos: [f32; 3],
    pub dir: [f32; 2],
    pub turn_in: u32,
    pub rng: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PeerMsg {
    Ghosts {
        tick: u32,
        /// Ordenados por id, sin repetidos.
        players: Vec<(EntityId, EntityState)>,
        npcs: Vec<(EntityId, EntityState)>,
    },
    NpcXfer(NpcXfer),
    NpcXferAck {
        xfer: u32,
        id: EntityId,
    },
    Prepare {
        xfer: u32,
        player: u64,
        pos: [i32; 3],
        last_processed: u32,
    },
    PrepareAck {
        xfer: u32,
        player: u64,
    },
    Activate {
        xfer: u32,
        player: u64,
    },
    Handover {
        xfer: u32,
        player: u64,
        pos: [i32; 3],
        last_processed: u32,
        /// Época que tendrá la cuenta en el destino (la del origen más uno). Una repetición con una
        /// época que el destino ya conoce no se adopta otra vez.
        epoch: u64,
    },
    HandoverAck {
        xfer: u32,
        player: u64,
    },
    Abort {
        xfer: u32,
        player: u64,
    },
    /// Shard → directorio: la cuenta quiere entrar en este shard. Si el directorio no la conoce, la
    /// registra en `home`.
    DirLogin {
        account: u64,
        home: ShardId,
    },
    /// Directorio → shard: la cuenta vive en `shard` desde la época `epoch`.
    DirRoute {
        account: u64,
        shard: ShardId,
        epoch: u64,
    },
    /// Shard → directorio: «la cuenta, que según yo está aquí con la época `epoch`, pasa a `to`».
    DirTransfer {
        xfer: u32,
        account: u64,
        epoch: u64,
        to: ShardId,
    },
    /// Directorio → shard: respuesta a un `DirTransfer`. `shard` y `epoch` son lo que el directorio
    /// tiene ahora para la cuenta (cero si no la conoce).
    DirResult {
        xfer: u32,
        account: u64,
        status: DirStatus,
        shard: ShardId,
        epoch: u64,
    },
    /// Réplica del directorio → shard: «yo no soy el líder; prueba con `leader`» (0.8.0, directorio replicado).
    DirRedirect {
        leader: ShardId,
    },
    /// Entre réplicas del directorio: un mensaje de Raft (véase `raft::Msg`), opaco para el bus.
    Raft(Vec<u8>),
}

/// Resultado de un `DirTransfer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirStatus {
    /// El directorio anotó el cambio (o ya lo había anotado: es idempotente).
    Committed = 1,
    /// El directorio tiene otro dueño u otra época para esa cuenta: el cambio no vale.
    Rejected = 2,
    /// El directorio no conoce la cuenta.
    Unknown = 3,
}

fn put_list(w: &mut Writer, list: &[(EntityId, EntityState)]) {
    debug_assert!(
        list.windows(2).all(|p| p[0].0 < p[1].0),
        "lista sin ordenar"
    );
    w.varint(list.len() as u64);
    let mut prev = 0u64;
    for (id, st) in list {
        w.varint(id - prev);
        prev = *id;
        for c in st.pos {
            w.zigzag(c as i64);
        }
        w.varint(st.data as u64);
    }
}

fn get_list(r: &mut Reader<'_>, npcs: bool) -> Result<Vec<(EntityId, EntityState)>, DecodeError> {
    let n = r.varint()? as usize;
    if n > MAX_GHOSTS_PER_LIST {
        return Err(DecodeError::BadReference);
    }
    let mut out: Vec<(EntityId, EntityState)> = Vec::new();
    let mut prev = 0u64;
    for i in 0..n {
        let delta = r.varint()?;
        let id = prev.checked_add(delta).ok_or(DecodeError::BadReference)?;
        if i > 0 && delta == 0 {
            return Err(DecodeError::BadReference); // repetido o desordenado
        }
        if (id >= NPC_BASE) != npcs {
            return Err(DecodeError::BadReference); // un NPC en la lista de jugadores o al revés
        }
        prev = id;
        let mut pos = [0i32; 3];
        for c in &mut pos {
            *c = i32::try_from(r.zigzag()?).map_err(|_| DecodeError::VarintOverflow)?;
        }
        let data = u16::try_from(r.varint()?).map_err(|_| DecodeError::VarintOverflow)?;
        out.push((id, EntityState { pos, data }));
    }
    Ok(out)
}

/// Bytes que ocupa una entrada de una lista de entidades cuyo id anterior es `prev`.
fn entry_len(prev: u64, id: u64, st: &EntityState) -> usize {
    varint_len(id - prev)
        + st.pos.iter().map(|&c| zigzag_len(c as i64)).sum::<usize>()
        + varint_len(st.data as u64)
}

/// Reparte los fantasmas de un tick en mensajes de, como máximo, `max_len` bytes codificados. Los
/// mensajes llevan el mismo `tick` (el receptor solo descarta lo anterior) y conservan el orden por id.
pub fn ghost_chunks(
    tick: u32,
    players: Vec<(EntityId, EntityState)>,
    npcs: Vec<(EntityId, EntityState)>,
    max_len: usize,
) -> Vec<PeerMsg> {
    // Versión, tipo, tick y las dos cuentas (2 bytes cada una: un datagrama no admite 16 384 entradas).
    let budget = max_len.saturating_sub(2 + varint_len(tick as u64) + 4);
    let mut out = Vec::new();
    let (mut cur_p, mut cur_n): (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
    let mut used = 0usize;
    let flush = |cur_p: &mut Vec<_>, cur_n: &mut Vec<_>, out: &mut Vec<PeerMsg>| {
        out.push(PeerMsg::Ghosts {
            tick,
            players: std::mem::take(cur_p),
            npcs: std::mem::take(cur_n),
        });
    };
    for (is_npc, list) in [(false, players), (true, npcs)] {
        for (id, st) in list {
            let cur = if is_npc { &cur_n } else { &cur_p };
            let mut len = entry_len(
                cur.last().map_or(0, |e: &(EntityId, EntityState)| e.0),
                id,
                &st,
            );
            if used + len > budget && !(cur_p.is_empty() && cur_n.is_empty()) {
                flush(&mut cur_p, &mut cur_n, &mut out);
                used = 0;
                len = entry_len(0, id, &st);
            }
            used += len;
            if is_npc {
                cur_n.push((id, st));
            } else {
                cur_p.push((id, st));
            }
        }
    }
    if !(cur_p.is_empty() && cur_n.is_empty()) {
        flush(&mut cur_p, &mut cur_n, &mut out);
    }
    out
}

fn put_f32(w: &mut Writer, v: f32) {
    w.varint(v.to_bits() as u64);
}

fn get_f32(r: &mut Reader<'_>) -> Result<f32, DecodeError> {
    let bits = u32::try_from(r.varint()?).map_err(|_| DecodeError::VarintOverflow)?;
    let v = f32::from_bits(bits);
    if v.is_finite() {
        Ok(v)
    } else {
        Err(DecodeError::BadReference)
    }
}

fn get_u32(r: &mut Reader<'_>) -> Result<u32, DecodeError> {
    u32::try_from(r.varint()?).map_err(|_| DecodeError::VarintOverflow)
}

/// Una cuenta de jugador: nunca puede ocupar el rango de los NPC.
fn get_account(r: &mut Reader<'_>) -> Result<u64, DecodeError> {
    let a = r.varint()?;
    if a >= NPC_BASE {
        return Err(DecodeError::BadReference);
    }
    Ok(a)
}

/// Una época de cuenta: empiezan en 1 (0 significa «ninguna»).
fn get_epoch(r: &mut Reader<'_>) -> Result<u64, DecodeError> {
    match r.varint()? {
        0 => Err(DecodeError::BadReference),
        e => Ok(e),
    }
}

fn get_shard(r: &mut Reader<'_>) -> Result<ShardId, DecodeError> {
    ShardId::try_from(r.varint()?).map_err(|_| DecodeError::VarintOverflow)
}

fn get_pos(r: &mut Reader<'_>) -> Result<[i32; 3], DecodeError> {
    let mut pos = [0i32; 3];
    for c in &mut pos {
        *c = i32::try_from(r.zigzag()?).map_err(|_| DecodeError::VarintOverflow)?;
    }
    Ok(pos)
}

impl PeerMsg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.u8(PEER_VERSION);
        match self {
            PeerMsg::Ghosts {
                tick,
                players,
                npcs,
            } => {
                w.u8(1);
                w.varint(*tick as u64);
                put_list(&mut w, players);
                put_list(&mut w, npcs);
            }
            PeerMsg::NpcXfer(x) => {
                w.u8(2);
                w.varint(x.xfer as u64);
                w.varint(x.id);
                for c in x.pos {
                    put_f32(&mut w, c);
                }
                for c in x.dir {
                    put_f32(&mut w, c);
                }
                w.varint(x.turn_in as u64);
                w.varint(x.rng);
            }
            PeerMsg::NpcXferAck { xfer, id } => {
                w.u8(3);
                w.varint(*xfer as u64);
                w.varint(*id);
            }
            PeerMsg::Prepare {
                xfer,
                player,
                pos,
                last_processed,
            } => {
                w.u8(4);
                w.varint(*xfer as u64);
                w.varint(*player);
                for c in pos {
                    w.zigzag(*c as i64);
                }
                w.varint(*last_processed as u64);
            }
            PeerMsg::Handover {
                xfer,
                player,
                pos,
                last_processed,
                epoch,
            } => {
                w.u8(7);
                w.varint(*xfer as u64);
                w.varint(*player);
                for c in pos {
                    w.zigzag(*c as i64);
                }
                w.varint(*last_processed as u64);
                w.varint(*epoch);
            }
            PeerMsg::DirLogin { account, home } => {
                w.u8(10);
                w.varint(*account);
                w.varint(*home as u64);
            }
            PeerMsg::DirRoute {
                account,
                shard,
                epoch,
            } => {
                w.u8(11);
                w.varint(*account);
                w.varint(*shard as u64);
                w.varint(*epoch);
            }
            PeerMsg::DirTransfer {
                xfer,
                account,
                epoch,
                to,
            } => {
                w.u8(12);
                w.varint(*xfer as u64);
                w.varint(*account);
                w.varint(*epoch);
                w.varint(*to as u64);
            }
            PeerMsg::DirResult {
                xfer,
                account,
                status,
                shard,
                epoch,
            } => {
                w.u8(13);
                w.varint(*xfer as u64);
                w.varint(*account);
                w.u8(*status as u8);
                w.varint(*shard as u64);
                w.varint(*epoch);
            }
            PeerMsg::DirRedirect { leader } => {
                w.u8(14);
                w.varint(*leader as u64);
            }
            PeerMsg::Raft(b) => {
                w.u8(15);
                w.varint(b.len() as u64);
                for x in b {
                    w.u8(*x);
                }
            }
            PeerMsg::PrepareAck { xfer, player }
            | PeerMsg::Activate { xfer, player }
            | PeerMsg::HandoverAck { xfer, player }
            | PeerMsg::Abort { xfer, player } => {
                w.u8(match self {
                    PeerMsg::PrepareAck { .. } => 5,
                    PeerMsg::Activate { .. } => 6,
                    PeerMsg::HandoverAck { .. } => 8,
                    _ => 9,
                });
                w.varint(*xfer as u64);
                w.varint(*player);
            }
        }
        w.buf
    }

    pub fn decode(bytes: &[u8]) -> Result<PeerMsg, DecodeError> {
        let mut r = Reader::new(bytes);
        if r.u8()? != PEER_VERSION {
            return Err(DecodeError::BadReference);
        }
        let msg = match r.u8()? {
            1 => PeerMsg::Ghosts {
                tick: get_u32(&mut r)?,
                players: get_list(&mut r, false)?,
                npcs: get_list(&mut r, true)?,
            },
            2 => {
                let xfer = get_u32(&mut r)?;
                let id = r.varint()?;
                if id < NPC_BASE {
                    return Err(DecodeError::BadReference);
                }
                let pos = [get_f32(&mut r)?, get_f32(&mut r)?, get_f32(&mut r)?];
                let dir = [get_f32(&mut r)?, get_f32(&mut r)?];
                let turn_in = get_u32(&mut r)?;
                let rng = r.varint()?;
                if rng == 0 {
                    return Err(DecodeError::BadReference); // el generador nunca vale 0
                }
                PeerMsg::NpcXfer(NpcXfer {
                    xfer,
                    id,
                    pos,
                    dir,
                    turn_in,
                    rng,
                })
            }
            3 => PeerMsg::NpcXferAck {
                xfer: get_u32(&mut r)?,
                id: r.varint()?,
            },
            4 => PeerMsg::Prepare {
                xfer: get_u32(&mut r)?,
                player: r.varint()?,
                pos: get_pos(&mut r)?,
                last_processed: get_u32(&mut r)?,
            },
            5 => PeerMsg::PrepareAck {
                xfer: get_u32(&mut r)?,
                player: r.varint()?,
            },
            6 => PeerMsg::Activate {
                xfer: get_u32(&mut r)?,
                player: r.varint()?,
            },
            7 => {
                let xfer = get_u32(&mut r)?;
                let player = get_account(&mut r)?;
                let pos = get_pos(&mut r)?;
                let last_processed = get_u32(&mut r)?;
                let epoch = get_epoch(&mut r)?;
                PeerMsg::Handover {
                    xfer,
                    player,
                    pos,
                    last_processed,
                    epoch,
                }
            }
            8 => PeerMsg::HandoverAck {
                xfer: get_u32(&mut r)?,
                player: r.varint()?,
            },
            9 => PeerMsg::Abort {
                xfer: get_u32(&mut r)?,
                player: r.varint()?,
            },
            10 => PeerMsg::DirLogin {
                account: get_account(&mut r)?,
                home: get_shard(&mut r)?,
            },
            11 => PeerMsg::DirRoute {
                account: get_account(&mut r)?,
                shard: get_shard(&mut r)?,
                epoch: get_epoch(&mut r)?,
            },
            12 => PeerMsg::DirTransfer {
                xfer: get_u32(&mut r)?,
                account: get_account(&mut r)?,
                epoch: get_epoch(&mut r)?,
                to: get_shard(&mut r)?,
            },
            13 => {
                let xfer = get_u32(&mut r)?;
                let account = get_account(&mut r)?;
                let status = match r.u8()? {
                    1 => DirStatus::Committed,
                    2 => DirStatus::Rejected,
                    3 => DirStatus::Unknown,
                    _ => return Err(DecodeError::BadReference),
                };
                let shard = get_shard(&mut r)?;
                let epoch = r.varint()?;
                // Una respuesta afirmativa o de rechazo describe un registro real (época ≥ 1); solo
                // «desconocida» puede traer ceros.
                if status != DirStatus::Unknown && epoch == 0 {
                    return Err(DecodeError::BadReference);
                }
                PeerMsg::DirResult {
                    xfer,
                    account,
                    status,
                    shard,
                    epoch,
                }
            }
            14 => PeerMsg::DirRedirect {
                leader: get_shard(&mut r)?,
            },
            15 => {
                let n = r.varint()? as usize;
                if n == 0 || n > MAX_PEER_MSG {
                    return Err(DecodeError::BadReference);
                }
                let mut b = Vec::with_capacity(n);
                for _ in 0..n {
                    b.push(r.u8()?);
                }
                PeerMsg::Raft(b)
            }
            _ => return Err(DecodeError::BadReference),
        };
        r.finish()?;
        Ok(msg)
    }
}

// ------------------------------------------------------------------------- bus simulado

#[derive(Debug, Clone)]
pub struct SimBusConfig {
    pub latency_ms: u64,
    pub jitter_ms: u64,
    pub loss: f32,
    pub duplicate: f32,
    pub seed: u64,
}

impl Default for SimBusConfig {
    fn default() -> Self {
        SimBusConfig {
            latency_ms: 5,
            jitter_ms: 0,
            loss: 0.0,
            duplicate: 0.0,
            seed: 1,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BusStats {
    pub sent: u64,
    pub dropped: u64,
    pub duplicated: u64,
    pub bytes: u64,
    /// Mensajes que no cabían en un datagrama ([`MAX_PEER_MSG`]) y no se enviaron.
    pub oversize: u64,
    /// Mensajes descartados por una partición de red o por una caída simulada (véase
    /// [`SimBus::isolate`], [`SimBus::set_link_down`] y [`SimBus::clear_inbox`]).
    pub partitioned: u64,
}

type Queued = Reverse<(u64, u64, ShardId, Vec<u8>)>;

struct Inner {
    cfg: SimBusConfig,
    rng: Rng,
    seq: u64,
    queues: Vec<BinaryHeap<Queued>>,
    stats: BusStats,
    /// Pares de shards sin comunicación (guardados como `(menor, mayor)`).
    down: BTreeSet<(ShardId, ShardId)>,
    /// Shards aislados del resto.
    isolated: BTreeSet<ShardId>,
}

impl Inner {
    fn blocked(&self, a: ShardId, b: ShardId) -> bool {
        self.isolated.contains(&a)
            || self.isolated.contains(&b)
            || self.down.contains(&(a.min(b), a.max(b)))
    }
}

/// Bus en memoria compartido por todos los shards de un proceso.
#[derive(Clone)]
pub struct SimBus {
    inner: Arc<Mutex<Inner>>,
}

impl SimBus {
    pub fn new(shards: usize, cfg: SimBusConfig) -> SimBus {
        SimBus {
            inner: Arc::new(Mutex::new(Inner {
                rng: Rng::new(cfg.seed),
                cfg,
                seq: 0,
                queues: (0..shards).map(|_| BinaryHeap::new()).collect(),
                stats: BusStats::default(),
                down: BTreeSet::new(),
                isolated: BTreeSet::new(),
            })),
        }
    }

    /// Corta (`down = true`) o restablece la comunicación entre dos shards, en los dos sentidos. Lo que
    /// se envíe mientras tanto se pierde; también lo que estuviera en camino y llegue durante el corte.
    pub fn set_link_down(&self, a: ShardId, b: ShardId, down: bool) {
        let mut g = self.lock();
        let key = (a.min(b), a.max(b));
        if down {
            g.down.insert(key);
        } else {
            g.down.remove(&key);
        }
    }

    /// Aísla un shard del resto (o lo reconecta). Equivale a cortar todos sus enlaces.
    pub fn isolate(&self, shard: ShardId, on: bool) {
        let mut g = self.lock();
        if on {
            g.isolated.insert(shard);
        } else {
            g.isolated.remove(&shard);
        }
    }

    /// Descarta lo que esperaba a un shard (lo que se perdería si el proceso cayera: nadie lo lee).
    pub fn clear_inbox(&self, shard: ShardId) {
        let mut g = self.lock();
        let lost = g.queues.get(shard as usize).map_or(0, |q| q.len()) as u64;
        if let Some(q) = g.queues.get_mut(shard as usize) {
            q.clear();
        }
        g.stats.partitioned += lost;
    }

    /// El extremo de un shard.
    pub fn end(&self, shard: ShardId) -> SimBusEnd {
        SimBusEnd {
            shard,
            bus: self.clone(),
        }
    }

    pub fn stats(&self) -> BusStats {
        self.lock().stats.clone()
    }

    /// Cambia las condiciones de la red (por ejemplo, para quitar la pérdida al final de una prueba).
    pub fn set_config(&self, cfg: SimBusConfig) {
        self.lock().cfg = cfg;
    }

    /// Mensajes aún en vuelo.
    pub fn in_flight(&self) -> usize {
        self.lock().queues.iter().map(|q| q.len()).sum()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct SimBusEnd {
    shard: ShardId,
    bus: SimBus,
}

impl PeerBus for SimBusEnd {
    fn send(&mut self, now_ms: u64, to: ShardId, data: Vec<u8>) {
        let mut g = self.bus.lock();
        if data.len() > MAX_PEER_MSG {
            g.stats.oversize += 1;
            return;
        }
        g.stats.sent += 1;
        g.stats.bytes += data.len() as u64;
        if g.blocked(self.shard, to) {
            g.stats.partitioned += 1;
            return;
        }
        if (to as usize) >= g.queues.len() || g.rng.unit() < g.cfg.loss {
            g.stats.dropped += 1;
            return;
        }
        let copies = if g.rng.unit() < g.cfg.duplicate {
            g.stats.duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let jitter = if g.cfg.jitter_ms > 0 {
                g.rng.next_u64() % (g.cfg.jitter_ms + 1)
            } else {
                0
            };
            // Como mínimo 1 ms: lo enviado en un paso solo se ve en el siguiente, sin importar en qué
            // orden se actualicen los shards.
            let at = now_ms + (g.cfg.latency_ms + jitter).max(1);
            g.seq += 1;
            let seq = g.seq;
            g.queues[to as usize].push(Reverse((at, seq, self.shard, data.clone())));
        }
    }

    fn recv(&mut self, now_ms: u64, out: &mut Vec<(ShardId, Vec<u8>)>) {
        let mut g = self.bus.lock();
        let mut cut = 0;
        while let Some(Reverse((from, data))) = {
            let q = &mut g.queues[self.shard as usize];
            if q.peek().is_some_and(|Reverse((at, ..))| *at <= now_ms) {
                q.pop()
                    .map(|Reverse((_, _, from, data))| Reverse((from, data)))
            } else {
                None
            }
        } {
            if g.blocked(from, self.shard) {
                cut += 1;
            } else {
                out.push((from, data));
            }
        }
        g.stats.partitioned += cut;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(x: i32, z: i32, data: u16) -> EntityState {
        EntityState {
            pos: [x, 0, z],
            data,
        }
    }

    fn samples() -> Vec<PeerMsg> {
        vec![
            PeerMsg::Ghosts {
                tick: 77,
                players: vec![(5, st(-640, 12, 1)), (9, st(100_000, -3, 1))],
                npcs: vec![
                    (NPC_BASE + 3, st(1, 2, 0)),
                    (NPC_BASE + 4, st(i32::MIN, i32::MAX, 0)),
                    (NPC_BASE + 90_000, st(0, 0, 65535)),
                ],
            },
            PeerMsg::Ghosts {
                tick: 0,
                players: vec![],
                npcs: vec![],
            },
            PeerMsg::NpcXfer(NpcXfer {
                xfer: 9,
                id: NPC_BASE + 12,
                pos: [-12.5, 0.0, 300.25],
                dir: [0.6, -0.8],
                turn_in: 41,
                rng: u64::MAX,
            }),
            PeerMsg::NpcXferAck {
                xfer: 9,
                id: NPC_BASE + 12,
            },
            PeerMsg::Prepare {
                xfer: 3,
                player: 1001,
                pos: [-5, 0, 77],
                last_processed: 4000,
            },
            PeerMsg::PrepareAck {
                xfer: 3,
                player: 1001,
            },
            PeerMsg::Activate {
                xfer: 3,
                player: 1001,
            },
            PeerMsg::Handover {
                xfer: 3,
                player: 1001,
                pos: [i32::MIN, 1, i32::MAX],
                last_processed: u32::MAX,
                epoch: u64::MAX,
            },
            PeerMsg::HandoverAck {
                xfer: 3,
                player: 1001,
            },
            PeerMsg::Abort {
                xfer: 3,
                player: 1001,
            },
            PeerMsg::DirLogin {
                account: 1001,
                home: 3,
            },
            PeerMsg::DirRoute {
                account: NPC_BASE - 1,
                shard: u16::MAX,
                epoch: 1,
            },
            PeerMsg::DirTransfer {
                xfer: u32::MAX,
                account: 7,
                epoch: u64::MAX,
                to: 0,
            },
            PeerMsg::DirResult {
                xfer: 5,
                account: 7,
                status: DirStatus::Committed,
                shard: 2,
                epoch: 9,
            },
            PeerMsg::DirResult {
                xfer: 5,
                account: 7,
                status: DirStatus::Rejected,
                shard: 1,
                epoch: 4,
            },
            PeerMsg::DirResult {
                xfer: 5,
                account: 7,
                status: DirStatus::Unknown,
                shard: 0,
                epoch: 0,
            },
        ]
    }

    #[test]
    fn ida_y_vuelta_de_todos_los_mensajes() {
        for m in samples() {
            let bytes = m.encode();
            assert_eq!(PeerMsg::decode(&bytes), Ok(m.clone()), "{m:?}");
        }
    }

    #[test]
    fn rechaza_truncados_y_sobrantes() {
        for m in samples() {
            let bytes = m.encode();
            for cut in 0..bytes.len() {
                assert!(
                    PeerMsg::decode(&bytes[..cut]).is_err(),
                    "{m:?} cortado en {cut}"
                );
            }
            let mut extra = bytes.clone();
            extra.push(0);
            assert_eq!(PeerMsg::decode(&extra), Err(DecodeError::Trailing), "{m:?}");
        }
    }

    #[test]
    fn rechaza_versiones_etiquetas_y_contenido_imposible() {
        let ok = PeerMsg::Abort { xfer: 1, player: 2 }.encode();
        let mut bad = ok.clone();
        bad[0] = PEER_VERSION + 1;
        assert!(PeerMsg::decode(&bad).is_err(), "versión futura");
        bad[0] = 1;
        assert!(
            PeerMsg::decode(&bad).is_err(),
            "la versión 1 (sin épocas) no se mezcla con la 2"
        );
        bad = ok.clone();
        bad[1] = 0;
        assert!(PeerMsg::decode(&bad).is_err(), "etiqueta 0");
        bad[1] = 200;
        assert!(PeerMsg::decode(&bad).is_err(), "etiqueta desconocida");

        // Ids repetidos o desordenados; NPC en la lista de jugadores y al revés.
        let mut w = Writer::default();
        w.u8(PEER_VERSION);
        w.u8(1);
        w.varint(1);
        w.varint(2); // dos jugadores
        w.varint(5);
        for _ in 0..3 {
            w.zigzag(0);
        }
        w.varint(1);
        w.varint(0); // delta 0 = repetido
        for _ in 0..3 {
            w.zigzag(0);
        }
        w.varint(1);
        w.varint(0);
        assert!(PeerMsg::decode(&w.buf).is_err(), "repetido");

        let npc_as_player = PeerMsg::Ghosts {
            tick: 1,
            players: vec![(NPC_BASE + 1, st(0, 0, 0))],
            npcs: vec![],
        }
        .encode();
        assert!(PeerMsg::decode(&npc_as_player).is_err());
        let player_as_npc = PeerMsg::Ghosts {
            tick: 1,
            players: vec![],
            npcs: vec![(7, st(0, 0, 0))],
        }
        .encode();
        assert!(PeerMsg::decode(&player_as_npc).is_err());

        // Un NPC con coordenadas no finitas o con identificador de jugador.
        let nan = PeerMsg::NpcXfer(NpcXfer {
            xfer: 1,
            id: NPC_BASE,
            pos: [f32::NAN, 0.0, 0.0],
            dir: [0.0, 0.0],
            turn_in: 0,
            rng: 5,
        })
        .encode();
        assert!(PeerMsg::decode(&nan).is_err(), "NaN");
        let inf = PeerMsg::NpcXfer(NpcXfer {
            xfer: 1,
            id: NPC_BASE,
            pos: [0.0, f32::INFINITY, 0.0],
            dir: [0.0, 0.0],
            turn_in: 0,
            rng: 5,
        })
        .encode();
        assert!(PeerMsg::decode(&inf).is_err(), "infinito");
        let zero_rng = PeerMsg::NpcXfer(NpcXfer {
            xfer: 1,
            id: NPC_BASE,
            pos: [0.0; 3],
            dir: [0.0; 2],
            turn_in: 0,
            rng: 0,
        })
        .encode();
        assert!(PeerMsg::decode(&zero_rng).is_err(), "rng 0");
        let not_npc = PeerMsg::NpcXfer(NpcXfer {
            xfer: 1,
            id: 12,
            pos: [0.0; 3],
            dir: [0.0; 2],
            turn_in: 0,
            rng: 5,
        })
        .encode();
        assert!(PeerMsg::decode(&not_npc).is_err(), "no es un NPC");
    }

    #[test]
    fn los_mensajes_del_directorio_rechazan_valores_imposibles() {
        let enc = |m: PeerMsg| m.encode();
        // Épocas a cero: una cuenta registrada siempre tiene al menos la época 1.
        for m in [
            PeerMsg::DirRoute {
                account: 1,
                shard: 0,
                epoch: 0,
            },
            PeerMsg::DirTransfer {
                xfer: 1,
                account: 1,
                epoch: 0,
                to: 1,
            },
            PeerMsg::Handover {
                xfer: 1,
                player: 1,
                pos: [0; 3],
                last_processed: 0,
                epoch: 0,
            },
            PeerMsg::DirResult {
                xfer: 1,
                account: 1,
                status: DirStatus::Committed,
                shard: 0,
                epoch: 0,
            },
            PeerMsg::DirResult {
                xfer: 1,
                account: 1,
                status: DirStatus::Rejected,
                shard: 0,
                epoch: 0,
            },
        ] {
            assert!(PeerMsg::decode(&enc(m.clone())).is_err(), "{m:?}");
        }
        // Una cuenta en el rango de los NPC no es una cuenta.
        for m in [
            PeerMsg::DirLogin {
                account: NPC_BASE,
                home: 0,
            },
            PeerMsg::DirRoute {
                account: NPC_BASE + 5,
                shard: 0,
                epoch: 1,
            },
            PeerMsg::DirTransfer {
                xfer: 1,
                account: u64::MAX,
                epoch: 1,
                to: 0,
            },
        ] {
            assert!(PeerMsg::decode(&enc(m.clone())).is_err(), "{m:?}");
        }
        // Un estado desconocido y un shard que no cabe en 16 bits.
        let mut w = Writer::default();
        w.u8(PEER_VERSION);
        w.u8(13);
        w.varint(1);
        w.varint(1);
        w.u8(9); // estado desconocido
        w.varint(1);
        w.varint(1);
        assert!(PeerMsg::decode(&w.buf).is_err(), "estado inventado");
        let mut w = Writer::default();
        w.u8(PEER_VERSION);
        w.u8(10);
        w.varint(1);
        w.varint(70_000); // no cabe en un ShardId
        assert!(PeerMsg::decode(&w.buf).is_err(), "shard fuera de rango");
    }

    #[test]
    fn el_bus_simulado_corta_enlaces_aisla_y_descarta_lo_que_esperaba() {
        let bus = SimBus::new(3, SimBusConfig::default());
        let (mut a, mut b, mut c) = (bus.end(0), bus.end(1), bus.end(2));
        let mut out = Vec::new();
        let mut got = |end: &mut SimBusEnd, now| {
            out.clear();
            end.recv(now, &mut out);
            out.len()
        };
        // Un enlace cortado pierde lo enviado, en los dos sentidos, pero no afecta a los demás.
        bus.set_link_down(0, 1, true);
        a.send(0, 1, vec![1]);
        b.send(0, 0, vec![2]);
        a.send(0, 2, vec![3]);
        assert_eq!(got(&mut b, 100), 0);
        assert_eq!(got(&mut a, 100), 0);
        assert_eq!(got(&mut c, 100), 1);
        assert_eq!(bus.stats().partitioned, 2);
        bus.set_link_down(1, 0, false);
        a.send(100, 1, vec![4]);
        assert_eq!(got(&mut b, 200), 1);
        // Un mensaje ya en camino cuando se corta el enlace tampoco llega.
        a.send(200, 1, vec![5]);
        bus.set_link_down(0, 1, true);
        assert_eq!(got(&mut b, 300), 0);
        bus.set_link_down(0, 1, false);
        // Aislar un shard lo deja sin enviar ni recibir.
        bus.isolate(2, true);
        a.send(300, 2, vec![6]);
        c.send(300, 0, vec![7]);
        assert_eq!(got(&mut c, 400), 0);
        assert_eq!(got(&mut a, 400), 0);
        bus.isolate(2, false);
        a.send(400, 2, vec![8]);
        assert_eq!(got(&mut c, 500), 1);
        // Una caída descarta lo que esperaba al shard.
        a.send(500, 1, vec![9]);
        a.send(500, 1, vec![10]);
        let before = bus.stats().partitioned;
        bus.clear_inbox(1);
        assert_eq!(bus.stats().partitioned, before + 2);
        assert_eq!(got(&mut b, 600), 0);
    }

    #[test]
    fn una_cuenta_enorme_no_reserva_memoria() {
        let mut w = Writer::default();
        w.u8(PEER_VERSION);
        w.u8(1);
        w.varint(1);
        w.varint(u32::MAX as u64); // «4000 millones de jugadores» en 8 bytes de mensaje
        assert!(PeerMsg::decode(&w.buf).is_err());
        // Una cuenta plausible pero sin datos: falla por truncado.
        let mut w = Writer::default();
        w.u8(PEER_VERSION);
        w.u8(1);
        w.varint(1);
        w.varint(40_000);
        assert_eq!(PeerMsg::decode(&w.buf), Err(DecodeError::Truncated));
    }

    #[test]
    fn los_fantasmas_se_trocean_sin_perder_ni_reordenar_nada() {
        let mut rng = Rng::new(42);
        for round in 0..60 {
            let np = (rng.next_u64() % 400) as usize * (round % 3 == 0) as usize;
            let nn = (rng.next_u64() % 3000) as usize;
            let mut id = 0u64;
            let mut mk = |rng: &mut Rng, base: u64, n: usize, wide: bool| {
                let mut v = Vec::new();
                let mut cur = base;
                for _ in 0..n {
                    cur += 1 + rng.next_u64() % if wide { 5000 } else { 4 };
                    let span = if wide { 1 << 30 } else { 200_000 };
                    let c = |rng: &mut Rng| (rng.next_u64() % (2 * span)) as i64 - span as i64;
                    v.push((
                        cur,
                        EntityState {
                            pos: [c(rng) as i32, c(rng) as i32 / 8, c(rng) as i32],
                            data: (rng.next_u64() % 3) as u16 * 30_000,
                        },
                    ));
                }
                id = cur;
                v
            };
            let wide = round % 2 == 1;
            let players = mk(&mut rng, 0, np, wide);
            let npcs = mk(&mut rng, NPC_BASE, nn, wide);
            let _ = id;
            for max_len in [120, 300, MAX_PEER_MSG] {
                let chunks = ghost_chunks(9_000_000, players.clone(), npcs.clone(), max_len);
                let (mut ps, mut ns) = (Vec::new(), Vec::new());
                for c in &chunks {
                    let bytes = c.encode();
                    assert!(bytes.len() <= max_len, "{} > {max_len}", bytes.len());
                    match PeerMsg::decode(&bytes).unwrap() {
                        PeerMsg::Ghosts {
                            tick,
                            players,
                            npcs,
                        } => {
                            assert_eq!(tick, 9_000_000);
                            assert!(!players.is_empty() || !npcs.is_empty(), "sin trozos vacíos");
                            ps.extend(players);
                            ns.extend(npcs);
                        }
                        other => panic!("{other:?}"),
                    }
                }
                assert_eq!(ps, players);
                assert_eq!(ns, npcs);
                // No se desperdicia: salvo el último, cada trozo está casi lleno.
                if chunks.len() > 1 {
                    for c in &chunks[..chunks.len() - 1] {
                        assert!(c.encode().len() + 30 > max_len, "trozo a medias");
                    }
                }
            }
        }
        assert!(ghost_chunks(1, vec![], vec![], MAX_PEER_MSG).is_empty());
    }

    #[test]
    fn el_bus_simulado_rechaza_lo_que_no_cabe_en_un_datagrama() {
        let bus = SimBus::new(2, SimBusConfig::default());
        let (mut a, mut b) = (bus.end(0), bus.end(1));
        a.send(0, 1, vec![0; MAX_PEER_MSG]);
        a.send(0, 1, vec![0; MAX_PEER_MSG + 1]);
        let mut out = Vec::new();
        b.recv(100, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(bus.stats().oversize, 1);
        assert_eq!(bus.stats().sent, 1);
    }

    #[test]
    fn el_bus_simulado_entrega_con_latencia_y_en_orden() {
        let bus = SimBus::new(
            3,
            SimBusConfig {
                latency_ms: 20,
                ..Default::default()
            },
        );
        let (mut a, mut b, mut c) = (bus.end(0), bus.end(1), bus.end(2));
        a.send(100, 1, vec![1]);
        a.send(100, 1, vec![2]);
        c.send(105, 1, vec![3]);
        a.send(100, 9, vec![4]); // destino inexistente: se descarta sin romper nada
        let mut out = Vec::new();
        b.recv(119, &mut out);
        assert!(out.is_empty(), "aún no ha pasado la latencia");
        b.recv(120, &mut out);
        assert_eq!(out, vec![(0, vec![1]), (0, vec![2])]);
        out.clear();
        b.recv(125, &mut out);
        assert_eq!(out, vec![(2, vec![3])]);
        a.recv(1000, &mut out);
        assert_eq!(out.len(), 1, "a no recibe nada de lo que no era para él");
        assert_eq!(bus.stats().sent, 4);
        assert_eq!(bus.stats().dropped, 1);
        assert_eq!(bus.in_flight(), 0);
        // Con latencia 0 se entrega como pronto en el milisegundo siguiente.
        let fast = SimBus::new(
            2,
            SimBusConfig {
                latency_ms: 0,
                ..Default::default()
            },
        );
        let (mut x, mut y) = (fast.end(0), fast.end(1));
        x.send(50, 1, vec![7]);
        let mut out = Vec::new();
        y.recv(50, &mut out);
        assert!(out.is_empty());
        y.recv(51, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn perdida_duplicados_y_determinismo() {
        let run = |seed| {
            let bus = SimBus::new(
                2,
                SimBusConfig {
                    latency_ms: 10,
                    jitter_ms: 15,
                    loss: 0.2,
                    duplicate: 0.1,
                    seed,
                },
            );
            let (mut a, mut b) = (bus.end(0), bus.end(1));
            for i in 0..2000u32 {
                a.send(i as u64, 1, i.to_le_bytes().to_vec());
            }
            let mut out = Vec::new();
            b.recv(10_000, &mut out);
            (out, bus.stats())
        };
        let (out1, s1) = run(5);
        let (out2, s2) = run(5);
        let (out3, _) = run(6);
        assert_eq!(out1, out2, "misma semilla, misma red");
        assert_eq!(s1, s2);
        assert_ne!(out1, out3);
        assert!(
            (300..=500).contains(&(s1.dropped as i64)),
            "≈20 % perdido: {}",
            s1.dropped
        );
        assert!(s1.duplicated > 100, "duplicados: {}", s1.duplicated);
        // Con jitter, los paquetes pueden llegar desordenados (el protocolo debe tolerarlo).
        let ids: Vec<u32> = out1
            .iter()
            .map(|(_, d)| u32::from_le_bytes(d[..4].try_into().unwrap()))
            .collect();
        assert!(
            ids.windows(2).any(|w| w[0] > w[1]),
            "debería haber desorden"
        );
    }
}
