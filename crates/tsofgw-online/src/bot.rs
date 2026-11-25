//! Jugador automático para pruebas de carga y demostraciones: camina en direcciones al azar.

use tsofgw_transport::Link;
use tsofgw_world::Rng;

use crate::client::GameClient;

pub struct Bot<L: Link> {
    pub client: GameClient<L>,
    rng: Rng,
    next_change: u64,
    frozen: bool,
}

impl<L: Link> Bot<L> {
    pub fn new(client: GameClient<L>, seed: u64) -> Self {
        Bot {
            client,
            rng: Rng::new(seed),
            next_change: 0,
            frozen: false,
        }
    }

    /// Deja de moverse (se sigue conectado). Sirve para comprobar que todo converge.
    pub fn freeze(&mut self) {
        self.frozen = true;
        self.client.set_input([0, 0, 0]);
    }

    pub fn update(&mut self, now: u64) {
        if !self.frozen && now >= self.next_change {
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
