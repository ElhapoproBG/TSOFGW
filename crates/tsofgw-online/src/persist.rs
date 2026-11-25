//! Persistencia del servidor de juego: **qué** se guarda, bajo qué claves y cuándo se hace durable.
//!
//! El almacén ([`tsofgw_store`]) es un mapa clave-valor con un diario; aquí se decide su contenido:
//!
//! ```text
//! m/init                 identidad del mundo y del shard (una carpeta de datos no se mezcla con otro mundo)
//! m/xfer                 reserva de números de traspaso (para no repetirlos tras un reinicio)
//! n/<npc>                un NPC que este shard simula: posición, dirección, giro pendiente, generador
//! xo/<npc>               un NPC en traspaso hacia otro shard, sin acuse todavía (buzón de salida)
//! xi/<shard><xfer>       un traspaso de NPC ya adoptado (para reconocer repeticiones tras un reinicio)
//! p/<cuenta>             un jugador residente aquí: posición y época
//! ho/<cuenta>            un jugador en traspaso hacia otro shard, sin acuse todavía (buzón de salida)
//! t/<cuenta>             lápida: la cuenta salió de aquí (época y destino) — una repetición vieja no la resucita
//! ```
//!
//! **Cada NPC y cada cuenta vive bajo exactamente una de esas claves a la vez** (`n`/`xo`, `p`/`ho`): el
//! paso de una a otra se escribe en **un solo lote**, así que tras cualquier caída hay un dueño, nunca
//! cero ni dos.
//!
//! # Orden de las escrituras
//!
//! Lo que se promete al otro lado («lo adopté», «te lo mando») solo se envía **después** de que lo
//! anotado esté en disco: los mensajes que dependen de eso se guardan en una cola ([`Persist::defer`]) y
//! salen tras `commit` al final de cada `update`. Un `commit` por vuelta de `update` (no por mensaje)
//! agrupa todo lo del tick en una sola sincronización.
//!
//! Lo que no es crítico (la posición de un jugador que camina, el estado de un NPC que deambula) se
//! escribe sin sincronizar y se hace durable cada `sync_ticks`: tras una caída se pierde, como mucho,
//! ese intervalo de movimiento, nunca la propiedad de nada.

use tsofgw_store::{Batch, DynStore, Recovery};

use crate::bus::{NpcXfer, PeerMsg};
use crate::partition::ShardId;

/// Clave de integridad del almacén de un shard (o de un servidor suelto, con `None`), derivada de un
/// secreto del operador. Cada shard tiene la suya, de modo que la carpeta de uno no se puede pasar por la
/// de otro aunque estén en el mismo disco.
pub fn store_key_for(secret: &[u8], shard: Option<crate::partition::ShardId>) -> [u8; 32] {
    let label = match shard {
        Some(s) => format!("shard-{s}"),
        None => "server".to_string(),
    };
    tsofgw_store::derive_key(secret, &label)
}

/// Cuándo se guarda cada cosa. Todo en ticks del servidor.
#[derive(Debug, Clone)]
pub struct PersistConfig {
    /// Cada cuántos ticks se escribe el estado de los NPC que cambiaron.
    pub npc_checkpoint_ticks: u32,
    /// Cada cuántos ticks se escribe la posición de los jugadores conectados que se movieron.
    pub player_save_ticks: u32,
    /// Como mucho cada cuántos ticks se sincroniza el disco por lo no crítico (lo crítico se
    /// sincroniza al final de la vuelta en que ocurre).
    pub sync_ticks: u32,
    /// Si el diario pasa de estos bytes, se compacta con un checkpoint.
    pub compact_bytes: u64,
    /// Cada cuántos ticks se reenvía un traspaso de jugador que lleva más de `handoff_timeout_ticks`
    /// sin acuse (con persistencia, el buzón de salida no se rinde nunca: reintenta despacio).
    pub slow_retry_ticks: u32,
    /// Una sesión que lleva tanto esperando entrar (a que el directorio responda, o a que le llegue
    /// el estado de un traspaso) se cierra; el cliente puede volver a intentarlo.
    pub login_timeout_ticks: u32,
}

