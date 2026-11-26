//! Directorio **replicado** (0.8.0): varias réplicas del directorio de sesiones puestas de acuerdo con Raft
//! (véase [`crate::raft`]).
//!
//! El estado (qué cuenta vive en qué shard y con qué época) es el mismo de siempre; lo que cambia es **cómo se
//! decide**. Cada petición de un shard (`DirLogin`, `DirTransfer`) se convierte en un **mandato** que el líder
//! añade al diario; cuando una mayoría de réplicas lo ha guardado, **todas** lo aplican en el mismo orden, y el
//! líder (solo él, y solo para lo que él propuso) contesta al shard. Como el resultado de cada mandato depende
//! únicamente del estado y del mandato, todas las réplicas llegan a lo mismo.
//!
//! * El que decide el cambio de dueño de una cuenta ya no es un proceso, sino **la mayoría**: con 3 réplicas se
//!   tolera la caída de 1 sin parar a nadie; con 5, la de 2.
//! * Una réplica que no lidera contesta `DirRedirect` con quién cree que lidera, o calla si no lo sabe; el shard
//!   reintenta y, si varias peticiones seguidas no reciben nada, prueba con otra réplica.
//! * Los reintentos de los shards ya eran seguros (las operaciones son idempotentes y por época); el líder además
//!   no vuelve a proponer lo que ya tiene en vuelo.
//! * **Lo primero al disco:** todo lo que Raft pide guardar se sincroniza antes de enviar ningún mensaje ni
//!   contestar a ningún shard.
//!
//! # Formato en el almacén
//! `m/repl` = `[réplicas, id]` (una carpeta no se reutiliza con otra identidad); `r/meta` = término y voto;
//! `r/base` = último índice compactado y su término; `r/applied` = hasta dónde se aplicó (se escribe en el mismo lote
//! que los cambios de cuentas que provoca); `r/e/<índice>` = entradas del diario; `a/<cuenta>` = el estado, igual
//! que sin réplicas.
//!
//! # Límites
//! Pertenencia fija (no se añaden ni quitan réplicas en caliente); todo pasa por el diario (también las lecturas); la
//! instantánea para una réplica atrasada se envía entera y se copia entera en memoria en el líder (18 bytes por
//! cuenta); y no se puede abrir como replicado una carpeta creada sin réplicas (ni al revés).

use std::collections::{HashMap, HashSet};

use tsofgw_store::Batch;

use super::{decode, encode, key, Directory, P_ACCOUNT};
use crate::bus::{DirStatus, PeerMsg};
use crate::partition::ShardId;
use crate::raft::{Boot, Entry, Event, Msg, Persist, Raft, RaftConfig};
use crate::server::NPC_BASE;

pub(super) const K_REPL: &[u8] = b"m/repl";
pub(super) const K_META: &[u8] = b"r/meta";
pub(super) const K_BASE: &[u8] = b"r/base";
pub(super) const K_APPLIED: &[u8] = b"r/applied";
pub(super) const P_ENTRY: &[u8] = b"r/e/";

fn k_entry(index: u64) -> Vec<u8> {
    let mut k = P_ENTRY.to_vec();
    k.extend_from_slice(&index.to_be_bytes());
    k
}

fn entry_index(k: &[u8]) -> Option<u64> {
    let b: [u8; 8] = k.strip_prefix(P_ENTRY)?.try_into().ok()?;
    Some(u64::from_be_bytes(b))
}

/// Mandatos que se replican.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cmd {
    Login {
        from: ShardId,
        account: u64,
        home: ShardId,
    },
    Transfer {
        from: ShardId,
        xfer: u32,
        account: u64,
        epoch: u64,
        to: ShardId,
    },
    /// Carga de una reconstrucción: registra la cuenta si no existe o si trae una época mayor.
    Seed {
        account: u64,
        shard: ShardId,
        epoch: u64,
    },
}

