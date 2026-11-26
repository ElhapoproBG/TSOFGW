//! Directorio de sesiones: dice **en qué shard vive cada cuenta** y desde qué época.
//!
//! Sin él, cada shard decide por su cuenta si una cuenta puede entrar, y dos shards pueden acabar con la
//! misma cuenta a la vez (por ejemplo, un cliente que se reconecta a otro shard tras una caída). Con él,
//! hay una sola fuente de verdad:
//!
//! * **Entrar** (`DirLogin`): el shard al que llega el cliente pregunta dónde vive la cuenta. Si el
//!   directorio no la conoce, la registra en el shard que el propio shard propone (el dueño de su punto de
//!   aparición) con la época 1. La respuesta (`DirRoute`) dice dónde vive: si es el shard que pregunta,
//!   entra; si no, redirige al cliente.
//! * **Cambiar de shard** (`DirTransfer`): es una operación *compare-and-swap* sobre la época. El origen
//!   dice «la cuenta está en mí con la época `e` y pasa a `destino`»; solo si el directorio tiene
//!   exactamente eso, anota `(destino, e + 1)` y responde `Committed`. Repetir la misma petición vuelve a
//!   responder `Committed` sin cambiar nada (es idempotente). Cualquier otra cosa se rechaza con lo que
//!   el directorio tiene de verdad. **Ese anotado es el instante en que la cuenta cambia de dueño**: el
//!   origen no envía el estado al destino hasta recibirlo.
//!
//! El directorio escribe en su almacén (véase [`tsofgw_store`]) **antes** de responder: una respuesta
//! `DirRoute` o `Committed` que llegó a un shard sigue siendo cierta aunque el directorio se caiga y
//! rearranque justo después.
//!
//! # Réplicas (0.8.0)
//!
//! Con `replicas = 3, 5 o 7` el directorio es **un grupo de réplicas de acuerdo con Raft** (módulo `repl` y
//! [`crate::raft`]): la caída de una minoría no para a nadie. Con `replicas = 1` (por omisión) funciona como en la
//! 0.5, sin diario ni elección. Una carpeta de un modo no se puede abrir en el otro.
//!
//! # Límites (sin réplicas, 0.5.0)
//!
//! * Es **un solo proceso**: si cae, nadie puede entrar ni cruzar bordes con una cuenta hasta que vuelva
//!   (lo que ya está jugando sigue jugando; los traspasos esperan). No hay réplica, ni elección de líder,
//!   ni arrendamientos.
//! * Si se **pierde** su carpeta de datos, se pierde el mapa de cuentas: hay que restaurarlo de una copia.
//!   No detecta que le devuelvan una copia vieja (véase `tsofgw-store`).
//! * Los mensajes del bus no van cifrados y la clave de cluster da confianza total.

mod repl;

use tsofgw_store::{Batch, DynStore, Medium, OpenMode, Recovery, Store, StoreConfig};

use crate::raft::RaftConfig;

use crate::bus::{DirStatus, PeerBus, PeerMsg};
use crate::partition::ShardId;
use crate::persist::{account_of, HoRec, HoStage, PlayerRec, PREFIX_HO, PREFIX_PLAYER};
use crate::server::NPC_BASE;

const K_INIT: &[u8] = b"m/init";
const P_ACCOUNT: &[u8] = b"a/";

/// Contadores del directorio (acumulados desde que arrancó el proceso).
#[derive(Debug, Clone, Default)]
pub struct DirectoryStats {
    pub msgs_in: u64,
    pub msgs_out: u64,
    /// Mensajes que no se pudieron decodificar, de un origen imposible o que no son para el directorio.
    pub malformed: u64,
    pub logins: u64,
    pub accounts_created: u64,
    pub transfers: u64,
    pub committed: u64,
    /// Peticiones repetidas de un cambio que ya estaba anotado.
    pub idempotent: u64,
    pub rejected: u64,
    pub unknown: u64,
    pub commits: u64,
    pub commit_errors: u64,
    pub compactions: u64,
    pub replies_dropped: u64,
    // --- directorio replicado (0.8.0)
    /// Mandatos propuestos al diario por este líder.
    pub proposed: u64,
    /// Peticiones repetidas que ya estaban en vuelo y no se volvieron a proponer.
    pub inflight_dups: u64,
    /// Peticiones que llegaron a una réplica que no lidera y se redirigieron al líder.
    pub redirects: u64,
    /// Peticiones que llegaron cuando no se conocía líder (se ignoran).
    pub no_leader: u64,
    /// Entradas del diario aplicadas al estado.
    pub applied: u64,
    /// Instantáneas instaladas.
    pub restores: u64,
    /// Compactaciones del diario de Raft.
    pub log_compactions: u64,
    /// Cuentas sembradas desde una reconstrucción.
    pub seeded: u64,
}

