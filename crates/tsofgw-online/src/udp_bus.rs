//! Bus entre shards sobre UDP real, autenticado con una clave de cluster.
//!
//! Cada mensaje del bus ([`crate::bus::PeerMsg`]) viaja en **un** datagrama:
//!
//! ```text
//! BUS  0x20 | versión(1) | desde(2) | hacia(2) | contador(8) | reloj_ms(8) | config(8) | carga | etiqueta(16)
//! ```
//! * **Autenticidad e integridad**: la etiqueta es HMAC-SHA256 truncado a 128 bits, calculado con una
//!   clave derivada de la clave de cluster, sobre toda la cabecera y la carga. Incluye `desde` y `hacia`:
//!   un datagrama no se puede redirigir a otro shard ni reflejar hacia quien lo envió.
//! * **Anti-replay**: ventana de 128 contadores por shard emisor ([`ReplayWindow`]). El contador de cada
//!   emisor arranca en el reloj de pared (µs), así que un shard que se reinicia nunca repite contadores.
//! * **Frescura**: se rechaza un datagrama cuyo `reloj_ms` difiera del reloj local más de
//!   [`DEFAULT_MAX_SKEW_MS`]. Es lo que impide reinyectar capturas antiguas en un shard recién
//!   reiniciado (su ventana de contadores empieza vacía). Exige relojes sincronizados (NTP).
//! * **Misma configuración**: `config` resume la forma del mundo (rejilla, NPC, tick, radios, semilla...).
//!   Dos shards con configuraciones distintas se descartan mutuamente (y se cuenta aparte), en lugar de
//!   repartir un mundo incoherente en silencio.
//! * **Sin amplificación**: el bus nunca responde a un datagrama recibido. Solo envía a las direcciones
//!   de la configuración, jamás a la dirección de origen de lo que llega.
//! * **Sin fragmentación**: un mensaje mayor que [`MAX_PEER_MSG`] no se envía (se cuenta). Los mensajes
//!   grandes —los fantasmas— los trocea la lógica del servidor en trozos independientes.
//!
//! No cifra: los mensajes van en claro (autenticados). Es un bus para una **red privada** entre los
//! servidores de un mismo operador; quien conozca la clave de cluster puede hablar como cualquier shard.

use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{SystemTime, UNIX_EPOCH};

use tsofgw_pack::{ct_eq, HmacKey};
use tsofgw_transport::ReplayWindow;

use crate::bus::{PeerBus, MAX_PEER_MSG};
use crate::partition::ShardId;

pub const BUS_PACKET: u8 = 0x20;
pub const BUS_VERSION: u8 = 1;
pub const HEADER_LEN: usize = 1 + 1 + 2 + 2 + 8 + 8 + 8;
pub const TAG_LEN: usize = 16;
pub const MAX_DATAGRAM: usize = 1200;
/// Diferencia máxima tolerada entre el reloj de un datagrama y el local.
pub const DEFAULT_MAX_SKEW_MS: u64 = 30_000;

const _: () = assert!(HEADER_LEN + MAX_PEER_MSG + TAG_LEN == MAX_DATAGRAM);

const RX_BUFFER: usize = 2048;
/// Tope de datagramas que se leen por llamada a `recv`: un inundador no puede monopolizar el tick.
const MAX_RECV_PER_POLL: usize = 8192;
/// Errores de lectura seguidos que se ignoran (en Windows, enviar a un puerto cerrado hace fallar al
/// siguiente `recv` con `ConnectionReset` aunque haya datos de otros).
const MAX_RECV_ERRORS: usize = 64;