impl Cmd {
    pub(super) fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(25);
        match self {
            Cmd::Login {
                from,
                account,
                home,
            } => {
                v.push(1);
                v.extend_from_slice(&from.to_le_bytes());
                v.extend_from_slice(&account.to_le_bytes());
                v.extend_from_slice(&home.to_le_bytes());
            }
            Cmd::Transfer {
                from,
                xfer,
                account,
                epoch,
                to,
            } => {
                v.push(2);
                v.extend_from_slice(&from.to_le_bytes());
                v.extend_from_slice(&xfer.to_le_bytes());
                v.extend_from_slice(&account.to_le_bytes());
                v.extend_from_slice(&epoch.to_le_bytes());
                v.extend_from_slice(&to.to_le_bytes());
            }
            Cmd::Seed {
                account,
                shard,
                epoch,
            } => {
                v.push(3);
                v.extend_from_slice(&account.to_le_bytes());
                v.extend_from_slice(&shard.to_le_bytes());
                v.extend_from_slice(&epoch.to_le_bytes());
            }
        }
        v
    }

    pub(super) fn decode(b: &[u8]) -> Option<Cmd> {
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        match (b.first()?, b.len()) {
            (1, 13) => Some(Cmd::Login {
                from: u16_at(1),
                account: u64_at(3),
                home: u16_at(11),
            }),
            (2, 25) => Some(Cmd::Transfer {
                from: u16_at(1),
                xfer: u32_at(3),
                account: u64_at(7),
                epoch: u64_at(15),
                to: u16_at(23),
            }),
            (3, 19) => Some(Cmd::Seed {
                account: u64_at(1),
                shard: u16_at(9),
                epoch: u64_at(11),
            }),
            _ => None,
        }
    }
}

/// Estado de la parte replicada de un directorio.
pub(super) struct Repl {
    pub raft: Raft,
    pub id: usize,
    pub n: usize,
    pub applied: u64,
    /// Índices que este líder propuso en su liderazgo actual: solo de esos contesta a los shards.
    pub pending: HashSet<u64>,
    /// Mandatos en vuelo (para no proponer dos veces lo mismo mientras no se confirme).
    pub inflight: HashMap<Vec<u8>, u64>,
    /// Cuentas por sembrar (reconstrucción), que el líder va proponiendo.
    pub seed_queue: std::collections::VecDeque<(u64, ShardId, u64)>,
    /// Ya se alineó el reloj de Raft con el del primer `update`.
    pub clock_set: bool,
}

fn enc_meta(term: u64, voted: Option<usize>) -> Vec<u8> {
    let mut v = term.to_le_bytes().to_vec();
    v.push(voted.map_or(0, |i| i as u8 + 1));
    v
}

fn dec_meta(b: &[u8]) -> Option<(u64, Option<usize>)> {
    let b: [u8; 9] = b.try_into().ok()?;
    let term = u64::from_le_bytes(b[..8].try_into().ok()?);
    Some((term, (b[8] != 0).then(|| b[8] as usize - 1)))
}

fn enc_pair(a: u64, b: u64) -> Vec<u8> {
    let mut v = a.to_le_bytes().to_vec();
    v.extend_from_slice(&b.to_le_bytes());
    v
}

fn dec_pair(b: &[u8]) -> Option<(u64, u64)> {
    let b: [u8; 16] = b.try_into().ok()?;
    Some((
        u64::from_le_bytes(b[..8].try_into().ok()?),
        u64::from_le_bytes(b[8..].try_into().ok()?),
    ))
}

fn enc_entry(e: &Entry) -> Vec<u8> {
    let mut v = e.term.to_le_bytes().to_vec();
    v.extend_from_slice(&e.data);
    v
}

fn dec_entry(b: &[u8]) -> Option<Entry> {
    if b.len() < 8 || b.len() - 8 > crate::raft::MAX_ENTRY_DATA {
        return None;
    }
    let term = u64::from_le_bytes(b[..8].try_into().ok()?);
    (term > 0).then(|| Entry {
        term,
        data: b[8..].to_vec(),
    })
}

/// Una cuenta en una instantánea: cuenta (8) + shard (2) + época (8).
const SNAP_REC: usize = 18;

