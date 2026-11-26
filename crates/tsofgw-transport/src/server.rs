//! Lado servidor del transporte seguro.

use std::collections::HashMap;

use tsofgw_pack::{ct_eq, HmacKey};

use crate::crypto::{
    accept_mac, auth_proof, derive_session, Authority, Ephemeral, ReplayWindow, SessionInputs,
    SessionKeys, COOKIE_LEN, EPH_LEN, NONCE_LEN,
};
use crate::entropy::Entropy;
use crate::frag::{Push, Reassembler};
use crate::link::{Link, PeerAddr};
use crate::wire::{self, SendError};
use crate::{DisconnectReason, SessionId, TransportStats};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Sin recibir nada de un cliente confirmado durante este tiempo, se cierra su sesión.
    pub timeout_ms: u64,
    /// Plazo para que un cliente que pasó AUTH envíe su primer paquete de datos.
    pub handshake_timeout_ms: u64,
    pub max_sessions: usize,
    /// Sesiones aceptadas pero aún sin primer paquete de datos.
    pub max_pending: usize,
    /// Paquetes autenticados por segundo y sesión; el exceso se descarta.
    pub max_packets_per_sec: u32,
    /// Cada cuánto cambia el secreto de las cookies (se aceptan la actual y la anterior).
    pub cookie_rotate_ms: u64,
    pub reassembly_timeout_ms: u64,
    /// Si no se envía nada en este tiempo, se manda un paquete vacío para mantener viva la sesión.
    pub keepalive_ms: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            timeout_ms: 5000,
            handshake_timeout_ms: 3000,
            max_sessions: 4096,
            max_pending: 1024,
            max_packets_per_sec: 400,
            cookie_rotate_ms: 10_000,
            reassembly_timeout_ms: 1500,
            keepalive_ms: 1000,
        }
    }
}

#[derive(Debug)]
pub enum ServerEvent<A> {
    /// La sesión quedó confirmada (el cliente envió su primer paquete de datos válido).
    Connected {
        session: SessionId,
        player_id: u64,
        addr: A,
    },
    Message {
        session: SessionId,
        data: Vec<u8>,
    },
    Disconnected {
        session: SessionId,
        player_id: u64,
        reason: DisconnectReason,
    },
}

struct Session<A> {
    addr: A,
    player_id: u64,
    client_nonce: [u8; NONCE_LEN],
    keys: SessionKeys,
    accept_packet: Vec<u8>,
    confirmed: bool,
    created: u64,
    last_seen: u64,
    last_sent: u64,
    send_counter: u64,
    next_msg_id: u16,
    replay: ReplayWindow,
    reasm: Reassembler,
    rate_start: u64,
    rate_count: u32,
}

pub struct SecureServer<L: Link> {
    link: L,
    authority: Authority,
    cfg: ServerConfig,
    entropy: Box<dyn Entropy>,
    cookie_current: HmacKey,
    cookie_previous: HmacKey,
    last_rotate: Option<u64>,
    last_sweep: u64,
    now: u64,
    sessions: HashMap<SessionId, Session<L::Addr>>,
    by_player: HashMap<u64, SessionId>,
    by_auth: HashMap<(u64, [u8; NONCE_LEN]), SessionId>,
    pending: usize,
    next_session: u32,
    stats: TransportStats,
}

impl<L: Link> SecureServer<L> {
    pub fn new(
        link: L,
        authority: Authority,
        cfg: ServerConfig,
        mut entropy: Box<dyn Entropy>,
    ) -> Self {
        let mut s1 = [0u8; 32];
        let mut s2 = [0u8; 32];
        entropy.fill(&mut s1);
        entropy.fill(&mut s2);
        let mut start = [0u8; 4];
        entropy.fill(&mut start);
        SecureServer {
            link,
            authority,
            cfg,
            entropy,
            cookie_current: HmacKey::new(&s1),
            cookie_previous: HmacKey::new(&s2),
            last_rotate: None,
            last_sweep: 0,
            now: 0,
            sessions: HashMap::new(),
            by_player: HashMap::new(),
            by_auth: HashMap::new(),
            pending: 0,
            next_session: u32::from_le_bytes(start) | 1,
            stats: TransportStats::default(),
        }
    }

