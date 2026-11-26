//! Banco de pruebas de un cluster: varios shards en un proceso, un bus simulado entre ellos, una red
//! simulada hacia los clientes y bots que cruzan los bordes una y otra vez. Produce un informe medido y
//! permite comprobar los invariantes del reparto (ningún NPC ni jugador se pierde o se duplica, y lo
//! que ve un jugador junto al borde coincide con la verdad del conjunto).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::time::Instant;

use tsofgw_store::{CrashKind, MemMedium, OpenMode};
use tsofgw_transport::{
    Authority, ClientConfig, DeterministicEntropy, DisconnectReason, Link, ServerConfig, SimAddr,
    SimConfig, SimLink, SimNet,
};
use tsofgw_world::Rng;

use crate::bus::{SimBus, SimBusConfig};
use crate::client::{NewConnection, Phase};
use crate::directory::{Directory, DirectoryConfig};
use crate::partition::{Partition, ShardId};
use crate::persist::{PersistConfig, RestoreReport};
use crate::server::{ClusterConfig, ClusterStats};
use crate::sim::HALF_EXTENT_Q;
use crate::{Bot, GameClient, GameConfig, GameServer, NPC_BASE};
use tsofgw_net::dequantize;

#[derive(Debug, Clone)]
pub struct ClusterScenario {
    /// Rejilla de shards (columnas × filas).
    pub grid: (u16, u16),
    pub clients: usize,
    /// Cuántos de los clientes patrullan cruzando un borde (el resto deambula).
    pub patrollers: usize,
    pub patrol_ms: u64,
    pub duration_ms: u64,
    /// Tiempo extra sin pérdida y con los bots quietos, para comprobar que todo converge.
    pub settle_ms: u64,
    /// Red entre clientes y shards.
    pub net: SimConfig,
    /// Red entre shards.
    pub bus: SimBusConfig,
    /// `npc_count` es el **total** del mundo, repartido entre los shards.
    pub game: GameConfig,
    pub step_ms: u64,
    pub master: Vec<u8>,
    /// Cada shard guarda su estado en un almacén (en memoria, con caídas y fallos inyectables) y puede
    /// reiniciarse recuperándolo. `None`: como en la 0.4, todo vive solo en memoria.
    pub persist: Option<PersistConfig>,
    /// Hay un directorio de sesiones en el bus (exige `persist`).
    pub directory: bool,
    /// Réplicas del directorio (1, 3, 5 o 7). Con más de una, el directorio es un grupo con Raft.
    pub dir_replicas: usize,
    /// Temporización de Raft de las réplicas del directorio.
    pub dir_raft: crate::raft::RaftConfig,
    /// Los clientes que pierden la conexión vuelven a entrar solos, como haría un juego real.
    pub reconnect: Reconnect,
}

/// Cómo se reconectan los bots cuyo cliente se desconectó (una caída del shard, una expulsión...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconnect {
    pub enabled: bool,
    /// Espera antes de volver a intentarlo (ms), más hasta otro tanto de azar.
    pub delay_ms: u64,
    /// Entran por un shard cualquiera (y el cluster, con directorio, los lleva al suyo), en lugar de
    /// por el shard donde empezaron.
    pub any_shard: bool,
}

impl Default for Reconnect {
    fn default() -> Self {
        Reconnect {
            enabled: false,
            delay_ms: 1000,
            any_shard: false,
        }
    }
}

impl Default for ClusterScenario {
    fn default() -> Self {
        ClusterScenario {
            grid: (2, 2),
            clients: 40,
            patrollers: 20,
            patrol_ms: 8000,
            duration_ms: 30_000,
            settle_ms: 4000,
            net: SimConfig {
                latency_ms: 30,
                jitter_ms: 10,
                loss: 0.02,
                duplicate: 0.0,
                seed: 1,
            },
            bus: SimBusConfig {
                latency_ms: 10,
                jitter_ms: 5,
                loss: 0.02,
                duplicate: 0.0,
                seed: 7,
            },
            game: GameConfig {
                npc_count: 4000,
                // Aparecen cerca del cruce de las regiones: así los bordes se cruzan de verdad.
                spawn_half_extent: 40.0,
                ..GameConfig::default()
            },
            step_ms: 10,
            master: b"cluster-master-key".to_vec(),
            persist: None,
            directory: false,
            dir_replicas: 1,
            dir_raft: crate::raft::RaftConfig::default(),
            reconnect: Reconnect::default(),
        }
    }
}

/// Resultado de comprobar los invariantes del reparto en un instante.
#[derive(Debug, Clone, Default)]
pub struct Invariants {
    pub npcs: usize,
    /// NPC de los que ningún shard es responsable (nunca debe ocurrir, ni siquiera con pérdida).
    pub npc_lost: usize,
    /// NPC con más de un responsable (solo es normal mientras un traspaso espera su acuse).
    pub npc_duplicated: usize,
    pub players_playing: usize,
    pub players_lost: usize,
    pub players_duplicated: usize,
    /// Mensajes aún en vuelo por el bus y traspasos sin acuse en todo el cluster.
    pub bus_in_flight: usize,
    pub pending_transfers: usize,
    pub views_checked: usize,
    /// Entidades que debería ver un jugador (verdad del cluster, bien dentro de su radio) y no estaban.
    pub view_missing: usize,
    /// Entidades que ve un jugador y no existen en ningún shard (o están muy lejos).
    pub view_phantoms: usize,
    /// Mayor diferencia (unidades) entre lo que ve un jugador y la verdad del cluster.
    pub view_error_max: f32,
    /// Lo que dicen los almacenes y el directorio de las cuentas (solo con persistencia).
    pub accounts: AccountInvariants,
}

impl Invariants {
    /// ¿El reparto es perfecto? (solo debe exigirse cuando la red está en calma y no hay traspasos en curso)
    pub fn is_exact(&self) -> bool {
        self.npc_lost == 0
            && self.npc_duplicated == 0
            && self.players_lost == 0
            && self.players_duplicated == 0
            && self.accounts.is_exact()
    }
}

/// Comprobaciones sobre las **cuentas**, hechas con lo que hay en los almacenes de los shards (no con lo
/// que ven los clientes) y, si lo hay, con el directorio.
#[derive(Debug, Clone, Default)]
pub struct AccountInvariants {
    /// Cuentas con registro en algún shard (residentes o en traspaso).
    pub accounts: usize,
    /// Cuentas residentes en **más de un** shard. Nunca debe pasar, ni siquiera a mitad de un traspaso.
    pub resident_duplicates: usize,
    /// Cuentas que llegaron a una época mayor y cuyo registro más alto ya no consta en ningún shard
    /// (se perdió la cuenta, o un estado viejo la resucitó).
    pub lost: usize,
    /// Cuentas residentes en un shard que el directorio sitúa en otro (o con otra época).
    pub dir_mismatch: usize,
    /// Cuentas residentes que el directorio no conoce.
    pub dir_unknown: usize,
    /// Traspasos de jugador entregados y todavía sin acuse, en todo el cluster.
    pub outbox: usize,
}

impl AccountInvariants {
    pub fn is_exact(&self) -> bool {
        self.resident_duplicates == 0
            && self.lost == 0
            && self.dir_mismatch == 0
            && self.dir_unknown == 0
    }
}

/// Cosas que **nunca** deben ocurrir y que el banco de pruebas vigila cada 100 ms simulados mientras el
/// cluster está en marcha (también durante caídas, particiones y reinicios). Cada campo guarda el mayor
/// valor visto.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Violations {
    /// Cuentas residentes en más de un shard a la vez.
    pub resident_duplicates: usize,
    /// Cuentas que dos shards simulan **a la vez** (conectadas y no congeladas). La autoridad sobre una
    /// cuenta es siempre de un solo shard.
    pub simulated_twice: usize,
    /// Cuentas con un registro más alto visto antes y que ya no consta en ningún shard.
    pub lost_accounts: usize,
    /// Cuentas residentes en un shard que el directorio sitúa en otro.
    pub dir_mismatch: usize,
    /// NPC de los que nadie era responsable, **con todos los shards en pie**.
    pub npc_lost: usize,
}

