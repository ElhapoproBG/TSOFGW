//! Pruebas del cluster de shards: reparto del mundo, fantasmas en los bordes, traspaso de NPC y de
//! jugadores, y tolerancia a una red entre shards que pierde, duplica y desordena.

use tsofgw_net::EntityState;
use tsofgw_online::bus::{NpcXfer, PeerBus, PeerMsg, SimBusConfig};
use tsofgw_online::cluster_scenario::{run_cluster, ClusterRun, ClusterScenario};
use tsofgw_online::partition::Partition;
use tsofgw_online::scenario::{self, Scenario};
use tsofgw_online::{ClusterConfig, ClusterStats, GameConfig, NPC_BASE};
use tsofgw_transport::SimConfig;

fn base(grid: (u16, u16), clients: usize, patrollers: usize) -> ClusterScenario {
    ClusterScenario {
        grid,
        clients,
        patrollers,
        duration_ms: 25_000,
        settle_ms: 5000,
        game: GameConfig {
            npc_count: 3000,
            // NPC más rápidos de lo normal: así cruzan los bordes muchas veces en poco tiempo simulado.
            npc_speed: 10.0,
            spawn_half_extent: 40.0,
            max_visible: 100_000,
            ..GameConfig::default()
        },
        ..ClusterScenario::default()
    }
}

fn lossless(mut s: ClusterScenario) -> ClusterScenario {
    s.net = SimConfig {
        loss: 0.0,
        duplicate: 0.0,
        ..s.net
    };
    s.bus = SimBusConfig {
        loss: 0.0,
        duplicate: 0.0,
        ..s.bus
    };
    s
}

fn total(r: &tsofgw_online::cluster_scenario::ClusterReport, f: fn(&ClusterStats) -> u64) -> u64 {
    r.shards.iter().map(|x| f(&x.cluster)).sum()
}

#[test]
fn un_cluster_sin_perdidas_reparte_y_traspasa_sin_perder_nada() {
    let s = lossless(base((2, 2), 32, 24));
    let mut run = ClusterRun::new(&s).unwrap();
    let mut mid_checks = 0;
    let mut step_max = 0.0f32;
    let mut last: Vec<Option<[f32; 3]>> = vec![None; s.clients];
    while run.now < s.duration_ms {
        run.step();
        // La posición predicha de cada cliente avanza como mucho un paso por tick, también al cambiar de shard.
        for (i, b) in run.bots.iter().enumerate() {
            if let Some(p) = b.client.predicted_position() {
                if let Some(q) = last[i] {
                    let d = ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2))
                        .sqrt();
                    step_max = step_max.max(d);
                }
                last[i] = Some(p);
            }
        }
        if run.now.is_multiple_of(1000) {
            // Durante el juego: nada se pierde nunca; un duplicado solo puede existir esperando un acuse.
            let inv = run.check(false);
            assert_eq!(inv.npc_lost, 0, "t={}", run.now);
            assert_eq!(inv.players_lost, 0, "t={}", run.now);
            assert_eq!(inv.players_playing, s.clients, "t={}", run.now);
            if inv.npc_duplicated > 0 || inv.players_duplicated > 0 {
                assert!(
                    inv.pending_transfers > 0 || inv.bus_in_flight > 0,
                    "t={}: {inv:?}",
                    run.now
                );
            }
            mid_checks += 1;
        }
    }
    assert_eq!(mid_checks, 25);
    // Los bots se detienen y todo converge.
    run.settle();
    run.run_for(s.settle_ms);
    let inv = run.check(true);
    let mut r = run.report();
    r.invariants = inv.clone();
    println!("{}", r.render());
    assert!(inv.is_exact(), "{inv:?}");
    assert_eq!(inv.players_playing, s.clients);
    assert_eq!(inv.pending_transfers, 0, "{inv:?}");
    assert_eq!(inv.bus_in_flight, 0);
    assert!(inv.views_checked >= s.clients, "{inv:?}");
    assert_eq!(
        inv.view_missing, 0,
        "faltan entidades junto a los bordes: {inv:?}"
    );
    assert_eq!(inv.view_phantoms, 0, "{inv:?}");
    assert!(
        inv.view_error_max < 1.0,
        "error de posición {}",
        inv.view_error_max
    );

    let started = total(&r, |c| c.handoffs_started);
    assert!(
        started >= 20,
        "los patrulleros deben cruzar bordes muchas veces: {started}"
    );
    assert_eq!(
        total(&r, |c| c.handoffs_completed),
        started,
        "todo traspaso empezado termina"
    );
    assert_eq!(total(&r, |c| c.arrivals_completed), started);
    assert_eq!(total(&r, |c| c.handoffs_aborted), 0);
    assert_eq!(
        r.switches, started,
        "cada traspaso es un cambio de shard en el cliente"
    );
    assert_eq!(r.switch_failures, 0);
    assert_eq!(
        r.corrections, 0,
        "sin pérdida, la predicción nunca se corrige, tampoco al cambiar de shard"
    );
    assert!(
        step_max <= 0.376,
        "la posición predicha dio un salto de {step_max} u"
    );
    let npc_out = total(&r, |c| c.npc_xfers_out);
    assert!(npc_out >= 20, "deben cruzar NPC: {npc_out}");
    assert_eq!(npc_out, total(&r, |c| c.npc_xfers_in));
    // Cada shard simula solo lo suyo: ninguno tiene todos los NPC.
    assert!(r.shards.iter().all(|x| x.npcs_owned < s.game.npc_count / 2));
    assert_eq!(
        r.shards.iter().map(|x| x.npcs_owned).sum::<usize>(),
        s.game.npc_count
    );
    // La cola de inputs de los jugadores no se queda atrasada tras tantos traspasos.
    for (i, b) in run.bots.iter().enumerate() {
        let id = b.client.entity();
        let owner = run.shards.iter().find(|sh| sh.owns_player(id)).unwrap();
        assert!(owner.queued_inputs(id).unwrap() <= 1, "cliente {i}");
    }
}