impl Default for PersistConfig {
    fn default() -> Self {
        PersistConfig {
            npc_checkpoint_ticks: 200,
            player_save_ticks: 100,
            sync_ticks: 20,
            compact_bytes: 16 << 20,
            slow_retry_ticks: 40,
            login_timeout_ticks: 600,
        }
    }
}

/// Contadores de la persistencia (acumulados desde que arrancó el proceso).
#[derive(Debug, Clone, Default)]
pub struct PersistStats {
    pub commits: u64,
    pub commit_errors: u64,
    pub compactions: u64,
    pub npc_saves: u64,
    pub player_saves: u64,
    /// Mensajes que esperaron a que lo anotado fuera durable, y los que se descartaron porque el
    /// almacén falló (se reintentan solos por los buzones de salida cuando el almacén vuelve).
    pub deferred_sent: u64,
    pub deferred_dropped: u64,
    /// Jugadores que entraron con la posición guardada (en vez de aparecer de cero).
    pub players_restored: u64,
    /// Sesiones cerradas por llevar demasiado esperando entrar.
    pub joins_timed_out: u64,
}

/// Lo que encontró el arranque al restaurar el estado.
#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    /// La carpeta estaba vacía: se inició con el mundo de la semilla.
    pub first_start: bool,
    pub recovery: Recovery,
    pub npcs: usize,
    pub npc_transfers: usize,
    pub handovers: usize,
    pub seen_transfers: usize,
    pub residents: usize,
    pub tombstones: usize,
}

/// Qué hay en el almacén de un shard, por clase de registro (para `tsofgw store info` y para pruebas).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShardStoreSummary {
    /// ¿Tiene el registro de identidad del mundo? `false`: carpeta vacía (o que no es de un shard).
    pub initialized: bool,
    /// Último reloj de ticks guardado.
    pub tick: Option<u32>,
    /// NPC que el shard simulaba.
    pub npcs: usize,
    /// NPC en traspaso hacia otro shard, sin acuse.
    pub npc_transfers_out: usize,
    /// Traspasos de NPC ya adoptados que se recuerdan para reconocer repeticiones.
    pub adopted_transfers: usize,
    /// Cuentas residentes.
    pub residents: usize,
    /// Cuentas en traspaso hacia otro shard, sin acuse; de ellas, las que aún esperan al directorio.
    pub handovers: usize,
    pub handovers_awaiting_directory: usize,
    /// Lápidas de cuentas que salieron de este shard.
    pub tombstones: usize,
    /// Claves que no se reconocen (almacén de otra versión o alterado).
    pub unknown: usize,
}

/// Cuenta lo que hay en el almacén de un shard. No modifica nada.
pub fn summarize_shard_store(store: &DynStore) -> ShardStoreSummary {
    let mut r = ShardStoreSummary {
        initialized: store.contains(K_INIT),
        tick: store.get(K_TICK).and_then(decode_u32),
        ..ShardStoreSummary::default()
    };
    for (k, v) in store.scan(b"") {
        if k == K_INIT || k == K_XFER || k == K_TICK {
            continue;
        }
        if k.starts_with(P_NPC) {
            r.npcs += 1;
        } else if k.starts_with(P_XO) {
            r.npc_transfers_out += 1;
        } else if k.starts_with(P_XI) {
            r.adopted_transfers += 1;
        } else if k.starts_with(P_PLAYER) {
            r.residents += 1;
        } else if k.starts_with(P_HO) {
            r.handovers += 1;
            if HoRec::decode(v).is_some_and(|h| h.stage == HoStage::Committing) {
                r.handovers_awaiting_directory += 1;
            }
        } else if k.starts_with(P_TOMB) {
            r.tombstones += 1;
        } else {
            r.unknown += 1;
        }
    }
    r
}

// ---------------------------------------------------------------- claves

const P_NPC: &[u8] = b"n/";
const P_XO: &[u8] = b"xo/";
const P_XI: &[u8] = b"xi/";
const P_PLAYER: &[u8] = b"p/";
const P_HO: &[u8] = b"ho/";
const P_TOMB: &[u8] = b"t/";

pub(crate) const K_INIT: &[u8] = b"m/init";
pub(crate) const K_XFER: &[u8] = b"m/xfer";
pub(crate) const K_TICK: &[u8] = b"m/tick";