#[derive(Debug, Clone)]
pub struct DirectoryConfig {
    /// Si el diario pasa de estos bytes, se compacta con un checkpoint.
    pub compact_bytes: u64,
    /// Réplicas del directorio: 1 (sin consenso) o 3, 5 o 7 (Raft).
    pub replicas: usize,
    /// Cuál de ellas es esta (0-based). Su identificador en el bus es `shards + replica`.
    pub replica: usize,
    /// Temporización de Raft (solo con réplicas).
    pub raft: RaftConfig,
}

impl Default for DirectoryConfig {
    fn default() -> Self {
        DirectoryConfig {
            compact_bytes: 16 << 20,
            replicas: 1,
            replica: 0,
            raft: RaftConfig::default(),
        }
    }
}

fn key(account: u64) -> Vec<u8> {
    let mut k = P_ACCOUNT.to_vec();
    k.extend_from_slice(&account.to_be_bytes());
    k
}

fn encode(shard: ShardId, epoch: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(10);
    v.extend_from_slice(&shard.to_le_bytes());
    v.extend_from_slice(&epoch.to_le_bytes());
    v
}

fn decode(b: &[u8]) -> Option<(ShardId, u64)> {
    let b: [u8; 10] = b.try_into().ok()?;
    let epoch = u64::from_le_bytes(b[2..10].try_into().ok()?);
    (epoch >= 1).then(|| (u16::from_le_bytes([b[0], b[1]]), epoch))
}

pub struct Directory {
    store: DynStore,
    bus: Box<dyn PeerBus>,
    shards: usize,
    cfg: DirectoryConfig,
    inbox: Vec<(ShardId, Vec<u8>)>,
    deferred: Vec<(ShardId, PeerMsg)>,
    repl: Option<repl::Repl>,
    stats: DirectoryStats,
    failed: bool,
    critical: bool,
}

/// Un bus que no lleva ni trae nada: para abrir el directorio solo como almacén (herramientas).
struct NullBus;

impl PeerBus for NullBus {
    fn send(&mut self, _now_ms: u64, _to: ShardId, _data: Vec<u8>) {}
    fn recv(&mut self, _now_ms: u64, _out: &mut Vec<(ShardId, Vec<u8>)>) {}
}

/// Qué contiene el almacén de un directorio (para `tsofgw store info`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectorySummary {
    /// Número de shards del cluster al que pertenece (`None`: carpeta vacía).
    pub shards: Option<u32>,
    pub accounts: usize,
    /// Cuentas por shard (vacío si no se conoce el número de shards).
    pub per_shard: Vec<usize>,
    /// Registros que no se entienden.
    pub unknown: usize,
}

/// Cuenta lo que hay en el almacén de un directorio. No modifica nada.
pub fn summarize_directory_store(store: &DynStore) -> DirectorySummary {
    let shards = store
        .get(K_INIT)
        .and_then(|v| <[u8; 4]>::try_from(v).ok())
        .map(u32::from_le_bytes);
    let mut r = DirectorySummary {
        shards,
        per_shard: vec![0; shards.unwrap_or(0) as usize],
        ..DirectorySummary::default()
    };
    for (k, v) in store.scan(b"") {
        if k == K_INIT
            || k == repl::K_REPL
            || k == repl::K_META
            || k == repl::K_BASE
            || k == repl::K_APPLIED
        {
            continue;
        }
        if k.starts_with(repl::P_ENTRY) {
            continue;
        }
        if k.starts_with(P_ACCOUNT) {
            match decode(v) {
                Some((s, _)) if (s as usize) < r.per_shard.len() => {
                    r.accounts += 1;
                    r.per_shard[s as usize] += 1;
                }
                _ => r.unknown += 1,
            }
        } else {
            r.unknown += 1;
        }
    }
    r
}

impl Directory {
    /// Abre el directorio **solo como almacén** (sin bus): para herramientas que lo inspeccionan o lo
    /// reconstruyen con el proceso parado. No atiende a nadie.
    pub fn open_offline(
        medium: Box<dyn Medium>,
        key: &[u8; 32],
        shards: usize,
        mode: OpenMode,
    ) -> Result<(Directory, Recovery), String> {
        Directory::open(
            medium,
            key,
            Box::new(NullBus),
            shards,
            DirectoryConfig::default(),
            mode,
        )
    }

    /// Cierra el almacén y lo devuelve (para herramientas que lo abren y cierran).
    pub fn into_store(self) -> DynStore {
        self.store
    }

