//! Mundo continuo para MMO: rejilla espacial 3D, gestión de interés (quién ve a quién),
//! regiones activas (solo se simula donde hay observadores) y reparto de celdas en shards.
//!
//! Todo es determinista: los resultados se devuelven ordenados.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// Hash multiplicativo rápido (el de rustc, "Fx") para los mapas internos del mundo.
///
/// El hash estándar de Rust (SipHash) resiste ataques de colisiones a costa de velocidad. Aquí las
/// claves son identificadores de entidad y coordenadas de celda que decide el servidor, no texto que
/// controle un atacante, y estas consultas están en el camino caliente de cada tick.
#[derive(Default, Clone, Copy)]
pub struct FastHasher(u64);

impl FastHasher {
    #[inline]
    fn add(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(b as u64);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
}

pub type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;
pub type FastSet<T> = HashSet<T, BuildHasherDefault<FastHasher>>;
/// Conjunto de celdas (lo que devuelve [`World::active_cells`]).
pub type CellSet = FastSet<Cell>;

pub type EntityId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Vec3 { x, y, z }
    }
    pub fn dist2(self, o: Vec3) -> f32 {
        let (dx, dy, dz) = (self.x - o.x, self.y - o.y, self.z - o.z);
        dx * dx + dy * dy + dz * dz
    }
}

/// Coordenadas enteras de una celda de la rejilla.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cell {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// Cambio de celda de una entidad.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellMove {
    pub id: EntityId,
    pub from: Cell,
    pub to: Cell,
}

pub struct World {
    cell_size: f32,
    cells: FastMap<Cell, Vec<EntityId>>,
    entities: FastMap<EntityId, (Vec3, Cell)>,
}

impl World {
    pub fn new(cell_size: f32) -> Self {
        assert!(cell_size > 0.0);
        World {
            cell_size,
            cells: FastMap::default(),
            entities: FastMap::default(),
        }
    }

    pub fn cell_size(&self) -> f32 {
        self.cell_size
    }

    pub fn cell_of(&self, p: Vec3) -> Cell {
        let f = |v: f32| (v / self.cell_size).floor() as i32;
        Cell {
            x: f(p.x),
            y: f(p.y),
            z: f(p.z),
        }
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
    pub fn position(&self, id: EntityId) -> Option<Vec3> {
        self.entities.get(&id).map(|e| e.0)
    }
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }
    pub fn entities_in(&self, cell: Cell) -> &[EntityId] {
        self.cells.get(&cell).map_or(&[], |v| v.as_slice())
    }

    /// Crea una entidad. Devuelve `false` si el id ya existe.
    pub fn spawn(&mut self, id: EntityId, pos: Vec3) -> bool {
        if self.entities.contains_key(&id) {
            return false;
        }
        let cell = self.cell_of(pos);
        self.entities.insert(id, (pos, cell));
        self.cells.entry(cell).or_default().push(id);
        true
    }

    fn remove_from_cell(&mut self, id: EntityId, cell: Cell) {
        if let Some(v) = self.cells.get_mut(&cell) {
            if let Some(i) = v.iter().position(|&e| e == id) {
                v.swap_remove(i);
            }
            if v.is_empty() {
                self.cells.remove(&cell); // las celdas vacías no ocupan memoria
            }
        }
    }

    pub fn despawn(&mut self, id: EntityId) -> bool {
        match self.entities.remove(&id) {
            Some((_, cell)) => {
                self.remove_from_cell(id, cell);
                true
            }
            None => false,
        }
    }

    /// Mueve una entidad. Devuelve `Some` solo si cambió de celda.
    pub fn move_to(&mut self, id: EntityId, pos: Vec3) -> Option<CellMove> {
        let new_cell = self.cell_of(pos);
        let (p, cell) = self.entities.get_mut(&id)?;
        *p = pos;
        if *cell == new_cell {
            return None;
        }
        let from = *cell;
        *cell = new_cell;
        self.remove_from_cell(id, from);
        self.cells.entry(new_cell).or_default().push(id);
        Some(CellMove {
            id,
            from,
            to: new_cell,
        })
    }

