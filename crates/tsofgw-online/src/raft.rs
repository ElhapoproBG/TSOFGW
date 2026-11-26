//! Consenso **Raft** sin entrada/salida, para replicar el directorio de sesiones.
//!
//! Este módulo es solo la máquina de estados del protocolo: no abre archivos, no usa sockets ni relojes.
//! Quien lo usa (`Directory`) le da los mensajes recibidos y la hora, y recoge de él tres cosas **en orden**:
//!
//! * [`Raft::take_persist`]: lo que hay que **guardar y sincronizar** antes de hacer nada visible;
//! * [`Raft::take_outbox`]: los mensajes para los demás réplicas, que **solo se envían después** de guardar;
//! * [`Raft::take_events`]: entradas ya confirmadas que hay que aplicar a la máquina de estados, y
//!   restauraciones desde una instantánea.
//!
//! Esa regla («primero al disco, después al mundo») es la que hace seguro el protocolo: una réplica nunca
//! contesta un voto ni confirma una entrada que luego pueda olvidar.
//!
//! # Qué incluye
//! Elección de líder con **pre-votación** (una réplica aislada no sube el término de las demás al volver) y con
//! protección de líder (se ignoran votaciones mientras se oye a uno vivo), comprobación de quórum del líder
//! (un líder que deja de ver a la mayoría abdica), replicación por lotes, confirmación de entradas solo del
//! término actual (§5.4.2 del artículo), compactación del diario y **envío de instantáneas** por trozos a las
//! réplicas que se quedaron atrás de lo compactado.
//!
//! # Qué NO incluye
//! * **Cambios de pertenencia**: el conjunto de réplicas es fijo (3, 5 o 7).
//! * Lecturas locales: todo pasa por el diario (más lento, pero sin lecturas rancias).
//! * Pruebas formales: se comprueba con simulación (véase `tests/raft.rs`), no con un demostrador.
//!
//! Los índices del diario empiezan en 1; el 0 es «antes de todo».

use std::collections::VecDeque;

/// Una entrada del diario: el término en que el líder la creó y un mandato opaco (lo interpreta quien usa Raft).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub term: u64,
    pub data: Vec<u8>,
}

/// Tamaño máximo del mandato de una entrada.
pub const MAX_ENTRY_DATA: usize = 32;
/// Máximo de bytes de instantánea por mensaje.
pub const SNAP_CHUNK: usize = 990;

#[derive(Debug, Clone)]
pub struct RaftConfig {
    /// El plazo de elección es aleatorio entre estos dos valores (ms).
    pub election_min_ms: u64,
    pub election_max_ms: u64,
    pub heartbeat_ms: u64,
    /// Entradas por mensaje de replicación.
    pub max_entries_per_append: usize,
    /// El diario se compacta cuando guarda más de estas entradas aplicadas...
    pub compact_after: u64,
    /// ... dejando al menos estas, para que las réplicas lentas no necesiten instantánea.
    pub keep_tail: u64,
}

impl Default for RaftConfig {
    fn default() -> Self {
        RaftConfig {
            election_min_ms: 300,
            election_max_ms: 600,
            heartbeat_ms: 60,
            max_entries_per_append: 20,
            compact_after: 4096,
            keep_tail: 512,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Follower,
    PreCandidate,
    Candidate,
    Leader,
}

/// Mensajes entre réplicas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    PreVote {
        term: u64,
        last_index: u64,
        last_term: u64,
    },
    PreVoteReply {
        term: u64,
        granted: bool,
    },
    Vote {
        term: u64,
        last_index: u64,
        last_term: u64,
    },
    VoteReply {
        term: u64,
        granted: bool,
    },
    Append {
        term: u64,
        prev_index: u64,
        prev_term: u64,
        commit: u64,
        entries: Vec<Entry>,
    },
    AppendReply {
        term: u64,
        success: bool,
        /// Con éxito: último índice que coincide con el líder. Sin él: dónde probar la próxima vez.
        index: u64,
    },
    SnapChunk {
        term: u64,
        last_index: u64,
        last_term: u64,
        offset: u64,
        data: Vec<u8>,
        done: bool,
    },
    SnapReply {
        term: u64,
        last_index: u64,
        ok: bool,
        next_offset: u64,
        done: bool,
    },
}