fn key_u32(prefix: &[u8], v: u32) -> Vec<u8> {
    let mut k = prefix.to_vec();
    k.extend_from_slice(&v.to_be_bytes());
    k
}

fn key_u64(prefix: &[u8], v: u64) -> Vec<u8> {
    let mut k = prefix.to_vec();
    k.extend_from_slice(&v.to_be_bytes());
    k
}

pub(crate) fn k_npc(idx: usize) -> Vec<u8> {
    key_u32(P_NPC, idx as u32)
}
pub(crate) fn k_xo(idx: usize) -> Vec<u8> {
    key_u32(P_XO, idx as u32)
}
pub(crate) fn k_xi(from: ShardId, xfer: u32) -> Vec<u8> {
    let mut k = P_XI.to_vec();
    k.extend_from_slice(&from.to_be_bytes());
    k.extend_from_slice(&xfer.to_be_bytes());
    k
}
pub(crate) fn k_player(id: u64) -> Vec<u8> {
    key_u64(P_PLAYER, id)
}
pub(crate) fn k_ho(id: u64) -> Vec<u8> {
    key_u64(P_HO, id)
}
pub(crate) fn k_tomb(id: u64) -> Vec<u8> {
    key_u64(P_TOMB, id)
}

pub(crate) const PREFIX_NPC: &[u8] = P_NPC;
pub(crate) const PREFIX_XO: &[u8] = P_XO;
pub(crate) const PREFIX_XI: &[u8] = P_XI;
pub(crate) const PREFIX_PLAYER: &[u8] = P_PLAYER;
pub(crate) const PREFIX_HO: &[u8] = P_HO;
pub(crate) const PREFIX_TOMB: &[u8] = P_TOMB;

/// Índice de NPC de una clave `n/…` o `xo/…`.
pub(crate) fn idx_of(key: &[u8], prefix: &[u8]) -> Option<usize> {
    let b: [u8; 4] = key.strip_prefix(prefix)?.try_into().ok()?;
    Some(u32::from_be_bytes(b) as usize)
}

pub(crate) fn account_of(key: &[u8], prefix: &[u8]) -> Option<u64> {
    let b: [u8; 8] = key.strip_prefix(prefix)?.try_into().ok()?;
    Some(u64::from_be_bytes(b))
}

pub(crate) fn xi_of(key: &[u8]) -> Option<(ShardId, u32)> {
    let rest = key.strip_prefix(P_XI)?;
    if rest.len() != 6 {
        return None;
    }
    Some((
        u16::from_be_bytes([rest[0], rest[1]]),
        u32::from_be_bytes([rest[2], rest[3], rest[4], rest[5]]),
    ))
}

// ---------------------------------------------------------------- valores

struct Enc(Vec<u8>);

impl Enc {
    fn new() -> Enc {
        Enc(Vec::with_capacity(48))
    }
    fn u8(mut self, v: u8) -> Enc {
        self.0.push(v);
        self
    }
    fn u16(mut self, v: u16) -> Enc {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u32(mut self, v: u32) -> Enc {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(mut self, v: u64) -> Enc {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn i32(mut self, v: i32) -> Enc {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn f32(mut self, v: f32) -> Enc {
        self.0.extend_from_slice(&v.to_bits().to_le_bytes());
        self
    }
}

struct Dec<'a>(&'a [u8]);

impl<'a> Dec<'a> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take()?))
    }
    /// Un `f32` finito (un valor no finito sería un registro dañado, nunca algo que simular).
    fn f32(&mut self) -> Option<f32> {
        let v = f32::from_bits(self.u32()?);
        v.is_finite().then_some(v)
    }
    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

/// Un NPC que este shard simula.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NpcRec {
    pub pos: [f32; 3],
    pub dir: [f32; 2],
    pub turn_in: u32,
    pub rng: u64,
}

impl NpcRec {
    pub fn encode(&self) -> Vec<u8> {
        Enc::new()
            .f32(self.pos[0])
            .f32(self.pos[1])
            .f32(self.pos[2])
            .f32(self.dir[0])
            .f32(self.dir[1])
            .u32(self.turn_in)
            .u64(self.rng)
            .0
    }
    pub fn decode(b: &[u8]) -> Option<NpcRec> {
        let mut d = Dec(b);
        let r = NpcRec {
            pos: [d.f32()?, d.f32()?, d.f32()?],
            dir: [d.f32()?, d.f32()?],
            turn_in: d.u32()?,
            rng: d.u64()?,
        };
        d.done().then_some(r)
    }
    pub fn from_xfer(x: &NpcXfer) -> NpcRec {
        NpcRec {
            pos: x.pos,
            dir: x.dir,
            turn_in: x.turn_in,
            rng: x.rng,
        }
    }
}