    /// Abre (o crea) el directorio de un cluster de `shards` shards. Su identificador en el bus es
    /// `shards`. Una carpeta que perteneció a un cluster con otro número de shards se rechaza.
    pub fn open(
        medium: Box<dyn Medium>,
        key: &[u8; 32],
        bus: Box<dyn PeerBus>,
        shards: usize,
        cfg: DirectoryConfig,
        mode: OpenMode,
    ) -> Result<(Directory, Recovery), String> {
        if shards == 0 || shards >= ShardId::MAX as usize {
            return Err("el número de shards debe estar entre 1 y 65 534".into());
        }
        let (mut store, recovery) = Store::open(
            medium,
            key,
            StoreConfig {
                mode,
                ..StoreConfig::default()
            },
        )
        .map_err(|e| format!("no se pudo abrir el almacén del directorio: {e}"))?;
        let io = |e: tsofgw_store::StoreError| format!("almacén del directorio: {e}");
        match store.get(K_INIT) {
            None => {
                let mut b = Batch::new();
                b.put(K_INIT, (shards as u32).to_le_bytes().to_vec());
                store.apply(&b).map_err(io)?;
                store.commit().map_err(io)?;
            }
            Some(v) => {
                let n = <[u8; 4]>::try_from(v)
                    .map(u32::from_le_bytes)
                    .map_err(|_| "registro de identidad dañado".to_string())?;
                if n as usize != shards {
                    return Err(format!(
                        "este directorio es de un cluster de {n} shards y se quiere abrir con {shards}"
                    ));
                }
            }
        }
        // Se valida todo lo guardado: un registro que no se entiende es un error al arrancar.
        for (k, v) in store.scan(P_ACCOUNT) {
            let ok = k.len() == P_ACCOUNT.len() + 8
                && decode(v).is_some_and(|(s, _)| (s as usize) < shards);
            if !ok {
                return Err("almacén del directorio dañado: registro de cuenta".into());
            }
        }
        let repl = if cfg.replicas > 1 {
            Some(Directory::load_repl(
                &mut store,
                cfg.replicas,
                cfg.replica,
                shards,
                cfg.raft.clone(),
                0,
            )?)
        } else {
            if store.get(repl::K_REPL).is_some() {
                return Err("esta carpeta es de una réplica de un directorio replicado: ábrela con sus réplicas".into());
            }
            None
        };
        Ok((
            Directory {
                store,
                bus,
                shards,
                cfg,
                inbox: Vec::new(),
                deferred: Vec::new(),
                repl,
                stats: DirectoryStats::default(),
                failed: false,
                critical: false,
            },
            recovery,
        ))
    }

    /// Clave de integridad del almacén del directorio, derivada de un secreto del operador.
    pub fn store_key(secret: &[u8]) -> [u8; 32] {
        tsofgw_store::derive_key(secret, "directory")
    }

    /// Identificador del directorio en el bus.
    pub fn peer_id(&self) -> ShardId {
        (self.shards + self.cfg.replica) as ShardId
    }

    /// Carga las cuentas de una reconstrucción ([`rebuild_from_shards`]) en un directorio **vacío**.
    /// Devuelve cuántas cuentas escribió. Un directorio que ya tiene cuentas no se toca.
    pub fn import(&mut self, entries: &[(u64, ShardId, u64)]) -> Result<usize, String> {
        if self.account_count() != 0 {
            return Err("el directorio ya tiene cuentas: solo se importa en uno vacío".into());
        }
        for &(account, shard, epoch) in entries {
            if account >= NPC_BASE || (shard as usize) >= self.shards || epoch == 0 {
                return Err(format!(
                    "cuenta {account}: entrada imposible ({shard}, {epoch})"
                ));
            }
        }
        let io = |e: tsofgw_store::StoreError| format!("almacén del directorio: {e}");
        let mut b = Batch::new();
        for &(account, shard, epoch) in entries {
            b.put(key(account), encode(shard, epoch));
            if b.len() >= 2048 {
                self.store.apply(&b).map_err(io)?;
                b.clear();
            }
        }
        if !b.is_empty() {
            self.store.apply(&b).map_err(io)?;
        }
        self.store.commit().map_err(io)?;
        Ok(entries.len())
    }

    pub fn stats(&self) -> &DirectoryStats {
        &self.stats
    }

    /// ¿El almacén falló? Desde entonces no responde nada que no esté ya en disco; conviene reiniciarlo.
    pub fn failed(&self) -> bool {
        self.failed
    }

    pub fn store_stats(&self) -> tsofgw_store::StoreStats {
        self.store.stats()
    }

    pub fn account_count(&self) -> usize {
        self.store.scan(P_ACCOUNT).count()
    }

    /// Dónde vive una cuenta y con qué época.
    pub fn lookup(&self, account: u64) -> Option<(ShardId, u64)> {
        self.store.get(&key(account)).and_then(decode)
    }

