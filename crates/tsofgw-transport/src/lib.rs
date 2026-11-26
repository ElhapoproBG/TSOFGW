//! Transporte de datagramas autenticado para TSOFGW.
//!
//! # Qué garantiza
//! * **Autenticación mutua** con una clave por cuenta ([`Authority`], [`Credentials`]): el servidor sabe
//!   qué cuenta es el cliente y el cliente sabe que habla con un servidor que conoce su clave.
//! * **Confidencialidad, integridad y autenticidad por paquete** (desde la 0.6, ChaCha20-Poly1305 de
//!   `tsofgw-aead`, una clave por sentido y sesión; la cabecera va autenticada y la carga cifrada).
//! * **Secreto hacia adelante** (desde la 0.7, X25519 efímero en AUTH/ACCEPT): las claves de sesión dependen de
//!   un secreto de Diffie-Hellman cuya parte privada se descarta; una clave de cuenta filtrada más tarde no
//!   descifra lo capturado antes.
//! * **Anti-replay**: ventana de 128 contadores por sentido.
//! * **Resistencia a suplantación de origen**: HELLO → CHALLENGE con *cookie* atada a la dirección; el
//!   servidor no guarda nada hasta verificar AUTH, y ninguna respuesta es mayor que lo que la provoca.
//! * Fragmentación de mensajes hasta ~18 KB y reensamblado con límites de memoria.
//! * Sesiones con timeout, mantenimiento de la conexión y límite de paquetes por segundo.
//!
//! # Qué NO garantiza (todavía)
//! * **Una implementación de cifrado auditada**: `tsofgw-aead` es código propio, comprobado contra los
//!   vectores del RFC 8439 y contra OpenSSL, pero nadie lo ha auditado (documento 11).
//! * **Credenciales asimétricas**: la clave maestra de la autoridad permite suplantar cualquier cuenta, y quien
//!   tenga la clave de una cuenta puede hacerse pasar por ella en sesiones nuevas. El **secreto hacia adelante**
//!   (0.7.0, X25519 efímero) protege lo ya capturado, no el presente. Es un esquema para desarrollo y redes
//!   privadas.
//! * **Ocultar metadatos**: el handshake va en claro (el número de cuenta se ve), y el tamaño y el ritmo
//!   de los paquetes también.
//! * **Migración de dirección** (cambio de IP/puerto): el cliente debe reconectar.
//! * **Entrega fiable ni orden**: es un datagrama con garantías de autenticidad, como UDP.
//! * Un tercero que *falsifique la dirección de origen de un jugador* y repita su AUTH mientras la cookie
//!   siga vigente solo consigue reservar una sesión pendiente que caduca; no puede expulsar al jugador.
//!
//! El tiempo (`now`) es un contador de milisegundos que aporta quien llama, así que todo es determinista
//! y se puede probar con la red simulada ([`SimNet`]).

mod client;
mod crypto;
mod entropy;
mod frag;
mod link;
mod server;
pub mod wire;

pub use client::{ClientConfig, ClientEvent, SecureClient};
pub use crypto::{Authority, Credentials, ReplayWindow};
pub use entropy::{DeterministicEntropy, Entropy, OsEntropy};
pub use link::{Link, PeerAddr, SimAddr, SimConfig, SimLink, SimNet, SimStats, UdpLink};
pub use server::{SecureServer, ServerConfig, ServerEvent};
pub use wire::{SendError, MAX_MESSAGE};

pub type SessionId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// No llegó nada durante el plazo permitido.
    Timeout,
    /// El otro extremo cerró la conexión.
    Requested,
    /// La misma cuenta se conectó de nuevo.
    Replaced,
    /// El servidor expulsó al cliente.
    Kicked,
    /// El handshake no se completó a tiempo.
    ConnectFailed,
}

/// Contadores de lo que vio el transporte, útiles para diagnóstico y para pruebas.
#[derive(Debug, Clone, Default)]
pub struct TransportStats {
    pub packets_in: u64,
    pub packets_out: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub challenges: u64,
    pub auth_ok: u64,
    pub confirmed: u64,
    pub bad_cookie: u64,
    pub bad_auth: u64,
    pub bad_tag: u64,
    pub replays: u64,
    pub malformed: u64,
    pub unknown_session: u64,
    pub addr_mismatch: u64,
    pub rate_limited: u64,
    pub refused: u64,
    pub timeouts: u64,
    pub handshake_timeouts: u64,
}
