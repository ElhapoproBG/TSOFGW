//! Credenciales, derivación de claves de sesión y ventana anti-replay.
//!
//! Esquema (clave compartida por cuenta, **para desarrollo y redes privadas**):
//! * La *autoridad* (servicio de cuentas) tiene una clave maestra y emite a cada cuenta una clave
//!   `HMAC(maestra, "tsofgw/player-key/v1" ‖ id)`. El servidor de juego deriva la misma clave al vuelo,
//!   así que no necesita una base de datos de claves.
//! * Quien conoce la clave maestra puede suplantar a cualquier cuenta. Una versión pública necesita
//!   credenciales asimétricas (Ed25519) emitidas por la autoridad: está en la lista de pendientes.

use tsofgw_pack::HmacKey;

pub const NONCE_LEN: usize = 16;
pub const COOKIE_LEN: usize = 16;
pub const TAG_LEN: usize = 16;

/// Clave de una cuenta. Su `Debug` no muestra la clave.
#[derive(Clone)]
pub struct Credentials {
    pub player_id: u64,
    key: [u8; 32],
}

impl Credentials {
    pub fn new(player_id: u64, key: [u8; 32]) -> Self {
        Credentials { player_id, key }
    }
    pub fn key_bytes(&self) -> &[u8; 32] {
        &self.key
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Credentials {{ player_id: {}, key: <oculta> }}",
            self.player_id
        )
    }
}

/// Emite y verifica credenciales a partir de una clave maestra.
#[derive(Clone)]
pub struct Authority {
    master: HmacKey,
}

impl Authority {
    pub fn new(master_key: &[u8]) -> Self {
        Authority {
            master: HmacKey::new(master_key),
        }
    }

    fn player_key(&self, player_id: u64) -> [u8; 32] {
        self.master
            .mac(&[b"tsofgw/player-key/v1", &player_id.to_le_bytes()])
    }

    pub fn issue(&self, player_id: u64) -> Credentials {
        Credentials::new(player_id, self.player_key(player_id))
    }

    pub(crate) fn player_hmac(&self, player_id: u64) -> HmacKey {
        HmacKey::new(&self.player_key(player_id))
    }
}

pub(crate) fn auth_proof(
    key: &HmacKey,
    player_id: u64,
    nonce: &[u8; NONCE_LEN],
    cookie: &[u8; COOKIE_LEN],
) -> [u8; 32] {
    key.mac(&[b"tsofgw/auth/v1", &player_id.to_le_bytes(), nonce, cookie])
}

pub(crate) fn accept_mac(
    key: &HmacKey,
    client_nonce: &[u8; NONCE_LEN],
    server_nonce: &[u8; NONCE_LEN],
    session_id: u32,
) -> [u8; 16] {
    let full = key.mac(&[
        b"tsofgw/accept/v1",
        client_nonce,
        server_nonce,
        &session_id.to_le_bytes(),
    ]);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

/// Claves de cada sentido de una sesión.
pub(crate) struct SessionKeys {
    pub c2s: HmacKey,
    pub s2c: HmacKey,
}

pub(crate) fn derive_session(
    key: &HmacKey,
    client_nonce: &[u8; NONCE_LEN],
    server_nonce: &[u8; NONCE_LEN],
    session_id: u32,
) -> SessionKeys {
    let master = key.mac(&[
        b"tsofgw/session/v1",
        client_nonce,
        server_nonce,
        &session_id.to_le_bytes(),
    ]);
    let master = HmacKey::new(&master);
    SessionKeys {
        c2s: HmacKey::new(&master.mac(&[b"c2s"])),
        s2c: HmacKey::new(&master.mac(&[b"s2c"])),
    }
}

pub(crate) fn tag(key: &HmacKey, signed: &[u8]) -> [u8; TAG_LEN] {
    let full = key.mac(&[signed]);
    let mut out = [0u8; TAG_LEN];
    out.copy_from_slice(&full[..TAG_LEN]);
    out
}

/// Ventana de 128 contadores: rechaza repetidos y los demasiado viejos.
#[derive(Default, Debug, Clone)]
pub struct ReplayWindow {
    max: u64,
    bits: u128,
    any: bool,
}

impl ReplayWindow {
    /// `true` si el contador es nuevo (y lo registra). Llamar solo con paquetes ya autenticados.
    pub fn accept(&mut self, counter: u64) -> bool {
        if !self.any {
            self.any = true;
            self.max = counter;
            self.bits = 1;
            return true;
        }
        if counter > self.max {
            let shift = counter - self.max;
            self.bits = if shift >= 128 { 0 } else { self.bits << shift };
            self.bits |= 1;
            self.max = counter;
            true
        } else {
            let diff = self.max - counter;
            if diff >= 128 || (self.bits >> diff) & 1 == 1 {
                false
            } else {
                self.bits |= 1u128 << diff;
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_rechaza_repetidos_y_viejos() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(10));
        assert!(!w.accept(10));
        assert!(w.accept(9)); // llegó tarde pero es nuevo
        assert!(!w.accept(9));
        assert!(w.accept(11));
        assert!(w.accept(5));
        assert!(!w.accept(5));
        // Salto grande: lo anterior fuera de la ventana queda rechazado.
        assert!(w.accept(10 + 128));
        assert!(!w.accept(10));
        assert!(!w.accept(10 + 128 - 128)); // diff = 128: fuera
        assert!(!w.accept(10 + 128 - 127)); // el 11 ya se había visto: sigue rechazado dentro de la ventana
        assert!(w.accept(10 + 128 - 126)); // diff = 126: dentro y no visto
        assert!(!w.accept(10 + 128 - 126));
    }

    #[test]
    fn replay_salto_enorme_no_desborda() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(0));
        assert!(w.accept(u64::MAX));
        assert!(!w.accept(0));
        assert!(!w.accept(u64::MAX));
        assert!(w.accept(u64::MAX - 1));
    }

    #[test]
    fn claves_de_cuenta_son_estables_y_distintas() {
        let a = Authority::new(b"maestra");
        assert_eq!(a.issue(1).key_bytes(), a.issue(1).key_bytes());
        assert_ne!(a.issue(1).key_bytes(), a.issue(2).key_bytes());
        assert_ne!(
            a.issue(1).key_bytes(),
            Authority::new(b"otra").issue(1).key_bytes()
        );
        assert!(!format!("{:?}", a.issue(1)).contains("key_bytes"));
    }

    #[test]
    fn direcciones_de_sesion_son_independientes() {
        let k = HmacKey::new(b"k");
        let (cn, sn) = ([1u8; 16], [2u8; 16]);
        let s = derive_session(&k, &cn, &sn, 7);
        assert_ne!(tag(&s.c2s, b"x"), tag(&s.s2c, b"x"));
        let s2 = derive_session(&k, &cn, &sn, 8);
        assert_ne!(tag(&s.c2s, b"x"), tag(&s2.c2s, b"x"));
    }
}