/// Milisegundos desde el 1-1-1970 según el reloj del sistema.
pub fn system_wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Clave del bus derivada de la clave de cluster (separación de dominio: la clave de cluster en sí no
/// se usa nunca directamente para firmar).
pub fn bus_key(cluster_key: &[u8]) -> HmacKey {
    HmacKey::new(&HmacKey::new(cluster_key).mac(&[b"tsofgw/bus-key/v1"]))
}

/// Construye un datagrama del bus.
pub fn seal(
    key: &HmacKey,
    from: ShardId,
    to: ShardId,
    counter: u64,
    wall_ms: u64,
    config: u64,
    payload: &[u8],
) -> Vec<u8> {
    let mut p = Vec::with_capacity(HEADER_LEN + payload.len() + TAG_LEN);
    p.push(BUS_PACKET);
    p.push(BUS_VERSION);
    p.extend_from_slice(&from.to_le_bytes());
    p.extend_from_slice(&to.to_le_bytes());
    p.extend_from_slice(&counter.to_le_bytes());
    p.extend_from_slice(&wall_ms.to_le_bytes());
    p.extend_from_slice(&config.to_le_bytes());
    p.extend_from_slice(payload);
    let full = key.mac(&[&p]);
    p.extend_from_slice(&full[..TAG_LEN]);
    p
}

/// Los campos de un datagrama bien formado (con la etiqueta aún sin verificar).
#[derive(Debug, PartialEq, Eq)]
pub struct Parsed<'a> {
    pub from: ShardId,
    pub to: ShardId,
    pub counter: u64,
    pub wall_ms: u64,
    pub config: u64,
    pub payload: &'a [u8],
}

/// Interpreta la estructura de un datagrama sin comprobar su etiqueta: sirve para descartar lo evidente
/// antes de gastar un HMAC.
pub fn parse(d: &[u8]) -> Option<Parsed<'_>> {
    if d.len() < HEADER_LEN + 1 + TAG_LEN
        || d.len() > MAX_DATAGRAM
        || d[0] != BUS_PACKET
        || d[1] != BUS_VERSION
    {
        return None;
    }
    let u16_at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
    let u64_at = |i: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&d[i..i + 8]);
        u64::from_le_bytes(b)
    };
    Some(Parsed {
        from: u16_at(2),
        to: u16_at(4),
        counter: u64_at(6),
        wall_ms: u64_at(14),
        config: u64_at(22),
        payload: &d[HEADER_LEN..d.len() - TAG_LEN],
    })
}

/// ¿La etiqueta del datagrama es válida para esta clave?
pub fn verify(key: &HmacKey, d: &[u8]) -> bool {
    if d.len() < TAG_LEN {
        return false;
    }
    let (signed, tag) = d.split_at(d.len() - TAG_LEN);
    ct_eq(&key.mac(&[signed])[..TAG_LEN], tag)
}

/// Lo que vio el bus. Todo lo que no es `received - rejected` llegó a la lógica del servidor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UdpBusStats {
    pub sent: u64,
    pub bytes_out: u64,
    pub received: u64,
    pub bytes_in: u64,
    pub delivered: u64,
    /// Mensajes que no cabían en un datagrama.
    pub oversize: u64,
    /// Envíos a un shard desconocido o sin dirección configurada.
    pub unknown_dest: u64,
    pub send_errors: u64,
    /// Basura: tamaño, tipo o versión imposibles.
    pub bad_format: u64,
    /// El campo `desde` no es un shard conocido (o es este mismo).
    pub unknown_peer: u64,
    /// Dirigido a otro shard.
    pub misrouted: u64,
    pub bad_tag: u64,
    /// Auténticos, pero de un shard con otra configuración de mundo.
    pub config_mismatch: u64,
    /// Reloj demasiado lejano.
    pub stale: u64,
    /// Contador repetido o demasiado viejo.
    pub replays: u64,
}

impl UdpBusStats {
    pub fn rejected(&self) -> u64 {
        self.bad_format
            + self.unknown_peer
            + self.misrouted
            + self.bad_tag
            + self.config_mismatch
            + self.stale
            + self.replays
    }
}