/// Un traspaso de NPC sin acuse (buzón de salida).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct XoRec {
    pub to: ShardId,
    pub msg: NpcXfer,
}

impl XoRec {
    pub fn encode(&self) -> Vec<u8> {
        let m = &self.msg;
        Enc::new()
            .u16(self.to)
            .u32(m.xfer)
            .u64(m.id)
            .f32(m.pos[0])
            .f32(m.pos[1])
            .f32(m.pos[2])
            .f32(m.dir[0])
            .f32(m.dir[1])
            .u32(m.turn_in)
            .u64(m.rng)
            .0
    }
    pub fn decode(b: &[u8]) -> Option<XoRec> {
        let mut d = Dec(b);
        let to = d.u16()?;
        let msg = NpcXfer {
            xfer: d.u32()?,
            id: d.u64()?,
            pos: [d.f32()?, d.f32()?, d.f32()?],
            dir: [d.f32()?, d.f32()?],
            turn_in: d.u32()?,
            rng: d.u64()?,
        };
        d.done().then_some(XoRec { to, msg })
    }
}

/// Un jugador residente en este shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlayerRec {
    pub pos: [i32; 3],
    pub epoch: u64,
}

impl PlayerRec {
    pub fn encode(&self) -> Vec<u8> {
        Enc::new()
            .i32(self.pos[0])
            .i32(self.pos[1])
            .i32(self.pos[2])
            .u64(self.epoch)
            .0
    }
    pub fn decode(b: &[u8]) -> Option<PlayerRec> {
        let mut d = Dec(b);
        let r = PlayerRec {
            pos: [d.i32()?, d.i32()?, d.i32()?],
            epoch: d.u64()?,
        };
        d.done().then_some(r)
    }
}

/// Fase de un traspaso de jugador en el buzón de salida.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HoStage {
    /// Anotado y congelado, esperando que el directorio confirme el cambio de dueño.
    Committing = 1,
    /// El cambio de dueño está decidido: se reenvía el estado al destino hasta que lo acuse.
    Committed = 2,
}

/// Un traspaso de jugador sin acuse (buzón de salida): lleva el estado final del jugador.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HoRec {
    pub to: ShardId,
    pub xfer: u32,
    /// Época que tendrá la cuenta en el destino.
    pub epoch: u64,
    pub pos: [i32; 3],
    pub last_processed: u32,
    pub stage: HoStage,
}

impl HoRec {
    pub fn encode(&self) -> Vec<u8> {
        Enc::new()
            .u16(self.to)
            .u32(self.xfer)
            .u64(self.epoch)
            .i32(self.pos[0])
            .i32(self.pos[1])
            .i32(self.pos[2])
            .u32(self.last_processed)
            .u8(self.stage as u8)
            .0
    }
    pub fn decode(b: &[u8]) -> Option<HoRec> {
        let mut d = Dec(b);
        let to = d.u16()?;
        let xfer = d.u32()?;
        let epoch = d.u64()?;
        let pos = [d.i32()?, d.i32()?, d.i32()?];
        let last_processed = d.u32()?;
        let stage = match d.u8()? {
            1 => HoStage::Committing,
            2 => HoStage::Committed,
            _ => return None,
        };
        d.done().then_some(HoRec {
            to,
            xfer,
            epoch,
            pos,
            last_processed,
            stage,
        })
    }
}

/// La cuenta ya no está aquí: a qué shard se fue y con qué época.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TombRec {
    pub epoch: u64,
    pub to: ShardId,
}

impl TombRec {
    pub fn encode(&self) -> Vec<u8> {
        Enc::new().u64(self.epoch).u16(self.to).0
    }
    pub fn decode(b: &[u8]) -> Option<TombRec> {
        let mut d = Dec(b);
        let r = TombRec {
            epoch: d.u64()?,
            to: d.u16()?,
        };
        d.done().then_some(r)
    }
}