    /// Cuántas cuentas hay registradas en cada shard.
    pub fn accounts_per_shard(&self) -> Vec<usize> {
        let mut v = vec![0usize; self.shards];
        for (_, val) in self.store.scan(P_ACCOUNT) {
            if let Some((s, _)) = decode(val) {
                v[s as usize] += 1;
            }
        }
        v
    }

    /// Lee del bus lo que haya llegado, lo atiende y envía las respuestas (tras sincronizar el disco).
    pub fn update(&mut self, now_ms: u64) {
        if let Some(r) = self.repl.as_mut() {
            if !r.clock_set {
                r.clock_set = true;
                r.raft.restart_timers(now_ms);
            }
        }
        let mut inbox = std::mem::take(&mut self.inbox);
        self.bus.recv(now_ms, &mut inbox);
        for (from, data) in inbox.drain(..) {
            self.stats.msgs_in += 1;
            let from_replica = self.replica_index(from).is_some();
            if (from as usize) >= self.shards && !from_replica {
                self.stats.malformed += 1;
                continue;
            }
            if self.failed {
                continue;
            }
            match PeerMsg::decode(&data) {
                Ok(PeerMsg::Raft(b)) if from_replica => self.on_raft_msg(now_ms, from, &b),
                Ok(_) if from_replica => self.stats.malformed += 1,
                Ok(m) if self.repl.is_some() => self.on_request_replicated(from, m),
                Ok(m) => self.on_msg(from, m),
                Err(_) => self.stats.malformed += 1,
            }
        }
        self.inbox = inbox;
        if let Some(r) = self.repl.as_mut() {
            if !self.failed {
                r.raft.tick(now_ms);
            }
        }
        self.flush(now_ms);
    }

    fn put(&mut self, account: u64, shard: ShardId, epoch: u64) {
        if self.failed {
            return;
        }
        let mut b = Batch::new();
        b.put(key(account), encode(shard, epoch));
        if self.store.apply(&b).is_err() {
            self.failed = true;
            self.stats.commit_errors += 1;
        }
        self.critical = true;
    }

    fn reply(&mut self, to: ShardId, msg: PeerMsg) {
        self.critical = true;
        self.deferred.push((to, msg));
    }

    fn on_msg(&mut self, from: ShardId, m: PeerMsg) {
        match m {
            PeerMsg::DirLogin { account, home } => {
                if account >= NPC_BASE || (home as usize) >= self.shards {
                    self.stats.malformed += 1;
                    return;
                }
                self.stats.logins += 1;
                let (shard, epoch) = match self.lookup(account) {
                    Some(r) => r,
                    None => {
                        self.put(account, home, 1);
                        self.stats.accounts_created += 1;
                        (home, 1)
                    }
                };
                self.reply(
                    from,
                    PeerMsg::DirRoute {
                        account,
                        shard,
                        epoch,
                    },
                );
            }
            PeerMsg::DirTransfer {
                xfer,
                account,
                epoch,
                to,
            } => {
                if (to as usize) >= self.shards || to == from {
                    self.stats.malformed += 1;
                    return;
                }
                self.stats.transfers += 1;
                let reply = |status, shard, epoch| PeerMsg::DirResult {
                    xfer,
                    account,
                    status,
                    shard,
                    epoch,
                };
                match self.lookup(account) {
                    None => {
                        self.stats.unknown += 1;
                        self.reply(from, reply(DirStatus::Unknown, 0, 0));
                    }
                    Some((shard, e)) if shard == from && e == epoch => match epoch.checked_add(1) {
                        Some(next) => {
                            self.put(account, to, next);
                            self.stats.committed += 1;
                            self.reply(from, reply(DirStatus::Committed, to, next));
                        }
                        None => {
                            self.stats.rejected += 1;
                            self.reply(from, reply(DirStatus::Rejected, shard, e));
                        }
                    },
                    // La misma petición otra vez (se perdió la respuesta): ya está hecho.
                    Some((shard, e)) if shard == to && Some(e) == epoch.checked_add(1) => {
                        self.stats.idempotent += 1;
                        self.reply(from, reply(DirStatus::Committed, shard, e));
                    }
                    Some((shard, e)) => {
                        self.stats.rejected += 1;
                        self.reply(from, reply(DirStatus::Rejected, shard, e));
                    }
                }
            }
            // Ninguno de los demás mensajes es para el directorio.
            _ => self.stats.malformed += 1,
        }
    }

