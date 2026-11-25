//! Comparación de imágenes RGBA8, para contrastar un backend de GPU con el de software.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Diff {
    /// Diferencia absoluta media por canal (RGB), de 0 a 255.
    pub mean_abs: f64,
    /// Fracción de píxeles (0 a 1) cuyo canal RGB más distinto difiere más que el umbral.
    pub fraction_over: f64,
    /// La mayor diferencia en cualquier canal RGB de cualquier píxel.
    pub max_channel: u8,
}

impl Diff {
    /// Las imágenes se consideran equivalentes si el error medio es pequeño y casi ningún píxel difiere mucho
    /// (los píxeles del borde de los triángulos pueden variar entre rasterizadores).
    pub fn is_close(&self) -> bool {
        self.mean_abs < 1.5 && self.fraction_over < 0.02
    }
}

/// Compara dos imágenes del mismo tamaño. `threshold` es la diferencia por canal a partir de la cual un píxel cuenta como distinto.
pub fn diff(a: &[u8], b: &[u8], threshold: u8) -> Result<Diff, String> {
    if a.len() != b.len() || a.is_empty() || !a.len().is_multiple_of(4) {
        return Err(format!(
            "las imágenes no son comparables ({} y {} bytes)",
            a.len(),
            b.len()
        ));
    }
    let (mut suma, mut sobre, mut max) = (0u64, 0usize, 0u8);
    for (p, q) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let mut peor = 0u8;
        for k in 0..3 {
            let d = p[k].abs_diff(q[k]);
            suma += d as u64;
            peor = peor.max(d);
        }
        max = max.max(peor);
        if peor > threshold {
            sobre += 1;
        }
    }
    let pixeles = a.len() / 4;
    Ok(Diff {
        mean_abs: suma as f64 / (pixeles * 3) as f64,
        fraction_over: sobre as f64 / pixeles as f64,
        max_channel: max,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imagenes_iguales() {
        let a = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        let d = diff(&a, &a, 8).unwrap();
        assert_eq!((d.mean_abs, d.fraction_over, d.max_channel), (0.0, 0.0, 0));
        assert!(d.is_close());
    }

    #[test]
    fn diferencias_medidas_a_mano() {
        let a = vec![0u8, 0, 0, 255, 100, 100, 100, 255];
        let b = vec![0u8, 0, 0, 255, 100, 100, 130, 255];
        let d = diff(&a, &b, 8).unwrap();
        assert_eq!(d.max_channel, 30);
        assert!((d.mean_abs - 30.0 / 6.0).abs() < 1e-12);
        assert_eq!(d.fraction_over, 0.5); // un píxel de dos
        assert!(!d.is_close());
        // el canal alfa no cuenta
        let c = vec![0u8, 0, 0, 0, 100, 100, 100, 7];
        assert_eq!(diff(&a, &c, 8).unwrap().max_channel, 0);
    }

    #[test]
    fn rechaza_tamanos_distintos_o_vacios() {
        assert!(diff(&[0; 8], &[0; 4], 8).is_err());
        assert!(diff(&[], &[], 8).is_err());
        assert!(diff(&[0; 6], &[0; 6], 8).is_err());
    }
}
