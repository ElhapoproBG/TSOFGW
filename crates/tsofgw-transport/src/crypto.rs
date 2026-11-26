//! Credenciales, derivación de claves de sesión y ventana anti-replay.
//!
//! Desde la 0.6.0 las claves de sesión son claves de **ChaCha20-Poly1305** (`tsofgw-aead`): los datos de
//! cada paquete van cifrados y autenticados. El handshake (HELLO, CHALLENGE, AUTH, ACCEPT) sigue yendo
//! en claro y autenticado con HMAC-SHA256: el número de cuenta es visible para quien escuche.
//!
//! Desde la 0.7.0 la sesión tiene **secreto hacia adelante**: cada lado genera una clave **efímera**
//! X25519 por intento de conexión, la manda en AUTH / ACCEPT (autenticada con la clave de la cuenta) y
//! las claves de sesión salen del secreto de Diffie-Hellman además de la clave de la cuenta. La clave
//! privada efímera se descarta al calcular el secreto, así que quien obtenga **más tarde** la clave de
//! la cuenta (o la maestra) no puede descifrar lo que capturó antes. Quien la tenga **durante** la
//! sesión y se interponga activamente sí puede suplantar: eso no lo evita este esquema.
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
pub const TAG_LEN: usize = tsofgw_aead::TAG_LEN;
pub const EPH_LEN: usize = tsofgw_aead::x25519::X25519_LEN;

/// Par de claves efímeras X25519 (la privada solo vive hasta calcular el secreto compartido).
pub(crate) struct Ephemeral {
    secret: [u8; EPH_LEN],
    pub public: [u8; EPH_LEN],
}

impl Ephemeral {
    pub fn generate(entropy: &mut dyn crate::entropy::Entropy) -> Self {
        let mut secret = [0u8; EPH_LEN];
        entropy.fill(&mut secret);
        let public = tsofgw_aead::x25519::public_key(&secret);
        Ephemeral { secret, public }
    }

    /// Secreto compartido con la pública del otro lado; `None` si el punto es de orden pequeño.
    pub fn agree(&self, peer: &[u8; EPH_LEN]) -> Option<[u8; 32]> {
        tsofgw_aead::x25519::shared_secret(&self.secret, peer)
    }
}

impl std::fmt::Debug for Ephemeral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ephemeral {{ <oculta> }}")
    }
}

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
    client_eph: &[u8; EPH_LEN],
) -> [u8; 32] {
    key.mac(&[
        b"tsofgw/auth/v2",
        &player_id.to_le_bytes(),
        nonce,
        cookie,
        client_eph,
    ])
}

pub(crate) fn accept_mac(
    key: &HmacKey,
    client_nonce: &[u8; NONCE_LEN],
    server_nonce: &[u8; NONCE_LEN],
    session_id: u32,
    client_eph: &[u8; EPH_LEN],
    server_eph: &[u8; EPH_LEN],
) -> [u8; 16] {
    let full = key.mac(&[
        b"tsofgw/accept/v2",
        client_nonce,
        server_nonce,
        &session_id.to_le_bytes(),
        client_eph,
        server_eph,
    ]);
    let mut out = [0u8; 16];
    out.copy_from_slice(&full[..16]);
    out
}

/// Claves AEAD de cada sentido de una sesión (cliente→servidor y servidor→cliente).
///
/// Cada sesión estrena claves (salen de los dos nonces y del identificador de sesión), y cada sentido
/// tiene la suya: por eso basta un contador creciente por sentido como nonce de ChaCha20-Poly1305,
/// sin repetir nunca una pareja (clave, nonce).
pub(crate) struct SessionKeys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionKeys {{ <ocultas> }}")
    }
}

/// Todo lo que entra en las claves de sesión: la clave de la cuenta, los dos nonces, el identificador, las
/// dos claves efímeras públicas y el secreto de Diffie-Hellman.
pub(crate) struct SessionInputs<'a> {
    pub client_nonce: &'a [u8; NONCE_LEN],
    pub server_nonce: &'a [u8; NONCE_LEN],
    pub session_id: u32,
    pub client_eph: &'a [u8; EPH_LEN],
    pub server_eph: &'a [u8; EPH_LEN],
    pub dh: &'a [u8; 32],
}

