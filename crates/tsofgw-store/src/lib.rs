//! `tsofgw-store`: un almacén clave-valor **durable** para lo que un servidor de juego no puede perder
//! (jugadores, NPC, traspasos en curso), hecho solo con la biblioteca estándar.
//!
//! # Cómo funciona
//!
//! El estado vive en memoria (un `BTreeMap` ordenado, así que se pueden recorrer prefijos) y cada cambio
//! se anota antes en un **diario** (*write-ahead log*). De vez en cuando un **checkpoint** guarda el mapa
//! entero y empieza un diario nuevo. Al arrancar se carga el último checkpoint y se repite el diario.
//!
//! ```text
//! diario:   registro := longitud u32 | secuencia u64 | etiqueta de cabecera (8) | carga | etiqueta (16)
//! carga:    nº de operaciones (varint) | [1 = poner, 2 = borrar] clave [valor]  (varint de longitudes)
//! etiqueta: HMAC-SHA256(clave, dominio | generación | longitud | secuencia | carga), truncada
//! checkpoint: "TSOFGWSN" | versión | generación u64 | última secuencia u64 | nº de entradas | entradas | etiqueta (32)
//! ```
//!
//! * **Un registro es atómico**: un [`Batch`] (varias operaciones) se escribe como un solo registro, así
//!   que «quitar el NPC de aquí y anotar que está en traspaso» ocurre entero o no ocurre.
//! * **Integridad con clave**: cada registro y cada checkpoint llevan un HMAC. Un daño del disco o una
//!   edición a mano se detectan; sin la clave no se puede fabricar un registro válido. La secuencia (que
//!   debe ser consecutiva) y la generación (dentro de la etiqueta) impiden quitar, repetir o mezclar
//!   registros.
//! * **Caídas**: tras una caída el final del diario puede estar a medias. Se descarta (es lo esperado) y se
//!   cuenta en [`Recovery::torn_bytes`]. Un registro dañado que **no** está en la cola es otra cosa: es un
//!   error ([`StoreError::Corrupt`]) salvo que se abra en modo [`OpenMode::Repair`].
//! * **Qué es «la cola»:** el almacén sincroniza al menos cada [`StoreConfig::max_unsynced`] bytes, así
//!   que todo lo anterior a los últimos [`StoreConfig::tail_window`] bytes del diario es seguro que
//!   estaba en disco. Un daño dentro de esa ventana es indistinguible de una escritura a medias y se
//!   trata como tal: si pasa sin que hubiera una caída, `torn_bytes > 0` es la señal para investigarlo.
//! * **Ancla**: un almacén nuevo escribe de inmediato un checkpoint vacío con su HMAC. Así abrir con la
//!   clave equivocada falla siempre con [`StoreError::WrongKey`] (nunca se confunde con una cola rota y
//!   nunca borra nada).
//!
//! # Lo que no hace
//!
//! * **No cifra**: los datos están en claro en el disco (solo están autenticados).
//! * **Un atacante que borre o devuelva a un estado anterior los archivos** (por ejemplo, restaurar el
//!   diario de ayer) no se detecta: la clave autentica el contenido, no la frescura. Para eso hace falta
//!   un ancla fuera de esa máquina.
//! * Es de **un solo hilo y un solo proceso** (la carpeta se bloquea).

mod medium;

pub use medium::{CrashKind, FsMedium, InstallFault, Loaded, Medium, MemMedium};

use std::collections::BTreeMap;
use std::fmt;
use std::io;

use tsofgw_pack::{ct_eq, HmacKey};

/// Longitud máxima de una clave.
pub const MAX_KEY: usize = 256;
/// Longitud máxima de un valor.
pub const MAX_VALUE: usize = 1 << 20;
/// Tamaño máximo de la carga de un registro (un `Batch` mayor se rechaza).
pub const MAX_RECORD: usize = 4 << 20;