pub struct UdpBus {
    me: ShardId,
    socket: UdpSocket,
    peers: Vec<Option<SocketAddr>>,
    key: HmacKey,
    config: u64,
    next_counter: u64,
    replay: Vec<ReplayWindow>,
    clock: Box<dyn Fn() -> u64>,
    max_skew_ms: u64,
    stats: UdpBusStats,
    buf: Vec<u8>,
}

impl UdpBus {
    /// Abre el socket del shard `me` en `bind`. Las direcciones de los demás shards se fijan después con
    /// [`UdpBus::set_peer`] (así se puede abrir primero y conocer el puerto asignado).
    pub fn bind(
        me: ShardId,
        bind: impl ToSocketAddrs,
        shards: usize,
        cluster_key: &[u8],
    ) -> Result<UdpBus, String> {
        if cluster_key.len() < 16 {
            return Err("la clave de cluster debe tener al menos 16 bytes".into());
        }
        if shards == 0 || shards > u16::MAX as usize || (me as usize) >= shards {
            return Err(format!("el shard {me} no existe en un cluster de {shards}"));
        }
        let socket = UdpSocket::bind(bind).map_err(|e| format!("no se pudo abrir el bus: {e}"))?;
        socket
            .set_nonblocking(true)
            .map_err(|e| format!("socket del bus: {e}"))?;
        let clock: Box<dyn Fn() -> u64> = Box::new(system_wall_ms);
        Ok(UdpBus {
            me,
            socket,
            peers: vec![None; shards],
            key: bus_key(cluster_key),
            config: 0,
            next_counter: clock().saturating_mul(1000),
            replay: vec![ReplayWindow::default(); shards],
            clock,
            max_skew_ms: DEFAULT_MAX_SKEW_MS,
            stats: UdpBusStats::default(),
            buf: vec![0u8; RX_BUFFER],
        })
    }

    /// Cambia la fuente de reloj de pared (en ms desde 1970). Reinicia el contador de envío a partir de
    /// ese reloj. Pensado para pruebas.
    pub fn with_clock(mut self, clock: Box<dyn Fn() -> u64>) -> Self {
        self.next_counter = clock().saturating_mul(1000);
        self.clock = clock;
        self
    }

    pub fn with_max_skew_ms(mut self, ms: u64) -> Self {
        self.max_skew_ms = ms;
        self
    }

    /// Fija el resumen de configuración (también lo hace `GameServer::new_shard`).
    pub fn with_config_digest(mut self, digest: u64) -> Self {
        self.config = digest;
        self
    }

    /// Fija la dirección a la que se envía lo destinado al shard `shard`.
    pub fn set_peer(&mut self, shard: ShardId, addr: SocketAddr) -> Result<(), String> {
        match self.peers.get_mut(shard as usize) {
            Some(slot) if shard != self.me => {
                *slot = Some(addr);
                Ok(())
            }
            Some(_) => Err("un shard no es su propio vecino".into()),
            None => Err(format!("el shard {shard} no existe")),
        }
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn stats(&self) -> &UdpBusStats {
        &self.stats
    }
}

impl PeerBus for UdpBus {
    fn send(&mut self, _now_ms: u64, to: ShardId, data: Vec<u8>) {
        if data.is_empty() || data.len() > MAX_PEER_MSG {
            self.stats.oversize += 1;
            return;
        }
        let Some(addr) = self.peers.get(to as usize).copied().flatten() else {
            self.stats.unknown_dest += 1;
            return;
        };
        let counter = self.next_counter;
        self.next_counter += 1;
        let d = seal(
            &self.key,
            self.me,
            to,
            counter,
            (self.clock)(),
            self.config,
            &data,
        );
        match self.socket.send_to(&d, addr) {
            Ok(_) => {
                self.stats.sent += 1;
                self.stats.bytes_out += d.len() as u64;
            }
            Err(_) => self.stats.send_errors += 1,
        }
    }