    pub fn link(&self) -> &L {
        &self.link
    }
    pub fn link_mut(&mut self) -> &mut L {
        &mut self.link
    }
    pub fn stats(&self) -> &TransportStats {
        &self.stats
    }
    /// Sesiones vivas (confirmadas y pendientes).
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
    pub fn session_addr(&self, id: SessionId) -> Option<L::Addr> {
        self.sessions.get(&id).map(|s| s.addr)
    }

    fn cookie(
        &self,
        secret: &HmacKey,
        addr: &L::Addr,
        nonce: &[u8; NONCE_LEN],
    ) -> [u8; COOKIE_LEN] {
        let mut ab = Vec::with_capacity(20);
        addr.write_bytes(&mut ab);
        let full = secret.mac(&[b"tsofgw/cookie/v1", &ab, nonce]);
        wire::array(&full)
    }

    /// Lee todo lo pendiente, atiende timeouts y devuelve los eventos en orden.
    pub fn poll(&mut self, now: u64, events: &mut Vec<ServerEvent<L::Addr>>) {
        self.now = now;
        match self.last_rotate {
            None => self.last_rotate = Some(now),
            Some(t) if now.saturating_sub(t) >= self.cfg.cookie_rotate_ms => {
                let mut s = [0u8; 32];
                self.entropy.fill(&mut s);
                self.cookie_previous =
                    std::mem::replace(&mut self.cookie_current, HmacKey::new(&s));
                self.last_rotate = Some(now);
            }
            _ => {}
        }

        let mut buf = [0u8; wire::RX_BUFFER];
        while let Some((from, n)) = self.link.recv(&mut buf) {
            self.stats.packets_in += 1;
            self.stats.bytes_in += n as u64;
            self.handle(now, from, &buf[..n], events);
        }
        if now.saturating_sub(self.last_sweep) >= 100 {
            self.last_sweep = now;
            self.sweep(now, events);
        }
    }

    fn handle(
        &mut self,
        now: u64,
        from: L::Addr,
        pkt: &[u8],
        events: &mut Vec<ServerEvent<L::Addr>>,
    ) {
        match pkt.first().copied() {
            Some(wire::HELLO) => self.on_hello(from, pkt),
            Some(wire::AUTH) => self.on_auth(now, from, pkt),
            Some(wire::DATA) => self.on_data(now, from, pkt, events),
            Some(wire::DISCONNECT) => self.on_disconnect(from, pkt, events),
            _ => self.stats.malformed += 1,
        }
    }

    fn on_hello(&mut self, from: L::Addr, pkt: &[u8]) {
        if pkt.len() < wire::HELLO_LEN || pkt[1] != wire::PROTOCOL_VERSION {
            self.stats.malformed += 1;
            return;
        }
        let nonce: [u8; NONCE_LEN] = wire::array(&pkt[2..]);
        let cookie = self.cookie(&self.cookie_current, &from, &nonce);
        self.stats.challenges += 1;
        self.send_raw(from, &wire::challenge(&cookie));
    }