impl Directory {
    /// Lee de la carpeta el estado de Raft y arma la parte replicada. Valida todo.
    pub(super) fn load_repl(
        store: &mut tsofgw_store::DynStore,
        replicas: usize,
        replica: usize,
        shards: usize,
        raft_cfg: RaftConfig,
        now_ms: u64,
    ) -> Result<Repl, String> {
        if !(3..=7).contains(&replicas) || replicas.is_multiple_of(2) || replica >= replicas {
            return Err("las réplicas deben ser 3, 5 o 7 y la identidad estar entre ellas".into());
        }
        let io = |e: tsofgw_store::StoreError| format!("almacén del directorio: {e}");
        let ident = [replicas as u8, replica as u8];
        match store.get(K_REPL) {
            None => {
                if store.scan(P_ACCOUNT).next().is_some() {
                    return Err(
                        "esta carpeta es de un directorio sin réplicas (0.5 – 0.7): no se puede abrir como una réplica; \
                         reconstruye el directorio desde los shards"
                            .into(),
                    );
                }
                let mut b = Batch::new();
                b.put(K_REPL, ident.to_vec());
                store.apply(&b).map_err(io)?;
                store.commit().map_err(io)?;
            }
            Some(v) => {
                if v != ident {
                    return Err(format!(
                        "esta carpeta es de la réplica {} de {} y se quiere abrir como la {} de {}",
                        v.get(1).copied().unwrap_or(0),
                        v.first().copied().unwrap_or(0),
                        replica,
                        replicas
                    ));
                }
            }
        }
        let bad = |what: &str| format!("almacén del directorio dañado: {what}");
        let (term, voted) = match store.get(K_META) {
            Some(v) => dec_meta(v).ok_or_else(|| bad("término y voto"))?,
            None => (0, None),
        };
        if voted.is_some_and(|v| v >= replicas) {
            return Err(bad("voto fuera de las réplicas"));
        }
        let (base_index, base_term) = match store.get(K_BASE) {
            Some(v) => dec_pair(v).ok_or_else(|| bad("base del diario"))?,
            None => (0, 0),
        };
        let applied = match store.get(K_APPLIED) {
            Some(v) => <[u8; 8]>::try_from(v)
                .map(u64::from_le_bytes)
                .map_err(|_| bad("índice aplicado"))?,
            None => 0,
        };
        let mut log = Vec::new();
        for (expected, (k, v)) in (base_index + 1..).zip(store.scan(P_ENTRY)) {
            let idx = entry_index(k).ok_or_else(|| bad("clave de entrada"))?;
            if idx != expected {
                return Err(bad("diario con huecos"));
            }
            log.push(dec_entry(v).ok_or_else(|| bad("entrada"))?);
        }
        if applied < base_index || applied > base_index + log.len() as u64 {
            return Err(bad("índice aplicado fuera del diario"));
        }
        for (k, v) in store.scan(P_ACCOUNT) {
            let ok = k.len() == P_ACCOUNT.len() + 8
                && decode(v).is_some_and(|(s, _)| (s as usize) < shards);
            if !ok {
                return Err(bad("registro de cuenta"));
            }
        }
        let boot = Boot {
            term,
            voted_for: voted,
            base_index,
            base_term,
            log,
            applied,
        };
        Ok(Repl {
            raft: Raft::new(replica, replicas, raft_cfg, boot, now_ms),
            id: replica,
            n: replicas,
            applied,
            pending: HashSet::new(),
            inflight: HashMap::new(),
            seed_queue: Default::default(),
            clock_set: false,
        })
    }

    /// Identificador de bus de la réplica `i`.
    fn replica_bus_id(&self, i: usize) -> ShardId {
        (self.shards + i) as ShardId
    }

    /// ¿Qué réplica (0-based) es `bus_id`?
    pub(super) fn replica_index(&self, bus_id: ShardId) -> Option<usize> {
        let r = self.repl.as_ref()?;
        let i = (bus_id as usize).checked_sub(self.shards)?;
        (i < r.n).then_some(i)
    }

    /// Mensaje de otra réplica.
    pub(super) fn on_raft_msg(&mut self, now_ms: u64, from: ShardId, bytes: &[u8]) {
        let Some(i) = self.replica_index(from) else {
            self.stats.malformed += 1;
            return;
        };
        let Some(msg) = Msg::decode(bytes) else {
            self.stats.malformed += 1;
            return;
        };
        if let Some(r) = self.repl.as_mut() {
            r.raft.step(now_ms, i, msg);
        }
    }