    /// Entidades a distancia `<= r` de `center`, ordenadas por id.
    pub fn query_radius(&self, center: Vec3, r: f32) -> Vec<EntityId> {
        self.query_radius_d2(center, r)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// Como [`World::query_radius`], pero devuelve también el cuadrado de la distancia de cada una
    /// (evita repetir la búsqueda de posiciones). Ordenado por id.
    pub fn query_radius_d2(&self, center: Vec3, r: f32) -> Vec<(EntityId, f32)> {
        let r2 = r * r;
        let lo = self.cell_of(Vec3::new(center.x - r, center.y - r, center.z - r));
        let hi = self.cell_of(Vec3::new(center.x + r, center.y + r, center.z + r));
        let span = (hi.x - lo.x + 1) as i64 * (hi.y - lo.y + 1) as i64 * (hi.z - lo.z + 1) as i64;
        let mut out = Vec::new();
        let mut visit = |ids: &[EntityId]| {
            for &id in ids {
                let d2 = self.entities[&id].0.dist2(center);
                if d2 <= r2 {
                    out.push((id, d2));
                }
            }
        };
        if span > self.cells.len() as i64 {
            // Radio enorme: es más barato recorrer las celdas ocupadas.
            for ids in self.cells.values() {
                visit(ids);
            }
        } else {
            for x in lo.x..=hi.x {
                for y in lo.y..=hi.y {
                    for z in lo.z..=hi.z {
                        if let Some(ids) = self.cells.get(&Cell { x, y, z }) {
                            visit(ids);
                        }
                    }
                }
            }
        }
        out.sort_unstable_by_key(|&(id, _)| id);
        out
    }

    /// Celdas ocupadas que tocan alguna esfera `(centro, radio)`: las regiones que deben simularse.
    pub fn active_cells(&self, spheres: &[(Vec3, f32)]) -> CellSet {
        let mut set = CellSet::default();
        for &(c, r) in spheres {
            let lo = self.cell_of(Vec3::new(c.x - r, c.y - r, c.z - r));
            let hi = self.cell_of(Vec3::new(c.x + r, c.y + r, c.z + r));
            let span =
                (hi.x - lo.x + 1) as i64 * (hi.y - lo.y + 1) as i64 * (hi.z - lo.z + 1) as i64;
            if span > self.cells.len() as i64 {
                for cell in self.cells.keys() {
                    if (lo.x..=hi.x).contains(&cell.x)
                        && (lo.y..=hi.y).contains(&cell.y)
                        && (lo.z..=hi.z).contains(&cell.z)
                    {
                        set.insert(*cell);
                    }
                }
            } else {
                for x in lo.x..=hi.x {
                    for y in lo.y..=hi.y {
                        for z in lo.z..=hi.z {
                            let cell = Cell { x, y, z };
                            if self.cells.contains_key(&cell) {
                                set.insert(cell);
                            }
                        }
                    }
                }
            }
        }
        set
    }

    /// Entidades que están en las celdas dadas (las únicas que hace falta simular), ordenadas.
    pub fn entities_in_cells(&self, cells: &CellSet) -> Vec<EntityId> {
        let mut v: Vec<EntityId> = cells
            .iter()
            .flat_map(|c| self.entities_in(*c).iter().copied())
            .collect();
        v.sort_unstable();
        v
    }
}

/// Cambios de visibilidad de un observador entre dos actualizaciones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterestDelta {
    pub enter: Vec<EntityId>,
    pub leave: Vec<EntityId>,
}

/// Gestión de interés: cada observador solo recibe lo que está cerca.
/// Usa histéresis: algo que ya se veía sigue visible hasta `radio * hysteresis`, para que
/// una entidad en el borde no entre y salga en cada tick.
pub struct Interest {
    hysteresis: f32,
    /// Lo que ve cada observador, siempre ordenado por id.
    seen: FastMap<EntityId, Vec<EntityId>>,
}

impl Interest {
    pub fn new(hysteresis: f32) -> Self {
        assert!(hysteresis >= 1.0);
        Interest {
            hysteresis,
            seen: FastMap::default(),
        }
    }

    /// Conjunto visible actualmente por un observador (ordenado).
    pub fn visible(&self, observer: EntityId) -> Vec<EntityId> {
        self.visible_ids(observer).to_vec()
    }

    /// Lo mismo, sin copiar.
    pub fn visible_ids(&self, observer: EntityId) -> &[EntityId] {
        self.seen.get(&observer).map_or(&[], |v| v.as_slice())
    }