    fn recv(&mut self, _now_ms: u64, out: &mut Vec<(ShardId, Vec<u8>)>) {
        let wall = (self.clock)();
        let mut errors = 0;
        for _ in 0..MAX_RECV_PER_POLL {
            let n = match self.socket.recv_from(&mut self.buf) {
                Ok((n, _from)) => n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    errors += 1;
                    if errors > MAX_RECV_ERRORS {
                        break;
                    }
                    continue;
                }
            };
            self.stats.received += 1;
            self.stats.bytes_in += n as u64;
            let d = &self.buf[..n];
            let Some(p) = parse(d) else {
                self.stats.bad_format += 1;
                continue;
            };
            let from = p.from as usize;
            if from >= self.peers.len() || p.from == self.me {
                self.stats.unknown_peer += 1;
                continue;
            }
            if p.to != self.me {
                self.stats.misrouted += 1;
                continue;
            }
            if !verify(&self.key, d) {
                self.stats.bad_tag += 1;
                continue;
            }
            if p.config != self.config {
                self.stats.config_mismatch += 1;
                continue;
            }
            if wall.abs_diff(p.wall_ms) > self.max_skew_ms {
                self.stats.stale += 1;
                continue;
            }
            if !self.replay[from].accept(p.counter) {
                self.stats.replays += 1;
                continue;
            }
            self.stats.delivered += 1;
            out.push((p.from, p.payload.to_vec()));
        }
    }

    fn rejected(&self) -> u64 {
        self.stats.rejected()
    }

    fn set_config_digest(&mut self, digest: u64) {
        self.config = digest;
    }