/// Un cluster en marcha. Las pruebas pueden avanzarlo paso a paso e inspeccionarlo y, con persistencia,
/// tirar y reiniciar shards y el directorio de sesiones.
pub struct ClusterRun {
    pub net: SimNet,
    pub bus: SimBus,
    pub partition: Partition,
    /// Un shard caído ocupa su sitio con un servidor vacío (no simula ni guarda nada) hasta que se
    /// reinicia con [`ClusterRun::restart_shard`].
    pub shards: Vec<GameServer<SimLink>>,
    pub bots: Vec<Bot<SimLink>>,
    /// Clientes añadidos a mano con [`ClusterRun::spawn_duplicate_bot`] (no se reconectan solos).
    pub extras: Vec<Bot<SimLink>>,
    /// El directorio de sesiones (`None` si no lo hay o está caído).
    pub dir: Option<Directory>,
    /// Los «discos»: siguen ahí cuando el proceso cae.
    pub media: Vec<MemMedium>,
    pub dir_medium: Option<MemMedium>,
    /// Las réplicas del directorio cuando es un grupo (`dir_replicas > 1`); `None` = caída.
    pub reps: Vec<Option<Directory>>,
    pub rep_media: Vec<MemMedium>,
    pub rep_restarts: u64,
    rep_incarnation: Vec<u32>,
    pub now: u64,
    pub step_ms: u64,
    /// Cómo vuelven a entrar los bots que se desconectan.
    pub reconnect: Reconnect,
    /// Veces que se reinició un shard o el directorio, y reconexiones de bots.
    pub restarts: u64,
    pub dir_restarts: u64,
    pub reconnects: u64,
    pub violations: Violations,
    /// Descripción de lo que se vio la primera vez que se violó un invariante (para diagnosticar).
    pub first_violation: Option<String>,
    /// Lo que encontró cada reinicio al restaurar: `(shard, informe)`.
    pub restores: Vec<(usize, RestoreReport)>,
    /// Contadores de las vidas anteriores de los shards que cayeron.
    dead_stats: ClusterStats,
    scenario: ClusterScenario,
    authority: Authority,
    addrs: Vec<SimAddr>,
    homes: Vec<usize>,
    down: Vec<bool>,
    incarnation: Vec<u32>,
    dir_incarnation: u32,
    frozen: bool,
    wall: Instant,
    rng: Rng,
    reconnect_at: Vec<Option<u64>>,
    attempts: Vec<u64>,
    /// Mayor época vista (en un residente o en un traspaso) de cada cuenta que ya pasó de la primera.
    epoch_floor: BTreeMap<u64, u64>,
}

/// Cuenta del bot `i`.
pub fn bot_account(i: usize) -> u64 {
    1000 + i as u64
}

#[allow(clippy::too_many_arguments)]
fn build_bot(
    s: &ClusterScenario,
    net: &SimNet,
    addrs: &[SimAddr],
    authority: &Authority,
    i: usize,
    target: usize,
    attempt: u64,
) -> Bot<SimLink> {
    let (net2, addrs2, seed) = (net.clone(), addrs.to_vec(), s.net.seed);
    let mut attempts = 0u64;
    let client = GameClient::new(
        net.endpoint(),
        addrs[target],
        authority.issue(bot_account(i)),
        ClientConfig::default(),
        Box::new(DeterministicEntropy(
            s.net.seed.wrapping_mul(31) + i as u64 + (attempt << 40),
        )),
    )
    .with_connector(Box::new(move |shard| {
        attempts += 1;
        addrs2.get(shard as usize).map(|&server| NewConnection {
            link: net2.endpoint(),
            server,
            entropy: Box::new(DeterministicEntropy(
                seed ^ ((i as u64) << 24) ^ (attempts + attempt * 1000).wrapping_mul(0x9e37),
            )),
        })
    }));
    let mut bot = Bot::new(
        client,
        s.net.seed.wrapping_add(i as u64 * 7919 + attempt * 104_729),
    );
    if i < s.patrollers {
        bot = bot.with_patrol(patrol_dir(i), s.patrol_ms);
    }
    bot
}

impl ClusterRun {
    pub fn new(s: &ClusterScenario) -> Result<ClusterRun, String> {
        if s.directory && s.persist.is_none() {
            return Err("el directorio de sesiones exige persistencia en los shards".into());
        }
        let partition = Partition::grid(s.grid.0, s.grid.1, dequantize(HALF_EXTENT_Q))?;
        let n = partition.shard_count();
        let net = SimNet::new(s.net.clone());
        if s.directory && !matches!(s.dir_replicas, 1 | 3 | 5 | 7) {
            return Err("las réplicas del directorio deben ser 1, 3, 5 o 7".into());
        }
        let nrep = if s.directory {
            s.dir_replicas.max(1)
        } else {
            0
        };
        let bus = SimBus::new(n + nrep, s.bus.clone());
        let authority = Authority::new(&s.master);
        let mut shards = Vec::with_capacity(n);
        let mut media = Vec::new();
        for sh in 0..n as ShardId {
            let mut cluster = ClusterConfig::new(sh, partition.clone(), &s.game)?;
            cluster.directory = s.directory;
            cluster.dir_replicas = s.dir_replicas.max(1);
            let mut g = GameServer::new_shard(
                net.endpoint(),
                authority.clone(),
                s.game.clone(),
                ServerConfig::default(),
                Box::new(DeterministicEntropy(
                    s.net.seed ^ 0xabcd ^ ((sh as u64) << 32),
                )),
                cluster,
                Box::new(bus.end(sh)),
            )?;
            if let Some(pc) = &s.persist {
                let medium = MemMedium::new();
                let key = g.store_key(&s.master);
                g.enable_persistence(
                    Box::new(medium.clone()),
                    &key,
                    pc.clone(),
                    OpenMode::Recover,
                )?;
                media.push(medium);
            }
            shards.push(g);
        }
        let addrs: Vec<_> = shards.iter().map(|g| g.link().addr()).collect();

        let mut reps = Vec::new();
        let mut rep_media = Vec::new();
        let (dir, dir_medium) = if s.directory && s.dir_replicas > 1 {
            for i in 0..s.dir_replicas {
                let medium = MemMedium::new();
                let (d, _) = Directory::open(
                    Box::new(medium.clone()),
                    &Directory::store_key(&s.master),
                    Box::new(bus.end((n + i) as ShardId)),
                    n,
                    replica_config(s.dir_replicas, i, &s.dir_raft),
                    OpenMode::Recover,
                )?;
                reps.push(Some(d));
                rep_media.push(medium);
            }
            (None, None)
        } else if s.directory {
            let medium = MemMedium::new();
            let (d, _) = Directory::open(
                Box::new(medium.clone()),
                &Directory::store_key(&s.master),
                Box::new(bus.end(n as ShardId)),
                n,
                DirectoryConfig::default(),
                OpenMode::Recover,
            )?;
            (Some(d), Some(medium))
        } else {
            (None, None)
        };

        let mut bots = Vec::with_capacity(s.clients);
        let mut homes = Vec::with_capacity(s.clients);
        for i in 0..s.clients {
            // Un servicio de entrada elegiría el shard según dónde aparece la cuenta.
            let spawn = GameServer::<SimLink>::spawn_position(&s.game, bot_account(i));
            let home = partition.owner_of(tsofgw_world::Vec3::new(
                dequantize(spawn[0]),
                0.0,
                dequantize(spawn[2]),
            )) as usize;
            homes.push(home);
            bots.push(build_bot(s, &net, &addrs, &authority, i, home, 0));
        }
        Ok(ClusterRun {
            net,
            bus,
            partition,
            shards,
            bots,
            extras: Vec::new(),
            dir,
            media,
            dir_medium,
            reps,
            rep_media,
            rep_restarts: 0,
            rep_incarnation: vec![0; s.dir_replicas.max(1)],
            now: 0,
            step_ms: s.step_ms,
            reconnect: s.reconnect,
            restarts: 0,
            dir_restarts: 0,
            reconnects: 0,
            violations: Violations::default(),
            first_violation: None,
            restores: Vec::new(),
            dead_stats: ClusterStats::default(),
            scenario: s.clone(),
            authority,
            addrs,
            homes,
            down: vec![false; n],
            incarnation: vec![0; n],
            dir_incarnation: 0,
            frozen: false,
            wall: Instant::now(),
            rng: Rng::new(s.net.seed ^ 0x5eed),
            reconnect_at: vec![None; s.clients],
            attempts: vec![0; s.clients],
            epoch_floor: BTreeMap::new(),
        })
    }

