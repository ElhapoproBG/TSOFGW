//! Cifrado autenticado **ChaCha20-Poly1305** (RFC 8439) y, en [`x25519`], acuerdo de claves **X25519**
//! (RFC 7748), solo con la biblioteca estándar.
//!
//! # Por qué existe y qué NO es
//! El transporte de jugadores y el bus entre shards necesitan confidencialidad, y en esta fase no hay
//! forma de traer una biblioteca de criptografía auditada (el proyecto no tiene dependencias externas).
//! Este módulo implementa el estándar y se comprueba de tres formas:
//! * contra los vectores de prueba del propio RFC 8439 (§2.3.2, §2.4.2, §2.5.2, §2.6.2, §2.8.2);
//! * contra una implementación independiente (OpenSSL, a través de la biblioteca `cryptography` de
//!   Python) con vectores generados aparte, que cubren longitudes en los bordes de bloque y mensajes
//!   hechos para forzar las reducciones de Poly1305;
//! * con pruebas de propiedades (cualquier bit alterado en cifrado, datos asociados, nonce, clave o
//!   etiqueta se rechaza; ida y vuelta para todas las longitudes pequeñas).
//!
//! **Nadie ha auditado este código.** Una implementación propia de criptografía es un riesgo aunque pase
//! todos los vectores: no se ha medido que sea de tiempo constante en cada CPU ni que no deje secretos
//! en memoria (no se borra la clave al soltarla: hacerlo de forma fiable pide `unsafe` o una dependencia).
//! La interfaz está aislada en este crate para poder **sustituirlo por una biblioteca auditada** sin
//! tocar el resto del motor. Hasta entonces, ver `SECURITY.md`.
//!
//! # Uso correcto
//! Una pareja (clave, nonce) **no puede usarse nunca dos veces**: repetirla revela el XOR de los textos
//! y permite falsificar mensajes. Quien llama garantiza la unicidad (en TSOFGW, un contador por sentido y
//! sesión; en el bus, una clave por emisor y un nonce con sal aleatoria por arranque).

#![forbid(unsafe_code)]

pub mod x25519;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

// ------------------------------------------------------------------------------------------------
// ChaCha20 (RFC 8439 §2.1–2.4)
// ------------------------------------------------------------------------------------------------

#[inline(always)]
fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Un bloque de 64 bytes del flujo de ChaCha20.
pub fn chacha20_block(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; NONCE_LEN]) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[0] = 0x6170_7865;
    init[1] = 0x3320_646e;
    init[2] = 0x7962_2d32;
    init[3] = 0x6b20_6574;
    for i in 0..8 {
        init[4 + i] = le32(&key[4 * i..]);
    }
    init[12] = counter;
    for i in 0..3 {
        init[13 + i] = le32(&nonce[4 * i..]);
    }
    let mut s = init;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

/// Cifra o descifra `data` con ChaCha20 empezando en el contador de bloque `counter`.
///
/// Panics si el flujo necesitaría más de 2^32 bloques (más de 256 GiB): el contador de 32 bits se
/// repetiría y con él el flujo de claves. No ocurre con datagramas de 1200 bytes.
pub fn chacha20_xor(key: &[u8; KEY_LEN], counter: u32, nonce: &[u8; NONCE_LEN], data: &mut [u8]) {
    let mut ctr = counter;
    for chunk in data.chunks_mut(64) {
        let ks = chacha20_block(key, ctr, nonce);
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= k;
        }
        ctr = ctr
            .checked_add(1)
            .expect("ChaCha20: contador de bloque agotado");
    }
}

// ------------------------------------------------------------------------------------------------
// Poly1305 (RFC 8439 §2.5), con limbs de 26 bits
// ------------------------------------------------------------------------------------------------

const M26: u32 = 0x3ff_ffff;

/// Autenticador de un solo uso: la clave de 32 bytes **no puede reutilizarse** para otro mensaje.
pub struct Poly1305 {
    r: [u32; 5],
    h: [u32; 5],
    pad: [u32; 4],
    buf: [u8; 16],
    len: usize,
}

impl Poly1305 {
    pub fn new(key: &[u8; 32]) -> Poly1305 {
        // `r` con los bits recortados que exige el estándar, repartido en 5 limbs de 26 bits.
        let r = [
            le32(&key[0..]) & 0x3ff_ffff,
            (le32(&key[3..]) >> 2) & 0x3ff_ff03,
            (le32(&key[6..]) >> 4) & 0x3ff_c0ff,
            (le32(&key[9..]) >> 6) & 0x3f0_3fff,
            (le32(&key[12..]) >> 8) & 0x00f_ffff,
        ];
        let pad = [
            le32(&key[16..]),
            le32(&key[20..]),
            le32(&key[24..]),
            le32(&key[28..]),
        ];
        Poly1305 {
            r,
            h: [0; 5],
            pad,
            buf: [0; 16],
            len: 0,
        }
    }