    fn on_auth(&mut self, now: u64, from: L::Addr, pkt: &[u8]) {
        if pkt.len() != wire::AUTH_LEN {
            self.stats.malformed += 1;
            return;
        }
        let player_id = u64::from_le_bytes(wire::array(&pkt[1..9]));
        let nonce: [u8; NONCE_LEN] = wire::array(&pkt[9..25]);
        let cookie: [u8; COOKIE_LEN] = wire::array(&pkt[25..41]);
        let client_eph: [u8; EPH_LEN] = wire::array(&pkt[41..73]);
        let proof: [u8; 32] = wire::array(&pkt[73..105]);

        let ok_cookie = ct_eq(&self.cookie(&self.cookie_current, &from, &nonce), &cookie)
            | ct_eq(&self.cookie(&self.cookie_previous, &from, &nonce), &cookie);
        if !ok_cookie {
            self.stats.bad_cookie += 1;
            return;
        }
        // Reintento de un AUTH ya atendido (se perdió el ACCEPT): se repite la misma respuesta.
        if let Some(&id) = self.by_auth.get(&(player_id, nonce)) {
            if let Some(s) = self.sessions.get(&id) {
                if s.addr == from {
                    let p = s.accept_packet.clone();
                    self.send_raw(from, &p);
                }
            }
            return;
        }
        if self.sessions.len() >= self.cfg.max_sessions || self.pending >= self.cfg.max_pending {
            self.stats.refused += 1;
            return;
        }
        let pkey = self.authority.player_hmac(player_id);
        if !ct_eq(
            &auth_proof(&pkey, player_id, &nonce, &cookie, &client_eph),
            &proof,
        ) {
            self.stats.bad_auth += 1;
            return;
        }

        let mut server_nonce = [0u8; NONCE_LEN];
        self.entropy.fill(&mut server_nonce);
        // Claves efímeras de esta sesión. Si el cliente mandó un punto de orden pequeño, el secreto sería
        // todo ceros: se rechaza (la prueba ya demostró que conoce la clave de la cuenta, así que es un
        // cliente roto o malicioso, no un intruso).
        let eph = Ephemeral::generate(&mut *self.entropy);
        let Some(dh) = eph.agree(&client_eph) else {
            self.stats.bad_auth += 1;
            return;
        };
        let mut id = self.next_session;
        while id == 0 || self.sessions.contains_key(&id) {
            id = id.wrapping_add(1);
        }
        self.next_session = id.wrapping_add(1);

        let mac = accept_mac(&pkey, &nonce, &server_nonce, id, &client_eph, &eph.public);
        let accept_packet = wire::accept(id, &server_nonce, &eph.public, &mac);
        self.sessions.insert(
            id,
            Session {
                addr: from,
                player_id,
                client_nonce: nonce,
                keys: derive_session(
                    &pkey,
                    &SessionInputs {
                        client_nonce: &nonce,
                        server_nonce: &server_nonce,
                        session_id: id,
                        client_eph: &client_eph,
                        server_eph: &eph.public,
                        dh: &dh,
                    },
                ),
                accept_packet: accept_packet.clone(),
                confirmed: false,
                created: now,
                last_seen: now,
                last_sent: now,
                send_counter: 0,
                next_msg_id: 0,
                replay: ReplayWindow::default(),
                reasm: Reassembler::new(self.cfg.reassembly_timeout_ms),
                rate_start: now,
                rate_count: 0,
            },
        );
        self.by_auth.insert((player_id, nonce), id);
        self.pending += 1;
        self.stats.auth_ok += 1;
        self.send_raw(from, &accept_packet);
    }

    fn on_data(
        &mut self,
        now: u64,
        from: L::Addr,
        pkt: &[u8],
        events: &mut Vec<ServerEvent<L::Addr>>,
    ) {
        let Some(d) = wire::parse_data(pkt) else {
            self.stats.malformed += 1;
            return;
        };
        let Some(s) = self.sessions.get_mut(&d.session) else {
            self.stats.unknown_session += 1;
            return;
        };
        if s.addr != from {
            self.stats.addr_mismatch += 1;
            return;
        }
        let Some(plain) = wire::open_data(&s.keys.c2s, pkt) else {
            self.stats.bad_tag += 1;
            return;
        };
        if !s.replay.accept(d.counter) {
            self.stats.replays += 1;
            return;
        }
        if now.saturating_sub(s.rate_start) >= 1000 {
            s.rate_start = now;
            s.rate_count = 0;
        }
        s.rate_count += 1;
        if s.rate_count > self.cfg.max_packets_per_sec {
            self.stats.rate_limited += 1;
            return;
        }
        s.last_seen = now;
        let first = !s.confirmed;
        s.confirmed = true;
        let player_id = s.player_id;
        let pushed = s.reasm.push(now, d.msg_id, d.idx, d.count, &plain);

        if first {
            self.pending = self.pending.saturating_sub(1);
            self.stats.confirmed += 1;
            if let Some(old) = self.by_player.insert(player_id, d.session) {
                if old != d.session {
                    // La misma cuenta se conectó de nuevo: la sesión anterior cae, pero solo ahora
                    // que la nueva demostró poseer las claves (un AUTH repetido por un tercero no basta).
                    self.send_disconnect(old);
                    self.remove(old);
                    events.push(ServerEvent::Disconnected {
                        session: old,
                        player_id,
                        reason: DisconnectReason::Replaced,
                    });
                }
            }
            events.push(ServerEvent::Connected {
                session: d.session,
                player_id,
                addr: from,
            });
        }
        match pushed {
            Push::Complete(data) if !data.is_empty() => events.push(ServerEvent::Message {
                session: d.session,
                data,
            }),
            Push::Invalid => self.stats.malformed += 1,
            _ => {}
        }
    }