    /// Avanza un paso de reloj: cada shard, el directorio y cada cliente se actualizan una vez.
    pub fn step(&mut self) {
        self.now += self.step_ms;
        self.net.set_now(self.now);
        for (i, sh) in self.shards.iter_mut().enumerate() {
            if !self.down[i] {
                sh.update(self.now);
            }
        }
        if let Some(d) = self.dir.as_mut() {
            d.update(self.now);
        }
        for d in self.reps.iter_mut().flatten() {
            d.update(self.now);
        }
        for b in self.bots.iter_mut().chain(self.extras.iter_mut()) {
            b.update(self.now);
        }
        if self.reconnect.enabled {
            self.reconnect_dead_bots();
        }
        if self.scenario.persist.is_some() && self.now / 100 != (self.now - self.step_ms) / 100 {
            self.observe();
        }
    }

    pub fn run_for(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.step();
        }
    }

    /// Detiene a los bots y quita la pérdida de las dos redes: a partir de aquí todo debe converger.
    pub fn settle(&mut self) {
        if self.frozen {
            return;
        }
        self.frozen = true;
        for b in self.bots.iter_mut().chain(self.extras.iter_mut()) {
            b.freeze();
        }
        let mut quiet = self.scenario.net.clone();
        quiet.loss = 0.0;
        quiet.duplicate = 0.0;
        self.net.set_config(quiet);
        let mut qb = self.scenario.bus.clone();
        qb.loss = 0.0;
        qb.duplicate = 0.0;
        self.bus.set_config(qb);
    }

    // ------------------------------------------------------------------ caídas y reinicios

    pub fn is_down(&self, shard: usize) -> bool {
        self.down.get(shard).copied().unwrap_or(false)
    }

    pub fn any_down(&self) -> bool {
        self.down.iter().any(|&d| d)
            || (self.scenario.directory
                && if self.scenario.dir_replicas > 1 {
                    self.reps.iter().any(Option::is_none)
                } else {
                    self.dir.is_none()
                })
    }

    /// Un servidor vacío que ocupa el sitio de un shard caído (no se actualiza nunca).
    fn placeholder(&self, sh: usize) -> GameServer<SimLink> {
        let game = GameConfig {
            npc_count: 0,
            ..self.scenario.game.clone()
        };
        let cluster = ClusterConfig::new(sh as ShardId, self.partition.clone(), &game)
            .expect("la configuración ya se validó al arrancar");
        GameServer::new_shard(
            self.net.endpoint(),
            self.authority.clone(),
            game,
            ServerConfig::default(),
            Box::new(DeterministicEntropy(0)),
            cluster,
            Box::new(self.bus.end(sh as ShardId)),
        )
        .expect("la configuración ya se validó al arrancar")
    }

    /// El proceso de un shard muere de golpe: se pierde su memoria y, del disco, lo que `kind` diga que
    /// no estaba sincronizado. Lo que llegue por el bus o por la red mientras está caído se pierde.
    pub fn crash_shard(&mut self, sh: usize, kind: CrashKind) {
        if self.down[sh] {
            return;
        }
        let placeholder = self.placeholder(sh);
        let dead = std::mem::replace(&mut self.shards[sh], placeholder);
        if let Some(st) = dead.cluster_stats() {
            self.dead_stats.merge(st);
        }
        drop(dead);
        if let Some(m) = self.media.get(sh) {
            m.crash(kind);
        }
        self.bus.isolate(sh as ShardId, true);
        self.bus.clear_inbox(sh as ShardId);
        self.down[sh] = true;
    }

    /// Arranca de nuevo un shard caído sobre su disco, en la misma dirección de red.
    pub fn restart_shard(&mut self, sh: usize) -> Result<RestoreReport, String> {
        if !self.down[sh] {
            return Err("el shard no está caído".into());
        }
        self.incarnation[sh] += 1;
        let link = self.net.rebind(self.addrs[sh]);
        let s = &self.scenario;
        let mut cluster = ClusterConfig::new(sh as ShardId, self.partition.clone(), &s.game)?;
        cluster.directory = s.directory;
        cluster.dir_replicas = s.dir_replicas.max(1);
        let mut g = GameServer::new_shard(
            link,
            self.authority.clone(),
            s.game.clone(),
            ServerConfig::default(),
            Box::new(DeterministicEntropy(
                s.net.seed ^ 0xabcd ^ ((sh as u64) << 32) ^ ((self.incarnation[sh] as u64) << 48),
            )),
            cluster,
            Box::new(self.bus.end(sh as ShardId)),
        )?;
        let report = match (&s.persist, self.media.get(sh)) {
            (Some(pc), Some(m)) => {
                let key = g.store_key(&s.master);
                g.enable_persistence(Box::new(m.clone()), &key, pc.clone(), OpenMode::Recover)?
            }
            _ => RestoreReport::default(),
        };
        self.shards[sh] = g;
        self.bus.clear_inbox(sh as ShardId);
        self.bus.isolate(sh as ShardId, false);
        self.down[sh] = false;
        self.restarts += 1;
        self.restores.push((sh, report.clone()));
        Ok(report)
    }

    /// Contadores de todo el cluster desde el principio, sumando también las vidas anteriores de los
    /// shards que cayeron.
    pub fn total_stats(&self) -> ClusterStats {
        let mut t = self.dead_stats.clone();
        for sh in &self.shards {
            if let Some(st) = sh.cluster_stats() {
                t.merge(st);
            }
        }
        t
    }

    /// Cierre ordenado de un shard (guarda todo y compacta) y arranque de nuevo.
    pub fn restart_shard_cleanly(&mut self, sh: usize) -> Result<RestoreReport, String> {
        self.shards[sh].shutdown()?;
        self.crash_shard(sh, CrashKind::Clean);
        self.restart_shard(sh)
    }

    /// Con réplicas: la que lidera ahora (y está viva), según ella misma.
    pub fn leader_replica(&self) -> Option<usize> {
        self.reps
            .iter()
            .position(|d| d.as_ref().is_some_and(|d| d.is_leader()))
    }

    /// La vista del directorio que vale para comprobar: el directorio solo, o el líder del grupo (que ya aplicó
    /// todo lo que ha contestado). Sin líder, `None` y no se comprueba nada contra el directorio.
    fn dir_view(&self) -> Option<&Directory> {
        if self.scenario.dir_replicas > 1 {
            self.leader_replica().and_then(|i| self.reps[i].as_ref())
        } else {
            self.dir.as_ref()
        }
    }

    /// La réplica `i` del directorio muere de golpe.
    pub fn crash_replica(&mut self, i: usize, kind: CrashKind) {
        if self.reps.get_mut(i).and_then(Option::take).is_none() {
            return;
        }
        self.rep_media[i].crash(kind);
        let id = (self.partition.shard_count() + i) as ShardId;
        self.bus.isolate(id, true);
        self.bus.clear_inbox(id);
    }

    /// Arranca de nuevo la réplica `i` sobre su disco.
    pub fn restart_replica(&mut self, i: usize) -> Result<(), String> {
        if self.reps.get(i).is_none_or(Option::is_some) {
            return Err("la réplica no está caída".into());
        }
        self.rep_incarnation[i] += 1;
        let n = self.partition.shard_count();
        let (d, _) = Directory::open(
            Box::new(self.rep_media[i].clone()),
            &Directory::store_key(&self.scenario.master),
            Box::new(self.bus.end((n + i) as ShardId)),
            n,
            replica_config(self.scenario.dir_replicas, i, &self.scenario.dir_raft),
            OpenMode::Recover,
        )?;
        let id = (n + i) as ShardId;
        self.bus.clear_inbox(id);
        self.bus.isolate(id, false);
        self.reps[i] = Some(d);
        self.rep_restarts += 1;
        Ok(())
    }

    /// El proceso del directorio muere de golpe.
    pub fn crash_directory(&mut self, kind: CrashKind) {
        let Some(m) = self.dir_medium.as_ref() else {
            return;
        };
        if self.dir.take().is_none() {
            return;
        }
        m.crash(kind);
        let id = self.partition.shard_count() as ShardId;
        self.bus.isolate(id, true);
        self.bus.clear_inbox(id);
    }

    fn open_directory(&mut self, medium: MemMedium) -> Result<Directory, String> {
        self.dir_incarnation += 1;
        let n = self.partition.shard_count();
        let (d, _) = Directory::open(
            Box::new(medium),
            &Directory::store_key(&self.scenario.master),
            Box::new(self.bus.end(n as ShardId)),
            n,
            DirectoryConfig::default(),
            OpenMode::Recover,
        )?;
        Ok(d)
    }

    /// Arranca de nuevo el directorio sobre su disco.
    pub fn restart_directory(&mut self) -> Result<(), String> {
        let medium = self
            .dir_medium
            .clone()
            .ok_or("este cluster no tiene directorio")?;
        if self.dir.is_some() {
            return Err("el directorio no está caído".into());
        }
        let d = self.open_directory(medium)?;
        let id = self.partition.shard_count() as ShardId;
        self.bus.clear_inbox(id);
        self.bus.isolate(id, false);
        self.dir = Some(d);
        self.dir_restarts += 1;
        Ok(())
    }

    /// Se pierde el directorio entero (el disco no existe): se arranca otro **vacío**, y hay que
    /// reconstruirlo con [`ClusterRun::rebuild_directory`] antes de que nadie entre.
    pub fn replace_directory_with_empty(&mut self) -> Result<(), String> {
        if self.dir_medium.is_none() {
            return Err("este cluster no tiene directorio".into());
        }
        self.crash_directory(CrashKind::Clean);
        self.dir_medium = Some(MemMedium::new());
        self.restart_directory()
    }

    /// Reconstruye el directorio (que debe estar vacío) con lo que dicen los discos de los shards. Los
    /// shards deberían estar parados para que lo leído no cambie.
    pub fn rebuild_directory(&mut self) -> Result<crate::directory::RebuildReport, String> {
        use tsofgw_store::{DynStore, Medium, Store, StoreConfig};
        let mut stores: Vec<(ShardId, DynStore)> = Vec::new();
        for (i, m) in self.media.iter().enumerate() {
            let key = crate::persist::store_key_for(&self.scenario.master, Some(i as ShardId));
            let (st, _) = Store::open(
                Box::new(m.clone()) as Box<dyn Medium>,
                &key,
                StoreConfig::default(),
            )
            .map_err(|e| format!("shard {i}: {e}"))?;
            stores.push((i as ShardId, st));
        }
        let refs: Vec<(ShardId, &DynStore)> = stores.iter().map(|(i, s)| (*i, s)).collect();
        let (entries, report) = crate::directory::rebuild_from_shards(&refs)?;
        if self.scenario.dir_replicas > 1 {
            for d in self.reps.iter_mut().flatten() {
                d.queue_import(&entries)?;
            }
            return Ok(report);
        }
        let d = self.dir.as_mut().ok_or("el directorio está caído")?;
        d.import(&entries)?;
        Ok(report)
    }

    // ------------------------------------------------------------------ clientes

    /// Sustituye al cliente del bot `i` por uno nuevo de la misma cuenta que entra por el shard `target`
    /// (lo que haría un juego cuando el jugador vuelve a abrirlo o la conexión se cayó).
    pub fn reconnect_bot(&mut self, i: usize, target: usize) {
        self.attempts[i] += 1;
        let mut bot = build_bot(
            &self.scenario,
            &self.net,
            &self.addrs,
            &self.authority,
            i,
            target,
            self.attempts[i],
        );
        if self.frozen {
            bot.freeze();
        }
        self.bots[i] = bot;
        self.reconnect_at[i] = None;
        self.reconnects += 1;
    }

    /// Añade un segundo cliente de la **misma cuenta** que el bot `i`, que entra por el shard `target`
    /// (alguien que abre el juego en dos sitios a la vez).
    pub fn spawn_duplicate_bot(&mut self, i: usize, target: usize) {
        self.attempts[i] += 1;
        let mut bot = build_bot(
            &self.scenario,
            &self.net,
            &self.addrs,
            &self.authority,
            i,
            target,
            self.attempts[i],
        );
        if self.frozen {
            bot.freeze();
        }
        self.extras.push(bot);
    }

    fn reconnect_dead_bots(&mut self) {
        let n = self.shards.len() as u64;
        for i in 0..self.bots.len() {
            // Quien cierra él mismo, o quien fue sustituido por otra conexión de su cuenta, no vuelve.
            let gone = matches!(
                self.bots[i].client.phase(),
                Phase::Disconnected(DisconnectReason::Requested | DisconnectReason::Replaced)
            );
            if gone || !matches!(self.bots[i].client.phase(), Phase::Disconnected(_)) {
                self.reconnect_at[i] = None;
                continue;
            }
            match self.reconnect_at[i] {
                None => {
                    let jitter = self.rng.next_u64() % (self.reconnect.delay_ms + 1);
                    self.reconnect_at[i] = Some(self.now + self.reconnect.delay_ms + jitter);
                }
                Some(t) if self.now >= t => {
                    let target = if self.reconnect.any_shard {
                        (self.rng.next_u64() % n) as usize
                    } else {
                        self.homes[i]
                    };
                    self.reconnect_bot(i, target);
                }
                Some(_) => {}
            }
        }
    }

    // ------------------------------------------------------------------ comprobaciones

    /// Lo que guardan los shards vivos de cada cuenta: residentes `(shard, época)` y traspasos sin acuse.
    #[allow(clippy::type_complexity)]
    fn scan_accounts(
        &self,
    ) -> (
        BTreeMap<u64, Vec<(ShardId, u64)>>,
        BTreeMap<u64, Vec<(ShardId, crate::OutboxEntry)>>,
    ) {
        let mut residents: BTreeMap<u64, Vec<(ShardId, u64)>> = BTreeMap::new();
        let mut transit: BTreeMap<u64, Vec<(ShardId, crate::OutboxEntry)>> = BTreeMap::new();
        for (i, sh) in self.shards.iter().enumerate() {
            if self.down[i] {
                continue;
            }
            for (a, e) in sh.resident_accounts() {
                residents.entry(a).or_default().push((i as ShardId, e));
            }
            for o in sh.handover_outbox() {
                transit
                    .entry(o.account)
                    .or_default()
                    .push((i as ShardId, o));
            }
        }
        (residents, transit)
    }

    /// Mayor época vista de cada cuenta que ya cambió de shard (para diagnóstico).
    pub fn epoch_floor(&self) -> &BTreeMap<u64, u64> {
        &self.epoch_floor
    }

    /// Todo lo que cada shard vivo guarda de una cuenta, en texto (para diagnóstico).
    fn replica_reports(&self) -> Vec<ReplicaReport> {
        self.reps
            .iter()
            .enumerate()
            .map(|(i, d)| match d {
                None => ReplicaReport {
                    index: i,
                    up: false,
                    ..Default::default()
                },
                Some(d) => ReplicaReport {
                    index: i,
                    up: true,
                    leader: d.is_leader(),
                    term: d.raft_term(),
                    applied: d.applied_index(),
                    log_len: d.raft_log_len(),
                    stats: d.raft_stats().unwrap_or_default(),
                    dir: d.stats().clone(),
                },
            })
            .collect()
    }

    pub fn describe_account(&self, account: u64) -> String {
        let mut o = format!(
            "cuenta {account}: suelo de época {:?}",
            self.epoch_floor.get(&account)
        );
        if let Some(d) = self.dir_view() {
            let _ = write!(o, ", directorio {:?}", d.lookup(account));
        }
        for (i, sh) in self.shards.iter().enumerate() {
            let r = sh.account_records(account);
            if self.down[i] {
                let _ = write!(o, "\n  shard {i}: CAÍDO");
            } else if r != Default::default() {
                let _ = write!(o, "\n  shard {i}: {r:?}");
            }
        }
        o
    }

    /// Las cuentas que ahora mismo incumplen algo, con lo que guarda cada shard de ellas.
    pub fn describe_problems(&self) -> String {
        let (residents, transit) = self.scan_accounts();
        let mut bad: BTreeSet<u64> = BTreeSet::new();
        for (a, list) in &residents {
            let wrong_dir = self.dir_view().is_some_and(|d| {
                d.lookup(*a)
                    .is_none_or(|(sh, ep)| !list.iter().any(|&(s, e)| s == sh && e == ep))
            });
            if list.len() > 1 || wrong_dir {
                bad.insert(*a);
            }
        }
        for (&a, &floor) in &self.epoch_floor {
            let now = residents
                .get(&a)
                .into_iter()
                .flatten()
                .map(|r| r.1)
                .chain(transit.get(&a).into_iter().flatten().map(|t| t.1.epoch))
                .max();
            if now.is_none_or(|e| e < floor) {
                bad.insert(a);
            }
        }
        let mut o = format!("t={} ms, caídos {:?}", self.now, self.down);
        for a in bad {
            let _ = write!(o, "\n{}", self.describe_account(a));
        }
        o
    }

    /// Comprobaciones sobre las cuentas con lo que hay ahora mismo en los almacenes y el directorio.
    pub fn check_accounts(&self) -> AccountInvariants {
        let mut inv = AccountInvariants::default();
        if self.scenario.persist.is_none() {
            return inv;
        }
        let (residents, transit) = self.scan_accounts();
        let all: BTreeSet<u64> = residents.keys().chain(transit.keys()).copied().collect();
        inv.accounts = all.len();
        inv.outbox = transit.values().map(Vec::len).sum();
        for (a, list) in &residents {
            if list.len() > 1 {
                inv.resident_duplicates += 1;
            }
            if let Some(dir) = self.dir_view() {
                match dir.lookup(*a) {
                    None => inv.dir_unknown += 1,
                    Some((sh, ep)) => {
                        if !list.iter().any(|&(s, e)| s == sh && e == ep) {
                            inv.dir_mismatch += 1;
                        }
                    }
                }
            }
        }
        for (&a, &floor) in &self.epoch_floor {
            let now = residents
                .get(&a)
                .into_iter()
                .flatten()
                .map(|r| r.1)
                .chain(transit.get(&a).into_iter().flatten().map(|t| t.1.epoch))
                .max();
            if now.is_none_or(|e| e < floor) {
                inv.lost += 1;
            }
        }
        inv
    }

    /// Muestrea el cluster y anota lo que no debería haber pasado nunca.
    fn observe(&mut self) {
        let (residents, transit) = self.scan_accounts();
        // Lo que ya se prometió no puede desaparecer: se recuerda la mayor época vista de cada cuenta que
        // ya cambió de shard alguna vez (la primera época no se garantiza en disco hasta el siguiente
        // guardado, y un traspaso sin anotar en el directorio aún puede cancelarse).
        // Un shard cuyo almacén falló sigue jugando en memoria, pero lo que anota ya no es durable ni
        // promete nada: no cuenta.
        let durable = |sh: ShardId| !self.shards[sh as usize].persistence_failed();
        for (a, list) in &residents {
            for &(sh, e) in list {
                if e >= 2 && durable(sh) {
                    let f = self.epoch_floor.entry(*a).or_insert(0);
                    *f = (*f).max(e);
                }
            }
        }
        for (a, list) in &transit {
            for (sh, o) in list {
                if o.committed && o.epoch >= 2 && durable(*sh) {
                    let f = self.epoch_floor.entry(*a).or_insert(0);
                    *f = (*f).max(o.epoch);
                }
            }
        }
        let acc = self.check_accounts();
        let inv = check_cluster(
            &self.shards,
            &self.bots,
            &self.scenario.game,
            self.bus.in_flight(),
            false,
        );
        let mut sims: BTreeMap<u64, usize> = BTreeMap::new();
        for (i, sh) in self.shards.iter().enumerate() {
            if !self.down[i] {
                for a in sh.simulated_player_ids() {
                    *sims.entry(a).or_default() += 1;
                }
            }
        }
        let twice = sims.values().filter(|&&n| n > 1).count();
        let all_up = !self.any_down();
        let v = &mut self.violations;
        v.resident_duplicates = v.resident_duplicates.max(acc.resident_duplicates);
        v.dir_mismatch = v.dir_mismatch.max(acc.dir_mismatch);
        v.simulated_twice = v.simulated_twice.max(twice);
        // Con un shard caído, lo que guarda no se puede ver: sus cuentas y sus NPC no están «perdidos»,
        // están en su disco. Solo se exige con todos en pie.
        if all_up {
            v.lost_accounts = v.lost_accounts.max(acc.lost);
            v.npc_lost = v.npc_lost.max(inv.npc_lost);
        }
        let bad = acc.resident_duplicates > 0
            || acc.dir_mismatch > 0
            || twice > 0
            || (all_up && (acc.lost > 0 || inv.npc_lost > 0));
        if bad && self.first_violation.is_none() {
            self.first_violation = Some(self.describe_problems());
        }
    }

    /// Comprueba el reparto ahora mismo. Con `view` también compara lo que ve cada jugador con la verdad.
    pub fn check(&self, view: bool) -> Invariants {
        let mut inv = check_cluster(
            &self.shards,
            &self.bots,
            &self.scenario.game,
            self.bus.in_flight(),
            view,
        );
        inv.accounts = self.check_accounts();
        inv
    }

    pub fn report(&self) -> ClusterReport {
        let mut r = ClusterReport {
            grid: self.scenario.grid,
            clients: self.scenario.clients,
            npcs: self.scenario.game.npc_count,
            sim_seconds: self.now as f64 / 1000.0,
            wall_seconds: self.wall.elapsed().as_secs_f64(),
            tick_budget_us: self.scenario.game.tick_ms * 1000,
            bus: self.bus.stats(),
            shards: self.shards.iter().map(shard_report).collect(),
            invariants: self.check(false),
            restarts: self.restarts,
            dir_restarts: self.dir_restarts + self.rep_restarts,
            reconnects: self.reconnects,
            directory: self.dir_view().map(|d| DirectoryReport {
                accounts: d.account_count(),
                per_shard: d.accounts_per_shard(),
                stats: d.stats().clone(),
                replicas: self.replica_reports(),
            }),
            violations: self.violations.clone(),
            persistent: self.scenario.persist.is_some(),
            ..Default::default()
        };
        fold_clients(&mut r, &self.bots);
        r
    }

    /// Una **tormenta de caídas**: `rounds` veces, el proceso de un shard muere de golpe (a veces dos a la
    /// vez, y con directorio a veces el del directorio) con una cola de disco rota al azar, queda caído un
    /// rato y vuelve a arrancar sobre lo que dejó en disco. Con la misma `seed` es repetible. Lo que hay
    /// que mirar después es [`ClusterRun::violations`] y el reparto final.
    pub fn chaos(&mut self, rounds: usize, seed: u64) -> Result<ChaosReport, String> {
        if self.scenario.persist.is_none() {
            return Err("una tormenta de caídas exige persistencia en los shards".into());
        }
        let mut rng = Rng::new(seed);
        let n = self.shards.len();
        let mut rep = ChaosReport {
            rounds,
            ..ChaosReport::default()
        };
        for round in 0..rounds {
            self.run_for(800 + rng.next_u64() % 2500);
            // A veces cae el directorio en lugar de un shard.
            if self.scenario.directory && self.scenario.dir_replicas > 1 && round % 4 == 3 {
                // Cae una réplica: la líder una de cada dos veces. Las de una minoría pueden caer a la vez.
                let victim = match self.leader_replica() {
                    Some(l) if rng.next_u64().is_multiple_of(2) => l,
                    _ => (rng.next_u64() % self.reps.len() as u64) as usize,
                };
                self.crash_replica(victim, pick_crash(&mut rng));
                let mut down = vec![victim];
                if self.reps.len() >= 5 && rng.next_u64().is_multiple_of(3) {
                    let other = (victim + 1 + (rng.next_u64() % 4) as usize) % self.reps.len();
                    if other != victim {
                        self.crash_replica(other, pick_crash(&mut rng));
                        down.push(other);
                    }
                }
                self.run_for(300 + rng.next_u64() % 1500);
                for i in down {
                    self.restart_replica(i)?;
                }
                rep.directory_crashes += 1;
                continue;
            }
            if self.scenario.directory && round % 4 == 3 {
                self.crash_directory(pick_crash(&mut rng));
                self.run_for(300 + rng.next_u64() % 1500);
                self.restart_directory()?;
                rep.directory_crashes += 1;
                continue;
            }
            let sh = (rng.next_u64() % n as u64) as usize;
            self.crash_shard(sh, pick_crash(&mut rng));
            rep.shard_crashes += 1;
            // A veces cae otro a la vez.
            let other =
                (sh + 1 + (rng.next_u64() % (n as u64).max(2).saturating_sub(1)) as usize) % n;
            let both = n > 1 && rng.next_u64().is_multiple_of(3);
            if both {
                self.crash_shard(other, pick_crash(&mut rng));
                rep.shard_crashes += 1;
                rep.double_crashes += 1;
            }
            self.run_for(200 + rng.next_u64() % 2500);
            self.restart_shard(sh)?;
            if both {
                self.run_for(rng.next_u64() % 700);
                self.restart_shard(other)?;
            }
        }
        Ok(rep)
    }
}