const REC_HEADER: usize = 4 + 8 + 8;
const REC_TAG: usize = 16;
const SNAP_MAGIC: &[u8; 8] = b"TSOFGWSN";
const SNAP_VERSION: u8 = 1;
const SNAP_TAG: usize = 32;
const OP_PUT: u8 = 1;
const OP_DEL: u8 = 2;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    /// Datos dañados que no son una cola rota por una caída.
    Corrupt {
        what: &'static str,
        /// Posición dentro del diario (o del checkpoint) donde se vio el daño.
        offset: u64,
    },
    /// El checkpoint no se autentica con esta clave (clave equivocada o archivo alterado).
    WrongKey,
    /// Una escritura anterior falló: el almacén se niega a seguir hasta reabrirse (el disco y la memoria
    /// podrían no coincidir).
    Poisoned,
    /// Clave, valor o lote demasiado grandes.
    TooLarge(&'static str),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "error de E/S: {e}"),
            StoreError::Corrupt { what, offset } => {
                write!(f, "datos dañados ({what}) en el byte {offset}")
            }
            StoreError::WrongKey => write!(
                f,
                "el checkpoint no se autentica con esta clave (¿clave equivocada o archivo alterado?)"
            ),
            StoreError::Poisoned => write!(
                f,
                "una escritura anterior falló: hay que reabrir el almacén antes de seguir"
            ),
            StoreError::TooLarge(what) => write!(f, "{what} demasiado grande"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(e: io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// Qué hacer con un diario dañado en medio (no en la cola).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenMode {
    /// Descarta la cola rota por una caída; cualquier otro daño es un error.
    #[default]
    Recover,
    /// Además, corta el diario en el primer daño y sigue (se pierde lo que venía después; se informa).
    Repair,
}

#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Bytes finales del diario en los que un registro inválido se toma por una escritura a medias.
    pub tail_window: u64,
    /// Si lo no sincronizado llega a esto, el siguiente `apply` sincroniza antes de seguir.
    pub max_unsynced: u64,
    pub mode: OpenMode,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            tail_window: 8 << 20,
            max_unsynced: 4 << 20,
            mode: OpenMode::Recover,
        }
    }
}

/// Lo que encontró la recuperación al abrir.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    /// El almacén se acaba de crear.
    pub created: bool,
    pub generation: u64,
    pub snapshot_entries: u64,
    /// Registros del diario repetidos y operaciones que contenían.
    pub records: u64,
    pub ops: u64,
    /// Bytes de una cola rota que se descartaron (lo normal tras una caída).
    pub torn_bytes: u64,
    /// Bytes descartados por un daño en medio del diario (solo con [`OpenMode::Repair`]).
    pub repaired_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreStats {
    pub records: u64,
    pub ops: u64,
    pub bytes: u64,
    pub syncs: u64,
    pub checkpoints: u64,
}

/// Un conjunto de operaciones que se aplican (y se escriben) **juntas**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    ops: Vec<Op>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
}

