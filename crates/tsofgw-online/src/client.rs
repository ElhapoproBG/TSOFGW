//! Cliente de juego: transporte seguro, predicción local, reconciliación e interpolación.
//!
//! En un cluster de shards el servidor puede pedir al cliente que **siga jugando en otro shard**
//! (`REDIRECT`). El cliente abre una segunda conexión, con su misma clave de cuenta, y solo cuando el
//! destino le da la bienvenida cambia a ella: reenvía los inputs que el origen no llegó a confirmar,
//! conserva la predicción y los últimos snapshots, y cierra la conexión vieja. Si el destino no
//! responde, sigue jugando donde estaba.

use std::collections::{BTreeMap, HashMap, VecDeque};

use tsofgw_net::{dequantize, Endpoint, EntityState, Predictor, Reader, Snapshot};
use tsofgw_transport::{
    ClientConfig, ClientEvent, Credentials, DisconnectReason, Entropy, Link, SecureClient,
    TransportStats,
};

use crate::protocol::{self as proto, MAX_INPUTS_PER_MESSAGE};
use crate::sim::{Input, MoveSim, PlayerState};

/// Cuántos inputs recientes se reenvían en cada mensaje (tolera la pérdida de paquetes sin reenvíos).
const REDUNDANCY: usize = 4;
const SNAPSHOT_RING: usize = 32;
/// Cuántos ticks por detrás del último snapshot se dibuja a los demás (suaviza la llegada irregular).
const INTERP_DELAY_TICKS: f64 = 2.0;
/// Plazo (ms) para que el shard de destino dé la bienvenida; si no, se abandona ese intento.
const PENDING_TIMEOUT_MS: u64 = 8000;
/// Cuántas redirecciones seguidas se aceptan **antes de entrar** (un shard que no es el de la cuenta
/// manda al cliente al correcto; más de esto es un bucle o un servidor que no sabe lo que hace).
const MAX_LOGIN_REDIRECTS: u32 = 4;

/// Lo que el cliente necesita para abrir una conexión nueva a otro shard.
pub struct NewConnection<L: Link> {
    pub link: L,
    pub server: L::Addr,
    pub entropy: Box<dyn Entropy>,
}

/// Dado el número de shard al que se redirige, crea el enlace hacia él (o `None` si no se conoce).
pub type Connector<L> = Box<dyn FnMut(u16) -> Option<NewConnection<L>>>;

struct Pending<L: Link> {
    shard: u16,
    net: SecureClient<L>,
    endpoint: Endpoint,
    connected: bool,
    last_join: Option<u64>,
    started: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Joining,
    Playing,
    Disconnected(DisconnectReason),
}

#[derive(Debug, Clone, Default)]
pub struct ClientStats {
    pub snapshots: u64,
    pub snapshots_dropped: u64,
    pub inputs_sent: u64,
    /// Cuántas veces el servidor corrigió la posición predicha.
    pub corrections: u64,
    pub correction_sum_q: u64,
    pub correction_max_q: u32,
    /// Redirecciones recibidas del servidor.
    pub redirects: u64,
    /// Cambios de shard completados.
    pub switches: u64,
    /// Intentos de cambio que se abandonaron (el destino no respondió o cerró la conexión).
    pub switch_failures: u64,
    /// Veces que, al pedir entrar, el servidor mandó al cliente a otro shard (el de su cuenta) y el
    /// cliente lo siguió.
    pub login_redirects: u64,
}

pub struct GameClient<L: Link> {
    net: SecureClient<L>,
    endpoint: Endpoint,
    phase: Phase,
    tick_ms: u64,
    entity: u64,
    predictor: Predictor<MoveSim>,
    wanted: Input,
    input_seq: u32,
    recent: VecDeque<(u32, Input)>,
    next_input_at: u64,
    last_join: Option<u64>,
    snaps: VecDeque<Snapshot>,
    latest_rx_ms: u64,
    sent_at: HashMap<u16, u64>,
    rtt_ms: Option<f64>,
    stats: ClientStats,
    events: Vec<ClientEvent>,
    creds: Credentials,
    transport_cfg: ClientConfig,
    connector: Option<Connector<L>>,
    pending: Option<Pending<L>>,
    /// La conexión activa se cayó mientras se esperaba la nueva: se sigue prediciendo hasta que llegue.
    old_lost: bool,
    /// Bytes y paquetes enviados por conexiones ya cerradas.
    carried_out: (u64, u64),
    /// Se acaba de cambiar de shard y falta el primer snapshot del nuevo: cada shard cuenta sus ticks
    /// desde que arrancó, así que el nuevo puede ir por detrás del anterior.
    just_switched: bool,
    /// Redirecciones seguidas antes de entrar (se pone a cero al empezar a jugar).
    login_hops: u32,
}