/// Configuración de la réplica `i` de un directorio replicado en el banco de pruebas.
fn replica_config(replicas: usize, i: usize, raft: &crate::raft::RaftConfig) -> DirectoryConfig {
    DirectoryConfig {
        replicas,
        replica: i,
        raft: raft.clone(),
        ..DirectoryConfig::default()
    }
}

/// Cómo queda el disco al caer el proceso: sincronizado, con la cola rota (poco o mucho) o con un hueco.
fn pick_crash(rng: &mut Rng) -> CrashKind {
    match rng.next_u64() % 4 {
        0 => CrashKind::Clean,
        1 => CrashKind::Torn((rng.next_u64() % 40) as usize),
        2 => CrashKind::Torn((rng.next_u64() % 5000) as usize),
        _ => CrashKind::Hole,
    }
}

/// Informe de un shard a partir de sus propios contadores.
pub(crate) fn shard_report<L: Link>(sh: &GameServer<L>) -> ShardReport {
    let g = sh.stats();
    ShardReport {
        shard: sh.shard_id().unwrap_or(0),
        players: sh.owned_player_ids().len(),
        npcs_owned: sh.owned_npc_count(),
        ghosts: sh.ghost_count(),
        ticks: g.ticks,
        tick_avg_us: if g.ticks > 0 {
            g.tick_time_us as f64 / g.ticks as f64
        } else {
            0.0
        },
        tick_max_us: g.tick_time_max_us,
        simulated_avg: if g.ticks > 0 {
            g.npcs_simulated as f64 / g.ticks as f64
        } else {
            0.0
        },
        cluster: sh.cluster_stats().cloned().unwrap_or_default(),
        store: sh.persist_stats().map(|st| StoreReport {
            stats: st.clone(),
            log_bytes: sh.store_log_bytes().unwrap_or(0),
            failed: sh.persistence_failed(),
        }),
    }
}

