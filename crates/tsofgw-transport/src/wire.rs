//! Formato de los paquetes (little-endian).
//!
//! ```text
//! HELLO      0x01 | versión(1) | nonce_cliente(16) | relleno hasta 64 bytes
//! CHALLENGE  0x02 | cookie(16)
//! AUTH       0x03 | player_id(8) | nonce_cliente(16) | cookie(16) | prueba(32)
//! ACCEPT     0x04 | session_id(4) | nonce_servidor(16) | mac(16)
//! DATA       0x10 | session_id(4) | contador(8) | msg_id(2) idx(1) total(1) | carga | etiqueta(16)
//! DISCONNECT 0x11 | session_id(4) | contador(8) | etiqueta(16)
//! ```
//! Ninguna respuesta del servidor antes de AUTH supera en tamaño a lo que la provocó, para que no sirva
//! de amplificador en ataques con dirección falsificada.

use crate::crypto::{tag, COOKIE_LEN, NONCE_LEN, TAG_LEN};
use crate::frag::MAX_FRAGMENTS;
use tsofgw_pack::{ct_eq, HmacKey};

pub const PROTOCOL_VERSION: u8 = 1;
pub const HELLO: u8 = 0x01;
pub const CHALLENGE: u8 = 0x02;
pub const AUTH: u8 = 0x03;
pub const ACCEPT: u8 = 0x04;
pub const DATA: u8 = 0x10;
pub const DISCONNECT: u8 = 0x11;

/// Tamaño máximo de un datagrama que enviamos (cabe en el MTU habitual de 1280 de IPv6 con margen).
pub const MAX_DATAGRAM: usize = 1200;
pub const HELLO_LEN: usize = 64;
pub const AUTH_LEN: usize = 1 + 8 + NONCE_LEN + COOKIE_LEN + 32;
pub const ACCEPT_LEN: usize = 1 + 4 + NONCE_LEN + 16;
pub const DATA_HEADER: usize = 1 + 4 + 8;
pub const FRAG_HEADER: usize = 4;
pub const MAX_FRAGMENT_PAYLOAD: usize = MAX_DATAGRAM - DATA_HEADER - FRAG_HEADER - TAG_LEN;
/// Mensaje más grande que se puede enviar (16 fragmentos).
pub const MAX_MESSAGE: usize = MAX_FRAGMENT_PAYLOAD * MAX_FRAGMENTS;
pub const RX_BUFFER: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    TooLarge,
    UnknownSession,
    NotConnected,
}

pub fn hello(nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut p = vec![0u8; HELLO_LEN];
    p[0] = HELLO;
    p[1] = PROTOCOL_VERSION;
    p[2..2 + NONCE_LEN].copy_from_slice(nonce);
    p
}

pub fn challenge(cookie: &[u8; COOKIE_LEN]) -> Vec<u8> {
    let mut p = vec![CHALLENGE];
    p.extend_from_slice(cookie);
    p
}

pub fn auth(
    player_id: u64,
    nonce: &[u8; NONCE_LEN],
    cookie: &[u8; COOKIE_LEN],
    proof: &[u8; 32],
) -> Vec<u8> {
    let mut p = Vec::with_capacity(AUTH_LEN);
    p.push(AUTH);
    p.extend_from_slice(&player_id.to_le_bytes());
    p.extend_from_slice(nonce);
    p.extend_from_slice(cookie);
    p.extend_from_slice(proof);
    p
}

pub fn accept(session: u32, server_nonce: &[u8; NONCE_LEN], mac: &[u8; 16]) -> Vec<u8> {
    let mut p = Vec::with_capacity(ACCEPT_LEN);
    p.push(ACCEPT);
    p.extend_from_slice(&session.to_le_bytes());
    p.extend_from_slice(server_nonce);
    p.extend_from_slice(mac);
    p
}

pub fn array<const N: usize>(b: &[u8]) -> [u8; N] {
    let mut a = [0u8; N];
    a.copy_from_slice(&b[..N]);
    a
}

pub fn build_data(
    key: &HmacKey,
    session: u32,
    counter: u64,
    msg_id: u16,
    idx: u8,
    count: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut p = Vec::with_capacity(DATA_HEADER + FRAG_HEADER + payload.len() + TAG_LEN);
    p.push(DATA);
    p.extend_from_slice(&session.to_le_bytes());
    p.extend_from_slice(&counter.to_le_bytes());
    p.extend_from_slice(&msg_id.to_le_bytes());
    p.push(idx);
    p.push(count);
    p.extend_from_slice(payload);
    let t = tag(key, &p);
    p.extend_from_slice(&t);
    p
}

pub fn build_disconnect(key: &HmacKey, session: u32, counter: u64) -> Vec<u8> {
    let mut p = Vec::with_capacity(DATA_HEADER + TAG_LEN);
    p.push(DISCONNECT);
    p.extend_from_slice(&session.to_le_bytes());
    p.extend_from_slice(&counter.to_le_bytes());
    let t = tag(key, &p);
    p.extend_from_slice(&t);
    p
}

/// Parte un mensaje en trozos que caben en un datagrama. Un mensaje vacío es un único trozo vacío.
pub fn split(msg: &[u8]) -> Result<Vec<&[u8]>, SendError> {
    if msg.len() > MAX_MESSAGE {
        return Err(SendError::TooLarge);
    }
    if msg.is_empty() {
        return Ok(vec![msg]);
    }
    Ok(msg.chunks(MAX_FRAGMENT_PAYLOAD).collect())
}

pub struct DataView<'a> {
    pub session: u32,
    pub counter: u64,
    pub msg_id: u16,
    pub idx: u8,
    pub count: u8,
    pub payload: &'a [u8],
}

/// Interpreta la estructura de un paquete DATA (todavía sin verificar su etiqueta).
pub fn parse_data(p: &[u8]) -> Option<DataView<'_>> {
    if p.len() < DATA_HEADER + FRAG_HEADER + TAG_LEN || p[0] != DATA {
        return None;
    }
    Some(DataView {
        session: u32::from_le_bytes(array(&p[1..5])),
        counter: u64::from_le_bytes(array(&p[5..13])),
        msg_id: u16::from_le_bytes(array(&p[13..15])),
        idx: p[15],
        count: p[16],
        payload: &p[DATA_HEADER + FRAG_HEADER..p.len() - TAG_LEN],
    })
}

/// Comprueba la etiqueta de un paquete DATA o DISCONNECT.
pub fn verify_tag(key: &HmacKey, packet: &[u8]) -> bool {
    if packet.len() < TAG_LEN {
        return false;
    }
    let (signed, t) = packet.split_at(packet.len() - TAG_LEN);
    ct_eq(&tag(key, signed), t)
}

/// (session, contador) de un DISCONNECT bien formado.
pub fn parse_disconnect(p: &[u8]) -> Option<(u32, u64)> {
    if p.len() != DATA_HEADER + TAG_LEN || p[0] != DISCONNECT {
        return None;
    }
    Some((
        u32::from_le_bytes(array(&p[1..5])),
        u64::from_le_bytes(array(&p[5..13])),
    ))
}
