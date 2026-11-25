//! Reparto del mundo entre shards.
//!
//! El mundo se divide en una rejilla de regiones rectangulares en el plano horizontal (x, z); la altura no
//! cuenta. Las regiones de los bordes se extienden hasta el infinito por fuera, de modo que **toda posición
//! tiene exactamente un dueño**, aunque esté ligeramente fuera del mundo. Cada shard sabe cuáles son sus
//! vecinos y a qué distancia está una posición de cada región.
//!
//! Una región contiene `x0 <= x < x1` y `z0 <= z < z1` (semiabierta), así que dos regiones contiguas
//! nunca se solapan ni dejan huecos.

use tsofgw_world::Vec3;

pub type ShardId = u16;

/// Máximo de shards por eje (la rejilla completa no pasa de 64 × 64).
pub const MAX_GRID_SIDE: u16 = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub x1: f32,
    pub z0: f32,
    pub z1: f32,
}

impl Rect {
    pub fn contains(&self, p: Vec3) -> bool {
        p.x >= self.x0 && p.x < self.x1 && p.z >= self.z0 && p.z < self.z1
    }

    /// Distancia horizontal de `p` a la región (0 si está dentro).
    pub fn distance(&self, p: Vec3) -> f32 {
        let dx = (self.x0 - p.x).max(p.x - self.x1).max(0.0);
        let dz = (self.z0 - p.z).max(p.z - self.z1).max(0.0);
        (dx * dx + dz * dz).sqrt()
    }

    /// El punto de la región (reducida `inset` unidades por cada lado) más cercano a `p`.
    pub fn clamp_inside(&self, p: Vec3, inset: f32) -> Vec3 {
        let cl = |v: f32, lo: f32, hi: f32| {
            let (lo, hi) = (lo + inset, hi - inset);
            if lo >= hi {
                (lo + hi) * 0.5
            } else {
                v.clamp(lo, hi)
            }
        };
        Vec3::new(cl(p.x, self.x0, self.x1), p.y, cl(p.z, self.z0, self.z1))
    }
}

/// Rejilla de `nx × nz` regiones sobre un mundo de ±`half` unidades.
#[derive(Debug, Clone, PartialEq)]
pub struct Partition {
    nx: u16,
    nz: u16,
    half: f32,
    tile_x: f32,
    tile_z: f32,
}

impl Partition {
    /// Un solo shard: todo el mundo es suyo.
    pub fn single(half: f32) -> Partition {
        Partition::grid(1, 1, half).expect("1x1 siempre es válida")
    }

    /// Rejilla de `nx × nz` regiones sobre todo el mundo de ejemplo (±512 u).
    pub fn world_grid(nx: u16, nz: u16) -> Result<Partition, String> {
        Partition::grid(nx, nz, tsofgw_net::dequantize(crate::sim::HALF_EXTENT_Q))
    }

    pub fn grid(nx: u16, nz: u16, half: f32) -> Result<Partition, String> {
        if nx == 0 || nz == 0 || nx > MAX_GRID_SIDE || nz > MAX_GRID_SIDE {
            return Err(format!(
                "la rejilla debe tener entre 1 y {MAX_GRID_SIDE} regiones por eje"
            ));
        }
        if !half.is_finite() || half <= 0.0 {
            return Err("el tamaño del mundo debe ser positivo".into());
        }
        Ok(Partition {
            nx,
            nz,
            half,
            tile_x: 2.0 * half / nx as f32,
            tile_z: 2.0 * half / nz as f32,
        })
    }

    /// `(columnas, filas, mitad del mundo)`: la forma de la rejilla.
    pub fn shape(&self) -> (u16, u16, f32) {
        (self.nx, self.nz, self.half)
    }

    pub fn shard_count(&self) -> usize {
        self.nx as usize * self.nz as usize
    }

    /// `(columna, fila)` de una región.
    pub fn coords(&self, s: ShardId) -> (u16, u16) {
        (s % self.nx, s / self.nx)
    }

    pub fn id_at(&self, ix: u16, iz: u16) -> ShardId {
        iz * self.nx + ix
    }

    pub fn is_valid(&self, s: ShardId) -> bool {
        (s as usize) < self.shard_count()
    }

    /// Lado de la región más pequeña (los márgenes de fantasmas deben ser menores que la mitad).
    pub fn tile_size(&self) -> f32 {
        self.tile_x.min(self.tile_z)
    }

    pub fn rect(&self, s: ShardId) -> Rect {
        let (ix, iz) = self.coords(s);
        let edge = |i: u16, n: u16, tile: f32| {
            let lo = if i == 0 {
                f32::NEG_INFINITY
            } else {
                -self.half + i as f32 * tile
            };
            let hi = if i + 1 == n {
                f32::INFINITY
            } else {
                -self.half + (i + 1) as f32 * tile
            };
            (lo, hi)
        };
        let (x0, x1) = edge(ix, self.nx, self.tile_x);
        let (z0, z1) = edge(iz, self.nz, self.tile_z);
        Rect { x0, x1, z0, z1 }
    }

    /// El único shard cuya región contiene `p`.
    pub fn owner_of(&self, p: Vec3) -> ShardId {
        let idx = |v: f32, tile: f32, n: u16| {
            (((v + self.half) / tile).floor().max(0.0) as i64).min(n as i64 - 1) as u16
        };
        // Un NaN no debe repartirse al azar: va a la región 0.
        if p.x.is_nan() || p.z.is_nan() {
            return 0;
        }
        self.id_at(
            idx(p.x, self.tile_x, self.nx),
            idx(p.z, self.tile_z, self.nz),
        )
    }