/// Suma lo que cuentan los clientes (cambios de shard, redirecciones, correcciones).
pub(crate) fn fold_clients<C: Link>(r: &mut ClusterReport, bots: &[Bot<C>]) {
    r.playing = bots.iter().filter(|b| b.client.is_playing()).count();
    for b in bots {
        let st = b.client.stats();
        r.switches += st.switches;
        r.switch_failures += st.switch_failures;
        r.redirects += st.redirects;
        r.corrections += st.corrections;
        r.correction_max_q = r.correction_max_q.max(st.correction_max_q);
    }
}

/// Sentido en que patrulla el bot `i`: sobre x, sobre z y en diagonal, para cruzar bordes y esquinas.
pub(crate) fn patrol_dir(i: usize) -> [i8; 3] {
    match i % 3 {
        0 => [127, 0, 0],
        1 => [0, 0, 127],
        _ => [90, 0, 90],
    }
}

/// Comprueba el reparto de un cluster, sea cual sea el enlace de los shards y de los clientes.
/// `in_flight` son los mensajes que el bus sabe que siguen en camino (0 si no lo puede saber).
pub fn check_cluster<L: Link, C: Link>(
    shards: &[GameServer<L>],
    bots: &[Bot<C>],
    game: &GameConfig,
    in_flight: usize,
    view: bool,
) -> Invariants {
    let mut inv = Invariants {
        npcs: game.npc_count,
        bus_in_flight: in_flight,
        pending_transfers: shards
            .iter()
            .map(|s| s.pending_npc_transfers() + s.pending_player_handoffs())
            .sum(),
        ..Default::default()
    };
    // NPC: cuántos shards son responsables de cada uno.
    let mut holders = vec![0u8; game.npc_count];
    for sh in shards {
        for id in sh.held_npc_ids() {
            holders[(id - NPC_BASE) as usize] += 1;
        }
    }
    inv.npc_lost = holders.iter().filter(|&&h| h == 0).count();
    inv.npc_duplicated = holders.iter().filter(|&&h| h > 1).count();
    // Jugadores.
    let playing: Vec<u64> = bots
        .iter()
        .filter(|b| b.client.is_playing())
        .map(|b| b.client.entity())
        .collect();
    inv.players_playing = playing.len();
    for &id in &playing {
        match shards.iter().filter(|s| s.owns_player(id)).count() {
            0 => inv.players_lost += 1,
            1 => {}
            _ => inv.players_duplicated += 1,
        }
    }
    if view {
        check_views(shards, game, &playing, &mut inv);
    }
    inv
}