    /// Petición de un shard en modo replicado.
    pub(super) fn on_request_replicated(&mut self, from: ShardId, m: PeerMsg) {
        let cmd = match m {
            PeerMsg::DirLogin { account, home } => {
                if account >= NPC_BASE || (home as usize) >= self.shards {
                    self.stats.malformed += 1;
                    return;
                }
                self.stats.logins += 1;
                Cmd::Login {
                    from,
                    account,
                    home,
                }
            }
            PeerMsg::DirTransfer {
                xfer,
                account,
                epoch,
                to,
            } => {
                if (to as usize) >= self.shards || to == from || account >= NPC_BASE {
                    self.stats.malformed += 1;
                    return;
                }
                self.stats.transfers += 1;
                Cmd::Transfer {
                    from,
                    xfer,
                    account,
                    epoch,
                    to,
                }
            }
            _ => {
                self.stats.malformed += 1;
                return;
            }
        };
        let leader = self.repl.as_ref().and_then(|r| r.raft.leader());
        let me = self.repl.as_ref().map_or(0, |r| r.id);
        match leader {
            Some(l) if l == me => {
                let data = cmd.encode();
                let r = self.repl.as_mut().unwrap();
                if r.inflight.contains_key(&data) {
                    self.stats.inflight_dups += 1;
                    return;
                }
                if let Some(idx) = r.raft.propose(data.clone()) {
                    r.inflight.insert(data, idx);
                    r.pending.insert(idx);
                    self.stats.proposed += 1;
                }
            }
            Some(l) => {
                self.stats.redirects += 1;
                let to = from;
                let msg = PeerMsg::DirRedirect {
                    leader: self.replica_bus_id(l),
                };
                self.deferred.push((to, msg));
            }
            // Sin líder conocido: se calla, y el shard probará con otra réplica.
            None => self.stats.no_leader += 1,
        }
    }

    /// Resultado de un mandato: aplica sus escrituras al lote y devuelve la respuesta para el shard que lo pidió.
    pub(super) fn exec(&mut self, cmd: &Cmd, b: &mut Batch) -> Option<(ShardId, PeerMsg)> {
        match *cmd {
            Cmd::Login {
                from,
                account,
                home,
            } => {
                let (shard, epoch) = match self.lookup(account) {
                    Some(r) => r,
                    None => {
                        b.put(key(account), encode(home, 1));
                        self.stats.accounts_created += 1;
                        (home, 1)
                    }
                };
                Some((
                    from,
                    PeerMsg::DirRoute {
                        account,
                        shard,
                        epoch,
                    },
                ))
            }
            Cmd::Transfer {
                from,
                xfer,
                account,
                epoch,
                to,
            } => {
                let reply = |status, shard, epoch| {
                    Some((
                        from,
                        PeerMsg::DirResult {
                            xfer,
                            account,
                            status,
                            shard,
                            epoch,
                        },
                    ))
                };
                match self.lookup(account) {
                    None => {
                        self.stats.unknown += 1;
                        reply(DirStatus::Unknown, 0, 0)
                    }
                    Some((shard, e)) if shard == from && e == epoch => match epoch.checked_add(1) {
                        Some(next) => {
                            b.put(key(account), encode(to, next));
                            self.stats.committed += 1;
                            reply(DirStatus::Committed, to, next)
                        }
                        None => {
                            self.stats.rejected += 1;
                            reply(DirStatus::Rejected, shard, e)
                        }
                    },
                    // La misma petición otra vez (se perdió la respuesta): ya está hecho.
                    Some((shard, e)) if shard == to && Some(e) == epoch.checked_add(1) => {
                        self.stats.idempotent += 1;
                        reply(DirStatus::Committed, shard, e)
                    }
                    Some((shard, e)) => {
                        self.stats.rejected += 1;
                        reply(DirStatus::Rejected, shard, e)
                    }
                }
            }
            Cmd::Seed {
                account,
                shard,
                epoch,
            } => {
                if (shard as usize) < self.shards
                    && epoch >= 1
                    && self.lookup(account).is_none_or(|(_, e)| epoch > e)
                {
                    b.put(key(account), encode(shard, epoch));
                    self.stats.seeded += 1;
                }
                None
            }
        }
    }