impl Batch {
    pub fn new() -> Batch {
        Batch::default()
    }

    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(Op::Put(key.into(), value.into()));
        self
    }

    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(Op::Del(key.into()));
        self
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn clear(&mut self) {
        self.ops.clear();
    }

    /// Bytes que ocuparía la carga del registro.
    fn payload_len(&self) -> usize {
        varint_len(self.ops.len() as u64)
            + self
                .ops
                .iter()
                .map(|op| match op {
                    Op::Put(k, v) => {
                        1 + varint_len(k.len() as u64)
                            + k.len()
                            + varint_len(v.len() as u64)
                            + v.len()
                    }
                    Op::Del(k) => 1 + varint_len(k.len() as u64) + k.len(),
                })
                .sum::<usize>()
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.payload_len());
        put_varint(&mut out, self.ops.len() as u64);
        for op in &self.ops {
            match op {
                Op::Put(k, v) => {
                    out.push(OP_PUT);
                    put_varint(&mut out, k.len() as u64);
                    out.extend_from_slice(k);
                    put_varint(&mut out, v.len() as u64);
                    out.extend_from_slice(v);
                }
                Op::Del(k) => {
                    out.push(OP_DEL);
                    put_varint(&mut out, k.len() as u64);
                    out.extend_from_slice(k);
                }
            }
        }
        out
    }

    fn decode(payload: &[u8]) -> Option<Batch> {
        let mut r = Cursor::new(payload);
        let n = r.varint()? as usize;
        // Cada operación ocupa al menos 2 bytes: así una cuenta enorme no reserva memoria.
        if n > payload.len() / 2 + 1 {
            return None;
        }
        let mut ops = Vec::with_capacity(n);
        for _ in 0..n {
            match r.u8()? {
                OP_PUT => {
                    let k = r.bytes_var(MAX_KEY)?;
                    let v = r.bytes_var(MAX_VALUE)?;
                    ops.push(Op::Put(k.to_vec(), v.to_vec()));
                }
                OP_DEL => {
                    let k = r.bytes_var(MAX_KEY)?;
                    ops.push(Op::Del(k.to_vec()));
                }
                _ => return None,
            }
        }
        r.is_done().then_some(Batch { ops })
    }
}

// ---------------------------------------------------------------- bytes

fn varint_len(v: u64) -> usize {
    (70 - (v | 1).leading_zeros() as usize) / 7
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cursor { b, pos: 0 }
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.u8()?;
            if shift == 63 && b > 1 {
                return None;
            }
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.b.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn bytes_var(&mut self, max: usize) -> Option<&'a [u8]> {
        let n = self.varint()? as usize;
        if n > max {
            return None;
        }
        self.take(n)
    }
    fn u64le(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn is_done(&self) -> bool {
        self.pos == self.b.len()
    }
}

// ---------------------------------------------------------------- almacén

pub struct Store<M: Medium> {
    medium: M,
    mac: HmacKey,
    cfg: StoreConfig,
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    generation: u64,
    /// Última secuencia aplicada (continúa entre generaciones).
    seq: u64,
    log_len: u64,
    unsynced: u64,
    poisoned: bool,
    stats: StoreStats,
}

/// Un almacén sobre un medio que se elige al ejecutar (archivos o memoria con fallos).
pub type DynStore = Store<Box<dyn Medium>>;

impl<M: Medium> Store<M> {
    /// Abre el almacén: carga el checkpoint, repite el diario y descarta una cola rota.
    ///
    /// `key` es la clave de integridad (derívala de un secreto del operador; véase `derive_key`).
    pub fn open(
        mut medium: M,
        key: &[u8],
        cfg: StoreConfig,
    ) -> Result<(Store<M>, Recovery), StoreError> {
        let mac = HmacKey::new(key);
        let loaded = medium.load()?;
        let mut rec = Recovery::default();
        let Some(snapshot) = loaded.snapshot else {
            if !loaded.log.is_empty() {
                return Err(StoreError::Corrupt {
                    what: "hay diario pero falta el checkpoint",
                    offset: 0,
                });
            }
            // Medio nuevo: se ancla ya con un checkpoint vacío autenticado.
            let mut s = Store {
                medium,
                mac,
                cfg,
                map: BTreeMap::new(),
                generation: loaded.generation,
                seq: 0,
                log_len: 0,
                unsynced: 0,
                poisoned: false,
                stats: StoreStats::default(),
            };
            s.checkpoint()?;
            rec.created = true;
            rec.generation = s.generation;
            return Ok((s, rec));
        };
        let (map, last_seq) = decode_snapshot(&mac, loaded.generation, &snapshot)?;
        rec.generation = loaded.generation;
        rec.snapshot_entries = map.len() as u64;
        let mut s = Store {
            medium,
            mac,
            cfg,
            map,
            generation: loaded.generation,
            seq: last_seq,
            log_len: 0,
            unsynced: 0,
            poisoned: false,
            stats: StoreStats::default(),
        };
        let good = s.replay(&loaded.log, &mut rec)?;
        if good < loaded.log.len() as u64 {
            s.medium.truncate_log(good)?;
        }
        s.log_len = good;
        Ok((s, rec))
    }

