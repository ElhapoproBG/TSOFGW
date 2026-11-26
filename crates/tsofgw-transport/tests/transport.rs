//! Pruebas del transporte seguro sobre la red simulada y sobre UDP real.

use tsofgw_transport::*;

const MASTER: &[u8] = b"clave-maestra-de-pruebas";

struct Rig {
    net: SimNet,
    server: SecureServer<SimLink>,
    clients: Vec<SecureClient<SimLink>>,
    srv_events: Vec<ServerEvent<SimAddr>>,
    cli_events: Vec<Vec<ClientEvent>>,
    now: u64,
}

impl Rig {
    fn new(cfg: SimConfig) -> Self {
        let net = SimNet::new(cfg);
        let link = net.endpoint();
        let server = SecureServer::new(
            link,
            Authority::new(MASTER),
            ServerConfig::default(),
            Box::new(DeterministicEntropy(99)),
        );
        Rig {
            net,
            server,
            clients: Vec::new(),
            srv_events: Vec::new(),
            cli_events: Vec::new(),
            now: 0,
        }
    }

    fn server_addr(&self) -> SimAddr {
        self.server.link().addr()
    }

    fn add_client(&mut self, creds: Credentials, seed: u64) -> usize {
        let link = self.net.endpoint();
        let c = SecureClient::new(
            link,
            self.server_addr(),
            creds,
            ClientConfig::default(),
            Box::new(DeterministicEntropy(seed)),
        );
        self.clients.push(c);
        self.cli_events.push(Vec::new());
        self.clients.len() - 1
    }

    fn step(&mut self, ms: u64) {
        for _ in 0..ms {
            self.now += 1;
            self.net.set_now(self.now);
            self.server.poll(self.now, &mut self.srv_events);
            for (c, ev) in self.clients.iter_mut().zip(self.cli_events.iter_mut()) {
                c.poll(self.now, ev);
            }
        }
    }

    fn messages_at_server(&mut self) -> Vec<(SessionId, Vec<u8>)> {
        let mut out = Vec::new();
        self.srv_events.retain(|e| {
            if let ServerEvent::Message { session, data } = e {
                out.push((*session, data.clone()));
                false
            } else {
                true
            }
        });
        out
    }

    fn messages_at_client(&mut self, i: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.cli_events[i].retain(|e| {
            if let ClientEvent::Message(d) = e {
                out.push(d.clone());
                false
            } else {
                true
            }
        });
        out
    }

    fn connected_at_server(&self) -> usize {
        self.srv_events
            .iter()
            .filter(|e| matches!(e, ServerEvent::Connected { .. }))
            .count()
    }
}

fn authority() -> Authority {
    Authority::new(MASTER)
}

#[test]
fn handshake_y_mensajes_en_ambos_sentidos() {
    let mut r = Rig::new(SimConfig {
        latency_ms: 20,
        ..Default::default()
    });
    let c = r.add_client(authority().issue(42), 1);
    r.step(300);
    assert!(r.clients[c].is_connected());
    assert_eq!(r.connected_at_server(), 1);
    let session = r
        .srv_events
        .iter()
        .find_map(|e| match e {
            ServerEvent::Connected {
                session, player_id, ..
            } => {
                assert_eq!(*player_id, 42);
                Some(*session)
            }
            _ => None,
        })
        .unwrap();

    r.clients[c].send(b"hola servidor").unwrap();
    r.server.send(session, b"hola cliente").unwrap();
    r.step(100);
    assert_eq!(
        r.messages_at_server(),
        vec![(session, b"hola servidor".to_vec())]
    );
    assert_eq!(r.messages_at_client(c), vec![b"hola cliente".to_vec()]);
}

