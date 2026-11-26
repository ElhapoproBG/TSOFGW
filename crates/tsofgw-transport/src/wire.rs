//! Formato de los paquetes (little-endian).
//!
//! ```text
//! HELLO      0x01 | versión(1) | nonce_cliente(16) | relleno hasta 64 bytes
//! CHALLENGE  0x02 | cookie(16)
//! AUTH       0x03 | player_id(8) | nonce_cliente(16) | cookie(16) | prueba(32)
//! ACCEPT     0x04 | session_id(4) | nonce_servidor(16) | mac(16)
//! DATA       0x10 | session_id(4) | contador(8) | msg_id(2) idx(1) total(1) | carga CIFRADA | etiqueta(16)
//! DISCONNECT 0x11 | session_id(4) | contador(8) | etiqueta(16)
//! ```
//! DATA y DISCONNECT usan ChaCha20-Poly1305 (`tsofgw-aead`): la carga va cifrada y la cabecera completa
//! (tipo, sesión, contador, `msg_id`, `idx`, `total`) va como datos asociados, autenticada pero visible.
//! El nonce es el contador del paquete; cada sentido de cada sesión tiene su clave, así que no se repite
//! nunca una pareja (clave, nonce). Lo que un espía ve: que hay una sesión, cuántos paquetes manda, su
//! tamaño y su ritmo; no el contenido.
//! Ninguna respuesta del servidor antes de AUTH supera en tamaño a lo que la provocó, para que no sirva
//! de amplificador en ataques con dirección falsificada.

use crate::crypto::{nonce_for, COOKIE_LEN, NONCE_LEN, TAG_LEN};
use crate::frag::MAX_FRAGMENTS;
use tsofgw_aead::{open_in_place, seal_in_place};

/// Versión 2: DATA y DISCONNECT cifrados (0.6.0). Un cliente o servidor de la versión 1 no se entiende
/// con uno de la 2: el servidor ignora el HELLO de otra versión.
pub const PROTOCOL_VERSION: u8 = 2;
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
    key: &[u8; 32],
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
    let (aad, body) = p.split_at_mut(DATA_HEADER + FRAG_HEADER);
    let t = seal_in_place(key, &nonce_for(counter), aad, body);
    p.extend_from_slice(&t);
    p
}