/// Verdad del cluster: posición de cada entidad según su dueño.
fn truth<L: Link>(shards: &[GameServer<L>]) -> HashMap<u64, [i32; 3]> {
    let mut t = HashMap::new();
    for sh in shards {
        for id in sh.held_npc_ids().into_iter().chain(sh.owned_player_ids()) {
            if let Some(st) = sh.entity_state(id) {
                t.insert(id, st.pos);
            }
        }
    }
    t
}

fn check_views<L: Link>(
    shards: &[GameServer<L>],
    cfg: &GameConfig,
    playing: &[u64],
    inv: &mut Invariants,
) {
    let truth = truth(shards);
    let d = |a: [i32; 3], b: [i32; 3]| -> f32 {
        let sq: f64 = (0..3)
            .map(|i| (dequantize(a[i]) as f64 - dequantize(b[i]) as f64).powi(2))
            .sum();
        sq.sqrt() as f32
    };
    for &id in playing {
        let Some(owner) = shards.iter().find(|s| s.owns_player(id)) else {
            continue;
        };
        let (Some(snap), Some(me)) = (owner.last_snapshot(id), owner.player_position(id)) else {
            continue;
        };
        inv.views_checked += 1;
        // Lo que está bien dentro del radio debe estar; lo que se ve no puede estar mucho más lejos
        // de lo que permite la histéresis. Los márgenes absorben el retraso de un tick del bus.
        let mut inner = cfg.interest_radius - 3.0;
        // Si el tope de entidades por snapshot muerde, solo se exige lo que está más cerca que lo
        // más lejano que se incluyó (el tope se queda con las más cercanas).
        if cfg.max_visible > 0 && snap.entities.len() > cfg.max_visible {
            let far = snap
                .entities
                .iter()
                .filter(|(&eid, _)| eid != id)
                .map(|(_, st)| d(st.pos, me))
                .fold(0.0f32, f32::max);
            inner = inner.min(far - 1.0);
        }
        let outer = cfg.interest_radius * cfg.hysteresis + 3.0;
        for (&eid, &pos) in &truth {
            if eid != id && d(pos, me) <= inner && !snap.entities.contains_key(&eid) {
                inv.view_missing += 1;
            }
        }
        for (&eid, st) in &snap.entities {
            if eid == id {
                continue;
            }
            match truth.get(&eid) {
                Some(&pos) => {
                    inv.view_error_max = inv.view_error_max.max(d(pos, st.pos));
                    if d(pos, me) > outer {
                        inv.view_phantoms += 1;
                    }
                }
                None => inv.view_phantoms += 1,
            }
        }
    }
}

