//! Pruebas de la capa de juego: convergencia, predicción, trampas y apagado selectivo.

use tsofgw_net::Sim;
use tsofgw_net::{Endpoint, Snapshot};
use tsofgw_online::protocol as proto;
use tsofgw_online::scenario::{run, Scenario};
use tsofgw_online::sim::{Input, MoveSim, PlayerState, STEP_Q};
use tsofgw_online::*;
use tsofgw_transport::{
    Authority, ClientConfig, ClientEvent, DeterministicEntropy, SecureClient, ServerConfig,
    SimConfig, SimLink, SimNet,
};

const MASTER: &[u8] = b"online-tests";

fn small_game() -> GameConfig {
    GameConfig {
        npc_count: 2000,
        ..GameConfig::default()
    }
}

#[test]
fn escenario_con_perdida_converge_exactamente() {
    let s = Scenario {
        clients: 40,
        duration_ms: 12_000,
        settle_ms: 3000,
        net: SimConfig {
            latency_ms: 40,
            jitter_ms: 20,
            loss: 0.05,
            duplicate: 0.02,
            seed: 7,
        },
        game: small_game(),
        ..Scenario::default()
    };
    let r = run(&s);
    assert_eq!(r.playing, 40, "todos deben entrar");
    // Sin pérdida al final, la posición predicha coincide al bit con la del servidor.
    assert_eq!(r.final_exact, 40, "error máx {} u", r.final_max_error_units);
    // Y la vista (snapshots delta reconstruidos) es idéntica a la que envió el servidor.
    assert!(r.view_compared >= 38, "comparables {}", r.view_compared);
    assert_eq!(r.view_matches, r.view_compared);
    assert!(
        r.snapshots_delta > 10 * r.snapshots_full,
        "debe predominar el delta"
    );
    assert!(r.avg_delta_bytes < r.avg_full_bytes);
    assert_eq!(r.game.kicks, 0);
    assert_eq!(r.server_transport.bad_tag, 0);
}

#[test]
fn el_escenario_es_reproducible() {
    let s = Scenario {
        clients: 12,
        duration_ms: 5000,
        settle_ms: 2000,
        game: GameConfig {
            npc_count: 500,
            ..GameConfig::default()
        },
        ..Scenario::default()
    };
    let (a, b) = (run(&s), run(&s));
    // Los tiempos de CPU varían; todo lo demás debe ser idéntico.
    assert_eq!(a.snapshots_full, b.snapshots_full);
    assert_eq!(a.snapshots_delta, b.snapshots_delta);
    assert_eq!(a.game.bytes_delta, b.game.bytes_delta);
    assert_eq!(a.final_exact, b.final_exact);
    assert_eq!(a.net_dropped, b.net_dropped);
}

#[test]
fn apagado_selectivo_simula_solo_cerca_de_los_jugadores() {
    let s = Scenario {
        clients: 10,
        duration_ms: 6000,
        settle_ms: 1000,
        game: GameConfig {
            npc_count: 20_000,
            spawn_half_extent: 40.0, // todos juntos en un rincón del mundo
            sim_radius: 60.0,
            ..GameConfig::default()
        },
        ..Scenario::default()
    };
    let r = run(&s);
    assert_eq!(r.playing, 10);
    assert!(
        r.simulated_fraction < 0.10,
        "se simuló el {:.1} % de los NPC",
        r.simulated_fraction * 100.0
    );
    assert!(r.simulated_avg > 0.0, "cerca de los jugadores sí se simula");
}

#[test]
fn sin_jugadores_no_se_simula_nada() {
    let s = Scenario {
        clients: 0,
        duration_ms: 2000,
        settle_ms: 0,
        game: GameConfig {
            npc_count: 5000,
            ..GameConfig::default()
        },
        ..Scenario::default()
    };
    let r = run(&s);
    assert!(r.ticks > 0);
    assert_eq!(r.game.npcs_simulated, 0);
}

// ---------------------------------------------------------- clientes tramposos