pub fn build_disconnect(key: &[u8; 32], session: u32, counter: u64) -> Vec<u8> {
    let mut p = Vec::with_capacity(DATA_HEADER + TAG_LEN);
    p.push(DISCONNECT);
    p.extend_from_slice(&session.to_le_bytes());
    p.extend_from_slice(&counter.to_le_bytes());
    // Sin carga: solo la etiqueta sobre la cabecera.
    let t = seal_in_place(key, &nonce_for(counter), &p, &mut []);
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

/// Interpreta la estructura de un paquete DATA (todavía sin verificar su etiqueta). La `payload` que
/// devuelve está **cifrada**: la carga en claro sale de [`open_data`].
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

/// Verifica y descifra un paquete DATA. `None` si la etiqueta no vale (alterado, otra clave, otra sesión,
/// contador cambiado...). Nada sin autenticar sale de aquí.
pub fn open_data(key: &[u8; 32], packet: &[u8]) -> Option<Vec<u8>> {
    if packet.len() < DATA_HEADER + FRAG_HEADER + TAG_LEN || packet[0] != DATA {
        return None;
    }
    let counter = u64::from_le_bytes(array(&packet[5..13]));
    let (clear_and_body, t) = packet.split_at(packet.len() - TAG_LEN);
    let (aad, body) = clear_and_body.split_at(DATA_HEADER + FRAG_HEADER);
    let mut buf = body.to_vec();
    open_in_place(key, &nonce_for(counter), aad, &mut buf, t).then_some(buf)
}

/// Comprueba la etiqueta de un paquete DISCONNECT.
pub fn verify_disconnect(key: &[u8; 32], packet: &[u8]) -> bool {
    if packet.len() != DATA_HEADER + TAG_LEN || packet[0] != DISCONNECT {
        return false;
    }
    let counter = u64::from_le_bytes(array(&packet[5..13]));
    let (aad, t) = packet.split_at(DATA_HEADER);
    open_in_place(key, &nonce_for(counter), aad, &mut [], t)
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

#[cfg(test)]
mod tests {
    use super::*;

    const K: [u8; 32] = [0x42; 32];

    #[test]
    fn data_ida_y_vuelta_y_la_carga_va_cifrada() {
        let msg = b"posicion=(10,20,30) vida=87 inventario=[espada]";
        let p = build_data(&K, 7, 99, 3, 0, 1, msg);
        assert_eq!(p.len(), DATA_HEADER + FRAG_HEADER + msg.len() + TAG_LEN);
        let v = parse_data(&p).unwrap();
        assert_eq!(
            (v.session, v.counter, v.msg_id, v.idx, v.count),
            (7, 99, 3, 0, 1)
        );
        assert_ne!(v.payload, &msg[..], "la carga del paquete no está en claro");
        assert!(!p.windows(8).any(|w| msg.windows(8).any(|m| m == w)));
        assert_eq!(open_data(&K, &p).as_deref(), Some(&msg[..]));
        assert_eq!(open_data(&[0x43; 32], &p), None, "otra clave");
    }

    #[test]
    fn cualquier_bit_alterado_de_data_se_rechaza() {
        for len in [0usize, 1, 16, 100] {
            let p = build_data(&K, 7, 99, 3, 1, 2, &vec![0xA5; len]);
            for i in 0..p.len() {
                for bit in 0..8 {
                    let mut m = p.clone();
                    m[i] ^= 1 << bit;
                    assert!(open_data(&K, &m).is_none(), "len {len} byte {i} bit {bit}");
                }
            }
            assert!(open_data(&K, &p[..p.len() - 1]).is_none());
            let mut largo = p.clone();
            largo.push(0);
            assert!(open_data(&K, &largo).is_none());
        }
        assert!(open_data(&K, &[]).is_none());
        assert!(open_data(&K, &[DATA; 20]).is_none());
    }

    #[test]
    fn disconnect_ida_y_vuelta_y_cualquier_bit_alterado_se_rechaza() {
        let p = build_disconnect(&K, 7, 123);
        assert_eq!(p.len(), DATA_HEADER + TAG_LEN);
        assert_eq!(parse_disconnect(&p), Some((7, 123)));
        assert!(verify_disconnect(&K, &p));
        assert!(!verify_disconnect(&[0x43; 32], &p));
        for i in 0..p.len() {
            for bit in 0..8 {
                let mut m = p.clone();
                m[i] ^= 1 << bit;
                assert!(!verify_disconnect(&K, &m), "byte {i} bit {bit}");
            }
        }
        // Un DISCONNECT no se hace pasar por DATA ni al revés (cambiar el tipo no basta).
        let mut como_data = p.clone();
        como_data[0] = DATA;
        assert!(open_data(&K, &como_data).is_none());
        let d = build_data(&K, 7, 123, 0, 0, 1, &[]);
        let mut como_disc = d.clone();
        como_disc[0] = DISCONNECT;
        assert!(!verify_disconnect(&K, &como_disc));
    }

    #[test]
    fn cada_contador_da_otro_nonce_y_otro_cifrado() {
        let carga = vec![0u8; 64];
        let mut vistos = std::collections::HashSet::new();
        for c in 0..2000u64 {
            let p = build_data(&K, 1, c, 0, 0, 1, &carga);
            assert!(
                vistos.insert(p[DATA_HEADER + FRAG_HEADER..].to_vec()),
                "{c}"
            );
        }
    }

    #[test]
    fn el_contador_y_la_sesion_estan_autenticados() {
        let p = build_data(&K, 7, 10, 0, 0, 1, b"x");
        // Mover el paquete a otra sesión o a otro contador (para esquivar el anti-replay) lo invalida.
        let mut otra = p.clone();
        otra[1..5].copy_from_slice(&8u32.to_le_bytes());
        assert!(open_data(&K, &otra).is_none());
        let mut otro_c = p.clone();
        otro_c[5..13].copy_from_slice(&11u64.to_le_bytes());
        assert!(open_data(&K, &otro_c).is_none());
    }
}
