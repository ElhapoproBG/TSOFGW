//! Capa de red de TSOFGW, independiente del transporte (UDP, WebTransport, etc.).
//!
//! * Codificación binaria compacta (varint + zigzag) con errores de decodificación explícitos.
//! * Snapshots completos y **delta** contra una línea base confirmada por el cliente.
//! * Cabecera con número de secuencia y confirmaciones (`ack` + máscara de 32 bits).
//! * Predicción de cliente con reconciliación.

use std::collections::{HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    VarintOverflow,
    BaselineMismatch,
    BadReference,
    Trailing,
}

// ---------------------------------------------------------------- bytes

#[derive(Default)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8 & 0x7F) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    pub fn zigzag(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
}

/// Bytes que ocupa `v` como varint (1–10), sin codificarlo.
pub fn varint_len(v: u64) -> usize {
    (64 - (v | 1).leading_zeros() as usize).div_ceil(7)
}

/// Bytes que ocupa `v` como zigzag + varint, sin codificarlo.
pub fn zigzag_len(v: i64) -> usize {
    varint_len(((v << 1) ^ (v >> 63)) as u64)
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        let b = *self.buf.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }
    pub fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.u8()?;
            if shift == 63 && b > 1 {
                return Err(DecodeError::VarintOverflow);
            }
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError::VarintOverflow)
    }
    pub fn zigzag(&mut self) -> Result<i64, DecodeError> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }
    pub fn finish(self) -> Result<(), DecodeError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(DecodeError::Trailing)
        }
    }
}

// ------------------------------------------------------------ snapshots

/// Posiciones cuantizadas a 1/64 de unidad (±33 millones de unidades de rango).
pub fn quantize(v: f32) -> i32 {
    (v * 64.0).round() as i32
}
pub fn dequantize(q: i32) -> f32 {
    q as f32 / 64.0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityState {
    pub pos: [i32; 3],
    pub data: u16,
}

/// Entidades de un snapshot: un vector **ordenado por id** (sin repetidos).
///
/// Ofrece la misma interfaz de uso que un `BTreeMap<u64, EntityState>` (`get`, `insert`, `iter`,
/// `keys`, índice…), pero con una sola reserva de memoria y recorrido secuencial: construir, comparar y
/// codificar un snapshot de cientos de entidades cuesta una fracción de lo que costaba con un árbol.
/// Las operaciones que insertan o quitan en medio son O(n); el servidor no las usa en el camino
/// caliente (construye el vector ya ordenado con [`EntityMap::from_vec`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityMap {
    items: Vec<(u64, EntityState)>,
}

impl EntityMap {
    pub fn new() -> Self {
        EntityMap::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        EntityMap {
            items: Vec::with_capacity(n),
        }
    }

    /// Construye a partir de pares cualesquiera: si ya vienen estrictamente ordenados por id (el caso del
    /// servidor) solo se comprueba; si no, se ordenan y, ante ids repetidos, gana el último.
    pub fn from_vec(mut items: Vec<(u64, EntityState)>) -> Self {
        if !items.windows(2).all(|w| w[0].0 < w[1].0) {
            // Orden estable: entre repetidos se conserva el orden de llegada y se queda el último.
            items.sort_by_key(|e| e.0);
            let mut out: Vec<(u64, EntityState)> = Vec::with_capacity(items.len());
            for e in items {
                match out.last_mut() {
                    Some(last) if last.0 == e.0 => *last = e,
                    _ => out.push(e),
                }
            }
            items = out;
        }
        EntityMap { items }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Los pares `(id, estado)` en orden de id.
    pub fn as_slice(&self) -> &[(u64, EntityState)] {
        &self.items
    }

    fn search(&self, id: u64) -> Result<usize, usize> {
        self.items.binary_search_by_key(&id, |e| e.0)
    }

    pub fn get(&self, id: &u64) -> Option<&EntityState> {
        self.search(*id).ok().map(|i| &self.items[i].1)
    }
    pub fn get_mut(&mut self, id: &u64) -> Option<&mut EntityState> {
        match self.search(*id) {
            Ok(i) => Some(&mut self.items[i].1),
            Err(_) => None,
        }
    }
    pub fn contains_key(&self, id: &u64) -> bool {
        self.search(*id).is_ok()
    }

    /// Inserta o reemplaza; devuelve el estado anterior si lo había.
    pub fn insert(&mut self, id: u64, st: EntityState) -> Option<EntityState> {
        match self.search(id) {
            Ok(i) => Some(std::mem::replace(&mut self.items[i].1, st)),
            Err(i) => {
                self.items.insert(i, (id, st));
                None
            }
        }
    }
    pub fn remove(&mut self, id: &u64) -> Option<EntityState> {
        self.search(*id).ok().map(|i| self.items.remove(i).1)
    }

    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&u64, &EntityState)> + ExactSizeIterator {
        self.items.iter().map(|(k, v)| (k, v))
    }
    pub fn iter_mut(
        &mut self,
    ) -> impl DoubleEndedIterator<Item = (&u64, &mut EntityState)> + ExactSizeIterator {
        self.items.iter_mut().map(|(k, v)| (&*k, v))
    }
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &u64> + ExactSizeIterator {
        self.items.iter().map(|e| &e.0)
    }
}