    fn on_disconnect(&mut self, from: L::Addr, pkt: &[u8], events: &mut Vec<ServerEvent<L::Addr>>) {
        let Some((id, counter)) = wire::parse_disconnect(pkt) else {
            self.stats.malformed += 1;
            return;
        };
        let Some(s) = self.sessions.get_mut(&id) else {
            self.stats.unknown_session += 1;
            return;
        };
        if s.addr != from || !wire::verify_disconnect(&s.keys.c2s, pkt) {
            self.stats.bad_tag += 1;
            return;
        }
        if !s.replay.accept(counter) {
            self.stats.replays += 1;
            return;
        }
        let (player_id, confirmed) = (s.player_id, s.confirmed);
        self.remove(id);
        if confirmed {
            events.push(ServerEvent::Disconnected {
                session: id,
                player_id,
                reason: DisconnectReason::Requested,
            });
        }
    }

    fn remove(&mut self, id: SessionId) {
        if let Some(s) = self.sessions.remove(&id) {
            if !s.confirmed {
                self.pending = self.pending.saturating_sub(1);
            }
            self.by_auth.remove(&(s.player_id, s.client_nonce));
            if self.by_player.get(&s.player_id) == Some(&id) {
                self.by_player.remove(&s.player_id);
            }
        }
    }

    fn sweep(&mut self, now: u64, events: &mut Vec<ServerEvent<L::Addr>>) {
        let mut dead = Vec::new();
        let mut idle = Vec::new();
        for (&id, s) in &mut self.sessions {
            s.reasm.expire(now);
            if s.confirmed {
                if now.saturating_sub(s.last_seen) > self.cfg.timeout_ms {
                    dead.push((id, s.player_id, true));
                } else if now.saturating_sub(s.last_sent) >= self.cfg.keepalive_ms {
                    idle.push(id);
                }
            } else if now.saturating_sub(s.created) > self.cfg.handshake_timeout_ms {
                dead.push((id, s.player_id, false));
            }
        }
        for id in idle {
            let _ = self.send(id, &[]);
        }
        for (id, player_id, confirmed) in dead {
            self.remove(id);
            if confirmed {
                self.stats.timeouts += 1;
                events.push(ServerEvent::Disconnected {
                    session: id,
                    player_id,
                    reason: DisconnectReason::Timeout,
                });
            } else {
                self.stats.handshake_timeouts += 1;
            }
        }
    }

    fn send_raw(&mut self, to: L::Addr, data: &[u8]) {
        self.stats.packets_out += 1;
        self.stats.bytes_out += data.len() as u64;
        self.link.send(to, data);
    }

    /// Envía un mensaje (hasta [`wire::MAX_MESSAGE`] bytes) a una sesión confirmada.
    pub fn send(&mut self, id: SessionId, msg: &[u8]) -> Result<(), SendError> {
        let parts = wire::split(msg)?;
        let now = self.now;
        let s = self
            .sessions
            .get_mut(&id)
            .ok_or(SendError::UnknownSession)?;
        if !s.confirmed {
            return Err(SendError::NotConnected);
        }
        let msg_id = s.next_msg_id;
        s.next_msg_id = s.next_msg_id.wrapping_add(1);
        s.last_sent = now;
        let addr = s.addr;
        let count = parts.len() as u8;
        let mut packets = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            let counter = s.send_counter;
            s.send_counter += 1;
            packets.push(wire::build_data(
                &s.keys.s2c,
                id,
                counter,
                msg_id,
                i as u8,
                count,
                part,
            ));
        }
        for p in &packets {
            self.send_raw(addr, p);
        }
        Ok(())
    }

    fn send_disconnect(&mut self, id: SessionId) {
        let Some(s) = self.sessions.get_mut(&id) else {
            return;
        };
        let addr = s.addr;
        let mut packets = Vec::new();
        for _ in 0..2 {
            let counter = s.send_counter;
            s.send_counter += 1;
            packets.push(wire::build_disconnect(&s.keys.s2c, id, counter));
        }
        for p in &packets {
            self.send_raw(addr, p);
        }
    }

    /// Expulsa a una sesión (avisándole) sin generar evento: quien llama ya sabe que la cerró.
    pub fn kick(&mut self, id: SessionId) {
        self.send_disconnect(id);
        self.remove(id);
    }
}