// ---------------------------------------------------------------------------------------------------------------
// Codificación (little-endian, longitudes fijas; todo se comprueba al leer)
// ---------------------------------------------------------------------------------------------------------------

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
}

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        match self {
            Msg::PreVote {
                term,
                last_index,
                last_term,
            } => {
                w.u8(1);
                w.u64(*term);
                w.u64(*last_index);
                w.u64(*last_term);
            }
            Msg::PreVoteReply { term, granted } => {
                w.u8(2);
                w.u64(*term);
                w.u8(*granted as u8);
            }
            Msg::Vote {
                term,
                last_index,
                last_term,
            } => {
                w.u8(3);
                w.u64(*term);
                w.u64(*last_index);
                w.u64(*last_term);
            }
            Msg::VoteReply { term, granted } => {
                w.u8(4);
                w.u64(*term);
                w.u8(*granted as u8);
            }
            Msg::Append {
                term,
                prev_index,
                prev_term,
                commit,
                entries,
            } => {
                w.u8(5);
                w.u64(*term);
                w.u64(*prev_index);
                w.u64(*prev_term);
                w.u64(*commit);
                w.u8(entries.len() as u8);
                for e in entries {
                    w.u64(e.term);
                    w.u8(e.data.len() as u8);
                    w.0.extend_from_slice(&e.data);
                }
            }
            Msg::AppendReply {
                term,
                success,
                index,
            } => {
                w.u8(6);
                w.u64(*term);
                w.u8(*success as u8);
                w.u64(*index);
            }
            Msg::SnapChunk {
                term,
                last_index,
                last_term,
                offset,
                data,
                done,
            } => {
                w.u8(7);
                w.u64(*term);
                w.u64(*last_index);
                w.u64(*last_term);
                w.u64(*offset);
                w.u8(*done as u8);
                w.u16(data.len() as u16);
                w.0.extend_from_slice(data);
            }
            Msg::SnapReply {
                term,
                last_index,
                ok,
                next_offset,
                done,
            } => {
                w.u8(8);
                w.u64(*term);
                w.u64(*last_index);
                w.u8(*ok as u8);
                w.u64(*next_offset);
                w.u8(*done as u8);
            }
        }
        w.0
    }

    /// `None` si el mensaje está mal formado (tipo desconocido, truncado, con restos o con valores imposibles).
    pub fn decode(b: &[u8]) -> Option<Msg> {
        let mut r = R(b);
        let m = match r.u8()? {
            1 => Msg::PreVote {
                term: r.u64()?,
                last_index: r.u64()?,
                last_term: r.u64()?,
            },
            2 => Msg::PreVoteReply {
                term: r.u64()?,
                granted: r.bool()?,
            },
            3 => Msg::Vote {
                term: r.u64()?,
                last_index: r.u64()?,
                last_term: r.u64()?,
            },
            4 => Msg::VoteReply {
                term: r.u64()?,
                granted: r.bool()?,
            },
            5 => {
                let term = r.u64()?;
                let prev_index = r.u64()?;
                let prev_term = r.u64()?;
                let commit = r.u64()?;
                let n = r.u8()? as usize;
                let mut entries = Vec::with_capacity(n);
                for _ in 0..n {
                    let term = r.u64()?;
                    let len = r.u8()? as usize;
                    if len > MAX_ENTRY_DATA || term == 0 {
                        return None;
                    }
                    entries.push(Entry {
                        term,
                        data: r.take(len)?.to_vec(),
                    });
                }
                Msg::Append {
                    term,
                    prev_index,
                    prev_term,
                    commit,
                    entries,
                }
            }
            6 => Msg::AppendReply {
                term: r.u64()?,
                success: r.bool()?,
                index: r.u64()?,
            },
            7 => {
                let term = r.u64()?;
                let last_index = r.u64()?;
                let last_term = r.u64()?;
                let offset = r.u64()?;
                let done = r.bool()?;
                let len = r.u16()? as usize;
                if len > SNAP_CHUNK {
                    return None;
                }
                Msg::SnapChunk {
                    term,
                    last_index,
                    last_term,
                    offset,
                    done,
                    data: r.take(len)?.to_vec(),
                }
            }
            8 => Msg::SnapReply {
                term: r.u64()?,
                last_index: r.u64()?,
                ok: r.bool()?,
                next_offset: r.u64()?,
                done: r.bool()?,
            },
            _ => return None,
        };
        if !r.0.is_empty() {
            return None;
        }
        Some(m)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Estado
// ---------------------------------------------------------------------------------------------------------------

/// Lo que hay que guardar, en orden, antes de enviar nada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Persist {
    /// Término actual y voto en él.
    Meta { term: u64, voted_for: Option<usize> },
    /// Escribe la entrada del índice dado (sustituye a la que hubiera).
    Put { index: u64, entry: Entry },
    /// Borra las entradas desde este índice (incluido) hasta el final.
    TruncateFrom { index: u64 },
    /// El diario empieza ahora después de `index`: borra las entradas hasta ese índice (incluido).
    Base { index: u64, term: u64 },
}