impl<L: Link> GameClient<L> {
    pub fn new(
        link: L,
        server: L::Addr,
        creds: Credentials,
        transport: ClientConfig,
        entropy: Box<dyn Entropy>,
    ) -> Self {
        GameClient {
            net: SecureClient::new(link, server, creds.clone(), transport.clone(), entropy),
            endpoint: Endpoint::default(),
            phase: Phase::Connecting,
            tick_ms: 50,
            entity: 0,
            predictor: Predictor::new(PlayerState::default()),
            wanted: Input::default(),
            input_seq: 0,
            recent: VecDeque::new(),
            next_input_at: 0,
            last_join: None,
            snaps: VecDeque::new(),
            latest_rx_ms: 0,
            sent_at: HashMap::new(),
            rtt_ms: None,
            stats: ClientStats::default(),
            events: Vec::new(),
            creds,
            transport_cfg: transport,
            connector: None,
            pending: None,
            old_lost: false,
            carried_out: (0, 0),
            just_switched: false,
            login_hops: 0,
        }
    }

    /// Permite seguir al servidor cuando redirige a otro shard (sin esto, las redirecciones se ignoran).
    pub fn with_connector(mut self, connector: Connector<L>) -> Self {
        self.connector = Some(connector);
        self
    }

    /// Bytes y paquetes enviados desde que se creó el cliente, sumando las conexiones ya cerradas.
    pub fn traffic_out(&self) -> (u64, u64) {
        let t = self.net.stats();
        (
            self.carried_out.0 + t.bytes_out,
            self.carried_out.1 + t.packets_out,
        )
    }

