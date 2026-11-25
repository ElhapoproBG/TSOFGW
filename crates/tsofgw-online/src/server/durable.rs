//! Persistencia del servidor de juego y entrada de jugadores.
//!
//! Sin almacén, un servidor es lo que fue hasta la 0.4: todo vive en memoria y una caída lo borra. Con
//! [`GameServer::enable_persistence`] el servidor guarda lo que no puede perder (véase el módulo
//! [`crate::persist`] para el esquema de claves) y, al arrancar sobre una carpeta que ya tiene datos, **lo
//! restaura**: los NPC que simulaba, los que tenía a medio traspaso, los traspasos de jugadores sin
//! acuse y la posición en que quedó cada jugador.
//!
//! # Qué se pierde en una caída, y qué no
//!
//! * **Se pierde** como mucho el movimiento reciente: lo que un NPC se movió desde su último guardado
//!   (`npc_checkpoint_ticks`), lo que un jugador se movió desde el suyo (`player_save_ticks`), y lo que
//!   el sistema operativo no llegó a escribir de lo no sincronizado (`sync_ticks`).
//! * **No se pierde** la propiedad de nada: cada NPC y cada cuenta consta como mínimo bajo una clave
//!   del almacén, y el paso de una clave a otra va en un solo lote atómico. Todo lo que se promete a
//!   otro servidor («lo recibí», «te lo mando») se envía **después** de sincronizar el disco.
//!
//! # Entrada de jugadores
//!
//! Con almacén, una cuenta que entra recupera la posición en que se quedó. Con almacén y **directorio de
//! sesiones** (`ClusterConfig::directory`), además, el shard al que llega el cliente le pregunta al
//! directorio dónde vive la cuenta: si es aquí, entra; si es en otro shard, el cliente es redirigido; si es
//! nueva, el directorio la registra en el shard que posee su punto de aparición. Así una cuenta no puede
//! existir en dos shards a la vez.
//!
//! **Sin directorio** cada shard decide con lo que tiene: si la cuenta es residente aquí, entra; si se fue
//! de aquí hacia otro shard, la lápida que dejó sirve de puntero de reenvío y el cliente es redirigido a
//! donde fue; si el shard no sabe nada de ella, la crea nueva. Eso funciona mientras los punteros no se
//! hayan perdido, pero **no garantiza** que una cuenta no acabe en dos shards (por ejemplo, si el cliente
//! entra por un shard que nunca la vio y esta vive en otro): para esa garantía hace falta el directorio.

use tsofgw_net::{quantize, EntityState};
use tsofgw_store::{Batch, Medium, OpenMode, Store, StoreConfig, StoreStats};
use tsofgw_transport::{Link, SessionId};
use tsofgw_world::{EntityId, Rng, Vec3};

use super::cluster::OutboxEntry;
use super::{vec3_from_q, GameServer, Npc, KIND_NPC, KIND_PLAYER, NPC_BASE};
use crate::bus::PeerMsg;
use crate::partition::ShardId;
use crate::persist::{
    account_of, decode_init, decode_u32, encode_init, encode_u32, idx_of, k_ho, k_npc, k_player,
    k_tomb, xi_of, HoRec, NpcRec, Persist, PersistConfig, PersistStats, PlayerRec, RestoreReport,
    TombRec, XoRec, K_INIT, K_TICK, K_XFER, PREFIX_HO, PREFIX_NPC, PREFIX_PLAYER, PREFIX_TOMB,
    PREFIX_XI, PREFIX_XO, XFER_BLOCK,
};
use crate::protocol::{self as proto, Welcome};
use crate::sim::PlayerState;
use crate::states::Own;

/// Lo que un shard guarda de una cuenta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountRecords {
    /// Residente aquí: posición y época.
    pub resident: Option<([i32; 3], u64)>,
    /// La está entregando a otro shard y aún no tiene acuse.
    pub outbox: Option<OutboxEntry>,
    /// Se fue de aquí hacia otro shard: época que tomó allí y shard al que fue.
    pub tombstone: Option<(u64, ShardId)>,
}

/// El estado completo de un NPC que este shard simula: lo que hace falta para que, tras un reinicio,
/// siga exactamente el mismo camino (posición, rumbo, cuenta atrás del giro y estado del generador).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NpcDump {
    pub id: u64,
    pub pos: [f32; 3],
    pub dir: [f32; 2],
    pub turn_in: u32,
    pub rng: u64,
}