#[test]
fn el_handshake_no_amplifica() {
    let net = SimNet::new(SimConfig::default());
    let mut server = SecureServer::new(
        net.endpoint(),
        authority(),
        ServerConfig::default(),
        Box::new(DeterministicEntropy(1)),
    );
    let mut attacker = net.endpoint();
    let victim = net.endpoint().addr();
    let mut ev = Vec::new();

    // HELLO con dirección falsificada: la respuesta (CHALLENGE) es mucho menor que la petición.
    let hello = wire::hello(&[7u8; 16]);
    net.start_log();
    attacker.send_as(victim, server.link().addr(), &hello);
    server.poll(1, &mut ev);
    let log = net.take_log();
    let reply: Vec<_> = log
        .iter()
        .filter(|(from, _, _)| *from == server.link().addr())
        .collect();
    assert_eq!(reply.len(), 1);
    assert_eq!(reply[0].1, victim);
    assert!(
        reply[0].2.len() < hello.len() / 2,
        "la respuesta debe ser menor que la petición"
    );
    assert_eq!(
        server.session_count(),
        0,
        "no se reserva estado antes de AUTH"
    );

    // HELLO sin el relleno mínimo: ni siquiera se responde.
    let before = net.stats().sent;
    attacker.send(server.link().addr(), &hello[..20]);
    server.poll(2, &mut ev);
    assert_eq!(net.stats().sent, before + 1); // solo el que mandó el atacante
}

#[test]
fn clave_equivocada_no_entra_ni_deja_estado() {
    let mut r = Rig::new(SimConfig::default());
    let bad = Authority::new(b"otra-clave-maestra").issue(5);
    let c = r.add_client(bad, 2);
    r.step(6000);
    assert!(!r.clients[c].is_connected());
    assert!(r.clients[c].is_closed(), "debe rendirse tras el plazo");
    assert_eq!(r.server.session_count(), 0);
    assert!(r.server.stats().bad_auth > 0);
    assert_eq!(r.connected_at_server(), 0);
    assert!(r.cli_events[c].iter().any(|e| matches!(
        e,
        ClientEvent::Disconnected(DisconnectReason::ConnectFailed)
    )));
}

#[test]
fn credenciales_de_otra_cuenta_no_sirven() {
    // Se usa la clave de la cuenta 1 pero se declara la cuenta 2.
    let mut r = Rig::new(SimConfig::default());
    let c1 = authority().issue(1);
    let forged = Credentials::new(2, *c1.key_bytes());
    let c = r.add_client(forged, 3);
    r.step(1500);
    assert!(!r.clients[c].is_connected());
    assert!(r.server.stats().bad_auth > 0);
}

#[test]
fn paquete_alterado_se_descarta() {
    let mut r = Rig::new(SimConfig::default());
    let c = r.add_client(authority().issue(1), 4);
    r.step(200);
    assert!(r.clients[c].is_connected());
    r.srv_events.clear();

    // Se captura un paquete legítimo del cliente, se altera un bit y se reenvía como si viniera de él.
    r.net.start_log();
    r.clients[c].send(b"mover a la derecha").unwrap();
    let log = r.net.take_log();
    let (from, to, data) = log.into_iter().next().unwrap();
    r.step(5); // el original llega
    assert_eq!(r.messages_at_server().len(), 1);

    let mut forged = data.clone();
    let mid = forged.len() / 2;
    forged[mid] ^= 0x01;
    let mut attacker = r.net.endpoint();
    attacker.send_as(from, to, &forged);
    r.step(5);
    assert!(r.messages_at_server().is_empty());
    assert_eq!(r.server.stats().bad_tag, 1);
}

#[test]
fn paquete_repetido_se_descarta() {
    let mut r = Rig::new(SimConfig::default());
    let c = r.add_client(authority().issue(1), 5);
    r.step(200);
    r.srv_events.clear();

    r.net.start_log();
    r.clients[c].send(b"comprar espada").unwrap();
    let (from, to, data) = r.net.take_log().into_iter().next().unwrap();
    r.step(5);
    assert_eq!(r.messages_at_server().len(), 1);

    // El mismo paquete, byte a byte, enviado otra vez (por un atacante que lo capturó).
    let mut attacker = r.net.endpoint();
    attacker.send_as(from, to, &data);
    r.step(5);
    assert!(
        r.messages_at_server().is_empty(),
        "un paquete repetido no debe ejecutarse dos veces"
    );
    assert_eq!(r.server.stats().replays, 1);
}

#[test]
fn paquete_valido_desde_otra_direccion_se_ignora() {
    let mut r = Rig::new(SimConfig::default());
    let c = r.add_client(authority().issue(1), 6);
    r.step(200);
    r.srv_events.clear();
    r.net.start_log();
    r.clients[c].send(b"x").unwrap();
    let (_, to, data) = r.net.take_log().into_iter().next().unwrap();
    // El mismo paquete, válido, pero enviado desde otro origen.
    let mut other = r.net.endpoint();
    other.send(to, &data);
    r.step(5);
    assert!(r.server.stats().addr_mismatch >= 1);
}