pub(crate) fn encode_init(identity: u64) -> Vec<u8> {
    Enc::new().u64(identity).0
}

pub(crate) fn decode_init(b: &[u8]) -> Option<u64> {
    let mut d = Dec(b);
    let v = d.u64()?;
    d.done().then_some(v)
}

/// Un contador de 32 bits (`m/xfer`, `m/tick`).
pub(crate) fn encode_u32(v: u32) -> Vec<u8> {
    Enc::new().u32(v).0
}

pub(crate) fn decode_u32(b: &[u8]) -> Option<u32> {
    let mut d = Dec(b);
    let v = d.u32()?;
    d.done().then_some(v)
}

// ---------------------------------------------------------------- estado

/// Cuántos números de traspaso se reservan de una vez (un reinicio salta a la reserva siguiente).
pub(crate) const XFER_BLOCK: u32 = 4096;

pub(crate) struct Persist {
    pub store: DynStore,
    pub cfg: PersistConfig,
    npc_dirty: Vec<bool>,
    dirty: usize,
    /// Última posición guardada de cada jugador conectado (para no reescribir lo que no se movió).
    pub saved_pos: tsofgw_world::FastMap<u64, [i32; 3]>,
    deferred: Vec<(ShardId, PeerMsg)>,
    pub last_checkpoint: u32,
    pub last_sync: u32,
    /// Hasta qué número de traspaso ya está reservado en disco.
    pub xfer_reserved: u32,
    /// El almacén falló: se deja de prometer cosas (nada nuevo sale de los buzones hasta reiniciar).
    pub failed: bool,
    /// Algo de lo escrito sostiene una promesa que se va a enviar: hay que sincronizar antes de enviarla.
    pub critical: bool,
    pub stats: PersistStats,
}

impl Persist {
    pub fn new(store: DynStore, cfg: PersistConfig, npc_count: usize) -> Persist {
        Persist {
            store,
            cfg,
            npc_dirty: vec![false; npc_count],
            dirty: 0,
            saved_pos: tsofgw_world::FastMap::default(),
            deferred: Vec::new(),
            last_checkpoint: 0,
            last_sync: 0,
            xfer_reserved: 0,
            failed: false,
            critical: false,
            stats: PersistStats::default(),
        }
    }

    pub fn mark_npc(&mut self, idx: usize) {
        if let Some(d) = self.npc_dirty.get_mut(idx) {
            if !*d {
                *d = true;
                self.dirty += 1;
            }
        }
    }

    pub fn clear_npc(&mut self, idx: usize) {
        if let Some(d) = self.npc_dirty.get_mut(idx) {
            if *d {
                *d = false;
                self.dirty -= 1;
            }
        }
    }

    pub fn is_dirty(&self, idx: usize) -> bool {
        self.npc_dirty.get(idx).copied().unwrap_or(false)
    }

    /// Escribe un lote. Si el almacén falla, se marca el fallo y no se propaga: el juego sigue, pero
    /// nada que dependa de lo anotado se promete.
    pub fn apply(&mut self, b: &Batch) -> bool {
        if self.failed {
            return false;
        }
        match self.store.apply(b) {
            Ok(()) => true,
            Err(_) => {
                self.failed = true;
                self.stats.commit_errors += 1;
                false
            }
        }
    }

    /// Como [`Persist::apply`], pero lo escrito sostiene una promesa que saldrá en cuanto se
    /// sincronice (véase [`Persist::defer`]).
    pub fn apply_critical(&mut self, b: &Batch) -> bool {
        self.critical = true;
        self.apply(b)
    }

    pub fn commit(&mut self) -> bool {
        if self.failed {
            return false;
        }
        match self.store.commit() {
            Ok(()) => {
                self.stats.commits += 1;
                true
            }
            Err(_) => {
                self.failed = true;
                self.stats.commit_errors += 1;
                false
            }
        }
    }

    /// Guarda un mensaje para enviarlo cuando lo anotado hasta ahora sea durable.
    pub fn defer(&mut self, to: ShardId, msg: PeerMsg) {
        if self.failed {
            self.stats.deferred_dropped += 1;
            return;
        }
        self.critical = true;
        self.deferred.push((to, msg));
    }

