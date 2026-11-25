//! Enlaces de datagramas: UDP real y una red simulada determinista (pérdida, duplicados, retardo y desorden).

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fmt::Debug;
use std::hash::Hash;
use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::rc::Rc;

/// Dirección de un extremo. Se serializa para atar las cookies a la dirección de origen.
pub trait PeerAddr: Copy + Eq + Hash + Debug {
    fn write_bytes(&self, out: &mut Vec<u8>);
}

impl PeerAddr for SocketAddr {
    fn write_bytes(&self, out: &mut Vec<u8>) {
        match self {
            SocketAddr::V4(a) => {
                out.push(4);
                out.extend_from_slice(&a.ip().octets());
            }
            SocketAddr::V6(a) => {
                out.push(6);
                out.extend_from_slice(&a.ip().octets());
            }
        }
        out.extend_from_slice(&self.port().to_le_bytes());
    }
}

/// Canal de datagramas no bloqueante y sin garantías (como UDP).
pub trait Link {
    type Addr: PeerAddr;
    /// Envía un datagrama. Los fallos se descartan: el transporte ya asume que se pierden paquetes.
    fn send(&mut self, to: Self::Addr, data: &[u8]);
    /// Siguiente datagrama recibido, si hay alguno. Copia en `buf` (truncando si no cabe).
    fn recv(&mut self, buf: &mut [u8]) -> Option<(Self::Addr, usize)>;
}

// ------------------------------------------------------------------ UDP

pub struct UdpLink {
    socket: UdpSocket,
    pub send_errors: u64,
}

