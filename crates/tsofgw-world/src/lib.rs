//! Mundo continuo para MMO: rejilla espacial 3D, gestión de interés (quién ve a quién),
//! regiones activas (solo se simula donde hay observadores) y reparto de celdas en shards.
//!
//! Todo es determinista: los resultados se devuelven ordenados.

use std::collections::{HashMap, HashSet};

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
    cells: HashMap<Cell, Vec<EntityId>>,
    entities: HashMap<EntityId, (Vec3, Cell)>,
}

impl World {
    pub fn new(cell_size: f32) -> Self {
        assert!(cell_size > 0.0);
        World {
            cell_size,
            cells: HashMap::new(),
            entities: HashMap::new(),
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
        let r2 = r * r;
        let lo = self.cell_of(Vec3::new(center.x - r, center.y - r, center.z - r));
        let hi = self.cell_of(Vec3::new(center.x + r, center.y + r, center.z + r));
        let span = (hi.x - lo.x + 1) as i64 * (hi.y - lo.y + 1) as i64 * (hi.z - lo.z + 1) as i64;
        let mut out = Vec::new();
        let mut visit = |ids: &[EntityId]| {
            for &id in ids {
                if self.entities[&id].0.dist2(center) <= r2 {
                    out.push(id);
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
        out.sort_unstable();
        out
    }

    /// Celdas ocupadas que tocan alguna esfera `(centro, radio)`: las regiones que deben simularse.
    pub fn active_cells(&self, spheres: &[(Vec3, f32)]) -> HashSet<Cell> {
        let mut set = HashSet::new();
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
    pub fn entities_in_cells(&self, cells: &HashSet<Cell>) -> Vec<EntityId> {
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
    seen: HashMap<EntityId, HashSet<EntityId>>,
}

impl Interest {
    pub fn new(hysteresis: f32) -> Self {
        assert!(hysteresis >= 1.0);
        Interest {
            hysteresis,
            seen: HashMap::new(),
        }
    }

    /// Conjunto visible actualmente por un observador.
    pub fn visible(&self, observer: EntityId) -> Vec<EntityId> {
        let mut v: Vec<EntityId> = self
            .seen
            .get(&observer)
            .map_or(vec![], |s| s.iter().copied().collect());
        v.sort_unstable();
        v
    }

    /// Recalcula la visibilidad de cada `(observador, radio)` y devuelve los cambios.
    /// Los observadores que ya no están en la lista (o en el mundo) se olvidan.
    pub fn update(
        &mut self,
        world: &World,
        observers: &[(EntityId, f32)],
    ) -> HashMap<EntityId, InterestDelta> {
        let mut deltas = HashMap::new();
        let keep: HashSet<EntityId> = observers.iter().map(|o| o.0).collect();
        self.seen.retain(|k, _| keep.contains(k));
        for &(obs, radius) in observers {
            let Some(pos) = world.position(obs) else {
                self.seen.remove(&obs);
                continue;
            };
            let prev = self.seen.remove(&obs).unwrap_or_default();
            let mut now = HashSet::new();
            for id in world.query_radius(pos, radius * self.hysteresis) {
                if id == obs {
                    continue;
                }
                let d2 = world.position(id).map_or(f32::MAX, |p| p.dist2(pos));
                if d2 <= radius * radius || prev.contains(&id) {
                    now.insert(id);
                }
            }
            let mut enter: Vec<EntityId> = now.difference(&prev).copied().collect();
            let mut leave: Vec<EntityId> = prev.difference(&now).copied().collect();
            enter.sort_unstable();
            leave.sort_unstable();
            self.seen.insert(obs, now);
            deltas.insert(obs, InterestDelta { enter, leave });
        }
        deltas
    }
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
}
