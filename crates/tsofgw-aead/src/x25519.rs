//! **X25519** (RFC 7748): acuerdo de claves de Diffie-Hellman sobre la curva 25519.
//!
//! Igual que el resto de este crate, es código propio, solo con la biblioteca estándar, y **nadie lo ha
//! auditado**. Se comprueba con los vectores del RFC 7748 (§5.2, §6.1 y la prueba iterada de §5.2) y
//! contra OpenSSL con vectores generados aparte.
//!
//! * Aritmética de campo módulo 2^255−19 con 5 miembros de 51 bits y productos en `u128`.
//! * Escalera de Montgomery con intercambio condicional por máscara (sin ramas ni índices que dependan
//!   del secreto). No se ha medido el tiempo constante en cada CPU.
//! * [`shared_secret`] rechaza un resultado todo ceros (punto de orden pequeño), como pide el RFC 7748 §6.1
//!   para quien quiera contribución real de ambas partes.

/// Elemento del campo: 5 miembros de 51 bits (con holgura entre operaciones).
#[derive(Clone, Copy)]
struct Fe([u64; 5]);

const MASK51: u64 = (1u64 << 51) - 1;

impl Fe {
    const ZERO: Fe = Fe([0; 5]);
    const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    fn from_bytes(b: &[u8; 32]) -> Fe {
        let mut w = [0u64; 4];
        for (i, c) in b.chunks_exact(8).enumerate() {
            w[i] = u64::from_le_bytes(c.try_into().unwrap());
        }
        w[3] &= 0x7fff_ffff_ffff_ffff; // el bit 255 se ignora (RFC 7748 §5)
        Fe([
            w[0] & MASK51,
            ((w[0] >> 51) | (w[1] << 13)) & MASK51,
            ((w[1] >> 38) | (w[2] << 26)) & MASK51,
            ((w[2] >> 25) | (w[3] << 39)) & MASK51,
            (w[3] >> 12) & MASK51,
        ])
    }

    fn carry(mut self) -> Fe {
        let h = &mut self.0;
        for _ in 0..2 {
            let c0 = h[0] >> 51;
            h[0] &= MASK51;
            h[1] += c0;
            let c1 = h[1] >> 51;
            h[1] &= MASK51;
            h[2] += c1;
            let c2 = h[2] >> 51;
            h[2] &= MASK51;
            h[3] += c2;
            let c3 = h[3] >> 51;
            h[3] &= MASK51;
            h[4] += c3;
            let c4 = h[4] >> 51;
            h[4] &= MASK51;
            h[0] += c4 * 19;
        }
        self
    }

    fn add(self, o: Fe) -> Fe {
        let mut r = self.0;
        for (x, y) in r.iter_mut().zip(o.0) {
            *x += y;
        }
        Fe(r).carry()
    }

    fn sub(self, o: Fe) -> Fe {
        // a + 2p − b: 2p tiene los miembros [2^52−38, 2^52−2, ...]; así nunca hay resta negativa.
        const TWO_P: [u64; 5] = [
            0xF_FFFF_FFFF_FFDA,
            0xF_FFFF_FFFF_FFFE,
            0xF_FFFF_FFFF_FFFE,
            0xF_FFFF_FFFF_FFFE,
            0xF_FFFF_FFFF_FFFE,
        ];
        let mut r = [0u64; 5];
        for i in 0..5 {
            r[i] = self.0[i] + TWO_P[i] - o.0[i];
        }
        Fe(r).carry()
    }

    fn mul(self, o: Fe) -> Fe {
        let a = self.0;
        let b = o.0;
        let b19 = [b[0], b[1] * 19, b[2] * 19, b[3] * 19, b[4] * 19];
        let m = |x: u64, y: u64| (x as u128) * (y as u128);
        let mut t = [0u128; 5];
        t[0] =
            m(a[0], b[0]) + m(a[1], b19[4]) + m(a[2], b19[3]) + m(a[3], b19[2]) + m(a[4], b19[1]);
        t[1] = m(a[0], b[1]) + m(a[1], b[0]) + m(a[2], b19[4]) + m(a[3], b19[3]) + m(a[4], b19[2]);
        t[2] = m(a[0], b[2]) + m(a[1], b[1]) + m(a[2], b[0]) + m(a[3], b19[4]) + m(a[4], b19[3]);
        t[3] = m(a[0], b[3]) + m(a[1], b[2]) + m(a[2], b[1]) + m(a[3], b[0]) + m(a[4], b19[4]);
        t[4] = m(a[0], b[4]) + m(a[1], b[3]) + m(a[2], b[2]) + m(a[3], b[1]) + m(a[4], b[0]);
        let mut r = [0u64; 5];
        let mut c: u128 = 0;
        for i in 0..5 {
            let v = t[i] + c;
            r[i] = (v as u64) & MASK51;
            c = v >> 51;
        }
        // c < 2^64 aquí: 19·c cabe de sobra.
        r[0] += (c as u64) * 19;
        Fe(r).carry()
    }