    /// Repite los registros de `log`. Devuelve cuántos bytes eran buenos (el resto se descarta).
    fn replay(&mut self, log: &[u8], rec: &mut Recovery) -> Result<u64, StoreError> {
        let mut off = 0usize;
        loop {
            if off == log.len() {
                return Ok(off as u64);
            }
            let rest = log.len() - off;
            let bad = |this: &mut Self, rec: &mut Recovery, what: &'static str, always: bool| {
                // ¿Está dentro de la ventana en la que una escritura a medias es plausible?
                if !always && rest as u64 <= this.cfg.tail_window {
                    rec.torn_bytes = rest as u64;
                    return Ok(off as u64);
                }
                match this.cfg.mode {
                    OpenMode::Repair => {
                        rec.repaired_bytes = rest as u64;
                        Ok(off as u64)
                    }
                    OpenMode::Recover => Err(StoreError::Corrupt {
                        what,
                        offset: off as u64,
                    }),
                }
            };
            if rest < REC_HEADER {
                return bad(self, rec, "cabecera de registro incompleta", false);
            }
            let len = u32::from_le_bytes(log[off..off + 4].try_into().unwrap()) as usize;
            let seq = u64::from_le_bytes(log[off + 4..off + 12].try_into().unwrap());
            let htag = &log[off + 12..off + 20];
            if !ct_eq(htag, &self.header_tag(len as u32, seq)) {
                return bad(self, rec, "cabecera de registro inválida", false);
            }
            if len > MAX_RECORD {
                return bad(self, rec, "registro demasiado grande", true);
            }
            let total = REC_HEADER + len + REC_TAG;
            if rest < total {
                return bad(self, rec, "registro incompleto", false);
            }
            let payload = &log[off + REC_HEADER..off + REC_HEADER + len];
            let tag = &log[off + REC_HEADER + len..off + total];
            if !ct_eq(tag, &self.record_tag(len as u32, seq, payload)) {
                return bad(self, rec, "registro con etiqueta inválida", false);
            }
            if seq != self.seq + 1 {
                return bad(self, rec, "secuencia fuera de orden", true);
            }
            let Some(batch) = Batch::decode(payload) else {
                return bad(self, rec, "carga ilegible en un registro auténtico", true);
            };
            self.apply_to_map(&batch);
            self.seq = seq;
            rec.records += 1;
            rec.ops += batch.len() as u64;
            off += total;
        }
    }

    fn header_tag(&self, len: u32, seq: u64) -> [u8; 8] {
        let m = self.mac.mac(&[
            b"tsofgw/store/hdr/v1",
            &self.generation.to_le_bytes(),
            &len.to_le_bytes(),
            &seq.to_le_bytes(),
        ]);
        m[..8].try_into().unwrap()
    }

    fn record_tag(&self, len: u32, seq: u64, payload: &[u8]) -> [u8; 16] {
        let m = self.mac.mac(&[
            b"tsofgw/store/rec/v1",
            &self.generation.to_le_bytes(),
            &len.to_le_bytes(),
            &seq.to_le_bytes(),
            payload,
        ]);
        m[..16].try_into().unwrap()
    }

    fn apply_to_map(&mut self, batch: &Batch) {
        for op in &batch.ops {
            match op {
                Op::Put(k, v) => {
                    self.map.insert(k.clone(), v.clone());
                }
                Op::Del(k) => {
                    self.map.remove(k);
                }
            }
        }
    }

    // ------------------------------------------------------------ lectura

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.map.get(key).map(|v| v.as_slice())
    }

    pub fn contains(&self, key: &[u8]) -> bool {
        self.map.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Entradas cuya clave empieza por `prefix`, en orden de clave.
    pub fn scan<'a>(&'a self, prefix: &'a [u8]) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + 'a {
        self.map
            .range::<[u8], _>((
                std::ops::Bound::Included(prefix),
                std::ops::Bound::Unbounded,
            ))
            .take_while(move |(k, _)| k.starts_with(prefix))
            .map(|(k, v)| (k.as_slice(), v.as_slice()))
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn log_len(&self) -> u64 {
        self.log_len
    }

    /// Bytes del diario que aún no se han sincronizado.
    pub fn unsynced(&self) -> u64 {
        self.unsynced
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn stats(&self) -> StoreStats {
        self.stats
    }

    /// Devuelve el medio (para reabrir en pruebas).
    pub fn into_medium(self) -> M {
        self.medium
    }

    // ------------------------------------------------------------ escritura

    /// Aplica un lote como **un solo registro**. No es durable hasta [`Store::commit`] (aunque se
    /// sincroniza solo si lo no sincronizado supera `max_unsynced`).
    pub fn apply(&mut self, batch: &Batch) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if batch.is_empty() {
            return Ok(());
        }
        for op in &batch.ops {
            match op {
                Op::Put(k, v) => {
                    if k.is_empty() || k.len() > MAX_KEY {
                        return Err(StoreError::TooLarge("clave"));
                    }
                    if v.len() > MAX_VALUE {
                        return Err(StoreError::TooLarge("valor"));
                    }
                }
                Op::Del(k) => {
                    if k.is_empty() || k.len() > MAX_KEY {
                        return Err(StoreError::TooLarge("clave"));
                    }
                }
            }
        }
        if batch.payload_len() > MAX_RECORD {
            return Err(StoreError::TooLarge("lote"));
        }
        let payload = batch.encode();
        let seq = self.seq + 1;
        let len = payload.len() as u32;
        let mut rec = Vec::with_capacity(REC_HEADER + payload.len() + REC_TAG);
        rec.extend_from_slice(&len.to_le_bytes());
        rec.extend_from_slice(&seq.to_le_bytes());
        rec.extend_from_slice(&self.header_tag(len, seq));
        rec.extend_from_slice(&payload);
        rec.extend_from_slice(&self.record_tag(len, seq, &payload));
        if let Err(e) = self.medium.append(&rec) {
            self.poisoned = true;
            return Err(e.into());
        }
        self.apply_to_map(batch);
        self.seq = seq;
        self.log_len += rec.len() as u64;
        self.unsynced += rec.len() as u64;
        self.stats.records += 1;
        self.stats.ops += batch.len() as u64;
        self.stats.bytes += rec.len() as u64;
        if self.unsynced >= self.cfg.max_unsynced {
            self.commit()?;
        }
        Ok(())
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), StoreError> {
        let mut b = Batch::new();
        b.put(key, value);
        self.apply(&b)
    }

    pub fn delete(&mut self, key: &[u8]) -> Result<(), StoreError> {
        let mut b = Batch::new();
        b.delete(key);
        self.apply(&b)
    }

    /// Hace durable todo lo escrito. Si falla, el almacén queda inutilizable hasta reabrirse: lo
    /// escrito podría estar o no en disco, y seguir adelante sin saberlo rompería las garantías.
    pub fn commit(&mut self) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        if self.unsynced == 0 {
            return Ok(());
        }
        if let Err(e) = self.medium.sync() {
            self.poisoned = true;
            return Err(e.into());
        }
        self.unsynced = 0;
        self.stats.syncs += 1;
        Ok(())
    }

    /// Guarda el mapa entero como una generación nueva y empieza un diario vacío. Es atómico: tras una
    /// caída vale el estado anterior o el nuevo, que son equivalentes.
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        if self.poisoned {
            return Err(StoreError::Poisoned);
        }
        // Lo escrito antes tiene que estar en el diario viejo por si el checkpoint no se completa.
        self.commit()?;
        let next = self.generation + 1;
        let bytes = encode_snapshot(&self.mac, next, self.seq, &self.map);
        match self.medium.install_checkpoint(next, &bytes) {
            Ok(()) => {
                self.generation = next;
                self.log_len = 0;
                self.unsynced = 0;
                self.stats.checkpoints += 1;
                Ok(())
            }
            Err(e) => {
                self.poisoned = true;
                Err(e.into())
            }
        }
    }

    /// Hace un checkpoint si el diario ya pasa de `max_log` bytes (compactación).
    pub fn checkpoint_if_larger(&mut self, max_log: u64) -> Result<bool, StoreError> {
        if self.log_len >= max_log {
            self.checkpoint()?;
            return Ok(true);
        }
        Ok(false)
    }
}

