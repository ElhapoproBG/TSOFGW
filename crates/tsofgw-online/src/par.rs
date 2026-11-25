//! Reparto de trabajo entre hilos usando solo la biblioteca estándar.
//!
//! La garantía que importa es el **determinismo**: el resultado de [`map_mut`] es el mismo con 1 o con
//! 64 hilos y en cualquier orden de planificación, porque cada elemento se procesa de forma
//! independiente y los resultados se devuelven en el orden de los elementos. Eso permite paralelizar
//! el servidor sin que el comportamiento (ni los paquetes que salen) dependan del número de núcleos.

use std::sync::Mutex;

/// Número de hilos que se usarán para `requested`: 0 significa «los núcleos disponibles» (con tope 16).
pub fn resolve_threads(requested: usize) -> usize {
    if requested == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(16)
    } else {
        requested.min(256)
    }
}

/// Aplica `f` a cada elemento con hasta `threads` hilos y devuelve los resultados en el orden de `items`.
///
/// Los elementos se reparten por lotes de `batch` a quien esté libre, así que un lote lento no deja al
/// resto esperando. Con `threads <= 1` (o muy pocos elementos) no se crea ningún hilo. Si `f` entra en
/// pánico, el pánico se propaga a quien llama.
pub fn map_mut<T, R, F>(items: &mut [T], threads: usize, batch: usize, f: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(&mut T) -> R + Sync,
{
    let n = items.len();
    let batch = batch.max(1);
    let batches = n.div_ceil(batch);
    let threads = threads.clamp(1, batches.max(1));
    if threads == 1 {
        return items.iter_mut().map(&f).collect();
    }

    let queue = Mutex::new(items.chunks_mut(batch).enumerate());
    let worker = || {
        let mut mine: Vec<(usize, Vec<R>)> = Vec::new();
        loop {
            let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
            let Some((i, chunk)) = next else { break };
            mine.push((i, chunk.iter_mut().map(&f).collect()));
        }
        mine
    };

    let mut parts: Vec<(usize, Vec<R>)> = Vec::with_capacity(batches);
    std::thread::scope(|sc| {
        let handles: Vec<_> = (1..threads).map(|_| sc.spawn(worker)).collect();
        // El hilo que llama también trabaja: un hilo menos que crear.
        parts.extend(worker());
        for h in handles {
            match h.join() {
                Ok(p) => parts.extend(p),
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
    });
    parts.sort_unstable_by_key(|p| p.0);
    let mut out = Vec::with_capacity(n);
    for (_, v) in parts {
        out.extend(v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devuelve_en_orden_con_cualquier_numero_de_hilos() {
        for n in [0usize, 1, 2, 7, 8, 9, 100, 1001] {
            for threads in [1usize, 2, 3, 8, 64] {
                for batch in [1usize, 3, 8, 5000] {
                    let mut items: Vec<u64> = (0..n as u64).collect();
                    let out = map_mut(&mut items, threads, batch, |x| {
                        *x += 1;
                        *x * 2
                    });
                    let want: Vec<u64> = (0..n as u64).map(|v| (v + 1) * 2).collect();
                    assert_eq!(out, want, "n={n} hilos={threads} lote={batch}");
                    let mutated: Vec<u64> = (1..=n as u64).collect();
                    assert_eq!(
                        items, mutated,
                        "cada elemento se modifica exactamente una vez"
                    );
                }
            }
        }
    }

    #[test]
    fn el_trabajo_desigual_no_cambia_el_resultado() {
        // Algunos elementos tardan mucho más que otros: el orden de salida debe seguir siendo el de entrada.
        let mut items: Vec<u32> = (0..64).collect();
        let out = map_mut(&mut items, 4, 2, |x| {
            if *x % 7 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
            *x * 10
        });
        assert_eq!(out, (0..64).map(|v| v * 10).collect::<Vec<_>>());
    }

    #[test]
    fn un_panico_en_un_hilo_llega_a_quien_llama() {
        let r = std::panic::catch_unwind(|| {
            let mut items: Vec<u32> = (0..32).collect();
            map_mut(&mut items, 4, 1, |x| {
                assert!(*x != 17, "boom");
                *x
            })
        });
        assert!(r.is_err());
    }

    #[test]
    fn resuelve_el_numero_de_hilos() {
        assert_eq!(resolve_threads(1), 1);
        assert_eq!(resolve_threads(5), 5);
        assert_eq!(resolve_threads(100_000), 256);
        let auto = resolve_threads(0);
        assert!((1..=16).contains(&auto));
    }
}
