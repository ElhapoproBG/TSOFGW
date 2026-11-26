//! Lado cliente del transporte seguro.

use tsofgw_pack::{ct_eq, HmacKey};

use crate::crypto::{
    accept_mac, auth_proof, derive_session, Credentials, ReplayWindow, SessionKeys, COOKIE_LEN,
    NONCE_LEN,
};
use crate::entropy::Entropy;
use crate::frag::{Push, Reassembler};
use crate::link::Link;
use crate::wire::{self, SendError};
use crate::{DisconnectReason, SessionId, TransportStats};

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Cada cuánto se repite HELLO o AUTH si no hay respuesta.
    pub retry_ms: u64,
    /// Tiempo total para completar el handshake.
    pub connect_timeout_ms: u64,
    /// Sin recibir nada del servidor durante este tiempo, se da la conexión por perdida.
    pub timeout_ms: u64,
    pub keepalive_ms: u64,
    pub reassembly_timeout_ms: u64,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            retry_ms: 250,
            connect_timeout_ms: 5000,
            timeout_ms: 5000,
            keepalive_ms: 1000,
            reassembly_timeout_ms: 1500,
        }
    }
}

#[derive(Debug)]
pub enum ClientEvent {
    Connected,
    Message(Vec<u8>),
    Disconnected(DisconnectReason),
}

enum State {
    Idle,
    Hello {
        last_sent: Option<u64>,
    },
    Auth {
        cookie: [u8; COOKIE_LEN],
        since: u64,
        last_sent: u64,
    },
    Connected,
    Closed,
}

struct Live {
    session: SessionId,
    keys: SessionKeys,
    send_counter: u64,
    next_msg_id: u16,
    replay: ReplayWindow,
    reasm: Reassembler,
    last_seen: u64,
    last_sent: u64,
}

pub struct SecureClient<L: Link> {
    link: L,
    server: L::Addr,
    creds: Credentials,
    pkey: HmacKey,
    cfg: ClientConfig,
    entropy: Box<dyn Entropy>,
    nonce: [u8; NONCE_LEN],
    state: State,
    started: Option<u64>,
    live: Option<Live>,
    stats: TransportStats,
    now: u64,
}

impl<L: Link> SecureClient<L> {
    pub fn new(
        link: L,
        server: L::Addr,
        creds: Credentials,
        cfg: ClientConfig,
        mut entropy: Box<dyn Entropy>,
    ) -> Self {
        let mut nonce = [0u8; NONCE_LEN];
        entropy.fill(&mut nonce);
        let pkey = HmacKey::new(creds.key_bytes());
        SecureClient {
            link,
            server,
            creds,
            pkey,
            cfg,
            entropy,
            nonce,
            state: State::Idle,
            started: None,
            live: None,
            stats: TransportStats::default(),
            now: 0,
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
    pub fn is_connected(&self) -> bool {
        matches!(self.state, State::Connected)
    }
    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed)
    }
    pub fn session(&self) -> Option<SessionId> {
        self.live.as_ref().map(|l| l.session)
    }
    pub fn player_id(&self) -> u64 {
        self.creds.player_id
    }

    fn send_raw(&mut self, data: &[u8]) {
        self.stats.packets_out += 1;
        self.stats.bytes_out += data.len() as u64;
        self.link.send(self.server, data);
    }

    pub fn poll(&mut self, now: u64, events: &mut Vec<ClientEvent>) {
        self.now = now;
        let started = *self.started.get_or_insert(now);
        if matches!(self.state, State::Idle) {
            self.state = State::Hello { last_sent: None };
        }

        let mut buf = [0u8; wire::RX_BUFFER];
        while let Some((from, n)) = self.link.recv(&mut buf) {
            self.stats.packets_in += 1;
            self.stats.bytes_in += n as u64;
            if from == self.server {
                self.handle(now, &buf[..n], events);
            } else {
                self.stats.addr_mismatch += 1;
            }
        }

        match self.state {
            State::Hello { last_sent } => {
                if now.saturating_sub(started) > self.cfg.connect_timeout_ms {
                    self.state = State::Closed;
                    events.push(ClientEvent::Disconnected(DisconnectReason::ConnectFailed));
                } else if last_sent.is_none_or(|t| now.saturating_sub(t) >= self.cfg.retry_ms) {
                    let p = wire::hello(&self.nonce);
                    self.send_raw(&p);
                    self.state = State::Hello {
                        last_sent: Some(now),
                    };
                }
            }
            State::Auth {
                cookie,
                since,
                last_sent,
            } => {
                if now.saturating_sub(started) > self.cfg.connect_timeout_ms {
                    self.state = State::Closed;
                    events.push(ClientEvent::Disconnected(DisconnectReason::ConnectFailed));
                } else if now.saturating_sub(since) > 4 * self.cfg.retry_ms {
                    // Probablemente la cookie caducó: se empieza de nuevo con un nonce nuevo.
                    self.entropy.fill(&mut self.nonce);
                    self.state = State::Hello { last_sent: None };
                } else if now.saturating_sub(last_sent) >= self.cfg.retry_ms {
                    self.send_auth(&cookie);
                    self.state = State::Auth {
                        cookie,
                        since,
                        last_sent: now,
                    };
                }
            }
            State::Connected => {
                let (idle_rx, idle_tx) = self.live.as_ref().map_or((false, false), |l| {
                    (
                        now.saturating_sub(l.last_seen) > self.cfg.timeout_ms,
                        now.saturating_sub(l.last_sent) >= self.cfg.keepalive_ms,
                    )
                });
                if idle_rx {
                    self.state = State::Closed;
                    self.stats.timeouts += 1;
                    events.push(ClientEvent::Disconnected(DisconnectReason::Timeout));
                } else if idle_tx {
                    let _ = self.send(&[]);
                }
            }
            State::Idle | State::Closed => {}
        }
        if let Some(l) = self.live.as_mut() {
            l.reasm.expire(now);
        }
    }