    /// Regiones que tocan la de `s` (también por una esquina).
    pub fn neighbors(&self, s: ShardId) -> Vec<ShardId> {
        let (ix, iz) = self.coords(s);
        let mut v = Vec::new();
        for dz in -1i32..=1 {
            for dx in -1i32..=1 {
                if dx == 0 && dz == 0 {
                    continue;
                }
                let (x, z) = (ix as i32 + dx, iz as i32 + dz);
                if (0..self.nx as i32).contains(&x) && (0..self.nz as i32).contains(&z) {
                    v.push(self.id_at(x as u16, z as u16));
                }
            }
        }
        v
    }

    /// Si `p` está a más de `depth` unidades dentro de **otra** región distinta de la de `current`,
    /// devuelve esa región. La profundidad evita que algo que va y viene sobre un borde cambie de dueño
    /// en cada paso.
    pub fn deep_owner(&self, current: ShardId, p: Vec3, depth: f32) -> Option<ShardId> {
        let o = self.owner_of(p);
        (o != current && self.rect(current).distance(p) > depth).then_some(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: f32, z: f32) -> Vec3 {
        Vec3::new(x, 0.0, z)
    }

    #[test]
    fn cada_posicion_tiene_un_unico_dueno() {
        for (nx, nz) in [(1, 1), (2, 1), (2, 2), (3, 2), (5, 5)] {
            let p = Partition::grid(nx, nz, 512.0).unwrap();
            let mut rng = tsofgw_world::Rng::new(9);
            for _ in 0..5000 {
                // Incluye puntos fuera del mundo y justo sobre los bordes.
                let pt = v(rng.range(-700.0, 700.0), rng.range(-700.0, 700.0));
                let owners: Vec<ShardId> = (0..p.shard_count() as u16)
                    .filter(|&s| p.rect(s).contains(pt))
                    .collect();
                assert_eq!(owners, vec![p.owner_of(pt)], "{pt:?} en {nx}x{nz}");
            }
            // Los bordes exactos pertenecen a la región de su derecha / de abajo.
            if nx == 2 {
                assert_eq!(p.owner_of(v(0.0, 0.0)), p.id_at(1, nz / 2));
            }
        }
    }

    #[test]
    fn distancia_vecinos_y_profundidad() {
        let p = Partition::grid(2, 2, 512.0).unwrap();
        assert_eq!(p.shard_count(), 4);
        assert_eq!(p.neighbors(0).len(), 3, "en 2×2 todos se tocan");
        assert_eq!(Partition::single(512.0).neighbors(0).len(), 0);
        assert_eq!(Partition::grid(3, 3, 512.0).unwrap().neighbors(4).len(), 8);
        assert_eq!(Partition::grid(3, 3, 512.0).unwrap().neighbors(0).len(), 3);

        // Región 0 = x<0, z<0.
        let r0 = p.rect(0);
        assert!(r0.contains(v(-1.0, -1.0)));
        assert!(!r0.contains(v(0.0, -1.0)), "el borde pertenece a la vecina");
        assert_eq!(r0.distance(v(-10.0, -10.0)), 0.0);
        assert!((r0.distance(v(3.0, -10.0)) - 3.0).abs() < 1e-6);
        assert!(
            (r0.distance(v(3.0, 4.0)) - 5.0).abs() < 1e-6,
            "por la esquina"
        );

        // Profundidad: a 2 u dentro de la vecina no cambia de dueño con depth 5; a 6 u, sí.
        assert_eq!(p.deep_owner(0, v(2.0, -10.0), 5.0), None);
        assert_eq!(p.deep_owner(0, v(6.0, -10.0), 5.0), Some(1));
        assert_eq!(
            p.deep_owner(0, v(-6.0, -10.0), 5.0),
            None,
            "sigue en la suya"
        );
        assert_eq!(p.deep_owner(0, v(6.0, 6.0), 5.0), Some(3), "diagonal");
    }

    #[test]
    fn limites_y_valores_hostiles() {
        assert!(Partition::grid(0, 1, 512.0).is_err());
        assert!(Partition::grid(65, 1, 512.0).is_err());
        assert!(Partition::grid(2, 2, 0.0).is_err());
        assert!(Partition::grid(2, 2, f32::NAN).is_err());
        let p = Partition::grid(2, 2, 512.0).unwrap();
        assert_eq!(p.owner_of(v(f32::NAN, 0.0)), 0);
        assert_eq!(
            p.owner_of(v(f32::INFINITY, f32::NEG_INFINITY)),
            p.id_at(1, 0)
        );
        assert_eq!(p.owner_of(v(1e30, 1e30)), 3);
        assert!(p.is_valid(3) && !p.is_valid(4));
        assert_eq!(p.tile_size(), 512.0);
    }

    #[test]
    fn recortar_dentro_de_la_region() {
        let p = Partition::grid(2, 1, 512.0).unwrap();
        let r = p.rect(1); // x >= 0
        let c = r.clamp_inside(v(-50.0, 7.0), 1.0);
        assert_eq!((c.x, c.z), (1.0, 7.0));
        assert!(r.contains(c));
        let far = r.clamp_inside(v(900.0, 7.0), 1.0);
        assert_eq!(far.x, 900.0);
        assert!(r.contains(far));
    }
}