/// Lo que cuenta el almacén de un shard (si tiene persistencia).
#[derive(Debug, Clone, Default)]
pub struct StoreReport {
    pub stats: crate::persist::PersistStats,
    /// Bytes del diario desde el último checkpoint.
    pub log_bytes: u64,
    /// El almacén falló y el shard no promete nada más hasta reiniciarse.
    pub failed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ShardReport {
    pub shard: ShardId,
    pub players: usize,
    pub npcs_owned: usize,
    pub ghosts: usize,
    pub ticks: u64,
    pub tick_avg_us: f64,
    pub tick_max_us: u64,
    pub simulated_avg: f64,
    pub cluster: ClusterStats,
    pub store: Option<StoreReport>,
}

#[derive(Debug, Clone, Default)]
pub struct ClusterReport {
    pub grid: (u16, u16),
    pub clients: usize,
    pub playing: usize,
    pub npcs: usize,
    pub sim_seconds: f64,
    pub wall_seconds: f64,
    pub tick_budget_us: u64,
    pub shards: Vec<ShardReport>,
    pub bus: crate::bus::BusStats,
    /// El bus entre shards era UDP real (y no la red simulada).
    pub udp: bool,
    pub switches: u64,
    pub switch_failures: u64,
    pub redirects: u64,
    pub corrections: u64,
    pub correction_max_q: u32,
    pub invariants: Invariants,
    /// Reinicios de shards y del directorio, y reconexiones de clientes durante la marcha.
    pub restarts: u64,
    pub dir_restarts: u64,
    pub reconnects: u64,
    /// El directorio de sesiones, si lo había y estaba en pie al terminar.
    pub directory: Option<DirectoryReport>,
    /// Lo que no debería haber ocurrido nunca y se vio durante la marcha.
    pub violations: Violations,
    /// Los shards tenían almacén durable.
    pub persistent: bool,
    /// La tormenta de caídas que se aplicó, si hubo.
    pub chaos: Option<ChaosReport>,
}

/// Lo que se hizo en una tormenta de caídas ([`ClusterRun::chaos`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChaosReport {
    pub rounds: usize,
    /// Caídas de un shard (cada ronda suele ser una; algunas añaden otro a la vez).
    pub shard_crashes: usize,
    pub double_crashes: usize,
    pub directory_crashes: usize,
}

/// Lo que cuenta el directorio de sesiones al terminar.
#[derive(Debug, Clone, Default)]
pub struct DirectoryReport {
    pub accounts: usize,
    pub per_shard: Vec<usize>,
    pub stats: crate::directory::DirectoryStats,
    /// Una por réplica (vacío sin réplicas).
    pub replicas: Vec<ReplicaReport>,
}

/// Una réplica del directorio al terminar.
#[derive(Debug, Clone, Default)]
pub struct ReplicaReport {
    pub index: usize,
    pub up: bool,
    pub leader: bool,
    pub term: u64,
    pub applied: u64,
    pub log_len: usize,
    pub stats: crate::raft::RaftStats,
    pub dir: crate::directory::DirectoryStats,
}

impl ClusterReport {
    pub fn render(&self) -> String {
        let mut o = String::new();
        let n = self.shards.len();
        let _ = writeln!(
            o,
            "== Cluster TSOFGW: {}×{} = {n} shards ==",
            self.grid.0, self.grid.1
        );
        let _ = writeln!(
            o,
            "Clientes: {}/{} jugando | NPC en el mundo: {} | tiempo simulado: {:.1} s (CPU real: {:.1} s)",
            self.playing, self.clients, self.npcs, self.sim_seconds, self.wall_seconds
        );
        let _ = writeln!(o, "\nShards (cada uno solo simula su región)");
        let _ = writeln!(
            o,
            "  shard  jugadores  NPC propios  copias  NPC simulados  tick medio / máx (µs)  presupuesto"
        );
        for s in &self.shards {
            let _ = writeln!(
                o,
                "  {:>5}  {:>9}  {:>11}  {:>6}  {:>13.0}  {:>10.0} / {:<8}  {:>5.1} %",
                s.shard,
                s.players,
                s.npcs_owned,
                s.ghosts,
                s.simulated_avg,
                s.tick_avg_us,
                s.tick_max_us,
                s.tick_avg_us / self.tick_budget_us.max(1) as f64 * 100.0
            );
        }
        let sum = |f: fn(&ClusterStats) -> u64| -> u64 {
            self.shards.iter().map(|s| f(&s.cluster)).sum()
        };
        let max_tick = self
            .shards
            .iter()
            .map(|s| s.tick_avg_us)
            .fold(0.0, f64::max);
        let tot_tick: f64 = self.shards.iter().map(|s| s.tick_avg_us).sum();
        let _ = writeln!(
            o,
            "  el shard más cargado usa {:.0} µs de media por tick; la suma de todos es {:.0} µs \
             (en máquinas separadas, el límite lo marca el más cargado)",
            max_tick, tot_tick
        );
        let _ = writeln!(o, "\nBordes");
        let _ = writeln!(
            o,
            "  traspasos de jugador: {} iniciados, {} completados, {} cancelados | redirecciones {} → cambios de shard en clientes: {} ({} fallidos)",
            sum(|c| c.handoffs_started),
            sum(|c| c.handoffs_completed),
            sum(|c| c.handoffs_aborted),
            self.redirects,
            self.switches,
            self.switch_failures
        );
        let acked = sum(|c| c.handover_acked);
        if acked > 0 {
            let tick_ms = self.tick_budget_us as f64 / 1000.0;
            let max_ticks = self
                .shards
                .iter()
                .map(|s| s.cluster.handover_ticks_max)
                .max()
                .unwrap_or(0);
            let _ = writeln!(
                o,
                "  tiempo de un traspaso de jugador (desde que el origen suelta la autoridad hasta el acuse del destino): \
                 media {:.0} ms, máximo {:.0} ms, en {} acusados",
                sum(|c| c.handover_ticks_sum) as f64 / acked as f64 * tick_ms,
                f64::from(max_ticks) * tick_ms,
                acked
            );
        }
        let _ = writeln!(
            o,
            "  traspasos de NPC: {} enviados, {} reenvíos, {} recibidos, {} repeticiones detectadas, {} conflictos",
            sum(|c| c.npc_xfers_out),
            sum(|c| c.npc_xfer_retries),
            sum(|c| c.npc_xfers_in),
            sum(|c| c.npc_xfer_dups),
            sum(|c| c.npc_xfer_conflicts)
        );
        let ticks = self.shards.first().map_or(1, |s| s.ticks.max(1)) as f64;
        let _ = writeln!(
            o,
            "  fantasmas enviados por tick (todo el cluster): {:.1} jugadores + {:.1} NPC | caducados {}",
            sum(|c| c.ghost_players_out) as f64 / ticks,
            sum(|c| c.ghost_npcs_out) as f64 / ticks,
            sum(|c| c.ghost_expired)
        );
        if self.udp {
            let _ = writeln!(
                o,
                "  bus entre shards (UDP real, HMAC): {} mensajes, {:.1} KB/s en total | datagramas rechazados: {} (por otra configuración de mundo: {}) | mensajes mal formados: {}",
                sum(|c| c.bus_msgs_out),
                sum(|c| c.bus_bytes_out) as f64 / 1000.0 / self.sim_seconds.max(0.001),
                sum(|c| c.bus_rejected),
                sum(|c| c.bus_config_mismatch),
                sum(|c| c.bus_malformed)
            );
        } else {
            let _ = writeln!(
                o,
                "  bus entre shards: {} mensajes, {:.1} KB/s en total ({} perdidos en la red simulada, {} duplicados, {} demasiado grandes)",
                self.bus.sent,
                self.bus.bytes as f64 / 1000.0 / self.sim_seconds.max(0.001),
                self.bus.dropped,
                self.bus.duplicated,
                self.bus.oversize
            );
        }
        let _ = writeln!(
            o,
            "  correcciones de predicción: {} (máxima {:.2} u)",
            self.corrections,
            self.correction_max_q as f32 / 64.0
        );
        let i = &self.invariants;
        let _ = writeln!(o, "\nReparto al final (red en calma)");
        let _ = writeln!(
            o,
            "  NPC: {} perdidos, {} duplicados de {} | jugadores: {} perdidos, {} duplicados de {}",
            i.npc_lost,
            i.npc_duplicated,
            i.npcs,
            i.players_lost,
            i.players_duplicated,
            i.players_playing
        );
        let _ = writeln!(
            o,
            "  vistas comprobadas: {} | entidades que faltaban: {} | entidades inexistentes: {} | error máximo de posición: {:.2} u",
            i.views_checked, i.view_missing, i.view_phantoms, i.view_error_max
        );
        if self.persistent {
            self.render_persistence(&mut o);
        }
        o
    }

