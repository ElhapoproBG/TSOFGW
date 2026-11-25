//! Capa FFI mínima hacia C. Todo `unsafe` queda encapsulado aquí.
extern "C" {
    fn tsofgw_hash64(data: *const u8, len: usize) -> u64;
}

/// Hash de contenido (FNV-1a 64) calculado por la librería C.
pub fn hash64(data: &[u8]) -> u64 {
    // SAFETY: puntero y longitud provienen de un slice válido.
    unsafe { tsofgw_hash64(data.as_ptr(), data.len()) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hash_estable() {
        assert_eq!(super::hash64(b""), 1469598103934665603);
        assert_ne!(super::hash64(b"a"), super::hash64(b"b"));
    }
}