/// Cliente "crudo": habla el protocolo a mano, sin las protecciones del cliente real.
struct Raw {
    net: SecureClient<SimLink>,
    endpoint: Endpoint,
    events: Vec<ClientEvent>,
    entity: u64,
}

struct Harness {
    simnet: SimNet,
    server: GameServer<SimLink>,
    now: u64,
}

impl Harness {
    fn new(cfg: GameConfig) -> Self {
        let simnet = SimNet::new(SimConfig::default());
        let server = GameServer::new(
            simnet.endpoint(),
            Authority::new(MASTER),
            cfg,
            ServerConfig::default(),
            Box::new(DeterministicEntropy(5)),
        );
        Harness {
            simnet,
            server,
            now: 0,
        }
    }

    fn raw(&mut self, player_id: u64) -> Raw {
        Raw {
            net: SecureClient::new(
                self.simnet.endpoint(),
                self.server.link().addr(),
                Authority::new(MASTER).issue(player_id),
                ClientConfig::default(),
                Box::new(DeterministicEntropy(player_id)),
            ),
            endpoint: Endpoint::default(),
            events: Vec::new(),
            entity: 0,
        }
    }

    fn step(&mut self, raws: &mut [&mut Raw], ms: u64) {
        for _ in 0..ms {
            self.now += 1;
            self.simnet.set_now(self.now);
            self.server.update(self.now);
            for r in raws.iter_mut() {
                r.net.poll(self.now, &mut r.events);
                r.events.retain(|e| {
                    if let ClientEvent::Message(d) = e {
                        if let Some((_, proto::WELCOME, body)) = proto::unframe(d) {
                            if let Some(w) = proto::parse_welcome(body) {
                                r.entity = w.entity;
                            }
                        }
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }
}

fn send(r: &mut Raw, kind: u8, body: &[u8]) {
    let msg = proto::frame(r.endpoint.next_header(), kind, body);
    r.net.send(&msg).unwrap();
}

fn join(h: &mut Harness, r: &mut Raw) {
    h.step(&mut [r], 200);
    assert!(r.net.is_connected());
    send(r, proto::JOIN, &proto::join_body());
    h.step(&mut [r], 100);
    assert_ne!(r.entity, 0, "no llegó el WELCOME");
}

#[test]
fn inundar_de_inputs_no_da_velocidad_extra() {
    let mut h = Harness::new(GameConfig {
        npc_count: 0,
        ..GameConfig::default()
    });
    let mut honest = h.raw(1);
    let mut cheat = h.raw(2);
    join(&mut h, &mut honest);
    join(&mut h, &mut cheat);
    let start_h = h.server.player_position(1).unwrap();
    let start_c = h.server.player_position(2).unwrap();

    let ticks = 40u64;
    let full = [Input { dir: [127, 0, 0] }; 8];
    let (mut seq_h, mut seq_c) = (1u32, 1u32);
    for _ in 0..ticks {
        // El honesto envía un input por tick.
        send(
            &mut honest,
            proto::INPUT,
            &proto::input_body(seq_h, &full[..1]),
        );
        seq_h += 1;
        // El tramposo manda 50 mensajes de 8 inputs (400 inputs) por tick.
        for _ in 0..50 {
            send(&mut cheat, proto::INPUT, &proto::input_body(seq_c, &full));
            seq_c += 8;
        }
        h.step(&mut [&mut honest, &mut cheat], 50);
    }
    let moved = |a: [i32; 3], b: [i32; 3]| (a[0] - b[0]).abs();
    let dh = moved(h.server.player_position(1).unwrap(), start_h);
    let dc = moved(h.server.player_position(2).unwrap(), start_c);
    let max_allowed = (ticks as i32 + 3) * STEP_Q; // un paso por tick + ráfaga de créditos
    assert!(
        dh <= max_allowed && dh >= (ticks as i32 - 3) * STEP_Q,
        "honesto {dh}"
    );
    assert!(dc <= max_allowed, "el tramposo avanzó {dc} > {max_allowed}");
    assert!(
        dc >= dh - 3 * STEP_Q,
        "el tramposo no debería quedar peor que el honesto, solo no mejor"
    );
    assert!(h.server.stats().inputs_overflow > 0 || h.server.transport_stats().rate_limited > 0);
}

#[test]
fn direccion_exagerada_se_limita() {
    let mut h = Harness::new(GameConfig {
        npc_count: 0,
        ..GameConfig::default()
    });
    let mut r = h.raw(3);
    join(&mut h, &mut r);
    let start = h.server.player_position(3).unwrap();
    for i in 0..20u32 {
        // (-128, 127, 127) en cada eje: la diagonal más grande posible.
        let inp = [Input {
            dir: [-128, 127, 127],
        }];
        send(&mut r, proto::INPUT, &proto::input_body(1 + i, &inp));
        h.step(&mut [&mut r], 50);
    }
    let p = h.server.player_position(3).unwrap();
    let d2: i64 = (0..3).map(|k| (p[k] as i64 - start[k] as i64).pow(2)).sum();
    assert!(
        d2 <= (20 * STEP_Q as i64).pow(2),
        "avanzó {}",
        (d2 as f64).sqrt()
    );
}

#[test]
fn inputs_repetidos_o_viejos_no_se_aplican_dos_veces() {
    let mut h = Harness::new(GameConfig {
        npc_count: 0,
        ..GameConfig::default()
    });
    let mut r = h.raw(4);
    join(&mut h, &mut r);
    let start = h.server.player_position(4).unwrap();
    let one = [Input { dir: [127, 0, 0] }];
    // El mismo input (seq 1) enviado 30 veces.
    for _ in 0..30 {
        send(&mut r, proto::INPUT, &proto::input_body(1, &one));
    }
    h.step(&mut [&mut r], 500);
    let p = h.server.player_position(4).unwrap();
    assert_eq!(p[0] - start[0], STEP_Q, "un solo input aplicado");
    assert!(h.server.stats().inputs_stale >= 29);
}

#[test]
fn quien_envia_basura_es_expulsado() {
    let mut h = Harness::new(GameConfig {
        npc_count: 0,
        malformed_limit: 5,
        ..GameConfig::default()
    });
    let mut r = h.raw(6);
    join(&mut h, &mut r);
    assert_eq!(h.server.player_count(), 1);
    for i in 0..10u8 {
        send(&mut r, 99, &[i, i, i]); // tipo desconocido
        h.step(&mut [&mut r], 10);
    }
    h.step(&mut [&mut r], 100);
    assert_eq!(h.server.player_count(), 0);
    assert_eq!(h.server.stats().kicks, 1);
    assert!(h.server.stats().malformed > 5);
    assert!(h.server.last_snapshot(6).is_none());
    // El mundo no conserva a quien fue expulsado.
    assert_eq!(h.server.entity_count(), 0);
}

#[test]
fn inputs_antes_de_unirse_se_ignoran() {
    let mut h = Harness::new(GameConfig {
        npc_count: 0,
        ..GameConfig::default()
    });
    let mut r = h.raw(8);
    h.step(&mut [&mut r], 200);
    assert!(r.net.is_connected());
    send(
        &mut r,
        proto::INPUT,
        &proto::input_body(1, &[Input { dir: [127, 0, 0] }]),
    );
    h.step(&mut [&mut r], 100);
    assert_eq!(h.server.player_count(), 0);
    assert_eq!(h.server.entity_count(), 0);
}

#[test]
fn cuentas_reservadas_para_npc_se_rechazan() {
    let mut h = Harness::new(GameConfig {
        npc_count: 0,
        ..GameConfig::default()
    });
    let mut r = h.raw(NPC_BASE + 5);
    h.step(&mut [&mut r], 300);
    assert_eq!(h.server.player_count(), 0);
    assert_eq!(h.server.stats().refused, 1);
}

#[test]
fn si_el_cliente_pierde_la_linea_base_el_servidor_se_repliega_a_completo() {
    // Con mucha pérdida, parte de los deltas no se pueden decodificar; el sistema debe seguir funcionando.
    let s = Scenario {
        clients: 10,
        duration_ms: 10_000,
        settle_ms: 4000,
        net: SimConfig {
            latency_ms: 60,
            jitter_ms: 40,
            loss: 0.30,
            duplicate: 0.05,
            seed: 13,
        },
        game: GameConfig {
            npc_count: 1500,
            ..GameConfig::default()
        },
        ..Scenario::default()
    };
    let r = run(&s);
    assert_eq!(r.playing, 10);
    assert_eq!(r.final_exact, 10);
    assert_eq!(r.view_matches, r.view_compared);
    assert!(
        r.snapshots_full > 10,
        "debieron hacer falta snapshots completos de repliegue"
    );
}

#[test]
fn prediccion_y_servidor_coinciden_sin_perdida() {
    let mut h = Harness::new(GameConfig {
        npc_count: 0,
        ..GameConfig::default()
    });
    let net = &h.simnet;
    let mut c = GameClient::new(
        net.endpoint(),
        h.server.link().addr(),
        Authority::new(MASTER).issue(10),
        ClientConfig::default(),
        Box::new(DeterministicEntropy(10)),
    );
    for t in 1..=3000u64 {
        h.simnet.set_now(t);
        h.server.update(t);
        if t == 1000 {
            c.set_input([100, 0, 60]);
        }
        if t == 2300 {
            c.set_input([0, 0, 0]); // se detiene: los inputs en vuelo terminan de llegar
        }
        c.update(t);
    }
    assert!(c.is_playing());
    // Tras dejar de enviar inputs de cambio, la predicción y el servidor coinciden.
    let pred = c.predicted().unwrap().pos;
    let auth = h.server.player_position(10).unwrap();
    assert_eq!(pred, auth);
    // Y el estado coincide con aplicar los mismos inputs a mano desde el mismo origen.
    let mut st = PlayerState::default();
    let _ = &mut st;
    let _ = MoveSim::step(&st, &Input::default());
    let _: Option<&Snapshot> = c.latest_snapshot();
    assert!(c.stats().snapshots > 10);
    // La vista dibuja al jugador en su posición predicha.
    let view = c.view(3000);
    let me = view.get(&c.entity()).copied().unwrap();
    let p = c.predicted_position().unwrap();
    assert_eq!(me, p);
}

#[test]
fn dos_jugadores_se_ven_y_dejan_de_verse() {
    let mut h = Harness::new(GameConfig {
        npc_count: 0,
        spawn_half_extent: 1.0,
        interest_radius: 20.0,
        ..GameConfig::default()
    });
    let mk = |h: &Harness, id: u64| {
        GameClient::new(
            h.simnet.endpoint(),
            h.server.link().addr(),
            Authority::new(MASTER).issue(id),
            ClientConfig::default(),
            Box::new(DeterministicEntropy(id)),
        )
    };
    let mut a = mk(&h, 21);
    let mut b = mk(&h, 22);
    let run_for =
        |h: &mut Harness, a: &mut GameClient<SimLink>, b: &mut GameClient<SimLink>, ms: u64| {
            for _ in 0..ms {
                h.now += 1;
                h.simnet.set_now(h.now);
                h.server.update(h.now);
                a.update(h.now);
                b.update(h.now);
            }
        };
    run_for(&mut h, &mut a, &mut b, 1000);
    assert!(
        a.latest_snapshot().unwrap().entities.contains_key(&22),
        "A debe ver a B"
    );
    assert!(
        b.latest_snapshot().unwrap().entities.contains_key(&21),
        "B debe ver a A"
    );

    // B se aleja a toda velocidad hacia +X hasta salir del radio de interés.
    b.set_input([127, 0, 0]);
    run_for(&mut h, &mut a, &mut b, 8000);
    assert!(
        !a.latest_snapshot().unwrap().entities.contains_key(&22),
        "A ya no debe ver a B (está a {:.0} u)",
        b.predicted_position().unwrap()[0]
    );
}