    fn square(self) -> Fe {
        self.mul(self)
    }

    fn mul_small(self, k: u64) -> Fe {
        let mut o = [0u64; 5];
        let mut c: u128 = 0;
        for (out, limb) in o.iter_mut().zip(self.0) {
            let v = (limb as u128) * (k as u128) + c;
            *out = (v as u64) & MASK51;
            c = v >> 51;
        }
        o[0] += (c as u64) * 19;
        Fe(o).carry()
    }

    /// Intercambia `a` y `b` si `swap` es 1 (y no si es 0), sin ramas.
    fn cswap(a: &mut Fe, b: &mut Fe, swap: u64) {
        let mask = 0u64.wrapping_sub(swap);
        for i in 0..5 {
            let x = mask & (a.0[i] ^ b.0[i]);
            a.0[i] ^= x;
            b.0[i] ^= x;
        }
    }

    /// z^(p−2) = z⁻¹. El exponente es público, así que el recorrido por sus bits no filtra nada.
    fn invert(self) -> Fe {
        let mut r = Fe::ONE;
        for i in (0..255).rev() {
            r = r.square();
            let bit = if i >= 5 { 1 } else { (0b01011u32 >> i) & 1 };
            if bit == 1 {
                r = r.mul(self);
            }
        }
        r
    }

    fn to_bytes(self) -> [u8; 32] {
        let mut h = self.carry().carry().0;
        // Reducción completa: restar p si h ≥ p.
        let mut q = (h[0] + 19) >> 51;
        q = (h[1] + q) >> 51;
        q = (h[2] + q) >> 51;
        q = (h[3] + q) >> 51;
        q = (h[4] + q) >> 51;
        h[0] += 19 * q;
        let mut c = h[0] >> 51;
        h[0] &= MASK51;
        for limb in h.iter_mut().skip(1) {
            *limb += c;
            c = *limb >> 51;
            *limb &= MASK51;
        }
        // (el acarreo final es el bit 255 de h + 19q, que se descarta: equivale a restar p)
        let w = [
            h[0] | (h[1] << 51),
            (h[1] >> 13) | (h[2] << 38),
            (h[2] >> 26) | (h[3] << 25),
            (h[3] >> 39) | (h[4] << 12),
        ];
        let mut out = [0u8; 32];
        for i in 0..4 {
            out[8 * i..8 * i + 8].copy_from_slice(&w[i].to_le_bytes());
        }
        out
    }
}

/// Longitud de claves privadas, públicas y secretos compartidos.
pub const X25519_LEN: usize = 32;

/// El punto base (u = 9).
pub const BASEPOINT: [u8; 32] = {
    let mut b = [0u8; 32];
    b[0] = 9;
    b
};

/// `X25519(k, u)` del RFC 7748 §5: multiplica el punto `u` por el escalar `k` (con el «recorte» del escalar).
pub fn x25519(k: &[u8; 32], u: &[u8; 32]) -> [u8; 32] {
    let mut s = *k;
    s[0] &= 248;
    s[31] &= 127;
    s[31] |= 64;

    let x1 = Fe::from_bytes(u);
    let mut x2 = Fe::ONE;
    let mut z2 = Fe::ZERO;
    let mut x3 = x1;
    let mut z3 = Fe::ONE;
    let mut swap = 0u64;

    for t in (0..=254).rev() {
        let kt = ((s[t / 8] >> (t % 8)) & 1) as u64;
        swap ^= kt;
        Fe::cswap(&mut x2, &mut x3, swap);
        Fe::cswap(&mut z2, &mut z3, swap);
        swap = kt;

        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.sub(z2);
        let bb = b.square();
        let e = aa.sub(bb);
        let c = x3.add(z3);
        let d = x3.sub(z3);
        let da = d.mul(a);
        let cb = c.mul(b);
        x3 = da.add(cb).square();
        z3 = x1.mul(da.sub(cb).square());
        x2 = aa.mul(bb);
        z2 = e.mul(aa.add(e.mul_small(121665)));
    }
    Fe::cswap(&mut x2, &mut x3, swap);
    Fe::cswap(&mut z2, &mut z3, swap);
    x2.mul(z2.invert()).to_bytes()
}

/// Clave pública de una clave privada: `X25519(privada, 9)`.
pub fn public_key(secret: &[u8; 32]) -> [u8; 32] {
    x25519(secret, &BASEPOINT)
}

/// Secreto compartido con la clave pública del otro lado. Devuelve `None` si el resultado es todo ceros
/// (el otro lado mandó un punto de orden pequeño: no aporta nada al secreto y se rechaza).
pub fn shared_secret(secret: &[u8; 32], peer_public: &[u8; 32]) -> Option<[u8; 32]> {
    let out = x25519(secret, peer_public);
    // Comparación sin cortocircuito.
    let mut acc = 0u8;
    for b in out {
        acc |= b;
    }
    if acc == 0 {
        None
    } else {
        Some(out)
    }
}