#[test]
fn fragmenta_y_reensambla_mensajes_grandes() {
    let mut r = Rig::new(SimConfig {
        latency_ms: 5,
        jitter_ms: 10,
        seed: 11,
        ..Default::default()
    });
    let c = r.add_client(authority().issue(1), 7);
    r.step(300);
    let session = r
        .srv_events
        .iter()
        .find_map(|e| match e {
            ServerEvent::Connected { session, .. } => Some(*session),
            _ => None,
        })
        .unwrap();
    let big: Vec<u8> = (0..15_000u32).map(|i| (i * 31 % 251) as u8).collect();
    r.clients[c].send(&big).unwrap();
    r.server.send(session, &big).unwrap();
    r.step(200);
    assert_eq!(r.messages_at_server(), vec![(session, big.clone())]);
    assert_eq!(r.messages_at_client(c), vec![big]);

    let too_big = vec![0u8; MAX_MESSAGE + 1];
    assert_eq!(r.clients[c].send(&too_big), Err(SendError::TooLarge));
    assert_eq!(r.server.send(session, &too_big), Err(SendError::TooLarge));
    assert_eq!(r.server.send(9999, b"x"), Err(SendError::UnknownSession));
    // Ningún datagrama supera el límite.
    r.net.start_log();
    r.clients[c].send(&vec![1u8; 10_000]).unwrap();
    assert!(r
        .net
        .take_log()
        .iter()
        .all(|(_, _, d)| d.len() <= wire::MAX_DATAGRAM));
}

#[test]
fn con_perdida_los_mensajes_fragmentados_llegan_enteros_o_no_llegan() {
    let mut r = Rig::new(SimConfig {
        latency_ms: 10,
        loss: 0.15,
        duplicate: 0.1,
        seed: 21,
        ..Default::default()
    });
    let c = r.add_client(authority().issue(1), 8);
    r.step(1500);
    assert!(r.clients[c].is_connected());
    r.srv_events.clear();
    let mut sent = Vec::new();
    for i in 0..60u32 {
        let msg: Vec<u8> = (0..3000u32).map(|j| ((i + j) % 256) as u8).collect();
        r.clients[c].send(&msg).unwrap();
        sent.push(msg);
        r.step(20);
    }
    r.step(200);
    let got = r.messages_at_server();
    assert!(!got.is_empty());
    for (_, m) in &got {
        assert!(
            sent.contains(m),
            "llegó un mensaje que nadie envió (corrupto o mezclado)"
        );
    }
    // Sin duplicados aunque la red duplique paquetes.
    let mut uniq: Vec<_> = got.iter().map(|(_, m)| m.clone()).collect();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), got.len());
}

#[test]
fn handshake_sobrevive_a_mucha_perdida() {
    for seed in 0..30u64 {
        let mut r = Rig::new(SimConfig {
            latency_ms: 25,
            jitter_ms: 20,
            loss: 0.35,
            duplicate: 0.1,
            seed,
        });
        let c = r.add_client(authority().issue(seed + 1), seed + 100);
        r.step(4500);
        assert!(r.clients[c].is_connected(), "semilla {seed}: no conectó");
        assert_eq!(r.connected_at_server(), 1, "semilla {seed}");
    }
}

#[test]
fn desconexion_pedida_por_el_cliente() {
    let mut r = Rig::new(SimConfig::default());
    let c = r.add_client(authority().issue(9), 9);
    r.step(200);
    r.srv_events.clear();
    r.clients[c].disconnect();
    r.step(20);
    assert!(r.srv_events.iter().any(|e| matches!(
        e,
        ServerEvent::Disconnected {
            player_id: 9,
            reason: DisconnectReason::Requested,
            ..
        }
    )));
    assert_eq!(r.server.session_count(), 0);
}

#[test]
fn el_servidor_cierra_sesiones_mudas() {
    let mut r = Rig::new(SimConfig::default());
    let c = r.add_client(authority().issue(3), 10);
    r.step(200);
    r.srv_events.clear();
    // El cliente desaparece de la red.
    r.net.set_config(SimConfig {
        loss: 1.0,
        ..Default::default()
    });
    r.step(7000);
    assert!(r.srv_events.iter().any(|e| matches!(
        e,
        ServerEvent::Disconnected {
            reason: DisconnectReason::Timeout,
            ..
        }
    )));
    assert_eq!(r.server.session_count(), 0);
    assert!(r.cli_events[c]
        .iter()
        .any(|e| matches!(e, ClientEvent::Disconnected(DisconnectReason::Timeout))));
}