    /// Instantánea de todo el estado (aplicado) para una réplica atrasada.
    fn snapshot_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for (k, val) in self.store.scan(P_ACCOUNT) {
            let Some((shard, epoch)) = decode(val) else {
                continue;
            };
            let account = u64::from_be_bytes(k[P_ACCOUNT.len()..].try_into().unwrap());
            v.extend_from_slice(&account.to_le_bytes());
            v.extend_from_slice(&shard.to_le_bytes());
            v.extend_from_slice(&epoch.to_le_bytes());
        }
        v
    }

    fn restore_snapshot(&mut self, index: u64, data: &[u8]) -> Result<(), String> {
        if !data.len().is_multiple_of(SNAP_REC) {
            return Err("instantánea con un tamaño imposible".into());
        }
        let mut b = Batch::new();
        let old: Vec<Vec<u8>> = self
            .store
            .scan(P_ACCOUNT)
            .map(|(k, _)| k.to_vec())
            .collect();
        for k in old {
            b.delete(k);
        }
        for rec in data.chunks_exact(SNAP_REC) {
            let account = u64::from_le_bytes(rec[..8].try_into().unwrap());
            let shard = u16::from_le_bytes(rec[8..10].try_into().unwrap());
            let epoch = u64::from_le_bytes(rec[10..].try_into().unwrap());
            if account >= NPC_BASE || (shard as usize) >= self.shards || epoch == 0 {
                return Err("instantánea con una cuenta imposible".into());
            }
            b.put(key(account), encode(shard, epoch));
        }
        b.put(K_APPLIED, index.to_le_bytes().to_vec());
        self.store
            .apply(&b)
            .map_err(|e| format!("almacén del directorio: {e}"))
    }

    fn apply_persist(&mut self, ops: Vec<Persist>) -> Result<(), String> {
        let io = |e: tsofgw_store::StoreError| format!("almacén del directorio: {e}");
        for op in ops {
            let mut b = Batch::new();
            match op {
                Persist::Meta { term, voted_for } => {
                    b.put(K_META, enc_meta(term, voted_for));
                }
                Persist::Put { index, entry } => {
                    b.put(k_entry(index), enc_entry(&entry));
                }
                Persist::TruncateFrom { index } => {
                    let doomed: Vec<Vec<u8>> = self
                        .store
                        .scan(P_ENTRY)
                        .filter(|(k, _)| entry_index(k).is_some_and(|i| i >= index))
                        .map(|(k, _)| k.to_vec())
                        .collect();
                    for k in doomed {
                        b.delete(k);
                    }
                }
                Persist::Base { index, term } => {
                    let doomed: Vec<Vec<u8>> = self
                        .store
                        .scan(P_ENTRY)
                        .filter(|(k, _)| entry_index(k).is_some_and(|i| i <= index))
                        .map(|(k, _)| k.to_vec())
                        .collect();
                    for k in doomed {
                        b.delete(k);
                    }
                    b.put(K_BASE, enc_pair(index, term));
                }
            }
            if !b.is_empty() {
                self.store.apply(&b).map_err(io)?;
            }
        }
        Ok(())
    }

    /// Un paso completo de la parte replicada: guardar, aplicar lo confirmado, compactar. Después de esto (y de
    /// sincronizar) se puede enviar lo pendiente.
    pub(super) fn repl_process(&mut self) -> Result<(), String> {
        let io = |e: tsofgw_store::StoreError| format!("almacén del directorio: {e}");
        // 0. Siembra: el líder propone las cuentas de una reconstrucción poco a poco.
        if let Some(r) = self.repl.as_mut() {
            if r.raft.is_leader() {
                let mut budget = 32;
                while budget > 0 {
                    let Some(&(account, shard, epoch)) = r.seed_queue.front() else {
                        break;
                    };
                    let data = Cmd::Seed {
                        account,
                        shard,
                        epoch,
                    }
                    .encode();
                    if r.raft.propose(data).is_none() {
                        break;
                    }
                    r.seed_queue.pop_front();
                    budget -= 1;
                }
            }
        }
        // 1. Lo que Raft pide guardar.
        let ops = self.repl.as_mut().unwrap().raft.take_persist();
        self.apply_persist(ops)?;
        // 2. Lo confirmado, en orden.
        let events = self.repl.as_mut().unwrap().raft.take_events();
        for ev in events {
            match ev {
                Event::Committed { index, entry } => {
                    let mut b = Batch::new();
                    let mut reply = None;
                    if !entry.data.is_empty() {
                        match Cmd::decode(&entry.data) {
                            Some(cmd) => reply = self.exec(&cmd, &mut b),
                            None => {
                                // Todas las réplicas aplican lo mismo: un mandato ilegible es un error grave.
                                return Err(format!("mandato ilegible en el índice {index}"));
                            }
                        }
                    }
                    b.put(K_APPLIED, index.to_le_bytes().to_vec());
                    self.store.apply(&b).map_err(io)?;
                    let r = self.repl.as_mut().unwrap();
                    r.applied = index;
                    r.inflight.remove(&entry.data);
                    if r.pending.remove(&index) {
                        if let Some((to, msg)) = reply {
                            self.deferred.push((to, msg));
                        }
                    }
                    self.stats.applied += 1;
                }
                Event::Restore { index, data, .. } => {
                    self.restore_snapshot(index, &data)?;
                    let r = self.repl.as_mut().unwrap();
                    r.applied = index;
                    r.pending.clear();
                    r.inflight.clear();
                    self.stats.restores += 1;
                }
            }
        }
        // 3. Un líder que dejó de serlo ya no contesta lo que propuso.
        {
            let r = self.repl.as_mut().unwrap();
            if !r.raft.is_leader() {
                r.pending.clear();
                r.inflight.clear();
            }
        }
        // 4. Instantánea para réplicas atrasadas y compactación del diario.
        let (wants, applied) = {
            let r = self.repl.as_ref().unwrap();
            (r.raft.wants_snapshot(), r.applied)
        };
        if wants {
            let snap = self.snapshot_bytes();
            self.repl
                .as_mut()
                .unwrap()
                .raft
                .provide_snapshot(applied, snap);
        }
        let compacted = self.repl.as_mut().unwrap().raft.maybe_compact(applied);
        if compacted {
            self.stats.log_compactions += 1;
            let ops = self.repl.as_mut().unwrap().raft.take_persist();
            self.apply_persist(ops)?;
        }
        Ok(())
    }

    /// Mensajes de Raft listos para enviar (solo después de sincronizar).
    pub(super) fn repl_outbox(&mut self) -> Vec<(ShardId, PeerMsg)> {
        let out = self.repl.as_mut().unwrap().raft.take_outbox();
        out.into_iter()
            .map(|(i, m)| (self.replica_bus_id(i), PeerMsg::Raft(m.encode())))
            .collect()
    }

    // ------------------------------------------------------------------------------------------ consultas

    /// ¿Esta réplica es la líder ahora? (Siempre `true` sin réplicas.)
    pub fn is_leader(&self) -> bool {
        self.repl.as_ref().is_none_or(|r| r.raft.is_leader())
    }

    /// El líder que esta réplica conoce, como identificador de bus.
    pub fn leader_peer(&self) -> Option<ShardId> {
        match &self.repl {
            None => Some(self.shards as ShardId),
            Some(r) => r.raft.leader().map(|l| self.replica_bus_id(l)),
        }
    }

    /// Término de Raft (0 sin réplicas).
    pub fn raft_term(&self) -> u64 {
        self.repl.as_ref().map_or(0, |r| r.raft.term())
    }

    /// Hasta qué índice del diario se ha aplicado (0 sin réplicas).
    pub fn applied_index(&self) -> u64 {
        self.repl.as_ref().map_or(0, |r| r.applied)
    }

    pub fn raft_stats(&self) -> Option<crate::raft::RaftStats> {
        self.repl.as_ref().map(|r| r.raft.stats().clone())
    }

    /// Número de entradas que guarda el diario ahora mismo.
    pub fn raft_log_len(&self) -> usize {
        self.repl.as_ref().map_or(0, |r| r.raft.log_len())
    }

    /// Encola cuentas de una reconstrucción para que el líder las proponga (solo en modo replicado y mientras esta
    /// réplica no sea la única fuente de verdad: úsese en **todas** las réplicas con la misma lista y el líder sembrará
    /// una vez; las demás réplicas descartan lo que ya esté).
    pub fn queue_import(&mut self, entries: &[(u64, ShardId, u64)]) -> Result<usize, String> {
        let Some(r) = self.repl.as_mut() else {
            return Err("sin réplicas, usa `import` (solo en un directorio vacío)".into());
        };
        for &(account, shard, epoch) in entries {
            if account >= NPC_BASE || (shard as usize) >= self.shards || epoch == 0 {
                return Err(format!("entrada imposible para la cuenta {account}"));
            }
            r.seed_queue.push_back((account, shard, epoch));
        }
        Ok(entries.len())
    }
}