fn snapshot_tag(mac: &HmacKey, body: &[u8]) -> [u8; 32] {
    mac.mac(&[b"tsofgw/store/snap/v1", body])
}

fn encode_snapshot(
    mac: &HmacKey,
    generation: u64,
    last_seq: u64,
    map: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SNAP_MAGIC);
    out.push(SNAP_VERSION);
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&last_seq.to_le_bytes());
    out.extend_from_slice(&(map.len() as u64).to_le_bytes());
    for (k, v) in map {
        put_varint(&mut out, k.len() as u64);
        out.extend_from_slice(k);
        put_varint(&mut out, v.len() as u64);
        out.extend_from_slice(v);
    }
    let tag = snapshot_tag(mac, &out);
    out.extend_from_slice(&tag);
    out
}

#[allow(clippy::type_complexity)]
fn decode_snapshot(
    mac: &HmacKey,
    generation: u64,
    bytes: &[u8],
) -> Result<(BTreeMap<Vec<u8>, Vec<u8>>, u64), StoreError> {
    const FIXED: usize = 8 + 1 + 8 + 8 + 8;
    if bytes.len() < FIXED + SNAP_TAG {
        return Err(StoreError::Corrupt {
            what: "checkpoint incompleto",
            offset: 0,
        });
    }
    if &bytes[..8] != SNAP_MAGIC {
        return Err(StoreError::Corrupt {
            what: "no es un checkpoint de TSOFGW",
            offset: 0,
        });
    }
    let (body, tag) = bytes.split_at(bytes.len() - SNAP_TAG);
    if !ct_eq(tag, &snapshot_tag(mac, body)) {
        return Err(StoreError::WrongKey);
    }
    if body[8] != SNAP_VERSION {
        return Err(StoreError::Corrupt {
            what: "versión de checkpoint desconocida",
            offset: 8,
        });
    }
    let mut r = Cursor::new(&body[9..]);
    let bad = |what| StoreError::Corrupt { what, offset: 9 };
    let g = r.u64le().ok_or_else(|| bad("checkpoint truncado"))?;
    if g != generation {
        return Err(bad(
            "la generación del checkpoint no coincide con su archivo",
        ));
    }
    let last_seq = r.u64le().ok_or_else(|| bad("checkpoint truncado"))?;
    let count = r.u64le().ok_or_else(|| bad("checkpoint truncado"))?;
    let mut map = BTreeMap::new();
    for _ in 0..count {
        let k = r
            .bytes_var(MAX_KEY)
            .ok_or_else(|| bad("entrada ilegible"))?
            .to_vec();
        let v = r
            .bytes_var(MAX_VALUE)
            .ok_or_else(|| bad("entrada ilegible"))?
            .to_vec();
        map.insert(k, v);
    }
    if !r.is_done() {
        return Err(bad("bytes sobrantes tras las entradas"));
    }
    Ok((map, last_seq))
}

/// Deriva una clave de 32 bytes para un uso concreto a partir de un secreto (HMAC con etiqueta).
pub fn derive_key(secret: &[u8], label: &str) -> [u8; 32] {
    tsofgw_pack::hmac_sha256(secret, format!("tsofgw/store-key/v1/{label}").as_bytes())
}

#[cfg(test)]
mod tests;
