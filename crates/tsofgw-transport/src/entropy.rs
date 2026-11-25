//! Fuentes de aleatoriedad.
//!
//! [`OsEntropy`] pide bytes al generador del sistema operativo (`/dev/urandom` en Unix y macOS,
//! `BCryptGenRandom` en Windows). [`DeterministicEntropy`] es **predecible y solo sirve para pruebas y
//! simulaciones**: nunca debe usarse en un servidor real.

pub trait Entropy {
    fn fill(&mut self, buf: &mut [u8]);
}

/// Aleatoriedad criptográfica del sistema operativo. Si el sistema no la entrega, entra en pánico:
/// seguir con valores predecibles sería peor que detenerse.
#[derive(Default)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        os_fill(buf).expect("el sistema operativo no entregó bytes aleatorios");
    }
}

#[cfg(unix)]
fn os_fill(buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(buf)
}

#[cfg(windows)]
mod win {
    #[link(name = "bcrypt")]
    extern "system" {
        pub fn BCryptGenRandom(
            algorithm: *mut core::ffi::c_void,
            buffer: *mut u8,
            len: u32,
            flags: u32,
        ) -> i32;
    }
}

#[cfg(windows)]
fn os_fill(buf: &mut [u8]) -> std::io::Result<()> {
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG: no hace falta abrir un algoritmo.
    const SYSTEM_PREFERRED_RNG: u32 = 2;
    for chunk in buf.chunks_mut(1 << 20) {
        // SAFETY: `chunk` es un búfer válido y modificable de `chunk.len()` bytes durante la llamada.
        let status = unsafe {
            win::BCryptGenRandom(
                core::ptr::null_mut(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                SYSTEM_PREFERRED_RNG,
            )
        };
        if status != 0 {
            return Err(std::io::Error::other(format!(
                "BCryptGenRandom devolvió {status:#x}"
            )));
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn os_fill(_buf: &mut [u8]) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "plataforma sin fuente de aleatoriedad conocida",
    ))
}

/// Generador predecible (splitmix64). **No es seguro**: existe para que las pruebas y las
/// simulaciones sean reproducibles.
pub struct DeterministicEntropy(pub u64);

impl DeterministicEntropy {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

impl Entropy for DeterministicEntropy {
    fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_entropy_entrega_bytes_distintos() {
        let mut e = OsEntropy;
        let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
        e.fill(&mut a);
        e.fill(&mut b);
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
    }

    #[test]
    fn deterministica_es_reproducible() {
        let (mut a, mut b) = (DeterministicEntropy(7), DeterministicEntropy(7));
        let (mut x, mut y) = ([0u8; 21], [0u8; 21]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_eq!(x, y);
        let mut z = [0u8; 21];
        DeterministicEntropy(8).fill(&mut z);
        assert_ne!(x, z);
    }
}