    /// Shard al que se está intentando pasar, si hay un cambio en curso.
    pub fn switching_to(&self) -> Option<u16> {
        self.pending.as_ref().map(|p| p.shard)
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn is_playing(&self) -> bool {
        self.phase == Phase::Playing
    }
    pub fn entity(&self) -> u64 {
        self.entity
    }
    pub fn rtt_ms(&self) -> Option<f64> {
        self.rtt_ms
    }
    pub fn stats(&self) -> &ClientStats {
        &self.stats
    }
    pub fn transport_stats(&self) -> &TransportStats {
        self.net.stats()
    }
    pub fn link_mut(&mut self) -> &mut L {
        self.net.link_mut()
    }

    /// Dirección que se enviará en cada tick mientras no se cambie.
    pub fn set_input(&mut self, dir: [i8; 3]) {
        self.wanted = Input { dir };
    }

    /// Estado predicho de mi jugador (aplica los inputs aún no confirmados por el servidor).
    pub fn predicted(&self) -> Option<PlayerState> {
        self.is_playing().then_some(self.predictor.state)
    }

    pub fn predicted_position(&self) -> Option<[f32; 3]> {
        self.predicted().map(|s| {
            [
                dequantize(s.pos[0]),
                dequantize(s.pos[1]),
                dequantize(s.pos[2]),
            ]
        })
    }

    /// Último snapshot recibido (lo que el servidor dice que veo).
    pub fn latest_snapshot(&self) -> Option<&Snapshot> {
        self.snaps.back()
    }

    pub fn disconnect(&mut self) {
        self.net.disconnect();
        if let Some(mut p) = self.pending.take() {
            p.net.disconnect();
        }
        self.phase = Phase::Disconnected(DisconnectReason::Requested);
    }

    pub fn update(&mut self, now: u64) {
        let mut events = std::mem::take(&mut self.events);
        self.net.poll(now, &mut events);
        for e in events.drain(..) {
            match e {
                ClientEvent::Connected => self.phase = Phase::Joining,
                ClientEvent::Message(data) => self.on_message(now, &data),
                ClientEvent::Disconnected(r) => {
                    if self.pending.is_some() && self.phase == Phase::Playing {
                        // El shard viejo cerró mientras llega el nuevo: se sigue prediciendo hasta entonces.
                        self.old_lost = true;
                    } else {
                        self.phase = Phase::Disconnected(r);
                    }
                }
            }
        }
        self.events = events;
        self.update_pending(now);

        match self.phase {
            Phase::Joining => {
                if self.last_join.is_none_or(|t| now.saturating_sub(t) >= 200) {
                    self.last_join = Some(now);
                    self.send_message(now, proto::JOIN, &proto::join_body());
                }
            }
            Phase::Playing => {
                let mut ran = 0;
                while now >= self.next_input_at && ran < 3 {
                    self.send_input(now);
                    self.next_input_at += self.tick_ms;
                    ran += 1;
                }
                if now >= self.next_input_at {
                    self.next_input_at = now + self.tick_ms; // demasiado atrasado: se reinicia
                }
            }
            _ => {}
        }
        self.sent_at.retain(|_, t| now.saturating_sub(*t) < 3000);
    }

    /// Avanza la conexión al shard de destino (si hay un cambio en curso).
    fn update_pending(&mut self, now: u64) {
        let Some(mut pend) = self.pending.take() else {
            return;
        };
        let mut events = Vec::new();
        pend.net.poll(now, &mut events);
        let mut welcomed = false;
        let mut dead = false;
        for e in events {
            match e {
                ClientEvent::Connected => pend.connected = true,
                ClientEvent::Message(data) => {
                    if let Some((hdr, proto::WELCOME, body)) = proto::unframe(&data) {
                        if proto::parse_welcome(body).is_some() {
                            pend.endpoint.on_receive(&hdr);
                            welcomed = true;
                        }
                    }
                }
                ClientEvent::Disconnected(_) => dead = true,
            }
        }
        if dead || now.saturating_sub(pend.started) > PENDING_TIMEOUT_MS {
            // El destino no respondió: el servidor volverá a pedir la redirección si sigue en pie.
            self.stats.switch_failures += 1;
            if self.old_lost {
                self.phase = Phase::Disconnected(DisconnectReason::Timeout);
            }
            return;
        }
        if welcomed {
            self.promote(now, pend);
            return;
        }
        if pend.connected && pend.last_join.is_none_or(|t| now.saturating_sub(t) >= 200) {
            pend.last_join = Some(now);
            let hdr = pend.endpoint.next_header();
            let msg = proto::frame(hdr, proto::JOIN, &proto::join_body());
            let _ = pend.net.send(&msg);
        }
        self.pending = Some(pend);
    }

    /// El destino dio la bienvenida: la conexión nueva pasa a ser la activa.
    fn promote(&mut self, now: u64, pend: Pending<L>) {
        let Pending { net, endpoint, .. } = pend;
        let mut old = std::mem::replace(&mut self.net, net);
        let t = old.stats();
        self.carried_out = (
            self.carried_out.0 + t.bytes_out,
            self.carried_out.1 + t.packets_out,
        );
        old.disconnect();
        self.endpoint = endpoint;
        self.sent_at.clear();
        self.old_lost = false;
        self.just_switched = true;
        self.stats.switches += 1;
        // Los inputs que el shard anterior no llegó a confirmar se reenvían al nuevo, que continúa la
        // numeración: nada de lo que el jugador pidió se pierde ni se repite.
        let pending: Vec<(u32, Input)> = self.predictor.pending_inputs().cloned().collect();
        for chunk in pending.chunks(MAX_INPUTS_PER_MESSAGE) {
            let inputs: Vec<Input> = chunk.iter().map(|(_, i)| *i).collect();
            self.send_message(now, proto::INPUT, &proto::input_body(chunk[0].0, &inputs));
        }
    }

    /// Pidió entrar en un shard que no es el de su cuenta y este le dice cuál es. Como aún no hay estado
    /// de juego que conservar, se abandona esta conexión y se entra por la del shard indicado. Sin
    /// conector (o con un número de shard que no se conoce) la redirección se ignora, y tras demasiadas
    /// seguidas se da por perdida la conexión: es un bucle.
    fn follow_login_redirect(&mut self, now: u64, shard: u16) {
        if self.login_hops >= MAX_LOGIN_REDIRECTS {
            self.net.disconnect();
            self.phase = Phase::Disconnected(DisconnectReason::ConnectFailed);
            return;
        }
        let Some(conn) = self.connector.as_mut().and_then(|f| f(shard)) else {
            return;
        };
        let fresh = SecureClient::new(
            conn.link,
            conn.server,
            self.creds.clone(),
            self.transport_cfg.clone(),
            conn.entropy,
        );
        let mut old = std::mem::replace(&mut self.net, fresh);
        let t = old.stats();
        self.carried_out = (
            self.carried_out.0 + t.bytes_out,
            self.carried_out.1 + t.packets_out,
        );
        old.disconnect();
        self.endpoint = Endpoint::default();
        self.sent_at.clear();
        self.last_join = None;
        self.login_hops += 1;
        self.stats.login_redirects += 1;
        self.phase = Phase::Connecting;
        let _ = now;
    }

    fn send_message(&mut self, now: u64, kind: u8, body: &[u8]) {
        let hdr = self.endpoint.next_header();
        self.sent_at.insert(hdr.seq, now);
        let msg = proto::frame(hdr, kind, body);
        let _ = self.net.send(&msg);
    }

    fn send_input(&mut self, now: u64) {
        self.input_seq += 1;
        let input = self.wanted;
        self.predictor.apply_local(self.input_seq, input);
        self.recent.push_back((self.input_seq, input));
        while self.recent.len() > REDUNDANCY.min(MAX_INPUTS_PER_MESSAGE) {
            self.recent.pop_front();
        }
        let first = self.recent.front().map_or(self.input_seq, |(s, _)| *s);
        let inputs: Vec<Input> = self.recent.iter().map(|(_, i)| *i).collect();
        self.send_message(now, proto::INPUT, &proto::input_body(first, &inputs));
        self.stats.inputs_sent += 1;
    }

    fn on_ack(&mut self, now: u64, acked: Vec<u16>) {
        for seq in acked {
            if let Some(t) = self.sent_at.remove(&seq) {
                let sample = now.saturating_sub(t) as f64;
                self.rtt_ms = Some(match self.rtt_ms {
                    None => sample,
                    Some(r) => r * 0.875 + sample * 0.125,
                });
            }
        }
    }

    fn on_message(&mut self, now: u64, data: &[u8]) {
        let Some((hdr, kind, body)) = proto::unframe(data) else {
            return;
        };
        match kind {
            proto::WELCOME => {
                let Some(w) = proto::parse_welcome(body) else {
                    return;
                };
                let acked = self.endpoint.on_receive(&hdr);
                self.on_ack(now, acked);
                if self.phase == Phase::Joining {
                    self.entity = w.entity;
                    self.tick_ms = (w.tick_ms as u64).max(1);
                    self.predictor = Predictor::new(PlayerState { pos: w.pos });
                    self.input_seq = 0;
                    self.recent.clear();
                    self.next_input_at = now + self.tick_ms;
                    self.login_hops = 0;
                    self.phase = Phase::Playing;
                }
            }
            proto::REDIRECT => {
                let Some(shard) = proto::parse_redirect(body) else {
                    return;
                };
                let acked = self.endpoint.on_receive(&hdr);
                self.on_ack(now, acked);
                self.stats.redirects += 1;
                if self.phase == Phase::Joining && self.pending.is_none() {
                    self.follow_login_redirect(now, shard);
                    return;
                }
                if self.phase != Phase::Playing
                    || self.pending.as_ref().is_some_and(|p| p.shard == shard)
                {
                    return;
                }
                if let Some(conn) = self.connector.as_mut().and_then(|f| f(shard)) {
                    self.pending = Some(Pending {
                        shard,
                        net: SecureClient::new(
                            conn.link,
                            conn.server,
                            self.creds.clone(),
                            self.transport_cfg.clone(),
                            conn.entropy,
                        ),
                        endpoint: Endpoint::default(),
                        connected: false,
                        last_join: None,
                        started: now,
                    });
                }
            }
            proto::SNAP_FULL | proto::SNAP_DELTA if self.phase == Phase::Playing => {
                let Some((last_input, encoded)) = proto::parse_snapshot(body) else {
                    return;
                };
                let decoded = if kind == proto::SNAP_FULL {
                    Snapshot::decode_full(encoded).ok()
                } else {
                    // La línea base se identifica leyendo el principio del propio mensaje.
                    let mut r = Reader::new(encoded);
                    let base = r.varint().and_then(|_| r.varint()).ok().map(|b| b as u32);
                    base.and_then(|b| self.snaps.iter().find(|s| s.tick == b))
                        .and_then(|b| Snapshot::decode_delta(encoded, b).ok())
                };
                let Some(snap) = decoded else {
                    // Sin línea base o corrupto: no se confirma, y el servidor se repliega solo.
                    self.stats.snapshots_dropped += 1;
                    return;
                };
                if std::mem::take(&mut self.just_switched)
                    && self.snaps.back().is_some_and(|s| snap.tick <= s.tick)
                {
                    // El primer snapshot del shard nuevo lleva un número de tick menor que el último del
                    // anterior: cuentan por separado. Lo viejo no sirve como línea base ni para
                    // interpolar (los números se podrían confundir): se empieza de cero.
                    self.snaps.clear();
                }
                if self.snaps.back().is_some_and(|s| snap.tick <= s.tick) {
                    return; // llegó tarde y fuera de orden: ya hay uno más nuevo
                }
                let acked = self.endpoint.on_receive(&hdr);
                self.on_ack(now, acked);
                self.stats.snapshots += 1;

                if let Some(me) = snap.entities.get(&self.entity) {
                    let before = self.predictor.state.pos;
                    self.predictor
                        .reconcile(PlayerState { pos: me.pos }, last_input);
                    let after = self.predictor.state.pos;
                    let d = (0..3)
                        .map(|i| (before[i] as i64 - after[i] as i64).pow(2))
                        .sum::<i64>() as f64;
                    let d = d.sqrt() as u32;
                    if d > 0 {
                        self.stats.corrections += 1;
                        self.stats.correction_sum_q += d as u64;
                        self.stats.correction_max_q = self.stats.correction_max_q.max(d);
                    }
                }
                self.snaps.push_back(snap);
                while self.snaps.len() > SNAPSHOT_RING {
                    self.snaps.pop_front();
                }
                self.latest_rx_ms = now;
            }
            _ => {}
        }
    }

    /// Posiciones (unidades) de lo que se debe dibujar en el instante `now`: mi jugador con predicción
    /// y las demás entidades interpoladas entre los dos snapshots que rodean `now` menos un pequeño retardo.
    pub fn view(&self, now: u64) -> BTreeMap<u64, [f32; 3]> {
        let mut out = BTreeMap::new();
        let Some(latest) = self.snaps.back() else {
            return out;
        };
        let est = latest.tick as f64
            + now.saturating_sub(self.latest_rx_ms) as f64 / self.tick_ms as f64
            - INTERP_DELAY_TICKS;
        let render = est.min(latest.tick as f64);
        let older = self.snaps.iter().rev().find(|s| (s.tick as f64) <= render);
        let q = |e: &EntityState| {
            [
                dequantize(e.pos[0]),
                dequantize(e.pos[1]),
                dequantize(e.pos[2]),
            ]
        };
        match older {
            None => {
                let first = self.snaps.front().unwrap_or(latest);
                for (id, e) in &first.entities {
                    out.insert(*id, q(e));
                }
            }
            Some(a) => {
                let b = self.snaps.iter().find(|s| s.tick > a.tick).unwrap_or(a);
                let t = if b.tick > a.tick {
                    ((render - a.tick as f64) / (b.tick - a.tick) as f64).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
                for (id, eb) in &b.entities {
                    let pb = q(eb);
                    let p = match a.entities.get(id) {
                        Some(ea) => {
                            let pa = q(ea);
                            [
                                pa[0] + (pb[0] - pa[0]) * t,
                                pa[1] + (pb[1] - pa[1]) * t,
                                pa[2] + (pb[2] - pa[2]) * t,
                            ]
                        }
                        None => pb,
                    };
                    out.insert(*id, p);
                }
            }
        }
        if let Some(me) = self.predicted_position() {
            out.insert(self.entity, me);
        }
        out
    }
}