/// Qué hacer con un cliente que pide entrar.
pub(super) enum Login {
    /// Es un jugador que otro shard está traspasando: el estado llega con su `HANDOVER`.
    Arriving,
    /// Todavía no se puede decidir (falta la respuesta del directorio o el estado de un traspaso). El
    /// cliente repite su `JOIN`, así que no hace falta recordar nada.
    Wait,
    /// La cuenta vive en otro shard.
    Redirect(ShardId),
    Spawn {
        pos: [i32; 3],
        epoch: u64,
        /// La posición viene del almacén (y no del punto de aparición de una cuenta nueva).
        restored: bool,
    },
}

impl<L: Link> GameServer<L> {
    // ------------------------------------------------------------------ consulta

    /// ¿Hay un almacén durable conectado?
    pub fn persistence_enabled(&self) -> bool {
        self.persist.is_some()
    }

    /// ¿El almacén falló? Desde entonces el servidor no promete nada más a otros servidores (no envía
    /// traspasos nuevos) y conviene reiniciarlo: lo que ya estaba durable se recupera al arrancar.
    pub fn persistence_failed(&self) -> bool {
        self.persist.as_ref().is_some_and(|p| p.failed)
    }

    pub fn persist_stats(&self) -> Option<&PersistStats> {
        self.persist.as_ref().map(|p| &p.stats)
    }

    pub fn store_stats(&self) -> Option<StoreStats> {
        self.persist.as_ref().map(|p| p.store.stats())
    }

    /// Bytes del diario desde el último checkpoint (para vigilar el crecimiento).
    pub fn store_log_bytes(&self) -> Option<u64> {
        self.persist.as_ref().map(|p| p.store.log_len())
    }

    /// Cuentas con registro de residente en este shard (conectadas o no) y su época. Orden por cuenta.
    pub fn resident_accounts(&self) -> Vec<(u64, u64)> {
        let Some(pe) = self.persist.as_ref() else {
            return Vec::new();
        };
        pe.store
            .scan(PREFIX_PLAYER)
            .filter_map(|(k, v)| Some((account_of(k, PREFIX_PLAYER)?, PlayerRec::decode(v)?.epoch)))
            .collect()
    }

    /// Posición guardada de una cuenta residente aquí.
    pub fn resident_position(&self, account: u64) -> Option<[i32; 3]> {
        let pe = self.persist.as_ref()?;
        PlayerRec::decode(pe.store.get(&k_player(account))?).map(|r| r.pos)
    }

    /// Estado completo de los NPC que este shard simula ahora mismo (no los que tiene en traspaso).
    /// Orden por NPC. Sirve para comprobar que un reinicio devuelve el mundo tal como estaba.
    pub fn npc_dump(&self) -> Vec<NpcDump> {
        (0..self.npcs.len())
            .filter(|&i| self.ents.npc_own(i) == Own::Owned)
            .filter_map(|i| {
                let r = self.npc_rec(i)?;
                Some(NpcDump {
                    id: NPC_BASE + i as u64,
                    pos: r.pos,
                    dir: r.dir,
                    turn_in: r.turn_in,
                    rng: r.rng,
                })
            })
            .collect()
    }

    /// Todo lo que este shard guarda de una cuenta (para inspección, pruebas y herramientas).
    pub fn account_records(&self, account: u64) -> AccountRecords {
        let Some(pe) = self.persist.as_ref() else {
            return AccountRecords::default();
        };
        AccountRecords {
            resident: pe
                .store
                .get(&k_player(account))
                .and_then(PlayerRec::decode)
                .map(|r| (r.pos, r.epoch)),
            outbox: self.cluster.as_ref().and_then(|c| {
                c.outbox_summary()
                    .into_iter()
                    .find(|e| e.account == account)
            }),
            tombstone: pe
                .store
                .get(&k_tomb(account))
                .and_then(TombRec::decode)
                .map(|t| (t.epoch, t.to)),
        }
    }