    fn config_mismatches(&self) -> u64 {
        self.stats.config_mismatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    const KEY: &[u8] = b"clave-de-cluster-de-prueba";

    /// `n` buses sobre loopback, todos conectados entre sí.
    fn mesh(n: usize, key: &[u8]) -> Vec<UdpBus> {
        let mut buses: Vec<UdpBus> = (0..n)
            .map(|i| UdpBus::bind(i as ShardId, "127.0.0.1:0", n, key).unwrap())
            .collect();
        let addrs: Vec<SocketAddr> = buses.iter().map(|b| b.local_addr().unwrap()).collect();
        for (i, b) in buses.iter_mut().enumerate() {
            for (j, a) in addrs.iter().enumerate() {
                if i != j {
                    b.set_peer(j as ShardId, *a).unwrap();
                }
            }
        }
        buses
    }

    /// Lee del bus hasta tener `want` mensajes o agotar el plazo.
    fn wait(bus: &mut UdpBus, want: usize) -> Vec<(ShardId, Vec<u8>)> {
        let mut out = Vec::new();
        let end = Instant::now() + Duration::from_secs(2);
        while out.len() < want && Instant::now() < end {
            bus.recv(0, &mut out);
            std::thread::sleep(Duration::from_millis(1));
        }
        out
    }

    /// Datagrama bueno para usar de «testigo»: cuando llega, todo lo anterior (que iba por el mismo
    /// socket, en orden) ya se procesó.
    fn flush(from: &mut UdpBus, to: &mut UdpBus) {
        from.send(0, to.me, vec![0xEE]);
        let got = wait(to, 1);
        assert_eq!(got.last().map(|g| g.1.clone()), Some(vec![0xEE]));
    }

    #[test]
    fn sellar_y_abrir_ida_y_vuelta_y_cualquier_alteracion_se_detecta() {
        let key = bus_key(KEY);
        let d = seal(&key, 3, 5, 1234, 99, 42, b"hola");
        let p = parse(&d).unwrap();
        assert_eq!(
            p,
            Parsed {
                from: 3,
                to: 5,
                counter: 1234,
                wall_ms: 99,
                config: 42,
                payload: b"hola"
            }
        );
        assert!(verify(&key, &d));
        assert!(!verify(&bus_key(b"otra-clave-distinta-1"), &d));
        // Cambiar cualquier bit de cualquier byte invalida el datagrama (o su estructura).
        for i in 0..d.len() {
            for bit in 0..8 {
                let mut m = d.clone();
                m[i] ^= 1 << bit;
                assert!(
                    parse(&m).is_none() || !verify(&key, &m),
                    "byte {i} bit {bit} no se detectó"
                );
            }
        }
        // Truncados, alargados y vacíos.
        assert!(parse(&d[..d.len() - 1]).is_some() && !verify(&key, &d[..d.len() - 1]));
        assert!(parse(&[]).is_none());
        assert!(parse(&d[..HEADER_LEN + TAG_LEN]).is_none(), "sin carga");
        let mut big = d.clone();
        big.resize(MAX_DATAGRAM + 1, 0);
        assert!(parse(&big).is_none());
        // La carga máxima cabe exactamente.
        let full = seal(&key, 0, 1, 1, 1, 0, &vec![7u8; MAX_PEER_MSG]);
        assert_eq!(full.len(), MAX_DATAGRAM);
        assert!(parse(&full).is_some());
    }

    #[test]
    fn dos_shards_se_hablan_por_loopback() {
        let mut b = mesh(3, KEY);
        let (mut s2, mut s1, mut s0) = (b.pop().unwrap(), b.pop().unwrap(), b.pop().unwrap());
        s0.send(0, 1, b"uno".to_vec());
        s0.send(0, 1, vec![9; MAX_PEER_MSG]);
        s2.send(0, 1, b"tres".to_vec());
        let got = wait(&mut s1, 3);
        assert_eq!(got.len(), 3);
        assert!(got.contains(&(0, b"uno".to_vec())));
        assert!(got.contains(&(0, vec![9; MAX_PEER_MSG])));
        assert!(got.contains(&(2, b"tres".to_vec())));
        assert_eq!(s1.stats().delivered, 3);
        assert_eq!(s1.rejected(), 0);
        assert_eq!(s0.stats().sent, 2);
        // Lo destinado a 1 no lo ve nadie más.
        let mut none = Vec::new();
        s0.recv(0, &mut none);
        s2.recv(0, &mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn envios_imposibles_se_cuentan_y_no_se_hacen() {
        let mut b = mesh(2, KEY);
        let mut s1 = b.pop().unwrap();
        let mut s0 = b.pop().unwrap();
        s0.send(0, 1, vec![0; MAX_PEER_MSG + 1]);
        s0.send(0, 1, vec![]);
        s0.send(0, 7, b"x".to_vec()); // shard que no existe
        s0.send(0, 0, b"x".to_vec()); // él mismo: sin dirección
        assert_eq!(s0.stats().oversize, 2);
        assert_eq!(s0.stats().unknown_dest, 2);
        assert_eq!(s0.stats().sent, 0);
        flush(&mut s0, &mut s1);
        assert_eq!(s1.stats().delivered, 1);
    }

    #[test]
    fn lo_que_no_esta_autenticado_se_descarta_y_se_cuenta() {
        let mut b = mesh(2, KEY);
        let mut s1 = b.pop().unwrap();
        let mut s0 = b.pop().unwrap();
        let target = s1.local_addr().unwrap();
        let raw = UdpSocket::bind("127.0.0.1:0").unwrap();
        let wall = system_wall_ms();
        let evil = bus_key(b"clave-de-un-atacante-xx");
        let good = bus_key(KEY);
        // Firmado con otra clave.
        raw.send_to(&seal(&evil, 0, 1, 5, wall, 0, b"mal"), target)
            .unwrap();
        // Bien firmado pero con una etiqueta alterada.
        let mut tampered = seal(&good, 0, 1, 6, wall, 0, b"mal");
        *tampered.last_mut().unwrap() ^= 1;
        raw.send_to(&tampered, target).unwrap();
        // Bien firmado, pero para otro shard.
        raw.send_to(&seal(&good, 0, 0, 7, wall, 0, b"mal"), target)
            .unwrap();
        // Desde un shard que no existe y desde sí mismo.
        raw.send_to(&seal(&good, 9, 1, 8, wall, 0, b"mal"), target)
            .unwrap();
        raw.send_to(&seal(&good, 1, 1, 9, wall, 0, b"mal"), target)
            .unwrap();
        // Basura de todas las formas.
        for junk in [
            vec![],
            vec![0x20],
            vec![0x20, 1, 0, 0, 1, 0],
            vec![0xFF; 1200],
            vec![0x20; 1500],
            vec![0u8; 40],
        ] {
            raw.send_to(&junk, target).unwrap();
        }
        // Reloj muy atrasado y muy adelantado.
        raw.send_to(&seal(&good, 0, 1, 10, wall - 3_600_000, 0, b"mal"), target)
            .unwrap();
        raw.send_to(&seal(&good, 0, 1, 11, wall + 3_600_000, 0, b"mal"), target)
            .unwrap();
        flush(&mut s0, &mut s1);
        let st = s1.stats();
        assert_eq!(st.delivered, 1, "solo el testigo: {st:?}");
        assert_eq!(st.bad_tag, 2, "{st:?}");
        assert_eq!(st.misrouted, 1, "{st:?}");
        assert_eq!(st.unknown_peer, 2, "{st:?}");
        assert_eq!(st.bad_format, 6, "{st:?}");
        assert_eq!(st.stale, 2, "{st:?}");
        assert_eq!(st.rejected(), 13);
        assert_eq!(s1.rejected(), 13);
    }

    #[test]
    fn un_datagrama_repetido_se_entrega_una_sola_vez_y_el_desorden_se_tolera() {
        let mut b = mesh(2, KEY);
        let mut s1 = b.pop().unwrap();
        let mut s0 = b.pop().unwrap();
        let target = s1.local_addr().unwrap();
        let raw = UdpSocket::bind("127.0.0.1:0").unwrap();
        let key = bus_key(KEY);
        let wall = system_wall_ms();
        // Contadores justo por debajo de los que usará el emisor real para el testigo.
        let base = s0.next_counter - 1000;
        // Orden 3, 1, 2 (todos nuevos) y luego repeticiones de cada uno.
        let dg = |c: u64, m: &[u8]| seal(&key, 0, 1, base + c, wall, 0, m);
        for c in [3u64, 1, 2, 3, 1, 2, 2] {
            raw.send_to(&dg(c, &[c as u8]), target).unwrap();
        }
        flush(&mut s0, &mut s1);
        let mut got = Vec::new();
        // `flush` ya recogió todo: lo entregado está en las estadísticas; volvemos a enviar para leerlo.
        assert_eq!(s1.stats().delivered, 4, "3 distintos + el testigo");
        assert_eq!(s1.stats().replays, 4);
        s1.recv(0, &mut got);
        assert!(got.is_empty());
    }

    #[test]
    fn un_shard_reiniciado_no_repite_contadores_y_las_capturas_viejas_no_valen() {
        let now = Rc::new(Cell::new(1_700_000_000_000u64));
        let clock = |now: &Rc<Cell<u64>>| {
            let n = now.clone();
            Box::new(move || n.get()) as Box<dyn Fn() -> u64>
        };
        let mut s1 = UdpBus::bind(1, "127.0.0.1:0", 2, KEY)
            .unwrap()
            .with_clock(clock(&now));
        let mut s0 = UdpBus::bind(0, "127.0.0.1:0", 2, KEY)
            .unwrap()
            .with_clock(clock(&now));
        s0.set_peer(1, s1.local_addr().unwrap()).unwrap();
        s1.set_peer(0, s0.local_addr().unwrap()).unwrap();
        for i in 0..50u8 {
            s0.send(0, 1, vec![i]);
        }
        assert_eq!(wait(&mut s1, 50).len(), 50);
        // El shard 0 «se reinicia» 10 segundos después: socket y estado nuevos, mismo puerto lógico.
        now.set(now.get() + 10_000);
        let mut s0b = UdpBus::bind(0, "127.0.0.1:0", 2, KEY)
            .unwrap()
            .with_clock(clock(&now));
        s0b.set_peer(1, s1.local_addr().unwrap()).unwrap();
        s0b.send(0, 1, b"de nuevo".to_vec());
        assert_eq!(wait(&mut s1, 1), vec![(0, b"de nuevo".to_vec())]);
        // Un shard 1 reiniciado (ventana vacía) no acepta una captura de hace una hora...
        let key = bus_key(KEY);
        let mut s1b = UdpBus::bind(1, "127.0.0.1:0", 2, KEY)
            .unwrap()
            .with_clock(clock(&now));
        let old = seal(
            &key,
            0,
            1,
            now.get() * 1000,
            now.get() - 3_600_000,
            0,
            b"vieja",
        );
        let raw = UdpSocket::bind("127.0.0.1:0").unwrap();
        raw.send_to(&old, s1b.local_addr().unwrap()).unwrap();
        s0b.set_peer(1, s1b.local_addr().unwrap()).unwrap();
        flush(&mut s0b, &mut s1b);
        assert_eq!(s1b.stats().stale, 1);
        assert_eq!(s1b.stats().delivered, 1, "solo el testigo");
        // ...y con relojes que difieren poco sí hablan.
        let mut s1c = UdpBus::bind(1, "127.0.0.1:0", 2, KEY)
            .unwrap()
            .with_clock(clock(&Rc::new(Cell::new(now.get() + 20_000))));
        s0b.set_peer(1, s1c.local_addr().unwrap()).unwrap();
        s0b.send(0, 1, b"ok".to_vec());
        assert_eq!(wait(&mut s1c, 1).len(), 1);
        // Con relojes muy distintos, no.
        let mut s1d = UdpBus::bind(1, "127.0.0.1:0", 2, KEY)
            .unwrap()
            .with_clock(clock(&Rc::new(Cell::new(now.get() + 120_000))));
        s0b.set_peer(1, s1d.local_addr().unwrap()).unwrap();
        s0b.send(0, 1, b"no".to_vec());
        std::thread::sleep(Duration::from_millis(50));
        let mut out = Vec::new();
        s1d.recv(0, &mut out);
        assert!(out.is_empty());
        assert_eq!(s1d.stats().stale, 1);
    }

    #[test]
    fn dos_shards_con_distinta_configuracion_de_mundo_no_se_hablan() {
        let mut b = mesh(3, KEY);
        let (mut s2, mut s1, mut s0) = (b.pop().unwrap(), b.pop().unwrap(), b.pop().unwrap());
        s0.set_config_digest(0xAAAA);
        s1.set_config_digest(0xAAAA);
        s2.set_config_digest(0xBBBB); // otro mundo
        s0.send(0, 1, b"mismo mundo".to_vec());
        s0.send(0, 2, b"otro mundo".to_vec());
        s2.send(0, 1, b"desde otro mundo".to_vec());
        assert_eq!(wait(&mut s1, 1), vec![(0, b"mismo mundo".to_vec())]);
        std::thread::sleep(Duration::from_millis(30));
        let mut out = Vec::new();
        s1.recv(0, &mut out);
        s2.recv(0, &mut out);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(s1.config_mismatches(), 1);
        assert_eq!(s2.config_mismatches(), 1);
        assert_eq!(s1.rejected(), 1);
        assert_eq!(s0.config_mismatches(), 0);
    }

    #[test]
    fn una_inundacion_no_monopoliza_una_lectura() {
        let mut b = mesh(2, KEY);
        let mut s1 = b.pop().unwrap();
        let target = s1.local_addr().unwrap();
        let raw = UdpSocket::bind("127.0.0.1:0").unwrap();
        for _ in 0..3000 {
            raw.send_to(&[0u8; 100], target).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        let mut out = Vec::new();
        s1.recv(0, &mut out);
        assert!(out.is_empty());
        assert!(
            s1.stats().bad_format > 100,
            "se descartó la basura: {:?}",
            s1.stats()
        );
    }
}