    /// Recalcula la visibilidad de cada `(observador, radio)` y devuelve los cambios.
    /// Los observadores que ya no están en la lista (o en el mundo) se olvidan.
    pub fn update(
        &mut self,
        world: &World,
        observers: &[(EntityId, f32)],
    ) -> HashMap<EntityId, InterestDelta> {
        let mut deltas = HashMap::new();
        let keep: FastSet<EntityId> = observers.iter().map(|o| o.0).collect();
        self.seen.retain(|k, _| keep.contains(k));
        for &(obs, radius) in observers {
            let Some(pos) = world.position(obs) else {
                self.seen.remove(&obs);
                continue;
            };
            let prev = self.seen.remove(&obs).unwrap_or_default();
            let r2 = radius * radius;
            // Los candidatos salen ordenados por id, así que `now` también lo está.
            let now: Vec<EntityId> = world
                .query_radius_d2(pos, radius * self.hysteresis)
                .into_iter()
                .filter(|&(id, d2)| id != obs && (d2 <= r2 || prev.binary_search(&id).is_ok()))
                .map(|(id, _)| id)
                .collect();
            let (enter, leave) = diff_sorted(&prev, &now);
            self.seen.insert(obs, now);
            deltas.insert(obs, InterestDelta { enter, leave });
        }
        deltas
    }
}