    fn send_auth(&mut self, cookie: &[u8; COOKIE_LEN]) {
        let proof = auth_proof(&self.pkey, self.creds.player_id, &self.nonce, cookie);
        let p = wire::auth(self.creds.player_id, &self.nonce, cookie, &proof);
        self.send_raw(&p);
    }

    fn handle(&mut self, now: u64, pkt: &[u8], events: &mut Vec<ClientEvent>) {
        match pkt.first().copied() {
            Some(wire::CHALLENGE) if pkt.len() == 1 + COOKIE_LEN => {
                if matches!(self.state, State::Hello { .. }) {
                    let cookie: [u8; COOKIE_LEN] = wire::array(&pkt[1..]);
                    self.send_auth(&cookie);
                    self.state = State::Auth {
                        cookie,
                        since: now,
                        last_sent: now,
                    };
                }
            }
            Some(wire::ACCEPT) if pkt.len() == wire::ACCEPT_LEN => {
                if !matches!(self.state, State::Auth { .. }) {
                    return; // duplicado o fuera de lugar
                }
                let session = u32::from_le_bytes(wire::array(&pkt[1..5]));
                let server_nonce: [u8; NONCE_LEN] = wire::array(&pkt[5..21]);
                let mac: [u8; 16] = wire::array(&pkt[21..37]);
                if !ct_eq(
                    &accept_mac(&self.pkey, &self.nonce, &server_nonce, session),
                    &mac,
                ) {
                    self.stats.bad_auth += 1;
                    return;
                }
                self.live = Some(Live {
                    session,
                    keys: derive_session(&self.pkey, &self.nonce, &server_nonce, session),
                    send_counter: 0,
                    next_msg_id: 0,
                    replay: ReplayWindow::default(),
                    reasm: Reassembler::new(self.cfg.reassembly_timeout_ms),
                    last_seen: now,
                    last_sent: now,
                });
                self.state = State::Connected;
                events.push(ClientEvent::Connected);
                // El primer paquete de datos confirma la sesión en el servidor.
                let _ = self.send(&[]);
            }
            Some(wire::DATA) => self.on_data(now, pkt, events),
            Some(wire::DISCONNECT) => self.on_disconnect(pkt, events),
            _ => self.stats.malformed += 1,
        }
    }

    fn on_data(&mut self, now: u64, pkt: &[u8], events: &mut Vec<ClientEvent>) {
        let Some(l) = self.live.as_mut() else {
            self.stats.unknown_session += 1;
            return;
        };
        let Some(d) = wire::parse_data(pkt) else {
            self.stats.malformed += 1;
            return;
        };
        if d.session != l.session {
            self.stats.unknown_session += 1;
            return;
        }
        let Some(plain) = wire::open_data(&l.keys.s2c, pkt) else {
            self.stats.bad_tag += 1;
            return;
        };
        if !l.replay.accept(d.counter) {
            self.stats.replays += 1;
            return;
        }
        l.last_seen = now;
        match l.reasm.push(now, d.msg_id, d.idx, d.count, &plain) {
            Push::Complete(data) if !data.is_empty() => events.push(ClientEvent::Message(data)),
            Push::Invalid => self.stats.malformed += 1,
            _ => {}
        }
    }

    fn on_disconnect(&mut self, pkt: &[u8], events: &mut Vec<ClientEvent>) {
        let Some((id, counter)) = wire::parse_disconnect(pkt) else {
            self.stats.malformed += 1;
            return;
        };
        let Some(l) = self.live.as_mut() else {
            return;
        };
        if id != l.session || !wire::verify_disconnect(&l.keys.s2c, pkt) {
            self.stats.bad_tag += 1;
            return;
        }
        if !l.replay.accept(counter) {
            self.stats.replays += 1;
            return;
        }
        self.state = State::Closed;
        events.push(ClientEvent::Disconnected(DisconnectReason::Kicked));
    }

    /// Envía un mensaje al servidor (hasta [`wire::MAX_MESSAGE`] bytes).
    pub fn send(&mut self, msg: &[u8]) -> Result<(), SendError> {
        let parts = wire::split(msg)?;
        let now = self.now;
        let l = self.live.as_mut().ok_or(SendError::NotConnected)?;
        let msg_id = l.next_msg_id;
        l.next_msg_id = l.next_msg_id.wrapping_add(1);
        l.last_sent = now;
        let count = parts.len() as u8;
        let mut packets = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            let counter = l.send_counter;
            l.send_counter += 1;
            packets.push(wire::build_data(
                &l.keys.c2s,
                l.session,
                counter,
                msg_id,
                i as u8,
                count,
                part,
            ));
        }
        for p in &packets {
            self.send_raw(p);
        }
        Ok(())
    }

    /// Cierra la conexión avisando al servidor (dos copias, por si una se pierde).
    pub fn disconnect(&mut self) {
        if let Some(l) = self.live.as_mut() {
            let mut packets = Vec::new();
            for _ in 0..2 {
                let counter = l.send_counter;
                l.send_counter += 1;
                packets.push(wire::build_disconnect(&l.keys.c2s, l.session, counter));
            }
            for p in &packets {
                self.send_raw(p);
            }
        }
        self.state = State::Closed;
    }
}