#[test]
fn el_keepalive_mantiene_viva_una_sesion_inactiva() {
    let mut r = Rig::new(SimConfig {
        latency_ms: 10,
        ..Default::default()
    });
    let c = r.add_client(authority().issue(3), 11);
    r.step(30_000); // nadie envía nada de aplicación
    assert!(r.clients[c].is_connected());
    assert_eq!(r.server.session_count(), 1);
}

#[test]
fn reconectar_la_misma_cuenta_reemplaza_la_sesion() {
    let mut r = Rig::new(SimConfig::default());
    let c1 = r.add_client(authority().issue(77), 12);
    r.step(200);
    assert!(r.clients[c1].is_connected());
    let c2 = r.add_client(authority().issue(77), 13);
    r.step(300);
    assert!(r.clients[c2].is_connected());
    assert!(r.srv_events.iter().any(|e| matches!(
        e,
        ServerEvent::Disconnected {
            player_id: 77,
            reason: DisconnectReason::Replaced,
            ..
        }
    )));
    assert_eq!(r.server.session_count(), 1);
    // La sesión vieja fue avisada.
    assert!(r.cli_events[c1]
        .iter()
        .any(|e| matches!(e, ClientEvent::Disconnected(DisconnectReason::Kicked))));
}

#[test]
fn repetir_un_auth_capturado_no_expulsa_al_jugador() {
    let mut r = Rig::new(SimConfig::default());
    r.net.start_log();
    let c = r.add_client(authority().issue(55), 14);
    r.step(200);
    assert!(r.clients[c].is_connected());
    let log = r.net.take_log();
    // AUTH capturado del handshake legítimo.
    let (from, to, auth) = log
        .into_iter()
        .find(|(_, _, d)| d[0] == wire::AUTH)
        .expect("hubo un AUTH");
    r.srv_events.clear();

    // El atacante lo repite desde la dirección falsificada del jugador, otra vez y otra.
    // Como el nonce coincide con la sesión viva, el servidor solo repite su ACCEPT.
    let mut attacker = r.net.endpoint();
    for _ in 0..3 {
        attacker.send_as(from, to, &auth);
        r.step(10);
    }
    assert!(r.clients[c].is_connected());
    assert_eq!(r.server.session_count(), 1);
    assert!(!r
        .srv_events
        .iter()
        .any(|e| matches!(e, ServerEvent::Disconnected { .. })));
}

#[test]
fn limite_de_paquetes_por_segundo() {
    let net = SimNet::new(SimConfig::default());
    let cfg = ServerConfig {
        max_packets_per_sec: 50,
        ..Default::default()
    };
    let mut server = SecureServer::new(
        net.endpoint(),
        authority(),
        cfg,
        Box::new(DeterministicEntropy(1)),
    );
    let mut client = SecureClient::new(
        net.endpoint(),
        server.link().addr(),
        authority().issue(1),
        ClientConfig::default(),
        Box::new(DeterministicEntropy(2)),
    );
    let (mut sev, mut cev) = (Vec::new(), Vec::new());
    for t in 0..300u64 {
        net.set_now(t);
        server.poll(t, &mut sev);
        client.poll(t, &mut cev);
    }
    assert!(client.is_connected());
    sev.clear();
    for _ in 0..500 {
        client.send(b"spam").unwrap();
    }
    net.set_now(300);
    server.poll(300, &mut sev);
    let delivered = sev
        .iter()
        .filter(|e| matches!(e, ServerEvent::Message { .. }))
        .count();
    assert!(delivered <= 50, "se entregaron {delivered}");
    assert!(server.stats().rate_limited >= 400);
}

#[test]
fn muchos_clientes_a_la_vez() {
    let mut r = Rig::new(SimConfig {
        latency_ms: 15,
        jitter_ms: 5,
        loss: 0.02,
        seed: 31,
        ..Default::default()
    });
    for i in 0..200u64 {
        r.add_client(authority().issue(1000 + i), 500 + i);
    }
    r.step(3000);
    assert!(r.clients.iter().all(|c| c.is_connected()));
    assert_eq!(r.connected_at_server(), 200);
    assert_eq!(r.server.session_count(), 200);
}