    pub fn take_deferred(&mut self) -> Vec<(ShardId, PeerMsg)> {
        std::mem::take(&mut self.deferred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn los_registros_ida_y_vuelta_y_rechazan_basura() {
        let n = NpcRec {
            pos: [1.5, -2.0, 3.25],
            dir: [0.6, -0.8],
            turn_in: 17,
            rng: 0xdead_beef_1234,
        };
        assert_eq!(NpcRec::decode(&n.encode()), Some(n.clone()));
        let x = XoRec {
            to: 3,
            msg: NpcXfer {
                xfer: 9,
                id: (1 << 48) + 5,
                pos: [4.0, 0.0, -9.5],
                dir: [1.0, 0.0],
                turn_in: 3,
                rng: 77,
            },
        };
        assert_eq!(XoRec::decode(&x.encode()), Some(x.clone()));
        let p = PlayerRec {
            pos: [-640, 0, 12_345],
            epoch: 7,
        };
        assert_eq!(PlayerRec::decode(&p.encode()), Some(p));
        for stage in [HoStage::Committing, HoStage::Committed] {
            let h = HoRec {
                to: 2,
                xfer: 41,
                epoch: 8,
                pos: [1, 2, 3],
                last_processed: 99,
                stage,
            };
            assert_eq!(HoRec::decode(&h.encode()), Some(h));
        }
        let t = TombRec { epoch: 5, to: 1 };
        assert_eq!(TombRec::decode(&t.encode()), Some(t));
        assert_eq!(decode_init(&encode_init(42)), Some(42));
        assert_eq!(decode_u32(&encode_u32(4096)), Some(4096));
        // Truncados: cada decodificador rechaza cualquier prefijo propio.
        for cut in 0..n.encode().len() {
            assert!(NpcRec::decode(&n.encode()[..cut]).is_none());
        }
        for cut in 0..x.encode().len() {
            assert!(XoRec::decode(&x.encode()[..cut]).is_none());
        }
        for cut in 0..p.encode().len() {
            assert!(PlayerRec::decode(&p.encode()[..cut]).is_none());
        }
        for cut in 0..t.encode().len() {
            assert!(TombRec::decode(&t.encode()[..cut]).is_none());
        }
        // Con sobrantes y con flotantes no finitos: nada se acepta.
        let mut long = n.encode();
        long.push(0);
        assert!(NpcRec::decode(&long).is_none());
        let mut nan = n.encode();
        nan[..4].copy_from_slice(&f32::NAN.to_bits().to_le_bytes());
        assert!(NpcRec::decode(&nan).is_none());
        let mut bad_stage = HoRec {
            to: 0,
            xfer: 1,
            epoch: 1,
            pos: [0; 3],
            last_processed: 0,
            stage: HoStage::Committed,
        }
        .encode();
        *bad_stage.last_mut().unwrap() = 9;
        assert!(HoRec::decode(&bad_stage).is_none());
    }

    #[test]
    fn las_claves_se_leen_de_vuelta_y_se_ordenan() {
        assert_eq!(idx_of(&k_npc(123_456), PREFIX_NPC), Some(123_456));
        assert_eq!(idx_of(&k_xo(7), PREFIX_XO), Some(7));
        assert_eq!(idx_of(&k_npc(7), PREFIX_XO), None);
        assert_eq!(
            account_of(&k_player(u64::MAX - 3), PREFIX_PLAYER),
            Some(u64::MAX - 3)
        );
        assert_eq!(account_of(&k_ho(5), PREFIX_HO), Some(5));
        assert_eq!(account_of(&k_tomb(5), PREFIX_TOMB), Some(5));
        assert_eq!(xi_of(&k_xi(3, 9_000)), Some((3, 9_000)));
        // Big-endian: el orden de las claves es el orden numérico.
        assert!(k_npc(9) < k_npc(10));
        assert!(k_npc(255) < k_npc(256));
        assert!(k_player(1) < k_player(1 << 40));
        // Un prefijo no se confunde con otro (`n/` frente a `xo/`, `p/` frente a `ho/`).
        assert!(!k_xo(1).starts_with(PREFIX_NPC));
        assert!(!k_ho(1).starts_with(PREFIX_PLAYER));
    }
}