#[test]
fn con_perdida_duplicados_y_desorden_el_reparto_converge() {
    let mut s = base((2, 2), 32, 24);
    s.net.loss = 0.05;
    s.bus = SimBusConfig {
        latency_ms: 15,
        jitter_ms: 40,
        loss: 0.25,
        duplicate: 0.25,
        seed: 99,
    };
    let r = run_cluster(&s).unwrap();
    println!("{}", r.render());
    let inv = &r.invariants;
    assert!(inv.is_exact(), "{inv:?}");
    assert_eq!(inv.players_playing, s.clients);
    assert_eq!(inv.pending_transfers, 0);
    assert_eq!(inv.view_missing, 0, "{inv:?}");
    assert_eq!(inv.view_phantoms, 0, "{inv:?}");
    assert!(total(&r, |c| c.npc_xfers_out) >= 20);
    assert!(
        total(&r, |c| c.npc_xfer_retries) > 0,
        "con 25 % de pérdida hay que reenviar traspasos"
    );
    assert!(
        total(&r, |c| c.npc_xfer_dups) > 0,
        "con 25 % de duplicados hay que detectar repeticiones"
    );
    assert_eq!(
        total(&r, |c| c.npc_xfer_conflicts),
        0,
        "nunca dos dueños a la vez"
    );
    assert!(total(&r, |c| c.handoffs_completed) >= 10);
    assert_eq!(total(&r, |c| c.handovers_unacked), 0);
    assert!(r.bus.dropped > 100 && r.bus.duplicated > 100);
}

#[test]
fn una_red_entre_shards_muy_mala_nunca_pierde_ni_duplica_nada() {
    let mut s = base((2, 1), 16, 12);
    s.duration_ms = 30_000;
    s.settle_ms = 10_000;
    s.net.loss = 0.10;
    s.bus = SimBusConfig {
        latency_ms: 30,
        jitter_ms: 120,
        loss: 0.5,
        duplicate: 0.4,
        seed: 5,
    };
    let mut run = ClusterRun::new(&s).unwrap();
    while run.now < s.duration_ms {
        run.step();
        if run.now.is_multiple_of(2000) {
            let inv = run.check(false);
            assert_eq!(inv.npc_lost, 0, "t={}", run.now);
            assert_eq!(inv.players_lost, 0, "t={}", run.now);
        }
    }
    run.settle();
    run.run_for(s.settle_ms);
    let inv = run.check(true);
    let mut r = run.report();
    r.invariants = inv.clone();
    println!("{}", r.render());
    assert!(inv.is_exact(), "{inv:?}");
    assert_eq!(
        inv.players_playing, s.clients,
        "nadie se desconecta por culpa del bus"
    );
    assert_eq!(inv.pending_transfers, 0);
    assert_eq!(inv.view_missing, 0, "{inv:?}");
    assert!(
        total(&r, |c| c.handoffs_completed) >= 5,
        "a pesar de todo se traspasa"
    );
    assert!(total(&r, |c| c.npc_xfer_retries) > 20);
}