#[test]
fn udp_real_handshake_y_mensajes() {
    let srv_link = UdpLink::bind("127.0.0.1:0").unwrap();
    let srv_addr = srv_link.local_addr().unwrap();
    let mut server = SecureServer::new(
        srv_link,
        authority(),
        ServerConfig::default(),
        Box::new(OsEntropy),
    );
    let mut client = SecureClient::new(
        UdpLink::bind("127.0.0.1:0").unwrap(),
        srv_addr,
        authority().issue(7),
        ClientConfig::default(),
        Box::new(OsEntropy),
    );
    let t0 = std::time::Instant::now();
    let (mut sev, mut cev) = (Vec::new(), Vec::new());
    let mut sent = false;
    let mut got_srv = None;
    let mut got_cli = None;
    let mut session = None;
    while t0.elapsed().as_millis() < 5000 && (got_srv.is_none() || got_cli.is_none()) {
        let now = t0.elapsed().as_millis() as u64;
        server.poll(now, &mut sev);
        client.poll(now, &mut cev);
        for e in sev.drain(..) {
            match e {
                ServerEvent::Connected { session: s, .. } => session = Some(s),
                ServerEvent::Message { data, .. } => got_srv = Some(data),
                _ => {}
            }
        }
        for e in cev.drain(..) {
            if let ClientEvent::Message(d) = e {
                got_cli = Some(d);
            }
        }
        if client.is_connected() && !sent {
            client.send(&vec![9u8; 5000]).unwrap(); // se fragmenta
            sent = true;
        }
        if let Some(s) = session.take() {
            server.send(s, b"bienvenido").unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(got_srv, Some(vec![9u8; 5000]));
    assert_eq!(got_cli, Some(b"bienvenido".to_vec()));
}

// ------------------------------------------------------------------------------------------------
// Confidencialidad (0.6.0): DATA y DISCONNECT van cifrados con ChaCha20-Poly1305
// ------------------------------------------------------------------------------------------------

/// Texto reconocible de `len` bytes: si cualquier trozo suyo aparece en un datagrama, el contenido
/// viajó en claro.
fn marcador(len: usize) -> Vec<u8> {
    b"TSOFGW-MARCADOR-SECRETO-"
        .iter()
        .cycle()
        .take(len)
        .copied()
        .collect()
}

/// ¿Aparece en `hay` alguna ventana de `w` bytes del marcador?
fn contiene_trozo_del_marcador(hay: &[u8], w: usize) -> bool {
    let m = marcador(48);
    m.windows(w).any(|n| hay.windows(w).any(|h| h == n))
}

#[test]
fn nada_del_contenido_viaja_en_claro_y_el_tamano_no_crece() {
    let mut r = Rig::new(SimConfig {
        latency_ms: 10,
        ..Default::default()
    });
    r.net.start_log();
    let c = r.add_client(authority().issue(9), 21);
    r.step(300);
    assert!(r.clients[c].is_connected());
    let session = r
        .srv_events
        .iter()
        .find_map(|e| match e {
            ServerEvent::Connected { session, .. } => Some(*session),
            _ => None,
        })
        .unwrap();
    // Mensajes de varios tamaños en los dos sentidos, incluidos varios fragmentos y uno de 0 bytes de
    // relleno exacto de un fragmento.
    let tamanos = [
        1usize,
        15,
        16,
        17,
        100,
        1000,
        3000,
        wire::MAX_FRAGMENT_PAYLOAD,
    ];
    for &n in &tamanos {
        r.clients[c].send(&marcador(n)).unwrap();
        r.server.send(session, &marcador(n)).unwrap();
        r.step(100);
    }
    assert_eq!(
        r.messages_at_server().len(),
        tamanos.len(),
        "todo llegó al servidor"
    );
    let del_servidor = r.messages_at_client(c);
    assert_eq!(del_servidor.len(), tamanos.len(), "y al cliente");
    for (m, &n) in del_servidor.iter().zip(&tamanos) {
        assert_eq!(m, &marcador(n));
    }

    let log = r.net.take_log();
    let datos: Vec<&Vec<u8>> = log
        .iter()
        .map(|(_, _, d)| d)
        .filter(|d| d[0] == wire::DATA)
        .collect();
    assert!(datos.len() >= 2 * tamanos.len(), "hubo tráfico de datos");
    for d in &log {
        assert!(
            !contiene_trozo_del_marcador(&d.2, 8),
            "un datagrama de {} bytes lleva trozos del contenido en claro",
            d.2.len()
        );
    }
    // Cifrar no agranda nada: cabecera (17) + carga + etiqueta (16), como antes.
    let con_marcador: usize = datos
        .iter()
        .map(|d| d.len() - wire::DATA_HEADER - wire::FRAG_HEADER - 16)
        .sum();
    let esperado: usize = tamanos.iter().map(|n| 2 * n).sum();
    assert_eq!(
        con_marcador, esperado,
        "bytes de carga en la red: tanto como en claro"
    );
}

#[test]
fn el_mismo_mensaje_enviado_dos_veces_no_se_parece() {
    let mut r = Rig::new(SimConfig::default());
    let c = r.add_client(authority().issue(3), 22);
    r.step(200);
    r.net.start_log();
    r.clients[c].send(&marcador(200)).unwrap();
    r.clients[c].send(&marcador(200)).unwrap();
    let log = r.net.take_log();
    let datos: Vec<_> = log.iter().filter(|(_, _, d)| d[0] == wire::DATA).collect();
    assert_eq!(datos.len(), 2);
    let (a, b) = (&datos[0].2, &datos[1].2);
    assert_eq!(a.len(), b.len());
    // Mismo texto en claro, contadores distintos: ni cifrado ni etiqueta coinciden (nonce distinto).
    let cuerpo = |p: &Vec<u8>| p[wire::DATA_HEADER + wire::FRAG_HEADER..p.len() - 16].to_vec();
    assert_ne!(cuerpo(a), cuerpo(b));
    assert_ne!(a[a.len() - 16..], b[b.len() - 16..]);
}

#[test]
fn un_paquete_de_un_sentido_no_vale_en_el_otro() {
    let mut r = Rig::new(SimConfig::default());
    let c = r.add_client(authority().issue(4), 23);
    r.step(200);
    let session = r
        .srv_events
        .iter()
        .find_map(|e| match e {
            ServerEvent::Connected { session, .. } => Some(*session),
            _ => None,
        })
        .unwrap();
    r.srv_events.clear();
    r.net.start_log();
    r.server.send(session, b"orden del servidor").unwrap();
    let (de_srv, a_cli, data) = r
        .net
        .take_log()
        .into_iter()
        .find(|(_, _, d)| d[0] == wire::DATA)
        .unwrap();
    r.step(5);
    assert_eq!(r.messages_at_client(c).len(), 1);
    // Reflejado hacia el servidor como si lo enviara el cliente: otra clave por sentido, se rechaza.
    let mut atacante = r.net.endpoint();
    atacante.send_as(a_cli, de_srv, &data);
    r.step(5);
    assert!(r.messages_at_server().is_empty());
    assert_eq!(r.server.stats().bad_tag, 1);
}

#[test]
fn quien_conoce_la_cuenta_pero_no_su_clave_no_forja_ni_lee_nada() {
    let mut r = Rig::new(SimConfig::default());
    let c = r.add_client(authority().issue(5), 24);
    r.step(200);
    let session = r
        .srv_events
        .iter()
        .find_map(|e| match e {
            ServerEvent::Connected { session, .. } => Some(*session),
            _ => None,
        })
        .unwrap();
    r.srv_events.clear();
    r.net.start_log();
    r.clients[c].send(b"original").unwrap();
    let (from, to, data) = r.net.take_log().into_iter().next().unwrap();
    r.step(5);
    r.messages_at_server();
    // Un atacante construye un DATA bien formado para esa sesión con una clave que se inventa.
    let mut forged = data.clone();
    for b in forged[wire::DATA_HEADER + wire::FRAG_HEADER..].iter_mut() {
        *b ^= 0x5a;
    }
    // Contador nuevo (para que no lo frene el anti-replay y llegue a la etiqueta).
    forged[5..13].copy_from_slice(&1_000_000u64.to_le_bytes());
    let mut atacante = r.net.endpoint();
    atacante.send_as(from, to, &forged);
    r.step(5);
    assert!(r.messages_at_server().is_empty());
    assert_eq!(r.server.stats().bad_tag, 1);
    assert!(r.server.session_count() == 1 && r.clients[c].is_connected());
    // Un DISCONNECT inventado tampoco expulsa a nadie.
    let mut falso = vec![wire::DISCONNECT];
    falso.extend_from_slice(&session.to_le_bytes());
    falso.extend_from_slice(&2_000_000u64.to_le_bytes());
    falso.extend_from_slice(&[0xAB; 16]);
    atacante.send_as(from, to, &falso);
    r.step(5);
    assert!(r.clients[c].is_connected());
    assert_eq!(r.server.stats().bad_tag, 2);
}

#[test]
fn un_hello_de_la_version_anterior_se_ignora() {
    let net = SimNet::new(SimConfig::default());
    let mut server = SecureServer::new(
        net.endpoint(),
        authority(),
        ServerConfig::default(),
        Box::new(DeterministicEntropy(1)),
    );
    let mut atacante = net.endpoint();
    let mut ev = Vec::new();
    let mut hello = wire::hello(&[7u8; 16]);
    assert_eq!(hello[1], wire::PROTOCOL_VERSION);
    assert_eq!(
        wire::PROTOCOL_VERSION,
        2,
        "los paquetes cifrados son la versión 2"
    );
    hello[1] = 1;
    let antes = net.stats().sent;
    atacante.send(server.link().addr(), &hello);
    server.poll(1, &mut ev);
    assert_eq!(
        net.stats().sent,
        antes + 1,
        "solo el HELLO: no hubo respuesta"
    );
    assert_eq!(server.session_count(), 0);
}

#[test]
fn udp_real_con_un_espia_en_medio_no_ve_el_contenido_y_el_juego_funciona() {
    use std::net::UdpSocket;
    let srv_link = UdpLink::bind("127.0.0.1:0").unwrap();
    let srv_addr = srv_link.local_addr().unwrap();
    let mut server = SecureServer::new(
        srv_link,
        authority(),
        ServerConfig::default(),
        Box::new(OsEntropy),
    );
    // El «espía» se interpone: el cliente cree que habla con él, y él reenvía a ambos lados y guarda
    // una copia de cada datagrama.
    let spy = UdpSocket::bind("127.0.0.1:0").unwrap();
    spy.set_nonblocking(true).unwrap();
    let spy_addr = spy.local_addr().unwrap();
    let mut client = SecureClient::new(
        UdpLink::bind("127.0.0.1:0").unwrap(),
        spy_addr,
        authority().issue(11),
        ClientConfig::default(),
        Box::new(OsEntropy),
    );
    let mut capturado: Vec<Vec<u8>> = Vec::new();
    let mut cliente_addr = None;
    let mut buf = [0u8; 2048];

    let t0 = std::time::Instant::now();
    let (mut sev, mut cev) = (Vec::new(), Vec::new());
    let (mut enviado, mut got_srv, mut got_cli, mut session) = (false, None, None, None);
    while t0.elapsed().as_millis() < 8000 && (got_srv.is_none() || got_cli.is_none()) {
        let now = t0.elapsed().as_millis() as u64;
        while let Ok((n, from)) = spy.recv_from(&mut buf) {
            capturado.push(buf[..n].to_vec());
            if from == srv_addr {
                if let Some(c) = cliente_addr {
                    spy.send_to(&buf[..n], c).unwrap();
                }
            } else {
                cliente_addr = Some(from);
                spy.send_to(&buf[..n], srv_addr).unwrap();
            }
        }
        server.poll(now, &mut sev);
        client.poll(now, &mut cev);
        for e in sev.drain(..) {
            match e {
                ServerEvent::Connected { session: s, .. } => session = Some(s),
                ServerEvent::Message { data, .. } => got_srv = Some(data),
                _ => {}
            }
        }
        for e in cev.drain(..) {
            if let ClientEvent::Message(d) = e {
                got_cli = Some(d);
            }
        }
        if client.is_connected() && !enviado {
            client.send(&marcador(5000)).unwrap(); // se fragmenta
            enviado = true;
        }
        if let Some(s) = session.take() {
            server.send(s, &marcador(3000)).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(got_srv, Some(marcador(5000)));
    assert_eq!(got_cli, Some(marcador(3000)));
    assert!(capturado.len() >= 8, "el espía vio el tráfico");
    for d in &capturado {
        assert!(
            !contiene_trozo_del_marcador(d, 8),
            "el espía leyó contenido en un datagrama de {} bytes",
            d.len()
        );
    }
}