    /// Variante replicada de [`Directory::flush`]: guarda lo de Raft, aplica lo confirmado, sincroniza y solo
    /// entonces envía mensajes entre réplicas y respuestas a los shards.
    fn flush_replicated(&mut self, now_ms: u64) {
        if !self.failed && self.repl_process().is_err() {
            self.failed = true;
            self.stats.commit_errors += 1;
        }
        if !self.failed {
            if self.store.unsynced() > 0 {
                match self.store.commit() {
                    Ok(()) => self.stats.commits += 1,
                    Err(_) => {
                        self.failed = true;
                        self.stats.commit_errors += 1;
                    }
                }
            }
            if !self.failed {
                match self.store.checkpoint_if_larger(self.cfg.compact_bytes) {
                    Ok(true) => self.stats.compactions += 1,
                    Ok(false) => {}
                    Err(_) => {
                        self.failed = true;
                        self.stats.commit_errors += 1;
                    }
                }
            }
        }
        let mut out = std::mem::take(&mut self.deferred);
        let raft_out = self.repl_outbox();
        if self.failed {
            self.stats.replies_dropped += out.len() as u64;
            return;
        }
        out.extend(raft_out);
        for (to, msg) in out {
            self.stats.msgs_out += 1;
            self.bus.send(now_ms, to, msg.encode());
        }
    }

    /// Sincroniza lo escrito en esta vuelta y solo entonces envía las respuestas.
    fn flush(&mut self, now_ms: u64) {
        if self.repl.is_some() {
            return self.flush_replicated(now_ms);
        }
        if self.critical && !self.failed {
            match self.store.commit() {
                Ok(()) => self.stats.commits += 1,
                Err(_) => {
                    self.failed = true;
                    self.stats.commit_errors += 1;
                }
            }
        }
        self.critical = false;
        if !self.failed {
            match self.store.checkpoint_if_larger(self.cfg.compact_bytes) {
                Ok(true) => self.stats.compactions += 1,
                Ok(false) => {}
                Err(_) => {
                    self.failed = true;
                    self.stats.commit_errors += 1;
                }
            }
        }
        let deferred = std::mem::take(&mut self.deferred);
        if self.failed {
            self.stats.replies_dropped += deferred.len() as u64;
            return;
        }
        for (to, msg) in deferred {
            self.stats.msgs_out += 1;
            self.bus.send(now_ms, to, msg.encode());
        }
    }

    /// Cierre limpio: compacta el almacén.
    pub fn shutdown(&mut self) -> Result<(), String> {
        if self.failed {
            return Err(
                "el almacén había fallado: se conserva lo que ya estaba sincronizado".into(),
            );
        }
        self.store
            .commit()
            .and_then(|()| self.store.checkpoint())
            .map_err(|e| format!("no se pudo cerrar el almacén del directorio: {e}"))
    }
}

/// Una cuenta reconstruida: `(cuenta, shard, época)`.
pub type RebuiltAccount = (u64, ShardId, u64);

/// Lo que se averiguó al reconstruir el directorio con lo que guarda cada shard.
#[derive(Debug, Clone, Default)]
pub struct RebuildReport {
    pub accounts: usize,
    /// Cuentas residentes en algún shard.
    pub residents: usize,
    /// Cuentas con un traspaso en curso (se anotan donde acabarán, o donde siguen si el directorio aún
    /// no había anotado el cambio).
    pub in_transit: usize,
    /// Cuentas que dos shards reclamaban a la vez con la misma época más alta: no se sabe cuál es la
    /// buena. Se anotan en el shard de menor número y hay que revisarlas a mano.
    pub conflicts: Vec<u64>,
}