impl UdpLink {
    pub fn bind(addr: impl ToSocketAddrs) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(addr)?;
        socket.set_nonblocking(true)?;
        Ok(UdpLink {
            socket,
            send_errors: 0,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

impl Link for UdpLink {
    type Addr = SocketAddr;

    fn send(&mut self, to: SocketAddr, data: &[u8]) {
        if self.socket.send_to(data, to).is_err() {
            self.send_errors += 1;
        }
    }

    fn recv(&mut self, buf: &mut [u8]) -> Option<(SocketAddr, usize)> {
        // En Windows, enviar a un puerto cerrado hace que el siguiente `recv` falle con ConnectionReset
        // aunque haya datos de otros: hay que ignorarlo y reintentar (con tope para no girar sin fin).
        for _ in 0..64 {
            match self.socket.recv_from(buf) {
                Ok((n, from)) => return Some((from, n)),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return None,
                Err(_) => continue,
            }
        }
        None
    }
}

// --------------------------------------------------------------- simulada

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SimAddr(pub u32);

impl PeerAddr for SimAddr {
    fn write_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0.to_le_bytes());
    }
}

#[derive(Debug, Clone)]
pub struct SimConfig {
    pub latency_ms: u64,
    pub jitter_ms: u64,
    /// Probabilidad de perder un paquete (0.0–1.0).
    pub loss: f64,
    /// Probabilidad de que un paquete llegue duplicado.
    pub duplicate: f64,
    pub seed: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            latency_ms: 0,
            jitter_ms: 0,
            loss: 0.0,
            duplicate: 0.0,
            seed: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SimStats {
    pub sent: u64,
    pub delivered: u64,
    pub dropped: u64,
    pub duplicated: u64,
    pub bytes_sent: u64,
}

struct Queued {
    at: u64,
    seq: u64,
    from: SimAddr,
    data: Vec<u8>,
}

impl PartialEq for Queued {
    fn eq(&self, o: &Self) -> bool {
        (self.at, self.seq) == (o.at, o.seq)
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Queued {
    // BinaryHeap es de máximos: invertimos para sacar primero lo que llega antes.
    fn cmp(&self, o: &Self) -> Ordering {
        (o.at, o.seq).cmp(&(self.at, self.seq))
    }
}

struct NetInner {
    now: u64,
    cfg: SimConfig,
    rng: u64,
    inboxes: HashMap<u32, BinaryHeap<Queued>>,
    next_addr: u32,
    seq: u64,
    log: Option<Vec<(SimAddr, SimAddr, Vec<u8>)>>,
    stats: SimStats,
}

impl NetInner {
    fn next(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }
}

/// Red en memoria compartida por todos los extremos de una simulación (un solo hilo).
#[derive(Clone)]
pub struct SimNet {
    inner: Rc<RefCell<NetInner>>,
}

impl SimNet {
    pub fn new(cfg: SimConfig) -> Self {
        SimNet {
            inner: Rc::new(RefCell::new(NetInner {
                now: 0,
                rng: cfg.seed,
                cfg,
                inboxes: HashMap::new(),
                next_addr: 1,
                seq: 0,
                log: None,
                stats: SimStats::default(),
            })),
        }
    }

    /// Crea un extremo con una dirección nueva.
    pub fn endpoint(&self) -> SimLink {
        let mut n = self.inner.borrow_mut();
        let addr = SimAddr(n.next_addr);
        n.next_addr += 1;
        n.inboxes.insert(addr.0, BinaryHeap::new());
        SimLink {
            net: self.clone(),
            addr,
        }
    }

    /// Vuelve a abrir un extremo con una dirección que ya existió (el servidor que la usaba se cayó y
    /// arranca de nuevo). Lo que llegó mientras nadie escuchaba se pierde, como pasaría con UDP hacia un
    /// puerto cerrado: el extremo nuevo empieza con el buzón vacío. Solo para simulaciones.
    pub fn rebind(&self, addr: SimAddr) -> SimLink {
        let mut n = self.inner.borrow_mut();
        n.inboxes.insert(addr.0, BinaryHeap::new());
        if addr.0 >= n.next_addr {
            n.next_addr = addr.0 + 1;
        }
        SimLink {
            net: self.clone(),
            addr,
        }
    }

    pub fn set_now(&self, now_ms: u64) {
        self.inner.borrow_mut().now = now_ms;
    }

    pub fn now(&self) -> u64 {
        self.inner.borrow().now
    }

    pub fn set_config(&self, cfg: SimConfig) {
        let mut n = self.inner.borrow_mut();
        n.rng ^= cfg.seed;
        n.cfg = cfg;
    }

    pub fn stats(&self) -> SimStats {
        self.inner.borrow().stats
    }

    /// Empieza a registrar todo lo enviado (origen, destino, bytes) para poder inspeccionarlo o reenviarlo.
    pub fn start_log(&self) {
        self.inner.borrow_mut().log = Some(Vec::new());
    }

    pub fn take_log(&self) -> Vec<(SimAddr, SimAddr, Vec<u8>)> {
        self.inner
            .borrow_mut()
            .log
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }

    fn deliver(&self, from: SimAddr, to: SimAddr, data: &[u8]) {
        let mut n = self.inner.borrow_mut();
        n.stats.sent += 1;
        n.stats.bytes_sent += data.len() as u64;
        if let Some(log) = n.log.as_mut() {
            log.push((from, to, data.to_vec()));
        }
        let loss = n.cfg.loss;
        if n.chance(loss) {
            n.stats.dropped += 1;
            return;
        }
        let dup = n.cfg.duplicate;
        let copies = if n.chance(dup) {
            n.stats.duplicated += 1;
            2
        } else {
            1
        };
        for _ in 0..copies {
            let jitter = if n.cfg.jitter_ms > 0 {
                n.next() % (n.cfg.jitter_ms + 1)
            } else {
                0
            };
            let at = n.now + n.cfg.latency_ms + jitter;
            n.seq += 1;
            let seq = n.seq;
            if let Some(inbox) = n.inboxes.get_mut(&to.0) {
                inbox.push(Queued {
                    at,
                    seq,
                    from,
                    data: data.to_vec(),
                });
            }
        }
    }
}

pub struct SimLink {
    net: SimNet,
    addr: SimAddr,
}

impl SimLink {
    pub fn addr(&self) -> SimAddr {
        self.addr
    }

    /// Envía haciéndose pasar por otro origen. Solo para pruebas (suplantación de dirección).
    pub fn send_as(&mut self, from: SimAddr, to: SimAddr, data: &[u8]) {
        self.net.deliver(from, to, data);
    }
}

impl Link for SimLink {
    type Addr = SimAddr;

    fn send(&mut self, to: SimAddr, data: &[u8]) {
        self.net.deliver(self.addr, to, data);
    }

    fn recv(&mut self, buf: &mut [u8]) -> Option<(SimAddr, usize)> {
        let mut n = self.net.inner.borrow_mut();
        let now = n.now;
        let inbox = n.inboxes.get_mut(&self.addr.0)?;
        if inbox.peek().is_none_or(|q| q.at > now) {
            return None;
        }
        let q = inbox.pop()?;
        let len = q.data.len().min(buf.len());
        buf[..len].copy_from_slice(&q.data[..len]);
        n.stats.delivered += 1;
        Some((q.from, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(l: &mut SimLink) -> Vec<(SimAddr, Vec<u8>)> {
        let mut buf = [0u8; 2048];
        let mut v = Vec::new();
        while let Some((a, n)) = l.recv(&mut buf) {
            v.push((a, buf[..n].to_vec()));
        }
        v
    }

    #[test]
    fn entrega_con_latencia() {
        let net = SimNet::new(SimConfig {
            latency_ms: 50,
            ..Default::default()
        });
        let (mut a, mut b) = (net.endpoint(), net.endpoint());
        a.send(b.addr(), b"hola");
        assert!(drain(&mut b).is_empty());
        net.set_now(49);
        assert!(drain(&mut b).is_empty());
        net.set_now(50);
        let got = drain(&mut b);
        assert_eq!(got, vec![(a.addr(), b"hola".to_vec())]);
    }

    #[test]
    fn reabrir_una_direccion_descarta_lo_que_llego_mientras_nadie_escuchaba() {
        let net = SimNet::new(SimConfig::default());
        let (mut a, b) = (net.endpoint(), net.endpoint());
        let baddr = b.addr();
        a.send(baddr, b"antes de la caida");
        drop(b);
        a.send(baddr, b"con el servidor caido");
        let mut b2 = net.rebind(baddr);
        assert_eq!(b2.addr(), baddr, "misma direccion");
        assert!(
            drain(&mut b2).is_empty(),
            "lo enviado durante la caida se perdio"
        );
        a.send(baddr, b"despues");
        assert_eq!(drain(&mut b2), vec![(a.addr(), b"despues".to_vec())]);
        // Una direccion nueva no choca con la reabierta.
        assert_ne!(net.endpoint().addr(), baddr);
    }

    #[test]
    fn perdida_y_duplicados_son_reproducibles() {
        let run = |seed| {
            let net = SimNet::new(SimConfig {
                loss: 0.3,
                duplicate: 0.2,
                seed,
                ..Default::default()
            });
            let (mut a, mut b) = (net.endpoint(), net.endpoint());
            for i in 0..1000u32 {
                a.send(b.addr(), &i.to_le_bytes());
            }
            (drain(&mut b).len(), net.stats().dropped)
        };
        assert_eq!(run(5), run(5));
        let (recv, dropped) = run(5);
        assert!((600..900).contains(&(recv)), "recibidos {recv}");
        assert!(
            (200..400).contains(&(dropped as usize)),
            "perdidos {dropped}"
        );
    }

    #[test]
    fn el_jitter_puede_desordenar() {
        let net = SimNet::new(SimConfig {
            latency_ms: 10,
            jitter_ms: 40,
            seed: 3,
            ..Default::default()
        });
        let (mut a, mut b) = (net.endpoint(), net.endpoint());
        for i in 0..50u32 {
            a.send(b.addr(), &i.to_le_bytes());
        }
        net.set_now(1000);
        let order: Vec<u32> = drain(&mut b)
            .iter()
            .map(|(_, d)| u32::from_le_bytes([d[0], d[1], d[2], d[3]]))
            .collect();
        assert_eq!(order.len(), 50);
        assert!(order.windows(2).any(|w| w[0] > w[1]), "no hubo desorden");
    }

    #[test]
    fn udp_real_por_loopback() {
        let mut a = UdpLink::bind("127.0.0.1:0").unwrap();
        let mut b = UdpLink::bind("127.0.0.1:0").unwrap();
        let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        a.send(ba, b"ping");
        let mut buf = [0u8; 64];
        let mut got = None;
        for _ in 0..200 {
            if let Some(r) = b.recv(&mut buf) {
                got = Some(r);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (from, n) = got.expect("no llegó el datagrama por loopback");
        assert_eq!((from, &buf[..n]), (aa, &b"ping"[..]));
        assert!(a.recv(&mut buf).is_none());
    }
}