impl std::ops::Index<&u64> for EntityMap {
    type Output = EntityState;
    fn index(&self, id: &u64) -> &EntityState {
        self.get(id).expect("entidad inexistente en el snapshot")
    }
}

impl FromIterator<(u64, EntityState)> for EntityMap {
    fn from_iter<I: IntoIterator<Item = (u64, EntityState)>>(iter: I) -> Self {
        EntityMap::from_vec(iter.into_iter().collect())
    }
}

impl<'a> IntoIterator for &'a EntityMap {
    type Item = (&'a u64, &'a EntityState);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (u64, EntityState)>,
        fn(&'a (u64, EntityState)) -> (&'a u64, &'a EntityState),
    >;
    fn into_iter(self) -> Self::IntoIter {
        fn split(e: &(u64, EntityState)) -> (&u64, &EntityState) {
            (&e.0, &e.1)
        }
        self.items.iter().map(split as fn(_) -> _)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub tick: u32,
    pub entities: EntityMap,
}

const F_X: u8 = 1;
const F_Y: u8 = 2;
const F_Z: u8 = 4;
const F_DATA: u8 = 8;
const F_NEW: u8 = 16;

/// Lee una secuencia de ids codificados como diferencias; exige que sean estrictamente crecientes
/// (un salto de 0 tras el primero sería un id repetido) y que no desborden.
struct IdCursor {
    id: u64,
    first: bool,
}

impl IdCursor {
    fn new() -> Self {
        IdCursor { id: 0, first: true }
    }
    fn next(&mut self, r: &mut Reader<'_>) -> Result<u64, DecodeError> {
        let gap = r.varint()?;
        if !self.first && gap == 0 {
            return Err(DecodeError::BadReference);
        }
        self.first = false;
        self.id = self.id.checked_add(gap).ok_or(DecodeError::BadReference)?;
        Ok(self.id)
    }
}

impl Snapshot {
    pub fn encode_full(&self) -> Vec<u8> {
        let mut w = Writer::default();
        w.buf.reserve(8 + self.entities.len() * 8);
        w.varint(self.tick as u64);
        w.varint(self.entities.len() as u64);
        let mut prev = 0u64;
        for (id, e) in self.entities.as_slice() {
            w.varint(id - prev);
            prev = *id;
            for c in e.pos {
                w.zigzag(c as i64);
            }
            w.varint(e.data as u64);
        }
        w.buf
    }

    pub fn decode_full(bytes: &[u8]) -> Result<Snapshot, DecodeError> {
        let mut r = Reader::new(bytes);
        let tick = r.varint()? as u32;
        let n = r.varint()?;
        // Cada entidad ocupa al menos 5 bytes: un `n` absurdo no debe reservar memoria absurda.
        let mut items = Vec::with_capacity((n as usize).min(bytes.len() / 5 + 1));
        let mut ids = IdCursor::new();
        for _ in 0..n {
            let id = ids.next(&mut r)?;
            let pos = [r.zigzag()? as i32, r.zigzag()? as i32, r.zigzag()? as i32];
            let data = r.varint()? as u16;
            items.push((id, EntityState { pos, data }));
        }
        r.finish()?;
        Ok(Snapshot {
            tick,
            entities: EntityMap { items },
        })
    }

    /// Codifica solo lo que cambió respecto de `baseline` (que el cliente ya confirmó haber recibido).
    ///
    /// Recorre las dos listas ordenadas a la vez (una sola pasada, sin búsquedas por id).
    pub fn encode_delta(&self, baseline: &Snapshot) -> Vec<u8> {
        let cur = self.entities.as_slice();
        let base = baseline.entities.as_slice();
        let mut removed: Vec<u64> = Vec::new();
        #[allow(clippy::type_complexity)]
        let mut changes: Vec<(u64, u8, &EntityState, Option<&EntityState>)> =
            Vec::with_capacity(cur.len() / 2 + 8);
        let (mut i, mut j) = (0, 0);
        while i < cur.len() || j < base.len() {
            match (cur.get(i), base.get(j)) {
                (Some((ia, ea)), None) => {
                    changes.push((*ia, F_NEW, ea, None));
                    i += 1;
                }
                (None, Some((ib, _))) => {
                    removed.push(*ib);
                    j += 1;
                }
                (Some((ia, ea)), Some((ib, eb))) => match ia.cmp(ib) {
                    std::cmp::Ordering::Less => {
                        changes.push((*ia, F_NEW, ea, None));
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        removed.push(*ib);
                        j += 1;
                    }
                    std::cmp::Ordering::Equal => {
                        if ea != eb {
                            let mut f = 0;
                            if ea.pos[0] != eb.pos[0] {
                                f |= F_X;
                            }
                            if ea.pos[1] != eb.pos[1] {
                                f |= F_Y;
                            }
                            if ea.pos[2] != eb.pos[2] {
                                f |= F_Z;
                            }
                            if ea.data != eb.data {
                                f |= F_DATA;
                            }
                            changes.push((*ia, f, ea, Some(eb)));
                        }
                        i += 1;
                        j += 1;
                    }
                },
                (None, None) => break,
            }
        }
        self.write_delta(baseline.tick, &removed, &changes)
    }

    #[allow(clippy::type_complexity)]
    fn write_delta(
        &self,
        base_tick: u32,
        removed: &[u64],
        changes: &[(u64, u8, &EntityState, Option<&EntityState>)],
    ) -> Vec<u8> {
        let mut w = Writer::default();
        w.buf.reserve(16 + removed.len() * 3 + changes.len() * 8);
        w.varint(self.tick as u64);
        w.varint(base_tick as u64);
        w.varint(removed.len() as u64);
        let mut prev = 0u64;
        for &id in removed {
            w.varint(id - prev);
            prev = id;
        }
        w.varint(changes.len() as u64);
        let mut prev = 0u64;
        for &(id, f, e, b) in changes {
            w.varint(id - prev);
            prev = id;
            w.u8(f);
            if let Some(b) = b {
                for (bit, i) in [(F_X, 0), (F_Y, 1), (F_Z, 2)] {
                    if f & bit != 0 {
                        w.zigzag(e.pos[i] as i64 - b.pos[i] as i64);
                    }
                }
                if f & F_DATA != 0 {
                    w.varint(e.data as u64);
                }
            } else {
                for c in e.pos {
                    w.zigzag(c as i64);
                }
                w.varint(e.data as u64);
            }
        }
        w.buf
    }

    /// Reconstruye el snapshot a partir de un delta y de la línea base sobre la que se calculó.
    ///
    /// Es estricto con los datos que no cuadran: ids repetidos o desordenados, quitar o modificar algo
    /// que la línea base no tiene, o «crear» algo que ya existe, son errores (el cliente no confirma ese
    /// paquete y el servidor se repliega solo a un snapshot completo).
    pub fn decode_delta(bytes: &[u8], baseline: &Snapshot) -> Result<Snapshot, DecodeError> {
        let mut r = Reader::new(bytes);
        let tick = r.varint()? as u32;
        let base_tick = r.varint()? as u32;
        if base_tick != baseline.tick {
            return Err(DecodeError::BaselineMismatch);
        }
        let base = baseline.entities.as_slice();

        // 1. Se quitan los eliminados.
        let n_removed = r.varint()?;
        let mut kept: Vec<(u64, EntityState)> = Vec::with_capacity(base.len());
        let mut bi = 0usize;
        let mut ids = IdCursor::new();
        for _ in 0..n_removed {
            let id = ids.next(&mut r)?;
            while bi < base.len() && base[bi].0 < id {
                kept.push(base[bi].clone());
                bi += 1;
            }
            if bi >= base.len() || base[bi].0 != id {
                return Err(DecodeError::BadReference);
            }
            bi += 1;
        }
        kept.extend_from_slice(&base[bi..]);

        // 2. Se aplican los cambios sobre lo que queda.
        let n_changed = r.varint()?;
        let mut out: Vec<(u64, EntityState)> =
            Vec::with_capacity(kept.len() + (n_changed as usize).min(bytes.len()));
        let mut ki = 0usize;
        let mut ids = IdCursor::new();
        for _ in 0..n_changed {
            let id = ids.next(&mut r)?;
            while ki < kept.len() && kept[ki].0 < id {
                out.push(kept[ki].clone());
                ki += 1;
            }
            let f = r.u8()?;
            let exists = ki < kept.len() && kept[ki].0 == id;
            if f & F_NEW != 0 {
                if exists {
                    return Err(DecodeError::BadReference);
                }
                let pos = [r.zigzag()? as i32, r.zigzag()? as i32, r.zigzag()? as i32];
                let data = r.varint()? as u16;
                out.push((id, EntityState { pos, data }));
            } else {
                if !exists {
                    return Err(DecodeError::BadReference);
                }
                let mut e = kept[ki].1.clone();
                ki += 1;
                for (bit, i) in [(F_X, 0), (F_Y, 1), (F_Z, 2)] {
                    if f & bit != 0 {
                        e.pos[i] = (e.pos[i] as i64 + r.zigzag()?) as i32;
                    }
                }
                if f & F_DATA != 0 {
                    e.data = r.varint()? as u16;
                }
                out.push((id, e));
            }
        }
        out.extend_from_slice(&kept[ki..]);
        r.finish()?;
        Ok(Snapshot {
            tick,
            entities: EntityMap { items: out },
        })
    }
}

// ----------------------------------------------------------- seq / acks

/// Cabecera de cada paquete: su número, el último recibido del otro lado y una máscara con los 32 anteriores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub seq: u16,
    pub ack: u16,
    pub ack_bits: u32,
}

impl Header {
    pub const SIZE: usize = 8;
    pub fn encode(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&self.seq.to_le_bytes());
        b[2..4].copy_from_slice(&self.ack.to_le_bytes());
        b[4..8].copy_from_slice(&self.ack_bits.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Result<Header, DecodeError> {
        if b.len() < Self::SIZE {
            return Err(DecodeError::Truncated);
        }
        Ok(Header {
            seq: u16::from_le_bytes([b[0], b[1]]),
            ack: u16::from_le_bytes([b[2], b[3]]),
            ack_bits: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        })
    }
}

/// `a` es más reciente que `b`, con vuelta de contador (u16).
pub fn seq_newer(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

/// Cuántos paquetes hacia atrás se siguen esperando confirmar.
const IN_FLIGHT_WINDOW: u16 = 64;

/// Un extremo de la conexión: numera lo que envía y recuerda qué recibió del otro.
pub struct Endpoint {
    local_seq: u16,
    remote_seq: u16,
    received: u32, // bit i = se recibió remote_seq - 1 - i
    have_remote: bool,
    in_flight: HashSet<u16>,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::with_start(0)
    }
}

impl Endpoint {
    pub fn with_start(seq: u16) -> Self {
        Endpoint {
            local_seq: seq,
            remote_seq: 0,
            received: 0,
            have_remote: false,
            in_flight: HashSet::new(),
        }
    }