    fn render_persistence(&self, o: &mut String) {
        let _ = writeln!(o, "\nPersistencia y sesiones");
        if let Some(c) = &self.chaos {
            let _ = writeln!(
                o,
                "  tormenta de caídas: {} rondas, {} caídas de shard ({} dobles), {} del directorio",
                c.rounds, c.shard_crashes, c.double_crashes, c.directory_crashes
            );
        }
        let _ = writeln!(
            o,
            "  reinicios: {} de shard, {} del directorio | reconexiones de clientes: {}",
            self.restarts, self.dir_restarts, self.reconnects
        );
        let _ = writeln!(
            o,
            "  shard  commits  NPC guardados  jugadores guardados  compactaciones  promesas retenidas  diario (KB)  estado"
        );
        for s in &self.shards {
            if let Some(st) = &s.store {
                let _ = writeln!(
                    o,
                    "  {:>5}  {:>7}  {:>13}  {:>19}  {:>14}  {:>18}  {:>11.1}  {}",
                    s.shard,
                    st.stats.commits,
                    st.stats.npc_saves,
                    st.stats.player_saves,
                    st.stats.compactions,
                    st.stats.deferred_dropped,
                    st.log_bytes as f64 / 1000.0,
                    if st.failed { "FALLÓ" } else { "bien" }
                );
            }
        }
        if let Some(d) = &self.directory {
            let _ = writeln!(
                o,
                "  directorio: {} cuentas (por shard {:?}) | cambios de dueño confirmados {}, repetidos {}, rechazados {}, desconocidos {} | entradas {} | respuestas retenidas {}",
                d.accounts,
                d.per_shard,
                d.stats.committed,
                d.stats.idempotent,
                d.stats.rejected,
                d.stats.unknown,
                d.stats.logins,
                d.stats.replies_dropped
            );
            if !d.replicas.is_empty() {
                let _ = writeln!(
                    o,
                    "  directorio replicado ({} réplicas): {} elecciones iniciadas, {} ganadas, {} renuncias, {} quórum perdido, {} instantáneas enviadas y {} instaladas, {} entradas truncadas por conflicto",
                    d.replicas.len(),
                    d.replicas.iter().map(|r| r.stats.elections_started).sum::<u64>(),
                    d.replicas.iter().map(|r| r.stats.elections_won).sum::<u64>(),
                    d.replicas.iter().map(|r| r.stats.stepped_down).sum::<u64>(),
                    d.replicas.iter().map(|r| r.stats.quorum_lost).sum::<u64>(),
                    d.replicas.iter().map(|r| r.stats.snapshots_sent).sum::<u64>(),
                    d.replicas.iter().map(|r| r.stats.snapshots_installed).sum::<u64>(),
                    d.replicas.iter().map(|r| r.stats.conflicts_truncated).sum::<u64>()
                );
                for r in &d.replicas {
                    let _ = writeln!(
                        o,
                        "    réplica {}: {} | término {} | aplicado hasta {} | diario {} entradas | propuestas {} redirigidas {} sin líder {}",
                        r.index,
                        if !r.up { "CAÍDA" } else if r.leader { "líder" } else { "seguidora" },
                        r.term,
                        r.applied,
                        r.log_len,
                        r.dir.proposed,
                        r.dir.redirects,
                        r.dir.no_leader
                    );
                }
            }
        }
        let a = &self.invariants.accounts;
        let _ = writeln!(
            o,
            "  cuentas al final: {} con registro | duplicadas {} | perdidas {} | distintas del directorio {} | sin constar en el directorio {} | traspasos sin acuse {}",
            a.accounts, a.resident_duplicates, a.lost, a.dir_mismatch, a.dir_unknown, a.outbox
        );
        let v = &self.violations;
        let _ = writeln!(
            o,
            "  vigilado durante toda la marcha (cada 100 ms): cuentas residentes en dos shards {} | simuladas por dos shards a la vez {} | cuentas que desaparecieron {} | residentes que el directorio sitúa en otro shard {} | NPC sin dueño con todos en pie {}",
            v.resident_duplicates, v.simulated_twice, v.lost_accounts, v.dir_mismatch, v.npc_lost
        );
    }
}

/// El escenario con una tormenta de caídas ([`ClusterRun::chaos`]) en medio y, al final, un tramo en
/// calma en el que los clientes vuelven a entrar y todo debe converger. Es el banco de pruebas de la
/// persistencia: el informe trae lo que se vigiló durante la marcha (`violations`) y el reparto final.
pub fn run_cluster_chaos(
    s: &ClusterScenario,
    rounds: usize,
    seed: u64,
) -> Result<ClusterReport, String> {
    let mut run = ClusterRun::new(s)?;
    run.run_for(s.duration_ms);
    let chaos = run.chaos(rounds, seed)?;
    run.run_for(4000);
    run.settle();
    // Tras una caída, los clientes de ese shard tardan el plazo del transporte (5 s) en notarlo: se
    // espera a verlos jugar de forma estable antes de dar el reparto por final.
    let end = run.now + 90_000;
    let mut since: Option<u64> = None;
    while run.now < end {
        run.step();
        if run.bots.iter().all(|b| b.client.is_playing()) {
            let t = *since.get_or_insert(run.now);
            if run.now - t >= 6500 {
                break;
            }
        } else {
            since = None;
        }
    }
    run.run_for(s.settle_ms);
    // Con los clientes quietos los NPC siguen moviéndose y siempre hay alguno cruzando un borde: en un
    // instante concreto puede haber un traspaso en vuelo. Se da un margen para ver uno sin ninguno; un
    // NPC perdido o duplicado de verdad se ve siempre.
    let mut inv = run.check(true);
    for _ in 0..60 {
        if inv.is_exact() {
            break;
        }
        run.run_for(50);
        inv = run.check(true);
    }
    let mut r = run.report();
    r.invariants = inv;
    r.chaos = Some(chaos);
    Ok(r)
}

/// Ejecuta el escenario completo (con tramo final en calma) y devuelve el informe.
pub fn run_cluster(s: &ClusterScenario) -> Result<ClusterReport, String> {
    let mut run = ClusterRun::new(s)?;
    run.run_for(s.duration_ms);
    run.settle();
    run.run_for(s.settle_ms);
    let mut r = run.report();
    r.invariants = run.check(true);
    Ok(r)
}
