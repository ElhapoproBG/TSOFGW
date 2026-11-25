//! Transporte de datagramas autenticado para TSOFGW.
//!
//! # Qué garantiza
//! * **Autenticación mutua** con una clave por cuenta ([`Authority`], [`Credentials`]): el servidor sabe
//!   qué cuenta es el cliente y el cliente sabe que habla con un servidor que conoce su clave.
//! * **Integridad y autenticidad por paquete** (HMAC-SHA256 truncado a 128 bits, una clave por sentido).
//! * **Anti-replay**: ventana de 128 contadores por sentido.
//! * **Resistencia a suplantación de origen**: HELLO → CHALLENGE con *cookie* atada a la dirección; el
//!   servidor no guarda nada hasta verificar AUTH, y ninguna respuesta es mayor que lo que la provoca.
//! * Fragmentación de mensajes hasta ~18 KB y reensamblado con límites de memoria.
//! * Sesiones con timeout, mantenimiento de la conexión y límite de paquetes por segundo.
//!
//! # Qué NO garantiza (todavía)
//! * **Confidencialidad**: los paquetes van en claro (autenticados, no cifrados). Falta un cifrado AEAD
//!   de una biblioteca auditada; no se implementa criptografía propia.
//! * **Secreto hacia adelante** ni credenciales asimétricas: la clave maestra de la autoridad permite
//!   suplantar cualquier cuenta. Es un esquema para desarrollo y redes privadas.
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
pub use crypto::{Authority, Credentials};
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