#[test]
fn oscilar_sobre_el_borde_no_provoca_traspasos_pero_alejarse_si() {
    // Todos aparecen pegados al cruce de las cuatro regiones y se mueven ±4,5 u: cruzan el borde, pero
    // nunca llegan a la profundidad (6 u) que exige un traspaso.
    let mut dither = lossless(base((2, 2), 8, 8));
    dither.game.spawn_half_extent = 1.0;
    dither.patrol_ms = 600;
    dither.duration_ms = 15_000;
    let r = run_cluster(&dither).unwrap();
    assert_eq!(
        total(&r, |c| c.handoffs_started),
        0,
        "sin ping-pong: {}",
        r.render()
    );
    assert_eq!(r.switches, 0);
    assert!(
        total(&r, |c| c.ghost_players_out) > 0,
        "pero sí se ven como copias en el vecino"
    );
    assert!(r.invariants.is_exact());
    assert_eq!(r.invariants.view_missing, 0);

    // El mismo escenario con paseos largos sí traspasa.
    let mut walk = dither.clone();
    walk.patrol_ms = 4000;
    let r = run_cluster(&walk).unwrap();
    assert!(total(&r, |c| c.handoffs_started) >= 4, "{}", r.render());
    assert!(r.invariants.is_exact());
}

#[test]
fn un_cluster_de_un_shard_es_identico_a_un_servidor_suelto() {
    // Con una sola región no hay bordes: el tráfico que sale por la red debe ser byte a byte el mismo.
    let game = GameConfig {
        npc_count: 800,
        max_visible: 100_000,
        ..GameConfig::default()
    };
    let net = SimConfig {
        loss: 0.03,
        ..SimConfig::default()
    };
    let (clients, duration, settle, step) = (12usize, 8000u64, 2000u64, 10u64);
    let plain = scenario::run(&Scenario {
        clients,
        duration_ms: duration,
        settle_ms: settle,
        net: net.clone(),
        game: game.clone(),
        step_ms: step,
        digest: true,
        ..Scenario::default()
    });

    let mut run = ClusterRun::new(&ClusterScenario {
        grid: (1, 1),
        clients,
        patrollers: 0,
        duration_ms: duration,
        settle_ms: settle,
        net,
        game,
        step_ms: step,
        master: Scenario::default().master,
        ..ClusterScenario::default()
    })
    .unwrap();
    run.net.start_log();
    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    let mut fold = |run: &mut ClusterRun| {
        for (from, to, data) in run.net.take_log() {
            for b in from
                .0
                .to_le_bytes()
                .iter()
                .chain(&to.0.to_le_bytes())
                .chain(&data)
            {
                digest = (digest ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
            digest = (digest ^ data.len() as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    while run.now + step < duration {
        run.step();
        fold(&mut run);
    }
    run.settle();
    while run.now < duration + settle {
        run.step();
        fold(&mut run);
    }
    assert_eq!(
        digest, plain.net_digest,
        "el tráfico difiere del de un servidor suelto"
    );
    assert_eq!(run.check(false).players_playing, clients);
}

#[test]
fn el_cluster_es_determinista() {
    let mut s = base((2, 2), 16, 12);
    s.duration_ms = 12_000;
    s.settle_ms = 3000;
    let a = run_cluster(&s).unwrap();
    let b = run_cluster(&s).unwrap();
    let key = |r: &tsofgw_online::cluster_scenario::ClusterReport| {
        r.shards
            .iter()
            .map(|x| {
                let c = &x.cluster;
                (
                    x.players,
                    x.npcs_owned,
                    x.ghosts,
                    c.handoffs_completed,
                    c.npc_xfers_out,
                    c.npc_xfer_retries,
                    c.ghost_applied,
                    c.bus_bytes_out,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(key(&a), key(&b));
    assert_eq!(
        (a.bus.sent, a.bus.dropped, a.switches),
        (b.bus.sent, b.bus.dropped, b.switches)
    );
    assert!(total(&a, |c| c.handoffs_completed) > 0);
}

#[test]
fn si_el_destino_no_responde_el_jugador_sigue_donde_estaba_y_luego_se_traspasa() {
    let mut s = lossless(base((2, 1), 8, 8));
    s.duration_ms = 60_000;
    let mut run = ClusterRun::new(&s).unwrap();
    // El bus entre shards está caído por completo.
    run.bus.set_config(SimBusConfig {
        loss: 1.0,
        ..s.bus.clone()
    });
    run.run_for(25_000);
    let r = run.report();
    assert!(total(&r, |c| c.handoffs_started) > 0);
    assert!(
        total(&r, |c| c.handoffs_aborted) > 0,
        "los plazos cancelan lo que no avanza"
    );
    assert_eq!(total(&r, |c| c.handoffs_completed), 0);
    assert_eq!(r.switches, 0);
    assert_eq!(run.check(false).players_playing, s.clients, "nadie se cae");
    assert_eq!(run.check(false).players_lost, 0);

    // Vuelve el bus: los traspasos pendientes se completan y todo queda en orden.
    run.bus.set_config(s.bus.clone());
    run.run_for(25_000);
    run.settle();
    run.run_for(8000);
    let inv = run.check(true);
    let mut r = run.report();
    r.invariants = inv.clone();
    println!("{}", r.render());
    assert!(inv.is_exact(), "{inv:?}");
    assert_eq!(inv.players_playing, s.clients);
    assert_eq!(inv.view_missing, 0, "{inv:?}");
    assert!(total(&r, |c| c.handoffs_completed) >= 4);
    assert!(r.switches >= 4);
}

#[test]
fn los_mensajes_hostiles_del_bus_no_rompen_ni_corrompen_nada() {
    let s = lossless(base((2, 1), 6, 4));
    let mut run = ClusterRun::new(&s).unwrap();
    run.run_for(3000);
    let before = run.check(false);
    let mut evil = run.bus.end(1); // se hace pasar por el shard 1 hacia el 0
    let mut selfish = run.bus.end(0); // y un mensaje «de sí mismo»
    let st = EntityState {
        pos: [100, 0, 100],
        data: 0,
    };
    let garbage: Vec<Vec<u8>> = vec![
        vec![],
        vec![1],
        vec![0xFF; 64],
        vec![
            1, 1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F,
        ],
        {
            let mut m = PeerMsg::Abort { xfer: 1, player: 2 }.encode();
            m.push(9); // bytes sobrantes
            m
        },
        PeerMsg::Ghosts {
            tick: 1,
            players: vec![],
            npcs: vec![(NPC_BASE + 1, st.clone())],
        }
        .encode()[..4]
            .to_vec(), // truncado
    ];
    for g in &garbage {
        evil.send(run.now, 0, g.clone());
    }
    selfish.send(run.now, 0, PeerMsg::Abort { xfer: 1, player: 2 }.encode());
    // Decodificables pero sin sentido para este shard: deben ignorarse sin efecto.
    let weird = [
        PeerMsg::Ghosts {
            tick: 5,
            players: vec![],
            npcs: vec![(NPC_BASE + 50_000_000, st.clone())], // NPC que no existe
        },
        PeerMsg::NpcXfer(NpcXfer {
            xfer: 1,
            id: NPC_BASE + 50_000_000,
            pos: [0.0; 3],
            dir: [0.0; 2],
            turn_in: 0,
            rng: 3,
        }),
        PeerMsg::Handover {
            xfer: 1,
            player: 77_777,
            pos: [0, 0, 0],
            last_processed: 1,
            epoch: 1,
        },
        // Mensajes del directorio de sesiones: ningún shard los puede enviar, y un shard no los atiende.
        PeerMsg::DirRoute {
            account: 77_777,
            shard: 0,
            epoch: 1,
        },
        PeerMsg::DirResult {
            xfer: 1,
            account: 77_777,
            status: tsofgw_online::bus::DirStatus::Committed,
            shard: 1,
            epoch: 2,
        },
        PeerMsg::DirLogin {
            account: 77_777,
            home: 0,
        },
        PeerMsg::DirTransfer {
            xfer: 1,
            account: 77_777,
            epoch: 1,
            to: 1,
        },
        PeerMsg::Activate {
            xfer: 1,
            player: 77_777,
        },
        PeerMsg::PrepareAck {
            xfer: 1,
            player: 77_777,
        },
        PeerMsg::HandoverAck {
            xfer: 1,
            player: 77_777,
        },
        PeerMsg::NpcXferAck {
            xfer: 1,
            id: NPC_BASE + 3,
        },
        PeerMsg::Abort {
            xfer: 1,
            player: 77_777,
        },
        PeerMsg::Prepare {
            xfer: 1,
            player: NPC_BASE + 1, // un id de NPC como jugador
            pos: [0, 0, 0],
            last_processed: 0,
        },
    ];
    for m in &weird {
        evil.send(run.now, 0, m.encode());
    }
    run.run_for(5000);
    let st0 = run.shards[0].cluster_stats().unwrap();
    assert!(
        st0.bus_malformed > garbage.len() as u64,
        "malformados: {}",
        st0.bus_malformed
    );
    assert!(!run.shards[0].owns_player(77_777) && !run.shards[1].owns_player(77_777));
    let after = run.check(false);
    assert_eq!(after.players_playing, before.players_playing);
    assert_eq!(after.npc_lost, 0);
    assert_eq!(after.npc_duplicated, 0);
    assert_eq!(after.players_lost, 0);
    run.settle();
    run.run_for(4000);
    assert!(run.check(true).is_exact());
}

#[test]
fn configuraciones_de_cluster_imposibles_se_rechazan_con_un_motivo() {
    let game = GameConfig::default();
    let p22 = Partition::grid(2, 2, 512.0).unwrap();
    assert!(ClusterConfig::new(0, p22.clone(), &game).is_ok());
    let e = ClusterConfig::new(4, p22.clone(), &game).unwrap_err();
    assert!(e.contains("no existe"), "{e}");
    // Regiones más pequeñas que el doble del margen de fantasmas.
    let e = ClusterConfig::new(0, Partition::grid(8, 8, 512.0).unwrap(), &game).unwrap_err();
    assert!(e.contains("demasiado pequeñas"), "{e}");
    // Un margen que no cubre lo que ve un jugador dejaría huecos en el borde.
    let mut c = ClusterConfig::new(0, p22.clone(), &game).unwrap();
    c.ghost_margin = 40.0;
    assert!(c.validate(&game).unwrap_err().contains("no cubre"));
    let mut c = ClusterConfig::new(0, p22, &game).unwrap();
    c.retry_ticks = 0;
    assert!(c.validate(&game).is_err());
    c.retry_ticks = 4;
    c.handoff_timeout_ticks = 8;
    assert!(c.validate(&game).is_err());
    // Un solo shard no necesita margen.
    let mut one = ClusterConfig::new(0, Partition::single(512.0), &game).unwrap();
    one.ghost_margin = 0.0;
    assert!(one.validate(&game).is_ok());
}

#[test]
fn un_cluster_mas_grande_con_muchos_clientes_cruzando() {
    let mut s = base((3, 2), 60, 45);
    s.game.npc_count = 6000;
    s.duration_ms = 20_000;
    let r = run_cluster(&s).unwrap();
    println!("{}", r.render());
    let inv = &r.invariants;
    assert!(inv.is_exact(), "{inv:?}");
    assert_eq!(inv.players_playing, s.clients);
    assert_eq!(inv.view_missing, 0, "{inv:?}");
    assert_eq!(inv.view_phantoms, 0, "{inv:?}");
    assert!(total(&r, |c| c.handoffs_completed) >= 20);
    assert_eq!(r.shards.len(), 6);
}

#[test]
fn los_shards_no_necesitan_llevar_la_misma_cuenta_de_ticks() {
    // En un despliegue real cada shard arranca cuando arranca: sus contadores de tick no coinciden. Aquí
    // el shard 1 empieza 3 s tarde (60 ticks de diferencia). Un cliente que pasa de uno a otro no puede
    // quedarse atascado esperando un tick «más nuevo» que el de la conexión anterior.
    let mut s = lossless(base((2, 1), 8, 8));
    s.duration_ms = 40_000;
    let mut run = ClusterRun::new(&s).unwrap();
    let late = 3000;
    let mut last_switch = vec![(0u64, 0u64); s.clients];
    let (mut checked, mut behind_switches) = (0, 0);
    while run.now < s.duration_ms {
        run.now += run.step_ms;
        run.net.set_now(run.now);
        for (i, sh) in run.shards.iter_mut().enumerate() {
            if i == 1 && run.now < late {
                continue;
            }
            sh.update(run.now);
        }
        for (i, b) in run.bots.iter_mut().enumerate() {
            b.update(run.now);
            let n = b.client.stats().switches;
            if n != last_switch[i].0 {
                last_switch[i] = (n, run.now);
            }
        }
        if run.now > late + 2000 && run.now.is_multiple_of(500) {
            for (i, b) in run.bots.iter().enumerate() {
                // Un segundo después de cada cambio, el cliente debe ir al día de su servidor.
                if run.now - last_switch[i].1 < 1000 || b.client.switching_to().is_some() {
                    continue;
                }
                let id = b.client.entity();
                let Some(owner) = run.shards.iter().find(|sh| sh.owns_player(id)) else {
                    continue;
                };
                let (Some(server), Some(client)) =
                    (owner.last_snapshot(id), b.client.latest_snapshot())
                else {
                    continue;
                };
                assert!(
                    server.tick.abs_diff(client.tick) <= 3,
                    "t={}: el cliente {id} (cambios: {}) va por el tick {} y su servidor por el {}",
                    run.now,
                    last_switch[i].0,
                    client.tick,
                    server.tick
                );
                checked += 1;
                if last_switch[i].0 > 0 && owner.shard_id() == Some(1) {
                    behind_switches += 1; // ya pasó a un shard que va por detrás
                }
            }
        }
    }
    assert!(
        checked > 100 && behind_switches > 5,
        "{checked} {behind_switches}"
    );
    run.settle();
    run.run_for(5000);
    let inv = run.check(true);
    assert!(inv.is_exact(), "{inv:?}");
    assert_eq!(inv.players_playing, s.clients);
    let r = run.report();
    assert!(r.switches >= 4, "debe haber cruzado: {}", r.render());
    let (t0, t1) = (run.shards[0].stats().ticks, run.shards[1].stats().ticks);
    assert!(t0 > t1 + 40, "los contadores de tick difieren: {t0} y {t1}");
}

#[test]
fn el_resumen_de_configuracion_distingue_lo_que_importa_y_solo_eso() {
    let game = GameConfig::default();
    let part = Partition::grid(2, 2, 512.0).unwrap();
    let cfg = ClusterConfig::new(0, part.clone(), &game).unwrap();
    let d = cfg.digest(&game);
    // Lo que cambia el mundo cambia el resumen...
    let changes: Vec<(&str, GameConfig)> = vec![
        (
            "npc_count",
            GameConfig {
                npc_count: game.npc_count + 1,
                ..game.clone()
            },
        ),
        (
            "seed",
            GameConfig {
                seed: 2,
                ..game.clone()
            },
        ),
        (
            "tick_ms",
            GameConfig {
                tick_ms: 40,
                ..game.clone()
            },
        ),
        (
            "npc_speed",
            GameConfig {
                npc_speed: 3.5,
                ..game.clone()
            },
        ),
        (
            "interest_radius",
            GameConfig {
                interest_radius: 61.0,
                ..game.clone()
            },
        ),
        (
            "sim_radius",
            GameConfig {
                sim_radius: 91.0,
                ..game.clone()
            },
        ),
        (
            "cell_size",
            GameConfig {
                cell_size: 16.0,
                ..game.clone()
            },
        ),
        (
            "hysteresis",
            GameConfig {
                hysteresis: 1.3,
                ..game.clone()
            },
        ),
    ];
    for (name, g) in changes {
        assert_ne!(cfg.digest(&g), d, "{name} debe cambiar el resumen");
    }
    let other_grid = ClusterConfig::new(0, Partition::grid(3, 1, 512.0).unwrap(), &game).unwrap();
    assert_ne!(other_grid.digest(&game), d);
    let mut wide = cfg.clone();
    wide.ghost_margin += 1.0;
    assert_ne!(wide.digest(&game), d);
    // ...y lo que es política local de cada shard, no. Tampoco depende de qué shard es.
    let local = GameConfig {
        threads: 8,
        max_players: 10,
        lod: vec![],
        snapshot_budget: 900,
        spawn_half_extent: 10.0,
        max_visible: 7,
        ..game.clone()
    };
    assert_eq!(cfg.digest(&local), d);
    let mut other = cfg.clone();
    other.shard = 3;
    other.retry_ticks = 9;
    other.player_depth = 12.0;
    assert_eq!(other.digest(&game), d);
}