    /// Traspasos de jugador que este shard tiene entregados y aún sin acuse. Orden por cuenta.
    pub fn handover_outbox(&self) -> Vec<OutboxEntry> {
        self.cluster
            .as_ref()
            .map(|c| c.outbox_summary())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------ envío tras sincronizar

    /// Envía un mensaje que promete algo que ya debe estar en disco. Con almacén espera al final de la
    /// vuelta de `update`, tras sincronizar; sin almacén sale ya.
    pub(super) fn peer_send_durable(&mut self, to: ShardId, msg: PeerMsg) {
        match self.persist.as_mut() {
            Some(pe) => pe.defer(to, msg),
            None => self.peer_send(to, &msg),
        }
    }

    /// Reserva un número de traspaso. Con almacén, los números se reservan por bloques para que tras un
    /// reinicio no se repita ninguno.
    pub(super) fn alloc_xfer(&mut self) -> u32 {
        let x = match self.cluster.as_mut() {
            Some(c) => c.next_xfer(),
            None => return 0,
        };
        if let Some(pe) = self.persist.as_mut() {
            // Comparación modular: tolera que el contador dé la vuelta.
            if (pe.xfer_reserved.wrapping_sub(x) as i32) <= 0 {
                pe.xfer_reserved = x.wrapping_add(XFER_BLOCK);
                let mut b = Batch::new();
                b.put(K_XFER, encode_u32(pe.xfer_reserved));
                pe.apply_critical(&b);
            }
        }
        x
    }

    // ------------------------------------------------------------------ activar y restaurar

    /// Identidad del mundo y del shard: qué datos pertenecen a esta carpeta.
    fn world_identity(&self) -> u64 {
        let mut b: Vec<u8> = b"tsofgw/world-identity/v1".to_vec();
        b.extend_from_slice(&self.cfg.seed.to_le_bytes());
        b.extend_from_slice(&(self.cfg.npc_count as u64).to_le_bytes());
        if let Some(c) = &self.cluster {
            let (nx, nz, half) = c.cfg.partition.shape();
            b.extend_from_slice(&nx.to_le_bytes());
            b.extend_from_slice(&nz.to_le_bytes());
            b.extend_from_slice(&half.to_bits().to_le_bytes());
            b.extend_from_slice(&c.cfg.shard.to_le_bytes());
        }
        let h = tsofgw_pack::sha256(&b);
        u64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]])
    }

    /// Clave de integridad del almacén de este servidor, derivada de un secreto del operador (la clave
    /// maestra, o la de cluster). Cada shard deriva la suya.
    pub fn store_key(&self, secret: &[u8]) -> [u8; 32] {
        crate::persist::store_key_for(secret, self.shard_id())
    }

    /// Conecta un almacén durable. Debe hacerse antes de la primera actualización. Si la carpeta está
    /// vacía, inicia el mundo (y guarda quién es dueño de qué); si ya tiene datos, **restaura** el
    /// estado en que quedó el servidor. `key` es la clave de integridad (véase [`GameServer::store_key`]).
    ///
    /// Un almacén de otro mundo u otro shard se rechaza, y un registro dañado en medio del diario es un
    /// error (salvo con [`OpenMode::Repair`], que corta el diario en el primer daño y sigue).
    pub fn enable_persistence(
        &mut self,
        medium: Box<dyn Medium>,
        key: &[u8; 32],
        cfg: PersistConfig,
        mode: OpenMode,
    ) -> Result<RestoreReport, String> {
        if self.persist.is_some() {
            return Err("la persistencia ya está activada".into());
        }
        if self.tick != 0 || !self.players.is_empty() {
            return Err("la persistencia debe activarse antes de la primera actualización".into());
        }
        let scfg = StoreConfig {
            mode,
            ..StoreConfig::default()
        };
        let (store, recovery) = Store::open(medium, key, scfg)
            .map_err(|e| format!("no se pudo abrir el almacén: {e}"))?;
        let identity = self.world_identity();
        let mut pe = Persist::new(store, cfg, self.npcs.len());
        let mut report = RestoreReport {
            recovery,
            ..RestoreReport::default()
        };
        match pe.store.get(K_INIT).map(decode_init) {
            None => {
                report.first_start = true;
                self.first_start(&mut pe, identity)?;
            }
            Some(Some(id)) if id == identity => self.restore(&mut pe, &mut report)?,
            Some(Some(_)) => {
                return Err(
                    "la carpeta de datos pertenece a otro mundo u otro shard (otra semilla, \
                            otro número de NPC u otra rejilla)"
                        .into(),
                );
            }
            Some(None) => return Err("registro de identidad dañado en el almacén".into()),
        }
        self.persist = Some(pe);
        Ok(report)
    }

    fn npc_rec(&self, idx: usize) -> Option<NpcRec> {
        let pos = self.world.position(NPC_BASE + idx as u64)?;
        let n = &self.npcs[idx];
        Some(NpcRec {
            pos: [pos.x, pos.y, pos.z],
            dir: n.dir,
            turn_in: n.turn_in,
            rng: n.rng.state(),
        })
    }

    /// Primer arranque: anota quién es el mundo y todos los NPC que este servidor simula.
    fn first_start(&mut self, pe: &mut Persist, identity: u64) -> Result<(), String> {
        let io = |e: tsofgw_store::StoreError| format!("almacén: {e}");
        let mut b = Batch::new();
        b.put(K_INIT, encode_init(identity));
        b.put(K_TICK, encode_u32(self.tick));
        pe.xfer_reserved = XFER_BLOCK;
        b.put(K_XFER, encode_u32(XFER_BLOCK));
        pe.store.apply(&b).map_err(io)?;
        let mut chunk = Batch::new();
        for idx in 0..self.npcs.len() {
            if self.ents.npc_own(idx) != Own::Owned {
                continue;
            }
            let Some(r) = self.npc_rec(idx) else {
                continue;
            };
            chunk.put(k_npc(idx), r.encode());
            if chunk.len() >= 2048 {
                pe.store.apply(&chunk).map_err(io)?;
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            pe.store.apply(&chunk).map_err(io)?;
        }
        pe.store.commit().map_err(io)?;
        // El estado inicial pasa al checkpoint: el diario empieza vacío.
        pe.store.checkpoint().map_err(io)?;
        Ok(())
    }

    /// Reconstruye el estado en memoria a partir del almacén.
    fn restore(&mut self, pe: &mut Persist, report: &mut RestoreReport) -> Result<(), String> {
        let bad = |what: &str| format!("almacén dañado: {what}");
        let n = self.npcs.len();
        // El reloj sigue creciendo entre reinicios: los vecinos descartan por número de tick lo que
        // llega «más viejo», y un reinicio que volviera a cero sería ignorado hasta que caducaran las
        // copias. Se salta lo que pudo haberse enviado desde el último guardado.
        let saved = pe
            .store
            .get(K_TICK)
            .and_then(decode_u32)
            .ok_or_else(|| bad("falta el reloj"))?;
        self.tick = saved.saturating_add(pe.cfg.npc_checkpoint_ticks.saturating_mul(2).max(2));
        let tick = self.tick;
        pe.last_sync = tick;
        pe.last_checkpoint = tick;
        pe.xfer_reserved = pe
            .store
            .get(K_XFER)
            .and_then(decode_u32)
            .ok_or_else(|| bad("falta la reserva de traspasos"))?;
        if let Some(c) = self.cluster.as_mut() {
            c.set_next_xfer(pe.xfer_reserved);
        }

        // Todo NPC empieza como «no lo tengo»; el almacén dice cuáles sí.
        for idx in 0..n {
            if self.ents.npc_own(idx) != Own::Absent {
                self.world.despawn(NPC_BASE + idx as u64);
                self.ents.set_npc_own(idx, Own::Absent);
            }
        }
        for (k, v) in pe.store.scan(PREFIX_NPC) {
            let idx = idx_of(k, PREFIX_NPC)
                .filter(|&i| i < n)
                .ok_or_else(|| bad("NPC fuera de rango"))?;
            let r = NpcRec::decode(v).ok_or_else(|| bad("registro de NPC"))?;
            self.place_npc(idx, r.pos, r.dir, r.turn_in, r.rng, Own::Owned);
            report.npcs += 1;
        }
        for (k, v) in pe.store.scan(PREFIX_XO) {
            let idx = idx_of(k, PREFIX_XO)
                .filter(|&i| i < n)
                .ok_or_else(|| bad("traspaso de NPC fuera de rango"))?;
            let r = XoRec::decode(v).ok_or_else(|| bad("registro de traspaso de NPC"))?;
            if self.ents.npc_own(idx) != Own::Absent || r.msg.id != NPC_BASE + idx as u64 {
                return Err(bad("un NPC consta a la vez como propio y en traspaso"));
            }
            self.place_npc(
                idx,
                r.msg.pos,
                r.msg.dir,
                r.msg.turn_in,
                r.msg.rng,
                Own::Transferring,
            );
            let c = self
                .cluster
                .as_mut()
                .ok_or_else(|| bad("traspasos de NPC en un servidor sin cluster"))?;
            c.restore_npc_out(r.msg.id, r.to, r.msg, tick);
            report.npc_transfers += 1;
        }
        for (k, _) in pe.store.scan(PREFIX_XI) {
            let (from, xfer) = xi_of(k).ok_or_else(|| bad("registro de traspaso adoptado"))?;
            let c = self
                .cluster
                .as_mut()
                .ok_or_else(|| bad("traspasos adoptados en un servidor sin cluster"))?;
            c.restore_seen(from, xfer, tick);
            report.seen_transfers += 1;
        }
        for (k, v) in pe.store.scan(PREFIX_HO) {
            let account = account_of(k, PREFIX_HO).ok_or_else(|| bad("clave de traspaso"))?;
            let r = HoRec::decode(v).ok_or_else(|| bad("registro de traspaso de jugador"))?;
            let c = self
                .cluster
                .as_mut()
                .ok_or_else(|| bad("traspasos de jugador en un servidor sin cluster"))?;
            c.restore_outbox(account, r, tick);
            report.handovers += 1;
        }
        report.residents = pe.store.scan(PREFIX_PLAYER).count();
        report.tombstones = pe.store.scan(PREFIX_TOMB).count();
        for (k, v) in pe.store.scan(PREFIX_PLAYER) {
            account_of(k, PREFIX_PLAYER)
                .filter(|&a| a < NPC_BASE)
                .ok_or_else(|| bad("clave de jugador"))?;
            PlayerRec::decode(v).ok_or_else(|| bad("registro de jugador"))?;
        }
        for (k, v) in pe.store.scan(PREFIX_TOMB) {
            account_of(k, PREFIX_TOMB).ok_or_else(|| bad("clave de lápida"))?;
            TombRec::decode(v).ok_or_else(|| bad("registro de lápida"))?;
        }
        Ok(())
    }

    /// Coloca un NPC con el estado dado y la propiedad indicada.
    fn place_npc(
        &mut self,
        idx: usize,
        pos: [f32; 3],
        dir: [f32; 2],
        turn_in: u32,
        rng: u64,
        own: Own,
    ) {
        let id = NPC_BASE + idx as u64;
        let p = Vec3::new(pos[0], pos[1], pos[2]);
        if self.world.position(id).is_some() {
            self.world.move_to(id, p);
        } else {
            self.world.spawn(id, p);
        }
        self.npcs[idx] = Npc {
            dir,
            turn_in,
            rng: Rng::from_state(rng),
        };
        *self.ents.npc_mut(idx) = EntityState {
            pos: [quantize(p.x), quantize(p.y), quantize(p.z)],
            data: KIND_NPC,
        };
        self.ents.set_npc_own(idx, own);
    }

    // ------------------------------------------------------------------ guardado periódico

    /// Una vez por tick, al final: guarda un trozo de los NPC que cambiaron, las posiciones de los
    /// jugadores que le toca guardar, el reloj, y expulsa a quien lleva demasiado esperando entrar.
    pub(super) fn persist_tick(&mut self) {
        let tick = self.tick;
        let Some(pe) = self.persist.as_mut() else {
            return;
        };
        if pe.failed {
            return;
        }
        let mut b = Batch::new();

        // NPC: cada tick se revisa un trozo del conjunto, de modo que en `npc_checkpoint_ticks` ticks se
        // ha pasado por todos (el coste se reparte en lugar de concentrarse en un tick).
        let n = self.npcs.len();
        let k = pe.cfg.npc_checkpoint_ticks.max(1) as usize;
        let chunk = n.div_ceil(k).max(1);
        let start = (tick as usize % k) * chunk;
        let mut saved = 0u64;
        for idx in start..(start + chunk).min(n) {
            if !pe.is_dirty(idx) {
                continue;
            }
            pe.clear_npc(idx);
            if self.ents.npc_own(idx) != Own::Owned {
                continue;
            }
            let id = NPC_BASE + idx as u64;
            let Some(pos) = self.world.position(id) else {
                continue;
            };
            let npc = &self.npcs[idx];
            b.put(
                k_npc(idx),
                NpcRec {
                    pos: [pos.x, pos.y, pos.z],
                    dir: npc.dir,
                    turn_in: npc.turn_in,
                    rng: npc.rng.state(),
                }
                .encode(),
            );
            saved += 1;
        }
        pe.stats.npc_saves += saved;

        // Jugadores conectados: a cada uno le toca guardarse una vez cada `player_save_ticks`, escalonado
        // por cuenta, y solo si se movió desde la última vez.
        let every = pe.cfg.player_save_ticks.max(1) as u64;
        let mut psaved = 0u64;
        for p in self.players.values() {
            if !p.joined || p.frozen() {
                continue;
            }
            if p.entity.wrapping_add(tick as u64) % every != 0 {
                continue;
            }
            if pe.saved_pos.get(&p.entity) == Some(&p.state.pos) {
                continue;
            }
            b.put(
                k_player(p.entity),
                PlayerRec {
                    pos: p.state.pos,
                    epoch: p.epoch,
                }
                .encode(),
            );
            pe.saved_pos.insert(p.entity, p.state.pos);
            psaved += 1;
        }
        pe.stats.player_saves += psaved;

        if (tick as usize).is_multiple_of(k) {
            b.put(K_TICK, encode_u32(tick));
        }
        if !b.is_empty() {
            pe.apply(&b);
        }

        // Sesiones que llevan demasiado esperando entrar.
        if tick.is_multiple_of(20) {
            let limit = pe.cfg.login_timeout_ticks;
            let late: Vec<SessionId> = self
                .players
                .iter()
                .filter(|(_, p)| !p.joined && tick.wrapping_sub(p.created_tick) > limit)
                .map(|(&s, _)| s)
                .collect();
            for s in late {
                if let Some(pe) = self.persist.as_mut() {
                    pe.stats.joins_timed_out += 1;
                }
                self.kick(s);
            }
        }
    }

    /// Al final de cada `update`: sincroniza si lo escrito sostiene alguna promesa (o si ya toca) y solo
    /// entonces suelta los mensajes que esperaban; compacta el diario si ha crecido.
    pub(super) fn persist_flush(&mut self) {
        let tick = self.tick;
        let Some(pe) = self.persist.as_mut() else {
            return;
        };
        let due = pe.store.unsynced() > 0 && tick.wrapping_sub(pe.last_sync) >= pe.cfg.sync_ticks;
        if !pe.failed && (pe.critical || due) {
            if pe.commit() {
                pe.last_sync = tick;
            }
            pe.critical = false;
        }
        if !pe.failed {
            match pe.store.checkpoint_if_larger(pe.cfg.compact_bytes) {
                Ok(true) => {
                    pe.stats.compactions += 1;
                    pe.last_checkpoint = tick;
                }
                Ok(false) => {}
                Err(_) => {
                    pe.failed = true;
                    pe.stats.commit_errors += 1;
                }
            }
        }
        let deferred = pe.take_deferred();
        if pe.failed {
            pe.stats.deferred_dropped += deferred.len() as u64;
            return;
        }
        pe.stats.deferred_sent += deferred.len() as u64;
        for (to, msg) in deferred {
            self.peer_send(to, &msg);
        }
    }

    /// Cierre limpio: guarda todo lo pendiente (NPC, jugadores conectados), sincroniza y compacta, y
    /// avisa a los clientes para que no esperen al plazo del transporte y vuelvan a conectar en cuanto
    /// el servidor esté de nuevo en pie. Un cierre limpio no pierde nada de movimiento; una caída
    /// pierde, como mucho, el último intervalo.
    ///
    /// Tras llamarlo el servidor ya no sirve para nada más: hay que dejarlo caer. Aunque el almacén
    /// hubiera fallado (y por eso devuelva error), los clientes reciben igualmente el aviso.
    pub fn shutdown(&mut self) -> Result<(), String> {
        let saved = self.shutdown_store();
        let sessions: Vec<SessionId> = self.players.keys().copied().collect();
        for s in sessions {
            self.net.kick(s);
        }
        self.players.clear();
        self.by_entity.clear();
        saved
    }

    fn shutdown_store(&mut self) -> Result<(), String> {
        let tick = self.tick;
        let Some(pe) = self.persist.as_mut() else {
            return Ok(());
        };
        if pe.failed {
            return Err(
                "el almacén había fallado: se conserva lo que ya estaba sincronizado".into(),
            );
        }
        let mut b = Batch::new();
        for idx in 0..self.npcs.len() {
            if !pe.is_dirty(idx) {
                continue;
            }
            pe.clear_npc(idx);
            if self.ents.npc_own(idx) != Own::Owned {
                continue;
            }
            if let Some(pos) = self.world.position(NPC_BASE + idx as u64) {
                let npc = &self.npcs[idx];
                b.put(
                    k_npc(idx),
                    NpcRec {
                        pos: [pos.x, pos.y, pos.z],
                        dir: npc.dir,
                        turn_in: npc.turn_in,
                        rng: npc.rng.state(),
                    }
                    .encode(),
                );
            }
            if b.len() >= 2048 {
                pe.apply(&b);
                b.clear();
            }
        }
        for p in self.players.values() {
            if p.joined && !p.frozen() {
                b.put(
                    k_player(p.entity),
                    PlayerRec {
                        pos: p.state.pos,
                        epoch: p.epoch,
                    }
                    .encode(),
                );
            }
        }
        b.put(K_TICK, encode_u32(tick));
        if !pe.apply(&b) || pe.store.commit().is_err() {
            pe.failed = true;
            return Err("no se pudo escribir el cierre en el almacén".into());
        }
        pe.store
            .checkpoint()
            .map_err(|e| format!("no se pudo compactar el almacén: {e}"))?;
        pe.stats.commits += 1;
        pe.stats.compactions += 1;
        Ok(())
    }

    /// La mayor época que este shard conoce de una cuenta: la que tiene como residente, la de su lápida
    /// (se fue) o la del traspaso que tiene entregado. Un `HANDOVER` que no la supere es una repetición.
    pub(super) fn known_epoch(&self, account: u64) -> u64 {
        let Some(pe) = self.persist.as_ref() else {
            return 0;
        };
        let p = pe
            .store
            .get(&k_player(account))
            .and_then(PlayerRec::decode)
            .map_or(0, |r| r.epoch);
        let t = pe
            .store
            .get(&k_tomb(account))
            .and_then(TombRec::decode)
            .map_or(0, |r| r.epoch);
        let h = pe
            .store
            .get(&k_ho(account))
            .and_then(HoRec::decode)
            .map_or(0, |r| r.epoch);
        p.max(t).max(h)
    }

    /// Guarda la posición final de un jugador que se va (o que fue expulsado).
    pub(super) fn persist_player_gone(&mut self, entity: EntityId, pos: [i32; 3], epoch: u64) {
        if let Some(pe) = self.persist.as_mut() {
            let mut b = Batch::new();
            b.put(k_player(entity), PlayerRec { pos, epoch }.encode());
            pe.apply(&b);
            pe.saved_pos.remove(&entity);
        }
    }

    // ------------------------------------------------------------------ entrada de jugadores

    /// Posición de aparición de una cuenta nueva, ajustada a la región de este shard.
    fn fresh_position(&self, account: u64) -> [i32; 3] {
        let mut pos = Self::spawn_position(&self.cfg, account);
        if let Some(c) = self.cluster.as_ref() {
            let v = c.region().clamp_inside(vec3_from_q(pos), 1.0);
            pos = [quantize(v.x), pos[1], quantize(v.z)];
        }
        pos
    }

    /// Decide qué hacer con una sesión que pide entrar (`JOIN`).
    pub(super) fn login_decision(&mut self, session: SessionId) -> Login {
        let tick = self.tick;
        let Some(account) = self.players.get(&session).map(|p| p.player_id) else {
            return Login::Wait;
        };
        // ¿Es un jugador que otro shard nos está traspasando?
        if let Some(c) = self.cluster.as_mut() {
            if c.register_arrival(account, session) {
                return Login::Arriving;
            }
        }
        let fresh = self.fresh_position(account);
        let Some(pe) = self.persist.as_ref() else {
            // Sin almacén: como siempre, aparece en su punto de partida.
            return Login::Spawn {
                pos: fresh,
                epoch: 0,
                restored: false,
            };
        };
        // Una cuenta que este shard acaba de entregar espera a que se sepa el resultado del traspaso:
        // no puede entrar ni aquí ni en el destino hasta entonces.
        if self.cluster.as_ref().is_some_and(|c| c.outbox_has(account)) {
            return Login::Wait;
        }
        let rec = pe.store.get(&k_player(account)).and_then(PlayerRec::decode);
        let tomb = pe.store.get(&k_tomb(account)).and_then(TombRec::decode);
        let me = self.cluster.as_ref().map(|c| c.cfg.shard);
        let dir = self.cluster.as_ref().and_then(|c| c.dir_peer());
        // El shard «de origen» de una cuenta es el que posee su punto de aparición. Una cuenta nueva solo
        // se crea allí: así un shard que no la conoce no la inventa mientras la de verdad viaja en un
        // traspaso (el de origen siempre deja rastro: residente, traspaso en curso o lápida).
        let home = self.cluster.as_ref().map_or(0, |c| {
            let spawn = Self::spawn_position(&self.cfg, account);
            c.cfg.partition.owner_of(vec3_from_q(spawn))
        });
        let Some(dir) = dir else {
            // Sin directorio: cada shard decide por lo que tiene.
            return match (rec, tomb) {
                (Some(r), _) => Login::Spawn {
                    pos: r.pos,
                    epoch: r.epoch,
                    restored: true,
                },
                // Se fue de aquí hacia otro shard: la lápida es un «puntero de reenvío» que lleva al
                // cliente a donde fue (y, si aquel shard también la entregó, él lo reenvía más lejos).
                (None, Some(t)) if Some(t.to) != me => Login::Redirect(t.to),
                // Ni la tiene ni la ha visto pasar: solo su shard de origen puede crearla.
                (None, _) if me.is_some_and(|m| m != home) => Login::Redirect(home),
                (None, _) => Login::Spawn {
                    pos: fresh,
                    epoch: 1,
                    restored: false,
                },
            };
        };
        // Con directorio: se pregunta y se espera la respuesta.
        let Some(me) = me else {
            return Login::Wait;
        };
        let Some(c) = self.cluster.as_mut() else {
            return Login::Wait;
        };
        let Some(route) = c.route(account, tick) else {
            if c.should_ask_dir(account, tick) {
                c.stats.dir_logins += 1;
                self.peer_send(dir, &PeerMsg::DirLogin { account, home });
            }
            return Login::Wait;
        };
        if route.shard != me {
            c.consume_route(account);
            c.stats.login_redirects += 1;
            return Login::Redirect(route.shard);
        }
        match rec {
            Some(r) if r.epoch >= route.epoch => {
                c.consume_route(account);
                Login::Spawn {
                    pos: r.pos,
                    epoch: r.epoch,
                    restored: true,
                }
            }
            None if route.epoch == 1 => {
                c.consume_route(account);
                Login::Spawn {
                    pos: fresh,
                    epoch: 1,
                    restored: false,
                }
            }
            // La cuenta es nuestra según el directorio, pero su estado aún no ha llegado (viene en un
            // traspaso en curso): se espera. El origen lo reenvía hasta que lo acusemos.
            _ => {
                c.stats.login_waits += 1;
                Login::Wait
            }
        }
    }

    /// Un cliente envió `JOIN`: entra, es redirigido, o espera.
    pub(super) fn on_join(&mut self, session: SessionId) {
        let tick = self.tick;
        let Some(p) = self.players.get_mut(&session) else {
            return;
        };
        p.join_requested = true;
        if !p.joined {
            match self.login_decision(session) {
                // El jugador llegará con el estado del traspaso: no hay bienvenida todavía.
                Login::Arriving => {}
                Login::Wait => return,
                Login::Redirect(to) => {
                    self.send_redirect(session, to);
                    return;
                }
                Login::Spawn {
                    pos,
                    epoch,
                    restored,
                } => {
                    self.spawn_player(session, pos, epoch, restored);
                }
            }
        }
        let Some(p) = self.players.get_mut(&session) else {
            return; // expulsado al intentar entrar
        };
        if p.joined {
            let welcome = Welcome {
                entity: p.entity,
                tick,
                tick_ms: self.cfg.tick_ms as u16,
                pos: p.state.pos,
            };
            let hdr = p.endpoint.next_header();
            let msg = proto::frame(hdr, proto::WELCOME, &proto::welcome_body(&welcome));
            let _ = self.net.send(session, &msg);
        }
    }

    /// Da de alta a un jugador en el mundo.
    fn spawn_player(&mut self, session: SessionId, pos: [i32; 3], epoch: u64, restored: bool) {
        let Some(p) = self.players.get_mut(&session) else {
            return;
        };
        // Una copia con ese id solo puede ser de una sesión anterior: se sustituye.
        if let Some(c) = self.cluster.as_mut() {
            if c.take_ghost(p.entity) {
                self.world.despawn(p.entity);
                self.ents.remove_player(p.entity);
            }
        }
        p.state = PlayerState { pos };
        if self.world.spawn(p.entity, vec3_from_q(pos)) {
            self.ents.set_player(
                p.entity,
                EntityState {
                    pos,
                    data: KIND_PLAYER,
                },
            );
            p.joined = true;
            p.epoch = epoch;
            self.by_entity.insert(p.entity, session);
            self.stats.joins += 1;
            let entity = p.entity;
            if let Some(pe) = self.persist.as_mut() {
                let mut b = Batch::new();
                b.put(k_player(entity), PlayerRec { pos, epoch }.encode());
                pe.apply(&b);
                pe.saved_pos.insert(entity, pos);
                if restored {
                    pe.stats.players_restored += 1;
                }
            }
        } else {
            // La entidad ya existe: no debería pasar; se rechaza esta sesión.
            self.stats.refused += 1;
            self.kick(session);
        }
    }
}
