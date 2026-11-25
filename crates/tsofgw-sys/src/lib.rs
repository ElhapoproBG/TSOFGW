//! Capa FFI mínima hacia C, con respaldo en Rust. Todo `unsafe` queda encapsulado aquí.
//!
//! Si hay un compilador de C compatible, `hash64` llama a la librería C (`c/hash.c`); si no,
//! usa [`hash64_rust`], que produce exactamente el mismo resultado.

#[cfg(not(tsofgw_no_c))]
extern "C" {
    fn tsofgw_hash64(data: *const u8, len: usize) -> u64;
}

pub mod stop;

/// Qué implementación se compiló: `"c"` o `"rust"`.
pub const BACKEND: &str = if cfg!(tsofgw_no_c) { "rust" } else { "c" };

/// Hash de contenido (FNV-1a de 64 bits, **no criptográfico**) en Rust puro. Referencia del comportamiento de la versión en C.
pub fn hash64_rust(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Hash de contenido (FNV-1a de 64 bits), con la implementación disponible en esta compilación.
#[cfg(not(tsofgw_no_c))]
pub fn hash64(data: &[u8]) -> u64 {
    // SAFETY: puntero y longitud provienen de un slice válido.
    unsafe { tsofgw_hash64(data.as_ptr(), data.len()) }
}

/// Hash de contenido (FNV-1a de 64 bits), con la implementación disponible en esta compilación.
#[cfg(tsofgw_no_c)]
pub fn hash64(data: &[u8]) -> u64 {
    hash64_rust(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_estable() {
        assert_eq!(hash64(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(hash64(b"a"), hash64(b"b"));
        // valor de referencia público de FNV-1a 64 para "a"
        assert_eq!(hash64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hash64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn c_y_rust_dan_lo_mismo() {
        let datos: Vec<u8> = (0..5000u32).map(|i| (i * 31 + 7) as u8).collect();
        for n in [0, 1, 2, 7, 8, 63, 64, 1000, 5000] {
            assert_eq!(
                hash64(&datos[..n]),
                hash64_rust(&datos[..n]),
                "longitud {n}"
            );
        }
    }

    #[test]
    fn el_backend_declarado_es_valido() {
        assert!(BACKEND == "c" || BACKEND == "rust");
    }
}