pub(crate) fn derive_session(key: &HmacKey, i: &SessionInputs<'_>) -> SessionKeys {
    let master = key.mac(&[
        b"tsofgw/session/v3",
        i.client_nonce,
        i.server_nonce,
        &i.session_id.to_le_bytes(),
        i.client_eph,
        i.server_eph,
        i.dh,
    ]);
    let master = HmacKey::new(&master);
    SessionKeys {
        c2s: master.mac(&[b"aead-c2s"]),
        s2c: master.mac(&[b"aead-s2c"]),
    }
}

/// Nonce de 96 bits de un paquete: 4 bytes a cero y el contador de 64 bits del paquete.
pub(crate) fn nonce_for(counter: u64) -> [u8; tsofgw_aead::NONCE_LEN] {
    let mut n = [0u8; tsofgw_aead::NONCE_LEN];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    n
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

    fn claves(
        k: &HmacKey,
        cn: [u8; 16],
        sn: [u8; 16],
        id: u32,
        ce: [u8; 32],
        se: [u8; 32],
        dh: [u8; 32],
    ) -> SessionKeys {
        derive_session(
            k,
            &SessionInputs {
                client_nonce: &cn,
                server_nonce: &sn,
                session_id: id,
                client_eph: &ce,
                server_eph: &se,
                dh: &dh,
            },
        )
    }

    #[test]
    fn direcciones_de_sesion_son_independientes() {
        let k = HmacKey::new(b"k");
        let (cn, sn, ce, se, dh) = ([1u8; 16], [2u8; 16], [4u8; 32], [5u8; 32], [6u8; 32]);
        let s = claves(&k, cn, sn, 7, ce, se, dh);
        assert_ne!(s.c2s, s.s2c);
        let s2 = claves(&k, cn, sn, 8, ce, se, dh);
        assert_ne!(s.c2s, s2.c2s);
        assert_ne!(s.s2c, s2.s2c);
        // Otros nonces o otra clave de cuenta: otras claves de sesión.
        let s3 = claves(&k, [3u8; 16], sn, 7, ce, se, dh);
        assert_ne!(s.c2s, s3.c2s);
        let s4 = claves(&HmacKey::new(b"k2"), cn, sn, 7, ce, se, dh);
        assert_ne!(s.c2s, s4.c2s);
        // Y son deterministas: las dos puntas llegan a lo mismo.
        let again = claves(&k, cn, sn, 7, ce, se, dh);
        assert_eq!((s.c2s, s.s2c), (again.c2s, again.s2c));
    }

    #[test]
    fn el_secreto_efimero_y_las_claves_efimeras_cambian_las_claves_de_sesion() {
        let k = HmacKey::new(b"k");
        let (cn, sn, ce, se, dh) = ([1u8; 16], [2u8; 16], [4u8; 32], [5u8; 32], [6u8; 32]);
        let base = claves(&k, cn, sn, 7, ce, se, dh);
        // Mismo todo salvo el secreto de DH: quien tiene la clave de la cuenta pero no el DH no llega.
        assert_ne!(base.c2s, claves(&k, cn, sn, 7, ce, se, [7u8; 32]).c2s);
        assert_ne!(base.s2c, claves(&k, cn, sn, 7, ce, se, [7u8; 32]).s2c);
        assert_ne!(base.c2s, claves(&k, cn, sn, 7, [8u8; 32], se, dh).c2s);
        assert_ne!(base.c2s, claves(&k, cn, sn, 7, ce, [8u8; 32], dh).c2s);
    }

    #[test]
    fn dos_efimeras_distintas_acuerdan_lo_mismo_y_se_rechaza_el_punto_pequeno() {
        use crate::entropy::DeterministicEntropy;
        let mut e = DeterministicEntropy(5);
        let (a, b) = (Ephemeral::generate(&mut e), Ephemeral::generate(&mut e));
        assert_ne!(a.public, b.public);
        assert_eq!(a.agree(&b.public), b.agree(&a.public));
        assert!(a.agree(&b.public).is_some());
        assert!(a.agree(&[0u8; 32]).is_none());
        assert!(!format!("{a:?}").contains("secret"));
    }
}
