//! Mensajes de juego. Cada uno lleva la cabecera `seq/ack` de `tsofgw-net` seguida de un byte de tipo.
//!
//! ```text
//! JOIN      (C→S) 1 | versión(1)
//! WELCOME   (S→C) 2 | entidad(8) | tick(4) | tick_ms(2) | posición(3×i32)
//! INPUT     (C→S) 3 | primera_seq(4) | n(1) | n × dirección(3×i8)
//! SNAP_FULL (S→C) 4 | último_input(4) | snapshot completo
//! SNAP_DELTA(S→C) 5 | último_input(4) | snapshot delta contra una línea base confirmada
//! ```

use tsofgw_net::Header;

use crate::sim::Input;

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_INPUTS_PER_MESSAGE: usize = 8;

pub const JOIN: u8 = 1;
pub const WELCOME: u8 = 2;
pub const INPUT: u8 = 3;
pub const SNAP_FULL: u8 = 4;
pub const SNAP_DELTA: u8 = 5;

pub fn frame(header: Header, kind: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(Header::SIZE + 1 + body.len());
    v.extend_from_slice(&header.encode());
    v.push(kind);
    v.extend_from_slice(body);
    v
}

pub fn unframe(msg: &[u8]) -> Option<(Header, u8, &[u8])> {
    let header = Header::decode(msg).ok()?;
    let kind = *msg.get(Header::SIZE)?;
    Some((header, kind, &msg[Header::SIZE + 1..]))
}

pub fn join_body() -> Vec<u8> {
    vec![PROTOCOL_VERSION]
}

pub fn parse_join(body: &[u8]) -> Option<u8> {
    (body.len() == 1).then(|| body[0])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Welcome {
    pub entity: u64,
    pub tick: u32,
    pub tick_ms: u16,
    pub pos: [i32; 3],
}

pub fn welcome_body(w: &Welcome) -> Vec<u8> {
    let mut v = Vec::with_capacity(26);
    v.extend_from_slice(&w.entity.to_le_bytes());
    v.extend_from_slice(&w.tick.to_le_bytes());
    v.extend_from_slice(&w.tick_ms.to_le_bytes());
    for c in w.pos {
        v.extend_from_slice(&c.to_le_bytes());
    }
    v
}

pub fn parse_welcome(b: &[u8]) -> Option<Welcome> {
    if b.len() != 26 {
        return None;
    }
    let i32_at = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    Some(Welcome {
        entity: u64::from_le_bytes(b[0..8].try_into().ok()?),
        tick: u32::from_le_bytes(b[8..12].try_into().ok()?),
        tick_ms: u16::from_le_bytes([b[12], b[13]]),
        pos: [i32_at(14), i32_at(18), i32_at(22)],
    })
}

pub fn input_body(first_seq: u32, inputs: &[Input]) -> Vec<u8> {
    debug_assert!(!inputs.is_empty() && inputs.len() <= MAX_INPUTS_PER_MESSAGE);
    let mut v = Vec::with_capacity(5 + 3 * inputs.len());
    v.extend_from_slice(&first_seq.to_le_bytes());
    v.push(inputs.len() as u8);
    for i in inputs {
        v.extend(i.dir.iter().map(|&d| d as u8));
    }
    v
}

/// `(primera_seq, inputs)`; `None` si el cuerpo no es exactamente lo que debe ser.
pub fn parse_input(b: &[u8]) -> Option<(u32, Vec<Input>)> {
    if b.len() < 5 {
        return None;
    }
    let first = u32::from_le_bytes(b[0..4].try_into().ok()?);
    let n = b[4] as usize;
    if first == 0 || n == 0 || n > MAX_INPUTS_PER_MESSAGE || b.len() != 5 + 3 * n {
        return None;
    }
    let inputs = b[5..]
        .chunks_exact(3)
        .map(|c| Input {
            dir: [c[0] as i8, c[1] as i8, c[2] as i8],
        })
        .collect();
    Some((first, inputs))
}

pub fn snapshot_body(last_input: u32, encoded: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + encoded.len());
    v.extend_from_slice(&last_input.to_le_bytes());
    v.extend_from_slice(encoded);
    v
}

pub fn parse_snapshot(b: &[u8]) -> Option<(u32, &[u8])> {
    if b.len() < 4 {
        return None;
    }
    Some((u32::from_le_bytes(b[0..4].try_into().ok()?), &b[4..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_y_vuelta() {
        let h = Header {
            seq: 5,
            ack: 3,
            ack_bits: 0xff,
        };
        let m = frame(h, INPUT, &[1, 2, 3]);
        assert_eq!(unframe(&m), Some((h, INPUT, &[1u8, 2, 3][..])));
        assert_eq!(unframe(&m[..8]), None);

        let w = Welcome {
            entity: 77,
            tick: 1234,
            tick_ms: 50,
            pos: [-5, 6, 70000],
        };
        assert_eq!(parse_welcome(&welcome_body(&w)), Some(w));
        assert_eq!(parse_welcome(&welcome_body(&w)[1..]), None);

        let inputs = [
            Input { dir: [1, -2, 3] },
            Input {
                dir: [-128, 127, 0],
            },
        ];
        assert_eq!(
            parse_input(&input_body(9, &inputs)),
            Some((9, inputs.to_vec()))
        );
        assert_eq!(
            parse_snapshot(&snapshot_body(4, b"xy")),
            Some((4, &b"xy"[..]))
        );
        assert_eq!(parse_join(&join_body()), Some(PROTOCOL_VERSION));
    }

    #[test]
    fn rechaza_inputs_mal_formados() {
        assert_eq!(parse_input(&[]), None);
        assert_eq!(parse_input(&[0, 0, 0, 0, 1, 1, 1, 1]), None, "seq 0");
        assert_eq!(parse_input(&[1, 0, 0, 0, 0]), None, "cero inputs");
        assert_eq!(parse_input(&[1, 0, 0, 0, 1, 1, 1]), None, "truncado");
        assert_eq!(parse_input(&[1, 0, 0, 0, 1, 1, 1, 1, 9]), None, "sobra");
        let mut big = vec![1, 0, 0, 0, 200];
        big.extend(std::iter::repeat_n(0u8, 600));
        assert_eq!(parse_input(&big), None, "demasiados");
    }
}