/// Reconstruye el mapa de cuentas a partir de los almacenes de los shards (por ejemplo, tras perder el
/// directorio). Cada shard sabe qué cuentas tiene (`p/`) y cuáles está entregando (`ho/`); no hace falta
/// nada más. **Debe hacerse con los shards parados**, para que lo que se lee no cambie mientras tanto.
///
/// Devuelve `(cuenta, shard, época)` ordenado por cuenta, listo para [`Directory::import`].
pub fn rebuild_from_shards(
    shards: &[(ShardId, &DynStore)],
) -> Result<(Vec<RebuiltAccount>, RebuildReport), String> {
    use std::collections::BTreeMap;
    // cuenta → reclamaciones (shard, época, es_residente)
    let mut claims: BTreeMap<u64, Vec<(ShardId, u64, bool)>> = BTreeMap::new();
    let mut transit = std::collections::BTreeSet::new();
    for &(shard, store) in shards {
        for (k, v) in store.scan(PREFIX_PLAYER) {
            let account = account_of(k, PREFIX_PLAYER)
                .filter(|&a| a < NPC_BASE)
                .ok_or_else(|| format!("shard {shard}: clave de jugador dañada"))?;
            let r = PlayerRec::decode(v)
                .ok_or_else(|| format!("shard {shard}: registro de jugador dañado"))?;
            claims
                .entry(account)
                .or_default()
                .push((shard, r.epoch, true));
        }
        for (k, v) in store.scan(PREFIX_HO) {
            let account = account_of(k, PREFIX_HO)
                .filter(|&a| a < NPC_BASE)
                .ok_or_else(|| format!("shard {shard}: clave de traspaso dañada"))?;
            let r = HoRec::decode(v)
                .ok_or_else(|| format!("shard {shard}: registro de traspaso dañado"))?;
            transit.insert(account);
            let claim = match r.stage {
                // El directorio ya lo había anotado (antes de perderse): la cuenta es del destino.
                HoStage::Committed => (r.to, r.epoch, false),
                // No se sabe si llegó a anotarlo: se supone que no, y el origen lo vuelve a pedir.
                HoStage::Committing => (shard, r.epoch.saturating_sub(1).max(1), false),
            };
            claims.entry(account).or_default().push(claim);
        }
    }
    let mut report = RebuildReport::default();
    let mut out = Vec::with_capacity(claims.len());
    for (account, list) in claims {
        let top = list.iter().map(|c| c.1).max().unwrap_or(1);
        let mut best: Vec<(ShardId, bool)> = list
            .iter()
            .filter(|c| c.1 == top)
            .map(|c| (c.0, c.2))
            .collect();
        best.sort_unstable();
        best.dedup();
        let mut shards_at_top: Vec<ShardId> = best.iter().map(|b| b.0).collect();
        shards_at_top.dedup();
        if shards_at_top.len() > 1 {
            report.conflicts.push(account);
        }
        let chosen = best
            .iter()
            .find(|b| b.1)
            .or_else(|| best.first())
            .map_or(0, |b| b.0);
        if list.iter().any(|c| c.2) {
            report.residents += 1;
        }
        if transit.contains(&account) {
            report.in_transit += 1;
        }
        out.push((account, chosen, top));
    }
    report.accounts = out.len();
    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{SimBus, SimBusConfig, SimBusEnd};
    use tsofgw_store::MemMedium;

    const KEY: [u8; 32] = [5; 32];

    fn rig(shards: usize) -> (SimBus, MemMedium, Directory, Vec<SimBusEnd>) {
        let bus = SimBus::new(
            shards + 1,
            SimBusConfig {
                latency_ms: 1,
                ..SimBusConfig::default()
            },
        );
        let medium = MemMedium::new();
        let (dir, _) = Directory::open(
            Box::new(medium.clone()),
            &KEY,
            Box::new(bus.end(shards as ShardId)),
            shards,
            DirectoryConfig::default(),
            OpenMode::Recover,
        )
        .unwrap();
        let ends = (0..shards as ShardId).map(|s| bus.end(s)).collect();
        (bus, medium, dir, ends)
    }

    /// Envía `msg` del shard `from` al directorio y devuelve la respuesta que llega.
    fn ask(
        dir: &mut Directory,
        ends: &mut [SimBusEnd],
        now: &mut u64,
        from: ShardId,
        msg: PeerMsg,
    ) -> Option<PeerMsg> {
        let dir_id = dir.peer_id();
        ends[from as usize].send(*now, dir_id, msg.encode());
        *now += 2;
        dir.update(*now);
        *now += 2;
        let mut out = Vec::new();
        ends[from as usize].recv(*now, &mut out);
        out.first().map(|(_, d)| PeerMsg::decode(d).unwrap())
    }

    #[test]
    fn registra_cuentas_nuevas_donde_se_propone_y_las_recuerda() {
        let (_bus, _m, mut dir, mut ends) = rig(3);
        let mut now = 0;
        let r = ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::DirLogin {
                account: 1000,
                home: 2,
            },
        );
        assert_eq!(
            r,
            Some(PeerMsg::DirRoute {
                account: 1000,
                shard: 2,
                epoch: 1
            })
        );
        // Otro shard pregunta por la misma cuenta proponiendo otro hogar: manda lo ya registrado.
        let r = ask(
            &mut dir,
            &mut ends,
            &mut now,
            1,
            PeerMsg::DirLogin {
                account: 1000,
                home: 1,
            },
        );
        assert_eq!(
            r,
            Some(PeerMsg::DirRoute {
                account: 1000,
                shard: 2,
                epoch: 1
            })
        );
        assert_eq!(dir.lookup(1000), Some((2, 1)));
        assert_eq!(dir.stats().accounts_created, 1);
        assert_eq!(dir.accounts_per_shard(), vec![0, 0, 1]);
    }

    #[test]
    fn el_cambio_de_dueno_es_un_compare_and_swap_sobre_la_epoca() {
        let (_bus, _m, mut dir, mut ends) = rig(3);
        let mut now = 0;
        ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::DirLogin {
                account: 7,
                home: 0,
            },
        );
        let xfer = |epoch, to| PeerMsg::DirTransfer {
            xfer: 9,
            account: 7,
            epoch,
            to,
        };
        // Quien no es el dueño no puede cederla; tampoco con la época equivocada.
        for (from, epoch) in [(1u16, 1u64), (0, 2), (0, 9)] {
            let r = ask(&mut dir, &mut ends, &mut now, from, xfer(epoch, 2));
            assert!(
                matches!(
                    r,
                    Some(PeerMsg::DirResult {
                        status: DirStatus::Rejected,
                        shard: 0,
                        epoch: 1,
                        ..
                    })
                ),
                "{from} {epoch}: {r:?}"
            );
        }
        // La época 0 no existe: el mensaje ni siquiera se decodifica y no recibe respuesta.
        let before = dir.stats().malformed;
        assert!(ask(&mut dir, &mut ends, &mut now, 0, xfer(0, 2)).is_none());
        assert_eq!(dir.stats().malformed, before + 1);
        assert_eq!(dir.lookup(7), Some((0, 1)), "los rechazos no cambian nada");
        // El dueño la cede con la época correcta.
        let r = ask(&mut dir, &mut ends, &mut now, 0, xfer(1, 2));
        assert_eq!(
            r,
            Some(PeerMsg::DirResult {
                xfer: 9,
                account: 7,
                status: DirStatus::Committed,
                shard: 2,
                epoch: 2
            })
        );
        assert_eq!(dir.lookup(7), Some((2, 2)));
        // Repetir la petición (se perdió la respuesta) devuelve lo mismo y no cambia nada.
        let r = ask(&mut dir, &mut ends, &mut now, 0, xfer(1, 2));
        assert!(matches!(
            r,
            Some(PeerMsg::DirResult {
                status: DirStatus::Committed,
                shard: 2,
                epoch: 2,
                ..
            })
        ));
        assert_eq!(dir.stats().idempotent, 1);
        // Ahora el origen ya no es el dueño: ceder otra vez desde allí se rechaza.
        let r = ask(&mut dir, &mut ends, &mut now, 0, xfer(2, 1));
        assert!(matches!(
            r,
            Some(PeerMsg::DirResult {
                status: DirStatus::Rejected,
                shard: 2,
                epoch: 2,
                ..
            })
        ));
        // Una cuenta que no existe.
        let r = ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::DirTransfer {
                xfer: 1,
                account: 12345,
                epoch: 1,
                to: 1,
            },
        );
        assert!(matches!(
            r,
            Some(PeerMsg::DirResult {
                status: DirStatus::Unknown,
                ..
            })
        ));
    }

    #[test]
    fn lo_anotado_sobrevive_a_una_caida_y_lo_no_anotado_no_se_inventa() {
        let (bus, medium, mut dir, mut ends) = rig(3);
        let mut now = 0;
        ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::DirLogin {
                account: 5,
                home: 0,
            },
        );
        ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::DirTransfer {
                xfer: 1,
                account: 5,
                epoch: 1,
                to: 1,
            },
        );
        drop(dir);
        medium.crash(tsofgw_store::CrashKind::Torn(3));
        bus.clear_inbox(3);
        let (mut dir, _) = Directory::open(
            Box::new(medium.clone()),
            &KEY,
            Box::new(bus.end(3)),
            3,
            DirectoryConfig::default(),
            OpenMode::Recover,
        )
        .unwrap();
        assert_eq!(
            dir.lookup(5),
            Some((1, 2)),
            "se respondió Committed: tiene que constar"
        );
        // Y repetir lo ya hecho sigue dando la misma respuesta tras el reinicio.
        let r = ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::DirTransfer {
                xfer: 1,
                account: 5,
                epoch: 1,
                to: 1,
            },
        );
        assert!(matches!(
            r,
            Some(PeerMsg::DirResult {
                status: DirStatus::Committed,
                shard: 1,
                epoch: 2,
                ..
            })
        ));
    }

    #[test]
    fn no_responde_nada_que_no_este_ya_en_disco() {
        // Si el almacén falla al escribir, ni la ruta ni el `Committed` llegan a salir.
        let bus = SimBus::new(2, SimBusConfig::default());
        let medium = MemMedium::new();
        let (mut dir, _) = Directory::open(
            Box::new(medium.clone()),
            &KEY,
            Box::new(bus.end(1)),
            1,
            DirectoryConfig::default(),
            OpenMode::Recover,
        )
        .unwrap();
        let mut shard0 = bus.end(0);
        medium.fail_syncs(1);
        shard0.send(
            0,
            1,
            PeerMsg::DirLogin {
                account: 1,
                home: 0,
            }
            .encode(),
        );
        dir.update(10);
        assert!(dir.failed());
        let mut out = Vec::new();
        shard0.recv(100, &mut out);
        assert!(out.is_empty(), "una respuesta salió sin estar sincronizada");
        assert_eq!(dir.stats().replies_dropped, 1);
    }

    fn shard_store(records: &[(&[u8], Vec<u8>)]) -> DynStore {
        let (mut st, _) = Store::open(
            Box::new(MemMedium::new()) as Box<dyn Medium>,
            &[1; 32],
            StoreConfig::default(),
        )
        .unwrap();
        let mut b = Batch::new();
        for (k, v) in records {
            b.put(k.to_vec(), v.clone());
        }
        st.apply(&b).unwrap();
        st
    }

    #[test]
    fn se_reconstruye_el_mapa_desde_lo_que_guardan_los_shards() {
        use crate::persist::{k_ho, k_player};
        let player = |pos: i32, epoch: u64| {
            PlayerRec {
                pos: [pos, 0, 0],
                epoch,
            }
            .encode()
        };
        let ho = |to: ShardId, epoch: u64, stage: HoStage| {
            HoRec {
                to,
                xfer: 1,
                epoch,
                pos: [0, 0, 0],
                last_processed: 0,
                stage,
            }
            .encode()
        };
        // Cuenta 1: residente en el shard 0 (época 3).
        // Cuenta 2: el shard 0 la entrega al 1 con el cambio ya anotado (época 5) y el 1 ya la adoptó.
        // Cuenta 3: el shard 1 la entrega al 2 sin saber si el directorio anotó el cambio (época 4).
        // Cuenta 4: reclamada por dos shards con la misma época: conflicto.
        let k1 = k_player(1);
        let k2h = k_ho(2);
        let k2p = k_player(2);
        let k3 = k_ho(3);
        let k4a = k_player(4);
        let k4b = k_player(4);
        let s0 = shard_store(&[
            (&k1, player(7, 3)),
            (&k2h, ho(1, 5, HoStage::Committed)),
            (&k4a, player(1, 2)),
        ]);
        let s1 = shard_store(&[
            (&k2p, player(9, 5)),
            (&k3, ho(2, 4, HoStage::Committing)),
            (&k4b, player(2, 2)),
        ]);
        let s2 = shard_store(&[]);
        let (entries, rep) = rebuild_from_shards(&[(0, &s0), (1, &s1), (2, &s2)]).unwrap();
        assert_eq!(
            entries,
            vec![(1, 0, 3), (2, 1, 5), (3, 1, 3), (4, 0, 2)],
            "{entries:?}"
        );
        assert_eq!(rep.accounts, 4);
        assert_eq!(rep.conflicts, vec![4]);
        assert_eq!(rep.in_transit, 2);
        // Se puede importar en un directorio vacío, y solo en uno vacío.
        let (_bus, _m, mut dir, _ends) = rig(3);
        assert_eq!(dir.import(&entries).unwrap(), 4);
        assert_eq!(dir.lookup(2), Some((1, 5)));
        assert!(dir.import(&entries).is_err());
        assert!(rig(3).2.import(&[(1, 9, 1)]).is_err());
        assert!(rig(3).2.import(&[(1, 0, 0)]).is_err());
    }

    #[test]
    fn rechaza_basura_origenes_imposibles_y_otras_carpetas() {
        let (bus, medium, mut dir, mut ends) = rig(2);
        let mut now = 0;
        // Un shard que no existe, un hogar que no existe y mensajes que no son del directorio.
        let mut evil = bus.end(2); // el propio id del directorio no es un shard: no debe contar
        let _ = &mut evil;
        let r = ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::DirLogin {
                account: 1,
                home: 9,
            },
        );
        assert!(r.is_none());
        let r = ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::Abort { xfer: 1, player: 1 },
        );
        assert!(r.is_none());
        let r = ask(
            &mut dir,
            &mut ends,
            &mut now,
            0,
            PeerMsg::DirTransfer {
                xfer: 1,
                account: 1,
                epoch: 1,
                to: 0, // a sí mismo
            },
        );
        assert!(r.is_none());
        ends[0].send(now, 2, vec![0xff, 0xff]);
        now += 3;
        dir.update(now);
        assert!(dir.stats().malformed >= 4, "{:?}", dir.stats());
        assert_eq!(dir.account_count(), 0);
        // Un directorio de 2 shards no se abre como uno de 3.
        drop(dir);
        let r = Directory::open(
            Box::new(medium.clone()),
            &KEY,
            Box::new(bus.end(2)),
            3,
            DirectoryConfig::default(),
            OpenMode::Recover,
        );
        assert!(r.is_err());
        // Ni con otra clave.
        let r = Directory::open(
            Box::new(medium),
            &[6; 32],
            Box::new(bus.end(2)),
            2,
            DirectoryConfig::default(),
            OpenMode::Recover,
        );
        assert!(r.is_err());
    }
}
