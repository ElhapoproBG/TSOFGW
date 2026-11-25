//! Un cluster completo sobre **sockets UDP reales** de este equipo: cada shard tiene su socket para los
//! clientes y su [`UdpBus`] (autenticado con HMAC) hacia los demás; los clientes son [`Bot`]s que abren
//! sus propios sockets y siguen las redirecciones entre shards.
//!
//! Es el mismo banco de pruebas que [`crate::cluster_scenario::ClusterRun`] (mismos escenarios,
//! mismos invariantes), pero con la pila de red real en lugar de la simulada. Se puede conducir con un
//! reloj virtual (`step`, más rápido que el tiempo real y sin esperas) o con el reloj de pared (`update`).

use std::net::SocketAddr;
use std::time::Instant;

use tsofgw_net::dequantize;
use tsofgw_transport::{Authority, ClientConfig, OsEntropy, ServerConfig, UdpLink};

use crate::client::NewConnection;
use crate::cluster_scenario::{
    check_cluster, fold_clients, patrol_dir, shard_report, ClusterReport, ClusterScenario,
    Invariants,
};
use crate::partition::{Partition, ShardId};
use crate::server::ClusterConfig;
use crate::sim::HALF_EXTENT_Q;
use crate::udp_bus::UdpBus;
use crate::{Bot, GameClient, GameServer};

pub struct UdpCluster {
    pub partition: Partition,
    pub shards: Vec<GameServer<UdpLink>>,
    pub bots: Vec<Bot<UdpLink>>,
    /// Dirección UDP de cada shard para los clientes.
    pub client_addrs: Vec<SocketAddr>,
    /// Dirección UDP del bus de cada shard.
    pub bus_addrs: Vec<SocketAddr>,
    pub now: u64,
    pub step_ms: u64,
    scenario: ClusterScenario,
    frozen: bool,
    wall: Instant,
}

impl UdpCluster {
    /// Abre todos los sockets en `127.0.0.1` (puertos asignados por el sistema). Ignora `s.net` y `s.bus`:
    /// la red es la del equipo.
    pub fn new(s: &ClusterScenario, cluster_key: &[u8]) -> Result<UdpCluster, String> {
        let n = s.grid.0 as usize * s.grid.1 as usize;
        Self::with_keys(s, &vec![cluster_key; n])
    }

    /// Como [`UdpCluster::new`], pero cada shard con su propia clave de cluster (para probar qué pasa
    /// cuando una está mal configurada).
    pub fn with_keys(s: &ClusterScenario, keys: &[&[u8]]) -> Result<UdpCluster, String> {
        let partition = Partition::grid(s.grid.0, s.grid.1, dequantize(HALF_EXTENT_Q))?;
        let n = partition.shard_count();
        if keys.len() != n {
            return Err(format!("hacen falta {n} claves y hay {}", keys.len()));
        }
        let authority = Authority::new(&s.master);

        let mut links = Vec::with_capacity(n);
        let mut buses = Vec::with_capacity(n);
        for (sh, key) in keys.iter().enumerate() {
            links.push(
                UdpLink::bind("127.0.0.1:0").map_err(|e| format!("socket del shard {sh}: {e}"))?,
            );
            buses.push(UdpBus::bind(sh as ShardId, "127.0.0.1:0", n, key)?);
        }
        let client_addrs: Vec<SocketAddr> = links
            .iter()
            .map(|l| l.local_addr().map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        let bus_addrs: Vec<SocketAddr> = buses
            .iter()
            .map(|b| b.local_addr().map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        for (i, b) in buses.iter_mut().enumerate() {
            for (j, a) in bus_addrs.iter().enumerate() {
                if i != j {
                    b.set_peer(j as ShardId, *a)?;
                }
            }
        }
        let mut shards = Vec::with_capacity(n);
        for (sh, (link, bus)) in links.into_iter().zip(buses).enumerate() {
            let cluster = ClusterConfig::new(sh as ShardId, partition.clone(), &s.game)?;
            shards.push(GameServer::new_shard(
                link,
                authority.clone(),
                s.game.clone(),
                ServerConfig::default(),
                Box::new(OsEntropy),
                cluster,
                Box::new(bus),
            )?);
        }

        let mut bots = Vec::with_capacity(s.clients);
        for i in 0..s.clients {
            let id = 1000 + i as u64;
            let spawn = GameServer::<UdpLink>::spawn_position(&s.game, id);
            let home = partition.owner_of(tsofgw_world::Vec3::new(
                dequantize(spawn[0]),
                0.0,
                dequantize(spawn[2]),
            ));
            let addrs = client_addrs.clone();
            let link =
                UdpLink::bind("127.0.0.1:0").map_err(|e| format!("socket de un cliente: {e}"))?;
            let client = GameClient::new(
                link,
                client_addrs[home as usize],
                authority.issue(id),
                ClientConfig::default(),
                Box::new(OsEntropy),
            )
            .with_connector(Box::new(move |shard| {
                let link = UdpLink::bind("127.0.0.1:0").ok()?;
                addrs.get(shard as usize).map(|&server| NewConnection {
                    link,
                    server,
                    entropy: Box::new(OsEntropy),
                })
            }));
            let mut bot = Bot::new(client, s.net.seed.wrapping_add(i as u64 * 7919));
            if i < s.patrollers {
                bot = bot.with_patrol(patrol_dir(i), s.patrol_ms);
            }
            bots.push(bot);
        }
        Ok(UdpCluster {
            partition,
            shards,
            bots,
            client_addrs,
            bus_addrs,
            now: 0,
            step_ms: s.step_ms,
            scenario: s.clone(),
            frozen: false,
            wall: Instant::now(),
        })
    }

    /// Actualiza todo al instante `now` (ms), que el llamador lleva con el reloj que quiera.
    pub fn update(&mut self, now: u64) {
        self.now = now;
        for sh in &mut self.shards {
            sh.update(now);
        }
        for b in &mut self.bots {
            b.update(now);
        }
    }

    /// Avanza un paso del reloj virtual.
    pub fn step(&mut self) {
        let now = self.now + self.step_ms;
        self.update(now);
    }

    pub fn run_for(&mut self, ms: u64) {
        let end = self.now + ms;
        while self.now < end {
            self.step();
        }
    }

    /// Detiene a los bots: a partir de aquí todo debe converger.
    pub fn settle(&mut self) {
        if !self.frozen {
            self.frozen = true;
            for b in &mut self.bots {
                b.freeze();
            }
        }
    }

    pub fn check(&self, view: bool) -> Invariants {
        check_cluster(&self.shards, &self.bots, &self.scenario.game, 0, view)
    }

    pub fn report(&self) -> ClusterReport {
        let mut r = ClusterReport {
            grid: self.scenario.grid,
            clients: self.scenario.clients,
            npcs: self.scenario.game.npc_count,
            sim_seconds: self.now as f64 / 1000.0,
            wall_seconds: self.wall.elapsed().as_secs_f64(),
            tick_budget_us: self.scenario.game.tick_ms * 1000,
            shards: self.shards.iter().map(shard_report).collect(),
            udp: true,
            invariants: self.check(false),
            ..Default::default()
        };
        fold_clients(&mut r, &self.bots);
        r
    }
}
