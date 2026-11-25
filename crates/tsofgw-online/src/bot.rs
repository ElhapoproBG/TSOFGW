//! Jugador automático para pruebas de carga y demostraciones: camina en direcciones al azar.

use tsofgw_transport::Link;
use tsofgw_world::Rng;

use crate::client::GameClient;

pub struct Bot<L: Link> {
    pub client: GameClient<L>,
    rng: Rng,
    next_change: u64,
    frozen: bool,
    /// Si está definido, el bot no deambula: camina en `dir` y luego en sentido contrario, alternando
    /// cada `period_ms` (sirve para cruzar los bordes de un cluster una y otra vez).
    patrol: Option<([i8; 3], u64)>,
}

impl<L: Link> Bot<L> {
    pub fn new(client: GameClient<L>, seed: u64) -> Self {
        Bot {
            client,
            rng: Rng::new(seed),
            next_change: 0,
            frozen: false,
            patrol: None,
        }
    }

    /// Hace que el bot patrulle en lugar de deambular.
    pub fn with_patrol(mut self, dir: [i8; 3], period_ms: u64) -> Self {
        self.patrol = Some((dir, period_ms.max(1)));
        self
    }

    /// Deja de moverse (se sigue conectado). Sirve para comprobar que todo converge.
    pub fn freeze(&mut self) {
        self.frozen = true;
        self.client.set_input([0, 0, 0]);
    }

    pub fn update(&mut self, now: u64) {
        if let (Some((dir, period)), false) = (self.patrol, self.frozen) {
            let d = if (now / period).is_multiple_of(2) {
                dir
            } else {
                dir.map(|v| v.saturating_neg())
            };
            self.client.set_input(d);
        } else if !self.frozen && now >= self.next_change {
            self.next_change = now + 500 + self.rng.next_u64() % 2500;
            let dir = if self.rng.next_u64().is_multiple_of(4) {
                [0, 0, 0]
            } else {
                let a = self.rng.range(0.0, std::f32::consts::TAU);
                [(a.cos() * 127.0) as i8, 0, (a.sin() * 127.0) as i8]
            };
            self.client.set_input(dir);
        }
        self.client.update(now);
    }
}