/// Lo que hay que hacer con la máquina de estados, en orden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// La entrada está confirmada: aplicarla.
    Committed { index: u64, entry: Entry },
    /// Sustituir **todo** el estado por esta instantánea (que corresponde al diario hasta `index`).
    Restore {
        index: u64,
        term: u64,
        data: Vec<u8>,
    },
}

/// Estado guardado con el que arranca una réplica.
#[derive(Debug, Clone, Default)]
pub struct Boot {
    pub term: u64,
    pub voted_for: Option<usize>,
    pub base_index: u64,
    pub base_term: u64,
    /// Entradas con índices `base_index + 1 ..`.
    pub log: Vec<Entry>,
    /// Hasta dónde se había aplicado a la máquina de estados (todo lo aplicado está confirmado).
    pub applied: u64,
}

#[derive(Debug, Clone)]
struct Snap {
    last_index: u64,
    last_term: u64,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct SnapIn {
    last_index: u64,
    last_term: u64,
    buf: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RaftStats {
    pub elections_started: u64,
    pub elections_won: u64,
    pub stepped_down: u64,
    pub quorum_lost: u64,
    pub appends_sent: u64,
    pub snapshots_sent: u64,
    pub snapshots_installed: u64,
    pub conflicts_truncated: u64,
    pub compactions: u64,
    pub bad_msgs: u64,
}

pub struct Raft {
    id: usize,
    n: usize,
    cfg: RaftConfig,
    term: u64,
    voted_for: Option<usize>,
    base_index: u64,
    base_term: u64,
    log: Vec<Entry>,
    commit: u64,
    delivered: u64,
    role: Role,
    leader: Option<usize>,
    /// Cuándo se oyó por última vez a un líder legítimo (o cuándo se arrancó).
    heard_leader: Option<u64>,
    election_deadline: u64,
    votes: Vec<bool>,
    next_index: Vec<u64>,
    match_index: Vec<u64>,
    last_sent: Vec<u64>,
    last_ack: Vec<u64>,
    dirty: Vec<bool>,
    snap: Option<Snap>,
    snap_out: Vec<Option<(u64, u64)>>, // por réplica: (last_index, offset)
    snap_in: Option<SnapIn>,
    want_snapshot: bool,
    rng: u64,
    outbox: Vec<(usize, Msg)>,
    persist: Vec<Persist>,
    events: VecDeque<Event>,
    stats: RaftStats,
}

impl Raft {
    /// `n` réplicas (1..=7), esta es la `id` (0-based). `now` es la hora en ms.
    pub fn new(id: usize, n: usize, cfg: RaftConfig, boot: Boot, now: u64) -> Raft {
        assert!((1..=7).contains(&n) && id < n, "réplicas fuera de rango");
        assert!(
            cfg.election_min_ms > 2 * cfg.heartbeat_ms
                && cfg.election_max_ms >= cfg.election_min_ms,
            "el plazo de elección debe superar con holgura al latido"
        );
        assert!(boot.applied >= boot.base_index);
        assert!(boot.applied <= boot.base_index + boot.log.len() as u64);
        let mut r = Raft {
            id,
            n,
            cfg,
            term: boot.term,
            voted_for: boot.voted_for,
            base_index: boot.base_index,
            base_term: boot.base_term,
            log: boot.log,
            commit: boot.applied,
            delivered: boot.applied,
            role: Role::Follower,
            leader: None,
            heard_leader: Some(now),
            election_deadline: 0,
            votes: vec![false; n],
            next_index: vec![1; n],
            match_index: vec![0; n],
            last_sent: vec![0; n],
            last_ack: vec![0; n],
            dirty: vec![false; n],
            snap: None,
            snap_out: vec![None; n],
            snap_in: None,
            want_snapshot: false,
            rng: 0x9E37_79B9_7F4A_7C15
                ^ ((id as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03))
                ^ boot.term,
            outbox: Vec::new(),
            persist: Vec::new(),
            events: VecDeque::new(),
            stats: RaftStats::default(),
        };
        r.reset_election(now);
        r
    }

    /// Alinea los temporizadores con el reloj real (el de `new` puede ser otro): se llama una vez, antes del primer
    /// `tick`.
    pub fn restart_timers(&mut self, now: u64) {
        self.heard_leader = Some(now);
        for a in self.last_ack.iter_mut() {
            *a = now;
        }
        self.reset_election(now);
    }

    // ----------------------------------------------------------------------------------------- consultas

    pub fn id(&self) -> usize {
        self.id
    }
    pub fn term(&self) -> u64 {
        self.term
    }
    pub fn role(&self) -> Role {
        self.role
    }
    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }
    /// El líder conocido (puede ser esta réplica).
    pub fn leader(&self) -> Option<usize> {
        self.leader
    }
    pub fn commit_index(&self) -> u64 {
        self.commit
    }
    pub fn last_index(&self) -> u64 {
        self.base_index + self.log.len() as u64
    }
    pub fn base_index(&self) -> u64 {
        self.base_index
    }
    pub fn log_len(&self) -> usize {
        self.log.len()
    }
    pub fn stats(&self) -> &RaftStats {
        &self.stats
    }
    /// Término de la entrada `index`, si se conoce (no compactada y no posterior al final).
    pub fn term_at(&self, index: u64) -> Option<u64> {
        if index == self.base_index {
            Some(self.base_term)
        } else if index > self.base_index && index <= self.last_index() {
            Some(self.log[(index - self.base_index - 1) as usize].term)
        } else {
            None
        }
    }
    pub fn entry(&self, index: u64) -> Option<&Entry> {
        (index > self.base_index && index <= self.last_index())
            .then(|| &self.log[(index - self.base_index - 1) as usize])
    }
    fn last_term(&self) -> u64 {
        self.term_at(self.last_index()).unwrap_or(0)
    }
    fn majority(&self) -> usize {
        self.n / 2 + 1
    }

    // ------------------------------------------------------------------------------------------ salidas

    pub fn take_persist(&mut self) -> Vec<Persist> {
        std::mem::take(&mut self.persist)
    }
    pub fn take_outbox(&mut self) -> Vec<(usize, Msg)> {
        std::mem::take(&mut self.outbox)
    }
    pub fn take_events(&mut self) -> Vec<Event> {
        self.events.drain(..).collect()
    }
    /// `true` si el líder necesita una instantánea del estado para una réplica atrasada: hay que llamar a
    /// [`Raft::provide_snapshot`].
    pub fn wants_snapshot(&self) -> bool {
        self.want_snapshot && self.role == Role::Leader
    }

    /// Entrega la instantánea del estado **aplicado hasta `last_index`** (lo confirmado y aplicado hasta ahí).
    pub fn provide_snapshot(&mut self, last_index: u64, data: Vec<u8>) {
        let Some(last_term) = self.term_at(last_index) else {
            return;
        };
        if last_index < self.base_index {
            return;
        }
        self.snap = Some(Snap {
            last_index,
            last_term,
            data,
        });
        self.want_snapshot = false;
        for d in self.dirty.iter_mut() {
            *d = true;
        }
    }

    // ------------------------------------------------------------------------------------------ utilidades

    fn rand(&mut self) -> u64 {
        // xorshift64*: determinista por réplica (y distinto entre réplicas).
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn reset_election(&mut self, now: u64) {
        let span = self.cfg.election_max_ms - self.cfg.election_min_ms + 1;
        let off = self.rand() % span;
        self.election_deadline = now + self.cfg.election_min_ms + off;
    }

    fn up_to_date(&self, last_index: u64, last_term: u64) -> bool {
        let (mi, mt) = (self.last_index(), self.last_term());
        last_term > mt || (last_term == mt && last_index >= mi)
    }

    fn recently_heard_leader(&self, now: u64) -> bool {
        match (self.leader, self.heard_leader) {
            (Some(_), Some(t)) => now.saturating_sub(t) < self.cfg.election_min_ms,
            _ => false,
        }
    }

    /// ¿Ha contestado la mayoría (contando al líder) hace menos que un plazo mínimo de elección?
    fn quorum_contact(&self, now: u64) -> bool {
        let alive = 1
            + (0..self.n)
                .filter(|&i| {
                    i != self.id && now.saturating_sub(self.last_ack[i]) < self.cfg.election_min_ms
                })
                .count();
        alive >= self.majority()
    }

    fn send(&mut self, to: usize, m: Msg) {
        self.outbox.push((to, m));
    }

    fn become_follower(&mut self, term: u64, leader: Option<usize>, now: u64) {
        if term > self.term {
            self.term = term;
            self.voted_for = None;
            self.persist.push(Persist::Meta {
                term,
                voted_for: None,
            });
        }
        if self.role == Role::Leader {
            self.stats.stepped_down += 1;
        }
        self.role = Role::Follower;
        self.leader = leader;
        if leader.is_some() {
            self.heard_leader = Some(now);
        }
        self.snap_out = vec![None; self.n];
        self.want_snapshot = false;
        self.reset_election(now);
    }

    // --------------------------------------------------------------------------------------------- reloj

    pub fn tick(&mut self, now: u64) {
        match self.role {
            Role::Leader => {
                // Quórum: si la mayoría no contesta desde hace más que un plazo de elección, abdica.
                if self.n > 1 {
                    let mut acks: Vec<u64> = (0..self.n)
                        .filter(|&i| i != self.id)
                        .map(|i| self.last_ack[i])
                        .collect();
                    acks.sort_unstable_by(|a, b| b.cmp(a));
                    // Con el propio líder, hacen falta `majority - 1` ajenos al día.
                    let need = self.majority() - 1;
                    let oldest_needed = acks[need - 1];
                    if now.saturating_sub(oldest_needed) > self.cfg.election_max_ms {
                        self.stats.quorum_lost += 1;
                        self.become_follower(self.term, None, now);
                        return;
                    }
                }
                for f in 0..self.n {
                    if f == self.id {
                        continue;
                    }
                    let due = now.saturating_sub(self.last_sent[f]) >= self.cfg.heartbeat_ms;
                    if due || self.dirty[f] {
                        self.send_append(f, now);
                    }
                }
            }
            Role::Follower | Role::PreCandidate | Role::Candidate => {
                if now >= self.election_deadline {
                    self.start_prevote(now);
                }
            }
        }
    }

    // ------------------------------------------------------------------------------------------ elección

    fn start_prevote(&mut self, now: u64) {
        self.reset_election(now);
        if self.n == 1 {
            self.start_election(now);
            return;
        }
        self.role = Role::PreCandidate;
        self.leader = None;
        self.votes = vec![false; self.n];
        self.votes[self.id] = true;
        let (li, lt) = (self.last_index(), self.last_term());
        for p in 0..self.n {
            if p != self.id {
                self.send(
                    p,
                    Msg::PreVote {
                        term: self.term + 1,
                        last_index: li,
                        last_term: lt,
                    },
                );
            }
        }
    }

    fn start_election(&mut self, now: u64) {
        self.stats.elections_started += 1;
        self.term += 1;
        self.voted_for = Some(self.id);
        self.persist.push(Persist::Meta {
            term: self.term,
            voted_for: self.voted_for,
        });
        self.role = Role::Candidate;
        self.leader = None;
        self.votes = vec![false; self.n];
        self.votes[self.id] = true;
        self.reset_election(now);
        if self.majority() <= 1 {
            self.become_leader(now);
            return;
        }
        let (li, lt) = (self.last_index(), self.last_term());
        for p in 0..self.n {
            if p != self.id {
                self.send(
                    p,
                    Msg::Vote {
                        term: self.term,
                        last_index: li,
                        last_term: lt,
                    },
                );
            }
        }
    }

    fn become_leader(&mut self, now: u64) {
        self.stats.elections_won += 1;
        self.role = Role::Leader;
        self.leader = Some(self.id);
        self.heard_leader = Some(now);
        let next = self.last_index() + 1;
        for i in 0..self.n {
            self.next_index[i] = next;
            self.match_index[i] = 0;
            self.last_sent[i] = 0;
            self.last_ack[i] = now;
            self.dirty[i] = true;
        }
        self.snap_out = vec![None; self.n];
        // Una entrada vacía de este término: al confirmarse, confirma también todo lo anterior.
        self.append_local(Vec::new());
        self.advance_commit();
    }

    fn append_local(&mut self, data: Vec<u8>) -> u64 {
        let entry = Entry {
            term: self.term,
            data,
        };
        self.log.push(entry.clone());
        let index = self.last_index();
        self.persist.push(Persist::Put { index, entry });
        for d in self.dirty.iter_mut() {
            *d = true;
        }
        index
    }

    /// Solo el líder: añade un mandato al diario. Devuelve su índice (aún sin confirmar).
    pub fn propose(&mut self, data: Vec<u8>) -> Option<u64> {
        if self.role != Role::Leader || data.len() > MAX_ENTRY_DATA {
            return None;
        }
        let index = self.append_local(data);
        self.advance_commit();
        Some(index)
    }

    // --------------------------------------------------------------------------------- recepción general

    pub fn step(&mut self, now: u64, from: usize, msg: Msg) {
        if from >= self.n || from == self.id {
            self.stats.bad_msgs += 1;
            return;
        }
        match msg {
            Msg::PreVote {
                term,
                last_index,
                last_term,
            } => {
                let grant = term > self.term
                    && self.role != Role::Leader
                    && !self.recently_heard_leader(now)
                    && self.up_to_date(last_index, last_term);
                self.send(
                    from,
                    Msg::PreVoteReply {
                        term: self.term,
                        granted: grant,
                    },
                );
            }
            Msg::PreVoteReply { term, granted } => {
                if term > self.term {
                    self.become_follower(term, None, now);
                    return;
                }
                if self.role == Role::PreCandidate && granted {
                    self.votes[from] = true;
                    if self.votes.iter().filter(|&&v| v).count() >= self.majority() {
                        self.start_election(now);
                    }
                }
            }
            Msg::Vote {
                term,
                last_index,
                last_term,
            } => self.on_vote(now, from, term, last_index, last_term),
            Msg::VoteReply { term, granted } => {
                if term > self.term {
                    self.become_follower(term, None, now);
                    return;
                }
                if self.role == Role::Candidate && term == self.term && granted {
                    self.votes[from] = true;
                    if self.votes.iter().filter(|&&v| v).count() >= self.majority() {
                        self.become_leader(now);
                    }
                }
            }
            Msg::Append {
                term,
                prev_index,
                prev_term,
                commit,
                entries,
            } => self.on_append(now, from, term, prev_index, prev_term, commit, entries),
            Msg::AppendReply {
                term,
                success,
                index,
            } => self.on_append_reply(now, from, term, success, index),
            Msg::SnapChunk {
                term,
                last_index,
                last_term,
                offset,
                data,
                done,
            } => self.on_snap_chunk(now, from, term, last_index, last_term, offset, data, done),
            Msg::SnapReply {
                term,
                last_index,
                ok,
                next_offset,
                done,
            } => self.on_snap_reply(now, from, term, last_index, ok, next_offset, done),
        }
    }

    fn on_vote(&mut self, now: u64, from: usize, term: u64, li: u64, lt: u64) {
        // Protección de líder: mientras se oye a uno vivo, una votación de un término mayor se ignora (sin
        // actualizar el término), para que una réplica con el reloj raro no tumbe un líder sano.
        if term > self.term {
            let sticky = if self.role == Role::Leader {
                self.quorum_contact(now)
            } else {
                self.recently_heard_leader(now)
            };
            if sticky {
                return;
            }
        }
        if term < self.term {
            self.send(
                from,
                Msg::VoteReply {
                    term: self.term,
                    granted: false,
                },
            );
            return;
        }
        if term > self.term {
            self.become_follower(term, None, now);
        }
        let can_vote = self.voted_for.is_none() || self.voted_for == Some(from);
        let grant = can_vote && self.up_to_date(li, lt);
        if grant {
            self.voted_for = Some(from);
            self.persist.push(Persist::Meta {
                term: self.term,
                voted_for: self.voted_for,
            });
            self.reset_election(now);
        }
        self.send(
            from,
            Msg::VoteReply {
                term: self.term,
                granted: grant,
            },
        );
    }

    // -------------------------------------------------------------------------------------- replicación

    fn send_append(&mut self, f: usize, now: u64) {
        self.dirty[f] = false;
        self.last_sent[f] = now;
        let next = self.next_index[f];
        if next <= self.base_index {
            self.send_snapshot(f);
            return;
        }
        let prev = next - 1;
        let Some(prev_term) = self.term_at(prev) else {
            // No debería pasar (next-1 ≥ base_index); se reinicia el seguimiento.
            self.next_index[f] = self.last_index() + 1;
            return;
        };
        let start = (next - self.base_index - 1) as usize;
        let end = (start + self.cfg.max_entries_per_append).min(self.log.len());
        let entries: Vec<Entry> = self.log[start..end].to_vec();
        self.stats.appends_sent += 1;
        self.send(
            f,
            Msg::Append {
                term: self.term,
                prev_index: prev,
                prev_term,
                commit: self.commit,
                entries,
            },
        );
    }

    fn send_snapshot(&mut self, f: usize) {
        let usable = self
            .snap
            .as_ref()
            .is_some_and(|s| s.last_index >= self.base_index);
        if !usable {
            self.want_snapshot = true;
            return;
        }
        let snap = self.snap.as_ref().unwrap();
        let (li, lt) = (snap.last_index, snap.last_term);
        let off = match self.snap_out[f] {
            Some((i, o)) if i == li => o,
            _ => 0,
        };
        self.snap_out[f] = Some((li, off));
        let total = snap.data.len() as u64;
        let end = (off + SNAP_CHUNK as u64).min(total);
        let data = snap.data[off as usize..end as usize].to_vec();
        self.stats.snapshots_sent += 1;
        let m = Msg::SnapChunk {
            term: self.term,
            last_index: li,
            last_term: lt,
            offset: off,
            data,
            done: end == total,
        };
        self.send(f, m);
    }

    #[allow(clippy::too_many_arguments)]
    fn on_append(
        &mut self,
        now: u64,
        from: usize,
        term: u64,
        prev_index: u64,
        prev_term: u64,
        leader_commit: u64,
        entries: Vec<Entry>,
    ) {
        if term < self.term {
            self.send(
                from,
                Msg::AppendReply {
                    term: self.term,
                    success: false,
                    index: 0,
                },
            );
            return;
        }
        if self.role == Role::Leader && term == self.term {
            // Dos líderes en el mismo término no pueden existir: mensaje corrupto o réplica mal configurada.
            self.stats.bad_msgs += 1;
            return;
        }
        self.become_follower(term, Some(from), now);

        // ¿Coincide el diario en `prev_index`?
        if prev_index > self.last_index() {
            self.send(
                from,
                Msg::AppendReply {
                    term: self.term,
                    success: false,
                    index: self.last_index() + 1,
                },
            );
            return;
        }
        if prev_index >= self.base_index {
            let ours = self.term_at(prev_index).unwrap_or(0);
            if ours != prev_term {
                // Retrocede hasta el primer índice de ese término conflictivo.
                let mut hint = prev_index;
                while hint > self.base_index + 1 && self.term_at(hint - 1) == Some(ours) {
                    hint -= 1;
                }
                // Lo confirmado nunca conflictúa: no se retrocede por debajo.
                let hint = hint.max(self.commit + 1).min(prev_index);
                self.send(
                    from,
                    Msg::AppendReply {
                        term: self.term,
                        success: false,
                        index: hint,
                    },
                );
                return;
            }
        }
        // (si prev_index < base_index, lo anterior está confirmado y coincide por construcción)

        let mut idx = prev_index;
        for e in entries {
            idx += 1;
            if idx <= self.base_index {
                continue;
            }
            match self.term_at(idx) {
                Some(t) if idx <= self.last_index() && t == e.term => continue,
                Some(_) if idx <= self.last_index() => {
                    if idx <= self.commit {
                        // Una entrada confirmada no puede cambiar: algo va muy mal. Se ignora el resto.
                        self.stats.bad_msgs += 1;
                        return;
                    }
                    self.stats.conflicts_truncated += 1;
                    let keep = (idx - self.base_index - 1) as usize;
                    self.log.truncate(keep);
                    self.persist.push(Persist::TruncateFrom { index: idx });
                }
                _ => {}
            }
            self.log.push(e.clone());
            self.persist.push(Persist::Put {
                index: idx,
                entry: e,
            });
        }
        let matched = idx;
        if leader_commit > self.commit {
            let new_commit = leader_commit.min(matched);
            self.set_commit(new_commit);
        }
        self.send(
            from,
            Msg::AppendReply {
                term: self.term,
                success: true,
                index: matched,
            },
        );
    }

    fn on_append_reply(&mut self, now: u64, from: usize, term: u64, success: bool, index: u64) {
        if term > self.term {
            self.become_follower(term, None, now);
            return;
        }
        if self.role != Role::Leader || term != self.term {
            return;
        }
        self.last_ack[from] = now;
        if success {
            let idx = index.min(self.last_index());
            if idx > self.match_index[from] {
                self.match_index[from] = idx;
            }
            self.next_index[from] = self.match_index[from] + 1;
            self.advance_commit();
            if self.next_index[from] <= self.last_index() {
                self.send_append(from, now);
            }
        } else {
            let cap = self.next_index[from].saturating_sub(1).max(1);
            self.next_index[from] = index.clamp(1, cap).max(self.match_index[from] + 1);
            self.send_append(from, now);
        }
    }

    fn advance_commit(&mut self) {
        if self.role != Role::Leader {
            return;
        }
        let mut n = self.last_index();
        while n > self.commit {
            if self.term_at(n) == Some(self.term) {
                let count = 1
                    + (0..self.n)
                        .filter(|&i| i != self.id && self.match_index[i] >= n)
                        .count();
                if count >= self.majority() {
                    self.set_commit(n);
                    for d in self.dirty.iter_mut() {
                        *d = true;
                    }
                    return;
                }
            }
            n -= 1;
        }
    }

    fn set_commit(&mut self, new_commit: u64) {
        if new_commit <= self.commit {
            return;
        }
        self.commit = new_commit;
        while self.delivered < self.commit {
            self.delivered += 1;
            if let Some(e) = self.entry(self.delivered) {
                let entry = e.clone();
                self.events.push_back(Event::Committed {
                    index: self.delivered,
                    entry,
                });
            } else {
                // Cubierto por una instantánea ya restaurada.
                continue;
            }
        }
    }

    // ----------------------------------------------------------------------------------------- instantánea

    #[allow(clippy::too_many_arguments)]
    fn on_snap_chunk(
        &mut self,
        now: u64,
        from: usize,
        term: u64,
        last_index: u64,
        last_term: u64,
        offset: u64,
        data: Vec<u8>,
        done: bool,
    ) {
        if term < self.term {
            self.send(
                from,
                Msg::SnapReply {
                    term: self.term,
                    last_index,
                    ok: false,
                    next_offset: 0,
                    done: false,
                },
            );
            return;
        }
        if self.role == Role::Leader && term == self.term {
            self.stats.bad_msgs += 1;
            return;
        }
        self.become_follower(term, Some(from), now);
        if last_index <= self.commit {
            // Ya tenemos eso (o más): se da por recibida.
            self.send(
                from,
                Msg::SnapReply {
                    term: self.term,
                    last_index,
                    ok: true,
                    next_offset: 0,
                    done: true,
                },
            );
            return;
        }
        if offset == 0 {
            self.snap_in = Some(SnapIn {
                last_index,
                last_term,
                buf: Vec::new(),
            });
        }
        let ok = match self.snap_in.as_mut() {
            Some(s)
                if s.last_index == last_index
                    && s.last_term == last_term
                    && s.buf.len() as u64 == offset =>
            {
                s.buf.extend_from_slice(&data);
                true
            }
            _ => false,
        };
        if !ok {
            let next = self
                .snap_in
                .as_ref()
                .filter(|s| s.last_index == last_index)
                .map_or(0, |s| s.buf.len() as u64);
            self.send(
                from,
                Msg::SnapReply {
                    term: self.term,
                    last_index,
                    ok: false,
                    next_offset: next,
                    done: false,
                },
            );
            return;
        }
        let next_offset = self.snap_in.as_ref().unwrap().buf.len() as u64;
        if done {
            let s = self.snap_in.take().unwrap();
            self.stats.snapshots_installed += 1;
            // Si nuestro diario ya tenía la última entrada de la instantánea (mismo índice y término), las
            // posteriores se conservan (puede que otras réplicas las hayan contado para confirmar); si no,
            // el diario propio queda reemplazado por la instantánea.
            if s.last_index > self.base_index
                && s.last_index <= self.last_index()
                && self.term_at(s.last_index) == Some(s.last_term)
            {
                let drop = (s.last_index - self.base_index) as usize;
                self.log.drain(..drop);
            } else {
                self.log.clear();
                self.persist.push(Persist::TruncateFrom {
                    index: s.last_index + 1,
                });
            }
            self.base_index = s.last_index;
            self.base_term = s.last_term;
            self.commit = s.last_index;
            self.delivered = s.last_index;
            self.persist.push(Persist::Base {
                index: s.last_index,
                term: s.last_term,
            });
            self.events.push_back(Event::Restore {
                index: s.last_index,
                term: s.last_term,
                data: s.buf,
            });
        }
        self.send(
            from,
            Msg::SnapReply {
                term: self.term,
                last_index,
                ok: true,
                next_offset,
                done,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn on_snap_reply(
        &mut self,
        now: u64,
        from: usize,
        term: u64,
        last_index: u64,
        ok: bool,
        next_offset: u64,
        done: bool,
    ) {
        if term > self.term {
            self.become_follower(term, None, now);
            return;
        }
        if self.role != Role::Leader || term != self.term {
            return;
        }
        self.last_ack[from] = now;
        let Some((li, _)) = self.snap_out[from] else {
            return;
        };
        if li != last_index {
            return;
        }
        if ok && done {
            self.snap_out[from] = None;
            self.match_index[from] = self.match_index[from].max(last_index);
            self.next_index[from] = self.match_index[from] + 1;
            self.dirty[from] = true;
            self.send_append(from, now);
        } else {
            // Siga o la réplica pida otro trozo (`ok` falso): se envía desde donde dice.
            self.snap_out[from] = Some((li, next_offset));
            self.send_snapshot(from);
        }
    }

    // ------------------------------------------------------------------------------------------ compactar

    /// Si el diario guarda demasiadas entradas ya aplicadas, descarta las más viejas. `applied` es lo que la
    /// máquina de estados ha aplicado (nunca más que lo confirmado). Devuelve `true` si compactó.
    pub fn maybe_compact(&mut self, applied: u64) -> bool {
        let applied = applied.min(self.commit);
        if applied <= self.base_index + self.cfg.compact_after {
            return false;
        }
        let new_base = applied - self.cfg.keep_tail.min(applied - self.base_index);
        if new_base <= self.base_index {
            return false;
        }
        let Some(term) = self.term_at(new_base) else {
            return false;
        };
        let drop = (new_base - self.base_index) as usize;
        self.log.drain(..drop);
        self.base_index = new_base;
        self.base_term = term;
        self.persist.push(Persist::Base {
            index: new_base,
            term,
        });
        self.stats.compactions += 1;
        true
    }
}
