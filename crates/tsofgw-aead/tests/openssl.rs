//! Comparación con una implementación independiente (OpenSSL 3, vía la biblioteca `cryptography` de
//! Python). Los vectores están en `vectores/mod.rs`; se generaron con una semilla fija y cubren
//! longitudes en los bordes de bloque (0, 1, 15, 16, 17, 63, 64, 65...), datos asociados de varios
//! tamaños, mensajes de todo `0xFF` y claves extremas de Poly1305 (las que fuerzan las reducciones
//! finales) y contadores de bloque de ChaCha20 extremos.

mod vectores;

use tsofgw_aead::*;
use vectores::{AEAD, BLOCKS, POLY1305};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn arr<const N: usize>(s: &str) -> [u8; N] {
    hex(s).try_into().unwrap()
}

#[test]
fn aead_coincide_con_openssl_en_todas_las_longitudes() {
    assert!(AEAD.len() >= 20);
    for (i, (k, n, a, p, c)) in AEAD.iter().enumerate() {
        let (k, n, a, p, c) = (arr::<32>(k), arr::<12>(n), hex(a), hex(p), hex(c));
        assert_eq!(seal(&k, &n, &a, &p), c, "cifrado del vector {i}");
        assert_eq!(open(&k, &n, &a, &c).as_deref(), Some(&p[..]), "vector {i}");
        // Y cualquier bit alterado del conjunto se rechaza (muestra: un bit por byte).
        for pos in 0..c.len() {
            let mut m = c.clone();
            m[pos] ^= 1 << (pos % 8);
            assert!(open(&k, &n, &a, &m).is_none(), "vector {i}, byte {pos}");
        }
    }
}

#[test]
fn poly1305_coincide_con_openssl_incluidas_las_reducciones_extremas() {
    assert!(POLY1305.len() >= 300);
    for (i, (k, m, t)) in POLY1305.iter().enumerate() {
        let (k, m, t) = (arr::<32>(k), hex(m), hex(t));
        assert_eq!(poly1305(&k, &m), &t[..], "vector {i}");
        // El mismo mensaje troceado de cualquier manera da la misma etiqueta.
        for cut in [1usize, 3, 15, 16, 17, 31] {
            let mut p = Poly1305::new(&k);
            for c in m.chunks(cut) {
                p.update(c);
            }
            assert_eq!(p.finalize(), &t[..], "vector {i}, trozos de {cut}");
        }
    }
}

#[test]
fn bloques_de_chacha20_coinciden_con_openssl_incluso_con_contadores_extremos() {
    for (i, (k, c, n, b)) in BLOCKS.iter().enumerate() {
        assert_eq!(
            chacha20_block(&arr(k), *c, &arr(n)),
            &hex(b)[..],
            "bloque {i}"
        );
    }
}