    /// Cabecera para el siguiente paquete a enviar.
    pub fn next_header(&mut self) -> Header {
        let h = Header {
            seq: self.local_seq,
            ack: self.remote_seq,
            ack_bits: self.received,
        };
        self.in_flight.insert(self.local_seq);
        self.local_seq = self.local_seq.wrapping_add(1);
        // Un paquete más de 64 puestos atrás ya no puede confirmarse (el ack cubre 33): se da por perdido.
        // Sin esto, con pérdida sostenida el conjunto crecería hasta 65 536 entradas por conexión.
        let newest = self.local_seq;
        self.in_flight
            .retain(|&s| newest.wrapping_sub(s) <= IN_FLIGHT_WINDOW);
        h
    }

    /// Procesa la cabecera de un paquete recibido. Devuelve los paquetes nuestros que quedaron confirmados (ordenados).
    pub fn on_receive(&mut self, h: &Header) -> Vec<u16> {
        if !self.have_remote {
            self.have_remote = true;
            self.remote_seq = h.seq;
            self.received = 0;
        } else if seq_newer(h.seq, self.remote_seq) {
            let shift = h.seq.wrapping_sub(self.remote_seq) as u32;
            self.received = if shift >= 32 {
                0
            } else {
                self.received << shift
            };
            if shift <= 32 {
                self.received |= 1u32.checked_shl(shift - 1).unwrap_or(0);
            }
            self.remote_seq = h.seq;
        } else {
            let diff = self.remote_seq.wrapping_sub(h.seq) as u32;
            if (1..=32).contains(&diff) {
                self.received |= 1 << (diff - 1);
            }
        }

        let mut acked = Vec::new();
        let mut take = |s: u16, in_flight: &mut HashSet<u16>| {
            if in_flight.remove(&s) {
                acked.push(s);
            }
        };
        take(h.ack, &mut self.in_flight);
        for i in 0..32u16 {
            if h.ack_bits >> i & 1 == 1 {
                take(h.ack.wrapping_sub(1 + i), &mut self.in_flight);
            }
        }
        acked.sort_unstable_by(|a, b| {
            if seq_newer(*a, *b) {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        });
        acked
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
}

// ------------------------------------------------------------ predicción

/// Simulación determinista compartida por cliente y servidor.
pub trait Sim {
    type State: Clone;
    type Input: Clone;
    fn step(state: &Self::State, input: &Self::Input) -> Self::State;
}

/// Predicción de cliente: aplica los inputs de inmediato y, cuando llega el estado autoritativo
/// del servidor, descarta los ya procesados y vuelve a aplicar los pendientes sobre ese estado.
pub struct Predictor<S: Sim> {
    pub state: S::State,
    pending: VecDeque<(u32, S::Input)>,
}

impl<S: Sim> Predictor<S> {
    pub fn new(state: S::State) -> Self {
        Predictor {
            state,
            pending: VecDeque::new(),
        }
    }

    pub fn apply_local(&mut self, seq: u32, input: S::Input) {
        self.state = S::step(&self.state, &input);
        self.pending.push_back((seq, input));
    }

    /// `server_state` incluye el efecto de todos los inputs con `seq <= last_processed`.
    pub fn reconcile(&mut self, server_state: S::State, last_processed: u32) {
        while self
            .pending
            .front()
            .is_some_and(|(s, _)| *s <= last_processed)
        {
            self.pending.pop_front();
        }
        let mut st = server_state;
        for (_, input) in &self.pending {
            st = S::step(&st, input);
        }
        self.state = st;
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Inputs aplicados localmente que el servidor aún no ha confirmado, del más viejo al más nuevo.
    pub fn pending_inputs(&self) -> impl Iterator<Item = &(u32, S::Input)> {
        self.pending.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn aleatorio(rng: &mut Rng, tick: u32, n: u64) -> Snapshot {
        let mut s = Snapshot {
            tick,
            entities: EntityMap::new(),
        };
        for _ in 0..n {
            let id = rng.below(5000);
            let c = |r: &mut Rng| r.below(2_000_000) as i32 - 1_000_000;
            s.entities.insert(
                id,
                EntityState {
                    pos: [c(rng), c(rng), c(rng)],
                    data: rng.below(65536) as u16,
                },
            );
        }
        s
    }

    #[test]
    fn las_longitudes_calculadas_coinciden_con_las_codificadas() {
        let mut vals: Vec<u64> = vec![0, 1, 127, 128, 16_383, 16_384, u32::MAX as u64, u64::MAX];
        for sh in 0..64 {
            vals.extend([1u64 << sh, (1u64 << sh).wrapping_sub(1), (1u64 << sh) + 1]);
        }
        for v in vals {
            let mut w = Writer::default();
            w.varint(v);
            assert_eq!(varint_len(v), w.buf.len(), "varint {v}");
            for z in [v as i64, (v as i64).wrapping_neg(), (v >> 1) as i64] {
                let mut w = Writer::default();
                w.zigzag(z);
                assert_eq!(zigzag_len(z), w.buf.len(), "zigzag {z}");
            }
        }
        let mut w = Writer::default();
        w.zigzag(i64::MIN);
        assert_eq!(zigzag_len(i64::MIN), w.buf.len());
    }

    #[test]
    fn varint_y_zigzag_ida_y_vuelta() {
        let mut w = Writer::default();
        let vals = [
            0u64,
            1,
            127,
            128,
            300,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX,
        ];
        for v in vals {
            w.varint(v);
        }
        let zs = [0i64, -1, 1, -64, 64, i64::MIN, i64::MAX];
        for z in zs {
            w.zigzag(z);
        }
        let mut r = Reader::new(&w.buf);
        for v in vals {
            assert_eq!(r.varint().unwrap(), v);
        }
        for z in zs {
            assert_eq!(r.zigzag().unwrap(), z);
        }
        assert_eq!(r.finish(), Ok(()));
    }

    #[test]
    fn decodificacion_rechaza_basura() {
        assert_eq!(Reader::new(&[0x80]).varint(), Err(DecodeError::Truncated));
        assert_eq!(
            Reader::new(&[0xFF; 11]).varint(),
            Err(DecodeError::VarintOverflow)
        );
        assert_eq!(Snapshot::decode_full(&[1, 5]), Err(DecodeError::Truncated));
        let mut ok = Snapshot::default().encode_full();
        ok.push(0);
        assert_eq!(Snapshot::decode_full(&ok), Err(DecodeError::Trailing));
        assert_eq!(Header::decode(&[1, 2, 3]), Err(DecodeError::Truncated));
    }

    #[test]
    fn cuantizacion_con_error_acotado() {
        for v in [-1000.37f32, -0.01, 0.0, 0.5, 123.456, 9999.99] {
            assert!((dequantize(quantize(v)) - v).abs() <= 1.0 / 128.0 + 1e-3);
        }
    }

    #[test]
    fn snapshot_completo_ida_y_vuelta() {
        let mut rng = Rng(11);
        for k in 0..100 {
            let n = rng.below(80);
            let s = aleatorio(&mut rng, k, n);
            assert_eq!(Snapshot::decode_full(&s.encode_full()).unwrap(), s);
        }
    }

    #[test]
    fn delta_ida_y_vuelta_con_altas_bajas_y_cambios() {
        let mut rng = Rng(42);
        for k in 0..200 {
            let n = rng.below(100);
            let base = aleatorio(&mut rng, k, n);
            let mut nuevo = base.clone();
            nuevo.tick = k + 3;
            // bajas
            let ids: Vec<u64> = nuevo.entities.keys().copied().collect();
            for id in ids {
                match rng.below(5) {
                    0 => {
                        nuevo.entities.remove(&id);
                    }
                    1 => {
                        nuevo.entities.get_mut(&id).unwrap().pos[(rng.below(3)) as usize] +=
                            rng.below(100) as i32 - 50
                    }
                    2 => nuevo.entities.get_mut(&id).unwrap().data ^= 0x55,
                    _ => {}
                }
            }
            // altas
            for _ in 0..rng.below(10) {
                nuevo.entities.insert(
                    5000 + rng.below(100),
                    EntityState {
                        pos: [1, 2, 3],
                        data: 9,
                    },
                );
            }
            let bytes = nuevo.encode_delta(&base);
            assert_eq!(
                Snapshot::decode_delta(&bytes, &base).unwrap(),
                nuevo,
                "iteración {k}"
            );
        }
    }

    #[test]
    fn delta_es_mucho_mas_pequeno_que_completo() {
        let mut rng = Rng(5);
        let base = aleatorio(&mut rng, 10, 1000);
        let mut nuevo = base.clone();
        nuevo.tick = 11;
        // solo se mueven 50 entidades un poco
        for (_, e) in nuevo.entities.iter_mut().take(50) {
            e.pos[0] += 3;
        }
        let completo = nuevo.encode_full().len();
        let delta = nuevo.encode_delta(&base).len();
        assert!(
            delta * 10 < completo,
            "delta {delta} vs completo {completo}"
        );
        // sin cambios: casi nada
        let mut igual = base.clone();
        igual.tick = 11;
        assert!(igual.encode_delta(&base).len() <= 6);
    }

    #[test]
    fn delta_con_linea_base_equivocada() {
        let mut rng = Rng(8);
        let base = aleatorio(&mut rng, 10, 20);
        let mut nuevo = base.clone();
        nuevo.tick = 12;
        let bytes = nuevo.encode_delta(&base);
        let mut otra = base.clone();
        otra.tick = 9;
        assert_eq!(
            Snapshot::decode_delta(&bytes, &otra),
            Err(DecodeError::BaselineMismatch)
        );
        // referencia a entidad que la línea base no tiene
        let mut sin = base.clone();
        let id = *sin.entities.keys().next().unwrap();
        sin.entities.remove(&id);
        let mut mov = base.clone();
        mov.tick = 12;
        mov.entities.get_mut(&id).unwrap().data ^= 1;
        let bytes = mov.encode_delta(&base);
        assert_eq!(
            Snapshot::decode_delta(&bytes, &sin),
            Err(DecodeError::BadReference)
        );
    }

    #[test]
    fn cabecera_ida_y_vuelta() {
        let h = Header {
            seq: 65535,
            ack: 3,
            ack_bits: 0xDEAD_BEEF,
        };
        assert_eq!(Header::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn comparacion_de_secuencias_con_vuelta() {
        assert!(seq_newer(1, 0));
        assert!(!seq_newer(0, 1));
        assert!(seq_newer(0, 65535));
        assert!(!seq_newer(65535, 0));
        assert!(!seq_newer(7, 7));
    }

    #[test]
    fn confirmaciones_con_perdida() {
        let (mut a, mut b) = (Endpoint::default(), Endpoint::default());
        let perdidos = [3u16, 6];
        for _ in 0..10 {
            let h = a.next_header();
            if !perdidos.contains(&h.seq) {
                b.on_receive(&h);
            }
        }
        let respuesta = b.next_header();
        let acked = a.on_receive(&respuesta);
        assert_eq!(acked, vec![0, 1, 2, 4, 5, 7, 8, 9]);
        assert_eq!(a.in_flight(), 2); // 3 y 6 siguen sin confirmar
    }

    #[test]
    fn confirmaciones_con_vuelta_de_contador() {
        let (mut a, mut b) = (Endpoint::with_start(65530), Endpoint::default());
        for _ in 0..12 {
            let h = a.next_header();
            b.on_receive(&h);
        }
        let acked = a.on_receive(&b.next_header());
        assert_eq!(acked.len(), 12);
        assert_eq!(acked[0], 65530);
        assert_eq!(*acked.last().unwrap(), 5);
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn paquetes_desordenados_y_duplicados() {
        let (mut a, mut b) = (Endpoint::default(), Endpoint::default());
        let hs: Vec<Header> = (0..5).map(|_| a.next_header()).collect();
        for i in [0usize, 2, 1, 4, 4, 3] {
            b.on_receive(&hs[i]);
        }
        let acked = a.on_receive(&b.next_header());
        assert_eq!(acked, vec![0, 1, 2, 3, 4]);
    }

    /// Mundo de ejemplo: avanza `input` unidades, pero hay un muro en 100 que el cliente no conoce.
    struct Mov;
    impl Sim for Mov {
        type State = i32;
        type Input = i32;
        fn step(s: &i32, i: &i32) -> i32 {
            (s + i).min(100)
        }
    }
    struct MovCliente;
    impl Sim for MovCliente {
        type State = i32;
        type Input = i32;
        fn step(s: &i32, i: &i32) -> i32 {
            s + i // el cliente no sabe del muro hasta que el servidor lo corrija
        }
    }

    #[test]
    fn reconciliacion_corrige_prediccion() {
        let mut cliente = Predictor::<MovCliente>::new(0);
        let mut servidor = 0;
        let mut ultimo_servidor = 0;
        for seq in 1..=30u32 {
            cliente.apply_local(seq, 10);
            if seq <= 20 {
                // el servidor va 10 inputs por detrás
                if seq <= 20 {
                    servidor = Mov::step(&servidor, &10);
                    ultimo_servidor = seq;
                }
            }
        }
        assert_eq!(cliente.state, 300); // predicción sin muro
        assert_eq!(servidor, 100);
        cliente.reconcile(servidor, ultimo_servidor);
        assert_eq!(cliente.pending(), 10);
        // el cliente reaplica 10 inputs sobre el estado autoritativo
        assert_eq!(cliente.state, 100 + 100);
        // cuando el servidor procesa el resto, ambos coinciden con la autoridad
        for _ in 21..=30 {
            servidor = Mov::step(&servidor, &10);
        }
        cliente.reconcile(servidor, 30);
        assert_eq!(cliente.state, servidor);
        assert_eq!(cliente.pending(), 0);
    }

    #[test]
    fn paquetes_perdidos_no_se_acumulan_para_siempre() {
        let mut a = Endpoint::default();
        // Se envían 100 000 paquetes y ninguno se confirma nunca (pérdida total del camino de vuelta).
        for _ in 0..100_000u32 {
            a.next_header();
            assert!(a.in_flight() <= IN_FLIGHT_WINDOW as usize + 1);
        }
    }

    impl Snapshot {
        fn encode_delta_referencia(&self, baseline: &Snapshot) -> Vec<u8> {
            let mut w = Writer::default();
            w.varint(self.tick as u64);
            w.varint(baseline.tick as u64);

            let removed: Vec<u64> = baseline
                .entities
                .keys()
                .filter(|id| !self.entities.contains_key(id))
                .copied()
                .collect();
            w.varint(removed.len() as u64);
            let mut prev = 0u64;
            for id in removed {
                w.varint(id - prev);
                prev = id;
            }

            let mut changes = Vec::new();
            for (&id, e) in &self.entities {
                match baseline.entities.get(&id) {
                    None => changes.push((id, F_NEW, e, None)),
                    Some(b) if b != e => {
                        let mut f = 0;
                        if e.pos[0] != b.pos[0] {
                            f |= F_X;
                        }
                        if e.pos[1] != b.pos[1] {
                            f |= F_Y;
                        }
                        if e.pos[2] != b.pos[2] {
                            f |= F_Z;
                        }
                        if e.data != b.data {
                            f |= F_DATA;
                        }
                        changes.push((id, f, e, Some(b)));
                    }
                    _ => {}
                }
            }
            w.varint(changes.len() as u64);
            let mut prev = 0u64;
            for (id, f, e, b) in changes {
                w.varint(id - prev);
                prev = id;
                w.u8(f);
                if let Some(b) = b {
                    for (bit, i) in [(F_X, 0), (F_Y, 1), (F_Z, 2)] {
                        if f & bit != 0 {
                            w.zigzag(e.pos[i] as i64 - b.pos[i] as i64);
                        }
                    }
                    if f & F_DATA != 0 {
                        w.varint(e.data as u64);
                    }
                } else {
                    for c in e.pos {
                        w.zigzag(c as i64);
                    }
                    w.varint(e.data as u64);
                }
            }
            w.buf
        }
    }

    #[test]
    fn encode_delta_por_fusion_da_los_mismos_bytes_que_la_version_simple() {
        let mut rng = 12345u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for _ in 0..300 {
            let mut base = Snapshot {
                tick: 7,
                entities: EntityMap::new(),
            };
            let mut cur = Snapshot {
                tick: 9,
                entities: EntityMap::new(),
            };
            for id in 0..(next() % 60) {
                let id = id * 3 + next() % 3; // ids dispersos
                let e = EntityState {
                    pos: [
                        (next() % 2000) as i32 - 1000,
                        (next() % 50) as i32,
                        (next() % 2000) as i32 - 1000,
                    ],
                    data: (next() % 3) as u16,
                };
                match next() % 5 {
                    0 => {
                        base.entities.insert(id, e.clone());
                    } // desaparece
                    1 => {
                        cur.entities.insert(id, e.clone());
                    } // aparece
                    2 => {
                        base.entities.insert(id, e.clone());
                        cur.entities.insert(id, e);
                    } // igual
                    _ => {
                        base.entities.insert(id, e.clone());
                        let mut m = e;
                        if next() % 2 == 0 {
                            m.pos[0] += (next() % 9) as i32 - 4;
                        }
                        if next() % 3 == 0 {
                            m.pos[2] -= 1;
                        }
                        if next() % 4 == 0 {
                            m.data += 1;
                        }
                        cur.entities.insert(id, m);
                    }
                }
            }
            assert_eq!(cur.encode_delta(&base), cur.encode_delta_referencia(&base));
        }
    }

    // ---- EntityMap y decodificador estricto

    fn es(x: i32) -> EntityState {
        EntityState {
            pos: [x, 0, 0],
            data: 0,
        }
    }

    #[test]
    fn entity_map_se_comporta_como_un_mapa_ordenado() {
        use std::collections::BTreeMap;
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut m = EntityMap::new();
        let mut reference: BTreeMap<u64, EntityState> = BTreeMap::new();
        for step in 0..4000 {
            let id = rng.below(300);
            match rng.below(4) {
                0 | 1 => {
                    let st = es(rng.below(1000) as i32);
                    assert_eq!(m.insert(id, st.clone()), reference.insert(id, st));
                }
                2 => assert_eq!(m.remove(&id), reference.remove(&id)),
                _ => {
                    assert_eq!(m.get(&id), reference.get(&id));
                    assert_eq!(m.contains_key(&id), reference.contains_key(&id));
                    if let Some(e) = m.get_mut(&id) {
                        e.data ^= 1;
                        reference.get_mut(&id).unwrap().data ^= 1;
                    }
                }
            }
            if step % 97 == 0 {
                assert_eq!(m.len(), reference.len());
                let a: Vec<_> = m.iter().map(|(k, v)| (*k, v.clone())).collect();
                let b: Vec<_> = reference.iter().map(|(k, v)| (*k, v.clone())).collect();
                assert_eq!(a, b);
                assert_eq!(
                    m.keys().copied().collect::<Vec<_>>(),
                    reference.keys().copied().collect::<Vec<_>>()
                );
            }
        }
        for (id, v) in &m {
            assert_eq!(&m[id], v);
        }
    }

    #[test]
    fn from_vec_ordena_y_en_repetidos_gana_el_ultimo() {
        let sorted = vec![(1, es(1)), (5, es(5)), (9, es(9))];
        assert_eq!(EntityMap::from_vec(sorted.clone()).as_slice(), &sorted[..]);
        let messy = vec![(9, es(9)), (1, es(1)), (5, es(50)), (5, es(5)), (1, es(11))];
        let m = EntityMap::from_vec(messy);
        assert_eq!(m.as_slice(), &[(1, es(11)), (5, es(5)), (9, es(9))][..]);
        let c: EntityMap = [(3u64, es(3)), (2, es(2))].into_iter().collect();
        assert_eq!(c.keys().copied().collect::<Vec<_>>(), vec![2, 3]);
    }

    fn raw_delta(base_tick: u32, removed: &[u64], changes: &[(u64, u8, Vec<i64>)]) -> Vec<u8> {
        let mut w = Writer::default();
        w.varint(9);
        w.varint(base_tick as u64);
        w.varint(removed.len() as u64);
        let mut prev = 0;
        for &id in removed {
            w.varint(id - prev);
            prev = id;
        }
        w.varint(changes.len() as u64);
        let mut prev = 0;
        for (id, f, vals) in changes {
            w.varint(id - prev);
            prev = *id;
            w.u8(*f);
            for v in vals {
                w.zigzag(*v);
            }
        }
        w.buf
    }

    #[test]
    fn el_decodificador_rechaza_deltas_incoherentes() {
        let base = Snapshot {
            tick: 4,
            entities: [(10u64, es(1)), (20, es(2))].into_iter().collect(),
        };
        // Referencia: un delta correcto sí se acepta (cambia X de 10, entra la 30 y sale la 20).
        let ok = raw_delta(
            4,
            &[20],
            &[(10, F_X, vec![5]), (30, F_NEW, vec![7, 0, 0, 0])],
        );
        let got = Snapshot::decode_delta(&ok, &base).unwrap();
        assert_eq!(got.entities.as_slice(), &[(10, es(6)), (30, es(7))][..]);
        // Quitar algo que no existe.
        assert!(Snapshot::decode_delta(&raw_delta(4, &[15], &[]), &base).is_err());
        // Ids repetidos al quitar, o desbordados.
        assert!(Snapshot::decode_delta(&raw_delta(4, &[10, 10], &[]), &base).is_err());
        // «Crear» algo que ya existe.
        let dup_new = raw_delta(4, &[], &[(10, F_NEW, vec![1, 0, 0, 0])]);
        assert!(Snapshot::decode_delta(&dup_new, &base).is_err());
        // Modificar algo que no existe.
        assert!(Snapshot::decode_delta(&raw_delta(4, &[], &[(11, F_X, vec![1])]), &base).is_err());
        // La misma entidad dos veces en la lista de cambios.
        let twice = raw_delta(4, &[], &[(10, F_X, vec![1]), (10, F_X, vec![1])]);
        assert!(Snapshot::decode_delta(&twice, &base).is_err());
        // Línea base equivocada.
        assert_eq!(
            Snapshot::decode_delta(&raw_delta(3, &[], &[]), &base),
            Err(DecodeError::BaselineMismatch)
        );
        // Un salto de id que desborda u64 no entra en pánico: es un error.
        let mut w = Writer::default();
        w.varint(9);
        w.varint(4);
        w.varint(0);
        w.varint(2);
        w.varint(u64::MAX);
        w.u8(F_NEW);
        for _ in 0..4 {
            w.zigzag(0);
        }
        w.varint(1);
        w.u8(F_NEW);
        for _ in 0..4 {
            w.zigzag(0);
        }
        assert!(Snapshot::decode_delta(&w.buf, &base).is_err());
    }

    #[test]
    fn decode_full_rechaza_ids_repetidos_y_cantidades_absurdas() {
        let mut w = Writer::default();
        w.varint(1); // tick
        w.varint(2); // n
        for gap in [5u64, 0] {
            w.varint(gap);
            for _ in 0..3 {
                w.zigzag(0);
            }
            w.varint(0);
        }
        assert!(Snapshot::decode_full(&w.buf).is_err(), "id repetido");
        // Una cantidad enorme con pocos bytes: error, sin reservar memoria descomunal.
        let mut w = Writer::default();
        w.varint(1);
        w.varint(1 << 50);
        assert!(Snapshot::decode_full(&w.buf).is_err());
        // Y un snapshot normal con el id 0 sigue siendo válido.
        let s = Snapshot {
            tick: 3,
            entities: [(0u64, es(1)), (4, es(2))].into_iter().collect(),
        };
        assert_eq!(Snapshot::decode_full(&s.encode_full()).unwrap(), s);
    }
}