/// `(en `now` y no en `prev`, en `prev` y no en `now`)` para dos listas ordenadas y sin repetidos.
fn diff_sorted(prev: &[EntityId], now: &[EntityId]) -> (Vec<EntityId>, Vec<EntityId>) {
    let (mut enter, mut leave) = (Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while i < prev.len() && j < now.len() {
        match prev[i].cmp(&now[j]) {
            std::cmp::Ordering::Less => {
                leave.push(prev[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                enter.push(now[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    leave.extend_from_slice(&prev[i..]);
    enter.extend_from_slice(&now[j..]);
    (enter, leave)
}

/// Reparto de regiones del mundo entre servidores (shards) con *rendezvous hashing*:
/// al añadir o quitar un shard solo se mueve la parte de regiones que corresponde.
pub struct ShardMap {
    region_cells: i32,
    shards: Vec<u32>,
}

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl ShardMap {
    /// `region_cells`: lado de una región, en celdas. Cada región entera pertenece a un shard.
    pub fn new(region_cells: i32, shards: Vec<u32>) -> Self {
        assert!(region_cells > 0 && !shards.is_empty());
        ShardMap {
            region_cells,
            shards,
        }
    }

    pub fn shards(&self) -> &[u32] {
        &self.shards
    }
    pub fn add_shard(&mut self, id: u32) {
        if !self.shards.contains(&id) {
            self.shards.push(id);
        }
    }
    pub fn remove_shard(&mut self, id: u32) {
        assert!(self.shards.len() > 1, "debe quedar al menos un shard");
        self.shards.retain(|&s| s != id);
    }

    pub fn region_of(&self, c: Cell) -> (i32, i32, i32) {
        (
            c.x.div_euclid(self.region_cells),
            c.y.div_euclid(self.region_cells),
            c.z.div_euclid(self.region_cells),
        )
    }

    pub fn shard_of(&self, c: Cell) -> u32 {
        let (rx, ry, rz) = self.region_of(c);
        let key = mix(rx as u32 as u64 | (ry as u32 as u64) << 21) ^ mix(rz as u32 as u64);
        *self
            .shards
            .iter()
            .max_by_key(|&&s| mix(key ^ mix(s as u64 + 1)))
            .unwrap()
    }

    /// Si el cambio de celda cruza a otro shard, devuelve `(origen, destino)`: hay que migrar la entidad.
    pub fn migration(&self, mv: &CellMove) -> Option<(u32, u32)> {
        let (a, b) = (self.shard_of(mv.from), self.shard_of(mv.to));
        (a != b).then_some((a, b))
    }
}

/// Generador pseudoaleatorio pequeño y determinista (xorshift64*), para pruebas y demos.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Número en `[0, 1)`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poblar(n: u64, lado: f32, seed: u64) -> (World, Vec<Vec3>) {
        let mut w = World::new(50.0);
        let mut rng = Rng::new(seed);
        let mut pos = vec![];
        for id in 0..n {
            let p = Vec3::new(
                rng.range(-lado, lado),
                rng.range(-5.0, 5.0),
                rng.range(-lado, lado),
            );
            assert!(w.spawn(id, p));
            pos.push(p);
        }
        (w, pos)
    }

    #[test]
    fn spawn_despawn_y_celdas_vacias() {
        let mut w = World::new(10.0);
        assert!(w.spawn(1, Vec3::new(1.0, 1.0, 1.0)));
        assert!(!w.spawn(1, Vec3::new(2.0, 2.0, 2.0)));
        assert_eq!(w.cell_count(), 1);
        assert!(w.despawn(1));
        assert!(!w.despawn(1));
        assert_eq!(w.cell_count(), 0);
        assert!(w.is_empty());
    }

    #[test]
    fn celdas_con_coordenadas_negativas() {
        let w = World::new(10.0);
        assert_eq!(
            w.cell_of(Vec3::new(-0.1, 0.0, 9.9)),
            Cell { x: -1, y: 0, z: 0 }
        );
        assert_eq!(
            w.cell_of(Vec3::new(-10.0, -10.1, 10.0)),
            Cell { x: -1, y: -2, z: 1 }
        );
    }

    #[test]
    fn mover_reporta_cambio_de_celda() {
        let mut w = World::new(10.0);
        w.spawn(1, Vec3::new(1.0, 0.0, 0.0));
        assert_eq!(w.move_to(1, Vec3::new(5.0, 0.0, 0.0)), None);
        let mv = w.move_to(1, Vec3::new(15.0, 0.0, 0.0)).unwrap();
        assert_eq!((mv.from.x, mv.to.x), (0, 1));
        assert_eq!(w.entities_in(Cell { x: 0, y: 0, z: 0 }), &[] as &[EntityId]);
        assert_eq!(w.move_to(99, Vec3::default()), None);
    }

    #[test]
    fn consulta_igual_a_fuerza_bruta() {
        let (w, pos) = poblar(3000, 500.0, 7);
        let mut rng = Rng::new(99);
        for _ in 0..50 {
            let c = Vec3::new(rng.range(-500.0, 500.0), 0.0, rng.range(-500.0, 500.0));
            let r = rng.range(10.0, 300.0);
            let esperado: Vec<EntityId> = (0..pos.len() as u64)
                .filter(|&i| pos[i as usize].dist2(c) <= r * r)
                .collect();
            assert_eq!(w.query_radius(c, r), esperado);
        }
        // radio enorme: recorre celdas ocupadas
        assert_eq!(w.query_radius(Vec3::default(), 1.0e6).len(), 3000);
    }

    #[test]
    fn interes_enter_leave_e_histeresis() {
        let mut w = World::new(10.0);
        w.spawn(1, Vec3::new(0.0, 0.0, 0.0)); // observador
        w.spawn(2, Vec3::new(9.0, 0.0, 0.0));
        let mut im = Interest::new(1.2);
        let d = im.update(&w, &[(1, 10.0)]);
        assert_eq!(
            d[&1],
            InterestDelta {
                enter: vec![2],
                leave: vec![]
            }
        );
        // 11 está fuera de 10 pero dentro de 12: sigue visible (histéresis)
        w.move_to(2, Vec3::new(11.0, 0.0, 0.0));
        assert_eq!(im.update(&w, &[(1, 10.0)])[&1], InterestDelta::default());
        assert_eq!(im.visible(1), vec![2]);
        // 13 > 12: sale
        w.move_to(2, Vec3::new(13.0, 0.0, 0.0));
        assert_eq!(
            im.update(&w, &[(1, 10.0)])[&1],
            InterestDelta {
                enter: vec![],
                leave: vec![2]
            }
        );
        // y para volver a entrar tiene que estar dentro del radio estricto
        w.move_to(2, Vec3::new(11.0, 0.0, 0.0));
        assert_eq!(im.update(&w, &[(1, 10.0)])[&1], InterestDelta::default());
        w.move_to(2, Vec3::new(9.0, 0.0, 0.0));
        assert_eq!(im.update(&w, &[(1, 10.0)])[&1].enter, vec![2]);
    }

    #[test]
    fn regiones_activas_ahorran_simulacion() {
        let (w, _) = poblar(5000, 2000.0, 3);
        let obs = [(Vec3::new(0.0, 0.0, 0.0), 120.0)];
        let celdas = w.active_cells(&obs);
        let a_simular = w.entities_in_cells(&celdas);
        assert!(!a_simular.is_empty());
        assert!(
            a_simular.len() < w.len() / 10,
            "debería simularse una fracción pequeña"
        );
        // todo lo que ve el observador está dentro de lo que se simula
        let ids: HashSet<EntityId> = a_simular.into_iter().collect();
        for id in w.query_radius(obs[0].0, obs[0].1) {
            assert!(ids.contains(&id));
        }
        // radio enorme = todo
        let todo = w.active_cells(&[(Vec3::default(), 1.0e6)]);
        assert_eq!(w.entities_in_cells(&todo).len(), 5000);
        // sin observadores no se simula nada
        assert!(w.active_cells(&[]).is_empty());
    }

    #[test]
    fn shards_deterministas_y_equilibrados() {
        let m = ShardMap::new(4, vec![1, 2, 3, 4]);
        let mut cuenta = HashMap::new();
        for x in -50..50 {
            for z in -50..50 {
                *cuenta
                    .entry(m.shard_of(Cell { x, y: 0, z }))
                    .or_insert(0u32) += 1;
            }
        }
        let total: u32 = cuenta.values().sum();
        assert_eq!(total, 10_000);
        for s in [1, 2, 3, 4] {
            let c = cuenta[&s] as f32 / total as f32;
            assert!((0.15..0.35).contains(&c), "shard {s} con {c}");
        }
        // celdas de una misma región comparten shard
        assert_eq!(
            m.shard_of(Cell { x: 0, y: 0, z: 0 }),
            m.shard_of(Cell { x: 3, y: 3, z: 3 })
        );
        // determinista
        assert_eq!(
            m.shard_of(Cell { x: -7, y: 2, z: 9 }),
            ShardMap::new(4, vec![1, 2, 3, 4]).shard_of(Cell { x: -7, y: 2, z: 9 })
        );
    }

    #[test]
    fn anadir_shard_mueve_pocas_regiones() {
        let mut m = ShardMap::new(1, vec![1, 2, 3, 4]);
        let celdas: Vec<Cell> = (0..100)
            .flat_map(|x| (0..100).map(move |z| Cell { x, y: 0, z }))
            .collect();
        let antes: Vec<u32> = celdas.iter().map(|&c| m.shard_of(c)).collect();
        m.add_shard(5);
        let movidas = celdas
            .iter()
            .zip(&antes)
            .filter(|(&c, &a)| m.shard_of(c) != a)
            .count();
        let f = movidas as f32 / celdas.len() as f32;
        assert!((0.12..0.30).contains(&f), "se movió {f}"); // ideal: 1/5
                                                            // y todo lo movido va al shard nuevo
        for (&c, &a) in celdas.iter().zip(&antes) {
            let ahora = m.shard_of(c);
            assert!(ahora == a || ahora == 5);
        }
        m.remove_shard(5);
        let despues: Vec<u32> = celdas.iter().map(|&c| m.shard_of(c)).collect();
        assert_eq!(antes, despues);
    }

    #[test]
    fn migracion_al_cruzar_frontera() {
        let m = ShardMap::new(1, vec![1, 2]);
        let mut w = World::new(10.0);
        w.spawn(1, Vec3::new(5.0, 0.0, 5.0));
        let mut cruces = 0;
        for i in 1..200 {
            if let Some(mv) = w.move_to(1, Vec3::new(5.0 + i as f32 * 10.0, 0.0, 5.0)) {
                if let Some((a, b)) = m.migration(&mv) {
                    assert_ne!(a, b);
                    cruces += 1;
                }
            }
        }
        assert!(cruces > 0);
    }

    #[test]
    fn rng_es_determinista() {
        let (mut a, mut b) = (Rng::new(5), Rng::new(5));
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let u = Rng::new(1).unit();
        assert!((0.0..1.0).contains(&u));
    }

    #[test]
    fn query_con_distancias_coincide_con_fuerza_bruta() {
        let mut rng = Rng::new(5);
        let mut w = World::new(10.0);
        let mut all = Vec::new();
        for id in 0..2000u64 {
            let p = Vec3::new(
                rng.range(-200.0, 200.0),
                rng.range(-20.0, 20.0),
                rng.range(-200.0, 200.0),
            );
            w.spawn(id, p);
            all.push((id, p));
        }
        for _ in 0..50 {
            let c = Vec3::new(
                rng.range(-200.0, 200.0),
                rng.range(-20.0, 20.0),
                rng.range(-200.0, 200.0),
            );
            let r = rng.range(1.0, 120.0);
            let got = w.query_radius_d2(c, r);
            let want: Vec<(u64, f32)> = all
                .iter()
                .map(|&(id, p)| (id, p.dist2(c)))
                .filter(|&(_, d)| d <= r * r)
                .collect();
            assert_eq!(got, want);
            assert_eq!(
                w.query_radius(c, r),
                want.iter().map(|x| x.0).collect::<Vec<_>>()
            );
        }
    }

    /// Referencia ingenua (con conjuntos) de lo que debe producir `Interest::update`.
    fn interes_ingenuo(
        w: &World,
        prev: &HashMap<u64, HashSet<u64>>,
        obs: &[(u64, f32)],
        hyst: f32,
    ) -> HashMap<u64, HashSet<u64>> {
        let mut out = HashMap::new();
        for &(o, r) in obs {
            let pos = w.position(o).unwrap();
            let empty = HashSet::new();
            let before = prev.get(&o).unwrap_or(&empty);
            let mut now = HashSet::new();
            for id in w.query_radius(pos, r * hyst) {
                if id != o && (w.position(id).unwrap().dist2(pos) <= r * r || before.contains(&id))
                {
                    now.insert(id);
                }
            }
            out.insert(o, now);
        }
        out
    }

    #[test]
    fn interes_coincide_con_la_referencia_ingenua_durante_cientos_de_ticks() {
        let mut rng = Rng::new(9);
        let mut w = World::new(16.0);
        let mut im = Interest::new(1.3);
        for id in 0..400u64 {
            w.spawn(
                id,
                Vec3::new(rng.range(-150.0, 150.0), 0.0, rng.range(-150.0, 150.0)),
            );
        }
        let observers: Vec<(u64, f32)> = (0..20u64).map(|i| (i, 30.0 + i as f32)).collect();
        let mut reference: HashMap<u64, HashSet<u64>> = HashMap::new();
        for tick in 0..300 {
            for id in 0..400u64 {
                let Some(p) = w.position(id) else { continue };
                w.move_to(
                    id,
                    Vec3::new(p.x + rng.range(-3.0, 3.0), 0.0, p.z + rng.range(-3.0, 3.0)),
                );
            }
            if tick % 50 == 25 {
                w.despawn(100 + tick as u64 / 50);
            }
            let deltas = im.update(&w, &observers);
            reference = interes_ingenuo(&w, &reference, &observers, 1.3);
            for &(o, _) in &observers {
                let want: Vec<u64> = {
                    let mut v: Vec<u64> = reference[&o].iter().copied().collect();
                    v.sort_unstable();
                    v
                };
                assert_eq!(im.visible_ids(o), &want[..], "tick {tick} observador {o}");
                // Los deltas son consistentes: lo visible = lo anterior + entradas - salidas.
                let d = &deltas[&o];
                assert!(d.enter.iter().all(|e| want.contains(e)));
                assert!(d.leave.iter().all(|l| !want.contains(l)));
            }
        }
    }

    #[test]
    fn diff_ordenado() {
        assert_eq!(diff_sorted(&[], &[]), (vec![], vec![]));
        assert_eq!(diff_sorted(&[1, 2, 3], &[]), (vec![], vec![1, 2, 3]));
        assert_eq!(diff_sorted(&[], &[1, 2]), (vec![1, 2], vec![]));
        assert_eq!(
            diff_sorted(&[1, 3, 5, 9], &[2, 3, 6, 9, 10]),
            (vec![2, 6, 10], vec![1, 5])
        );
    }

    #[test]
    fn hash_rapido_reparte_bien_claves_secuenciales() {
        let mut m: FastMap<u64, u64> = FastMap::default();
        for i in 0..100_000u64 {
            m.insert(i, i * 2);
        }
        assert_eq!(m.len(), 100_000);
        assert!((0..100_000u64).all(|i| m[&i] == i * 2));
        let mut cells: CellSet = CellSet::default();
        for x in -50..50 {
            for z in -50..50 {
                cells.insert(Cell { x, y: 0, z });
            }
        }
        assert_eq!(cells.len(), 10_000);
    }
}
