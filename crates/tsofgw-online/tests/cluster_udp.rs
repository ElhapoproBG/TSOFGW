//! El cluster sobre sockets UDP reales de este equipo: cada shard con su socket de clientes y su bus
//! autenticado, clientes que abren sus propios sockets y siguen las redirecciones. Se conduce con un
//! reloj virtual para que sea rápido, pero todos los datagramas pasan por el sistema operativo.

use std::net::UdpSocket;

use tsofgw_net::EntityState;
use tsofgw_online::bus::{NpcXfer, PeerMsg};
use tsofgw_online::cluster_scenario::ClusterScenario;
use tsofgw_online::cluster_udp::UdpCluster;
use tsofgw_online::udp_bus::{bus_key, seal, system_wall_ms};
use tsofgw_online::{ClusterStats, GameConfig, NPC_BASE};

const KEY: &[u8] = b"clave-de-cluster-de-las-pruebas";

fn scenario(grid: (u16, u16), clients: usize, patrollers: usize) -> ClusterScenario {
    ClusterScenario {
        grid,
        clients,
        patrollers,
        duration_ms: 20_000,
        settle_ms: 4000,
        game: GameConfig {
            npc_count: 3000,
            npc_speed: 10.0,
            spawn_half_extent: 40.0,
            max_visible: 100_000,
            ..GameConfig::default()
        },
        ..ClusterScenario::default()
    }
}

fn total(c: &UdpCluster, f: fn(&ClusterStats) -> u64) -> u64 {
    c.shards.iter().map(|s| f(s.cluster_stats().unwrap())).sum()
}

#[test]
fn un_cluster_sobre_udp_real_reparte_y_traspasa_sin_perder_nada() {
    let s = scenario((2, 2), 32, 24);
    let mut c = UdpCluster::new(&s, KEY).unwrap();
    while c.now < s.duration_ms {
        c.step();
        if c.now.is_multiple_of(1000) {
            let inv = c.check(false);
            assert_eq!(inv.npc_lost, 0, "t={}", c.now);
            assert_eq!(inv.players_lost, 0, "t={}", c.now);
            assert_eq!(inv.players_playing, s.clients, "t={}", c.now);
        }
    }
    c.settle();
    c.run_for(s.settle_ms);
    let inv = c.check(true);
    let mut r = c.report();
    r.invariants = inv.clone();
    println!("{}", r.render());
    assert!(inv.is_exact(), "{inv:?}");
    assert_eq!(inv.players_playing, s.clients);
    assert_eq!(inv.pending_transfers, 0, "{inv:?}");
    assert_eq!(inv.view_missing, 0, "{inv:?}");
    assert_eq!(inv.view_phantoms, 0, "{inv:?}");
    assert!(inv.views_checked >= s.clients);
    let started = total(&c, |x| x.handoffs_started);
    assert!(started >= 15, "traspasos: {started}");
    assert_eq!(total(&c, |x| x.handoffs_completed), started);
    assert_eq!(r.switches, started);
    assert_eq!(r.switch_failures, 0);
    assert_eq!(
        total(&c, |x| x.bus_rejected),
        0,
        "ningún datagrama legítimo se rechaza"
    );
    assert_eq!(total(&c, |x| x.bus_malformed), 0);
    assert!(total(&c, |x| x.npc_xfers_out) >= 15);
    assert_eq!(
        total(&c, |x| x.npc_xfers_out),
        total(&c, |x| x.npc_xfers_in)
    );
}

#[test]
fn un_intruso_en_el_bus_no_consigue_nada_y_queda_registrado() {
    let s = scenario((2, 1), 12, 10);
    let mut c = UdpCluster::new(&s, KEY).unwrap();
    let raw = UdpSocket::bind("127.0.0.1:0").unwrap();
    let wall = system_wall_ms();
    let wrong = bus_key(b"la-clave-de-un-atacante-0123456789");
    let st = EntityState {
        pos: [0, 0, 0],
        data: 0,
    };
    let mut forged = 0u64;
    let mut counter = wall * 1000;
    while c.now < s.duration_ms {
        c.step();
        if c.now.is_multiple_of(100) {
            // Se hace pasar por el shard 1 ante el 0 (y al revés) con mensajes que, de ser aceptados,
            // robarían NPC, teletransportarían jugadores o inventarían fantasmas.
            for (from, to) in [(1u16, 0u16), (0, 1)] {
                let msgs = [
                    PeerMsg::NpcXfer(NpcXfer {
                        xfer: 9_000_000 + counter as u32,
                        id: NPC_BASE + 5,
                        pos: [9999.0, 0.0, 9999.0],
                        dir: [1.0, 0.0],
                        turn_in: 5,
                        rng: 77,
                    }),
                    PeerMsg::Handover {
                        xfer: 1,
                        player: 1000,
                        pos: [0, 0, 0],
                        last_processed: 1,
                    },
                    PeerMsg::Ghosts {
                        tick: u32::MAX,
                        players: vec![(1000, st.clone())],
                        npcs: vec![(NPC_BASE + 7, st.clone())],
                    },
                ];
                for m in msgs {
                    counter += 1;
                    let d = seal(&wrong, from, to, counter, wall, 0, &m.encode());
                    raw.send_to(&d, c.bus_addrs[to as usize]).unwrap();
                    forged += 1;
                }
            }
        }
    }
    c.settle();
    c.run_for(s.settle_ms);
    let inv = c.check(true);
    assert!(inv.is_exact(), "{inv:?}");
    assert_eq!(inv.players_playing, s.clients);
    assert_eq!(inv.view_missing, 0, "{inv:?}");
    assert_eq!(inv.view_phantoms, 0, "{inv:?}");
    assert_eq!(
        total(&c, |x| x.bus_rejected),
        forged,
        "todo lo falsificado se rechazó"
    );
    assert_eq!(
        total(&c, |x| x.bus_malformed),
        0,
        "ni siquiera llegó a decodificarse"
    );
    assert!(
        total(&c, |x| x.handoffs_completed) >= 5,
        "el tráfico legítimo siguió"
    );
}

#[test]
fn con_una_clave_de_cluster_distinta_los_shards_no_se_fian_y_nada_se_pierde() {
    let s = scenario((2, 1), 10, 8);
    let mut c = UdpCluster::with_keys(&s, &[KEY, b"otra-clave-de-cluster-bien-larga"]).unwrap();
    while c.now < s.duration_ms {
        c.step();
        if c.now.is_multiple_of(1000) {
            let inv = c.check(false);
            assert_eq!(inv.npc_lost, 0, "t={}", c.now);
            assert_eq!(inv.players_lost, 0, "t={}", c.now);
            assert_eq!(inv.players_duplicated, 0, "t={}", c.now);
            assert_eq!(inv.players_playing, s.clients, "t={}", c.now);
        }
    }
    assert_eq!(
        total(&c, |x| x.handoffs_completed),
        0,
        "sin confianza no hay traspasos"
    );
    assert!(
        total(&c, |x| x.handoffs_aborted) > 0,
        "los plazos cancelan lo que no avanza"
    );
    assert!(
        c.shards
            .iter()
            .all(|s| s.cluster_stats().unwrap().bus_rejected > 100),
        "los dos shards registran el rechazo"
    );
    assert_eq!(
        total(&c, |x| x.ghost_applied),
        0,
        "no se acepta ni un fantasma"
    );
    let inv = c.check(false);
    assert!(inv.is_exact(), "{inv:?}");
}