    /// Procesa un bloque de 16 bytes; `hibit` es `1 << 24` en los bloques completos y 0 en el último
    /// bloque parcial (que ya lleva su byte 0x01).
    fn block(&mut self, m: &[u8; 16], hibit: u32) {
        let [r0, r1, r2, r3, r4] = self.r;
        let (s1, s2, s3, s4) = (r1 * 5, r2 * 5, r3 * 5, r4 * 5);
        let [mut h0, mut h1, mut h2, mut h3, mut h4] = self.h;

        h0 += le32(&m[0..]) & M26;
        h1 += (le32(&m[3..]) >> 2) & M26;
        h2 += (le32(&m[6..]) >> 4) & M26;
        h3 += (le32(&m[9..]) >> 6) & M26;
        h4 += (le32(&m[12..]) >> 8) | hibit;

        let (h0w, h1w, h2w, h3w, h4w) = (h0 as u64, h1 as u64, h2 as u64, h3 as u64, h4 as u64);
        let (r0w, r1w, r2w, r3w, r4w) = (r0 as u64, r1 as u64, r2 as u64, r3 as u64, r4 as u64);
        let (s1w, s2w, s3w, s4w) = (s1 as u64, s2 as u64, s3 as u64, s4 as u64);

        let d0 = h0w * r0w + h1w * s4w + h2w * s3w + h3w * s2w + h4w * s1w;
        let mut d1 = h0w * r1w + h1w * r0w + h2w * s4w + h3w * s3w + h4w * s2w;
        let mut d2 = h0w * r2w + h1w * r1w + h2w * r0w + h3w * s4w + h4w * s3w;
        let mut d3 = h0w * r3w + h1w * r2w + h2w * r1w + h3w * r0w + h4w * s4w;
        let mut d4 = h0w * r4w + h1w * r3w + h2w * r2w + h3w * r1w + h4w * r0w;

        let mut c = d0 >> 26;
        h0 = (d0 as u32) & M26;
        d1 += c;
        c = d1 >> 26;
        h1 = (d1 as u32) & M26;
        d2 += c;
        c = d2 >> 26;
        h2 = (d2 as u32) & M26;
        d3 += c;
        c = d3 >> 26;
        h3 = (d3 as u32) & M26;
        d4 += c;
        c = d4 >> 26;
        h4 = (d4 as u32) & M26;
        // 2^130 ≡ 5 (mod 2^130 - 5): el acarreo de arriba vuelve al limb 0 multiplicado por 5.
        h0 += (c as u32) * 5;
        let c2 = h0 >> 26;
        h0 &= M26;
        h1 += c2;
        self.h = [h0, h1, h2, h3, h4];
    }

    pub fn update(&mut self, mut data: &[u8]) {
        if self.len > 0 {
            let want = 16 - self.len;
            let take = want.min(data.len());
            self.buf[self.len..self.len + take].copy_from_slice(&data[..take]);
            self.len += take;
            data = &data[take..];
            if self.len < 16 {
                return;
            }
            let b = self.buf;
            self.block(&b, 1 << 24);
            self.len = 0;
        }
        while data.len() >= 16 {
            let mut b = [0u8; 16];
            b.copy_from_slice(&data[..16]);
            self.block(&b, 1 << 24);
            data = &data[16..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; TAG_LEN] {
        if self.len > 0 {
            let mut b = [0u8; 16];
            b[..self.len].copy_from_slice(&self.buf[..self.len]);
            b[self.len] = 1;
            self.block(&b, 0);
        }
        let [mut h0, mut h1, mut h2, mut h3, mut h4] = self.h;

        // Reducción completa de h módulo 2^130 - 5.
        let mut c = h1 >> 26;
        h1 &= M26;
        h2 += c;
        c = h2 >> 26;
        h2 &= M26;
        h3 += c;
        c = h3 >> 26;
        h3 &= M26;
        h4 += c;
        c = h4 >> 26;
        h4 &= M26;
        h0 += c * 5;
        c = h0 >> 26;
        h0 &= M26;
        h1 += c;

        // g = h + 5 - 2^130; si g no es negativo, h >= p y hay que usar g.
        let mut g0 = h0 + 5;
        c = g0 >> 26;
        g0 &= M26;
        let mut g1 = h1 + c;
        c = g1 >> 26;
        g1 &= M26;
        let mut g2 = h2 + c;
        c = g2 >> 26;
        g2 &= M26;
        let mut g3 = h3 + c;
        c = g3 >> 26;
        g3 &= M26;
        let g4 = (h4 + c).wrapping_sub(1 << 26);

        // Selección sin ramas: máscara de unos si g es no negativo (bit 31 en 0), de ceros si no.
        let mask = (g4 >> 31).wrapping_sub(1);
        g0 &= mask;
        g1 &= mask;
        g2 &= mask;
        g3 &= mask;
        let g4 = g4 & mask;
        let nmask = !mask;
        h0 = (h0 & nmask) | g0;
        h1 = (h1 & nmask) | g1;
        h2 = (h2 & nmask) | g2;
        h3 = (h3 & nmask) | g3;
        h4 = (h4 & nmask) | g4;

        // h mod 2^128, en 4 palabras de 32 bits.
        let w0 = h0 | (h1 << 26);
        let w1 = (h1 >> 6) | (h2 << 20);
        let w2 = (h2 >> 12) | (h3 << 14);
        let w3 = (h3 >> 18) | (h4 << 8);

        // tag = (h + pad) mod 2^128
        let mut f = w0 as u64 + self.pad[0] as u64;
        let t0 = f as u32;
        f = w1 as u64 + self.pad[1] as u64 + (f >> 32);
        let t1 = f as u32;
        f = w2 as u64 + self.pad[2] as u64 + (f >> 32);
        let t2 = f as u32;
        f = w3 as u64 + self.pad[3] as u64 + (f >> 32);
        let t3 = f as u32;

        let mut out = [0u8; TAG_LEN];
        out[0..4].copy_from_slice(&t0.to_le_bytes());
        out[4..8].copy_from_slice(&t1.to_le_bytes());
        out[8..12].copy_from_slice(&t2.to_le_bytes());
        out[12..16].copy_from_slice(&t3.to_le_bytes());
        out
    }
}

/// Poly1305 de un mensaje completo.
pub fn poly1305(key: &[u8; 32], msg: &[u8]) -> [u8; TAG_LEN] {
    let mut p = Poly1305::new(key);
    p.update(msg);
    p.finalize()
}

// ------------------------------------------------------------------------------------------------
// Construcción AEAD (RFC 8439 §2.6 y §2.8)
// ------------------------------------------------------------------------------------------------

/// Clave de un solo uso de Poly1305: los primeros 32 bytes del bloque 0 de ChaCha20.
pub fn poly_key(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN]) -> [u8; 32] {
    let block = chacha20_block(key, 0, nonce);
    let mut k = [0u8; 32];
    k.copy_from_slice(&block[..32]);
    k
}

fn pad16(p: &mut Poly1305, len: usize) {
    let rem = len % 16;
    if rem != 0 {
        p.update(&[0u8; 16][..16 - rem]);
    }
}

fn aead_tag(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
) -> [u8; TAG_LEN] {
    let mut p = Poly1305::new(&poly_key(key, nonce));
    p.update(aad);
    pad16(&mut p, aad.len());
    p.update(ciphertext);
    pad16(&mut p, ciphertext.len());
    p.update(&(aad.len() as u64).to_le_bytes());
    p.update(&(ciphertext.len() as u64).to_le_bytes());
    p.finalize()
}

/// Comparación de tiempo constante (no sale antes al encontrar una diferencia).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// Cifra `buf` en el sitio y devuelve la etiqueta. `aad` se autentica pero no se cifra.
pub fn seal_in_place(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    buf: &mut [u8],
) -> [u8; TAG_LEN] {
    chacha20_xor(key, 1, nonce, buf);
    aead_tag(key, nonce, aad, buf)
}

/// Verifica la etiqueta **antes** de descifrar. Si no coincide devuelve `false` y deja `buf` intacto
/// (nada de texto sin autenticar sale de aquí).
#[must_use]
pub fn open_in_place(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    buf: &mut [u8],
    tag: &[u8],
) -> bool {
    if tag.len() != TAG_LEN {
        return false;
    }
    let expected = aead_tag(key, nonce, aad, buf);
    if !ct_eq(&expected, tag) {
        return false;
    }
    chacha20_xor(key, 1, nonce, buf);
    true
}

/// Cifra y devuelve `texto cifrado ‖ etiqueta`.
pub fn seal(key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(plaintext.len() + TAG_LEN);
    out.extend_from_slice(plaintext);
    let tag = seal_in_place(key, nonce, aad, &mut out);
    out.extend_from_slice(&tag);
    out
}

/// Descifra `texto cifrado ‖ etiqueta`; `None` si no se autentica.
pub fn open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    sealed: &[u8],
) -> Option<Vec<u8>> {
    if sealed.len() < TAG_LEN {
        return None;
    }
    let (ct, tag) = sealed.split_at(sealed.len() - TAG_LEN);
    let mut buf = ct.to_vec();
    open_in_place(key, nonce, aad, &mut buf, tag).then_some(buf)
}

#[cfg(test)]
mod tests;
