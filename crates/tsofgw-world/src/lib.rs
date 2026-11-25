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

/// Ficha de una entidad: dónde está y en qué hueco de la lista de su celda.
#[derive(Clone, Copy)]
struct Rec {
    pos: Vec3,
    cell: Cell,
    slot: u32,
}

/// El mundo: una rejilla de celdas. Cada celda guarda sus entidades **con su posición**, de modo que una
/// consulta por radio recorre memoria contigua en lugar de hacer una búsqueda hash por candidato.
pub struct World {
    cell_size: f32,
    cells: FastMap<Cell, Vec<(EntityId, Vec3)>>,
    entities: FastMap<EntityId, Rec>,
    /// Cota (solo crece) de las capas verticales que alguna vez tuvieron entidades. En un mundo casi plano
    /// recorta las consultas a una capa en lugar de decenas; nunca excluye una celda ocupada.
    y_layers: (i32, i32),
}

impl World {
    pub fn new(cell_size: f32) -> Self {
        assert!(cell_size > 0.0);
        World {
            cell_size,
            cells: FastMap::default(),
            entities: FastMap::default(),
            y_layers: (i32::MAX, i32::MIN),
        }
    }

    pub fn cell_size(&self) -> f32 {
        self.cell_size
    }

    /// Recorta un rango de capas verticales a las que alguna vez tuvieron entidades (vacío: `(0, -1)`).
    fn clamp_y(&self, lo: i32, hi: i32) -> (i32, i32) {
        let (lo, hi) = (lo.max(self.y_layers.0), hi.min(self.y_layers.1));
        if lo > hi {
            (0, -1)
        } else {
            (lo, hi)
        }
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
        self.entities.get(&id).map(|e| e.pos)
    }
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }
    /// Entidades de una celda (sin orden concreto).
    pub fn entities_in(&self, cell: Cell) -> impl Iterator<Item = EntityId> + '_ {
        self.cells
            .get(&cell)
            .into_iter()
            .flat_map(|v| v.iter().map(|e| e.0))
    }

    /// Añade `(id, pos)` a la lista de `cell` y devuelve el hueco que ocupa.
    fn push_to_cell(&mut self, cell: Cell, id: EntityId, pos: Vec3) -> u32 {
        self.y_layers = (self.y_layers.0.min(cell.y), self.y_layers.1.max(cell.y));
        let v = self.cells.entry(cell).or_default();
        v.push((id, pos));
        (v.len() - 1) as u32
    }

    /// Quita el hueco `slot` de la lista de `cell`; la última entidad ocupa su sitio y se actualiza su ficha.
    fn remove_from_cell(&mut self, cell: Cell, slot: u32) {
        let Some(v) = self.cells.get_mut(&cell) else {
            return;
        };
        v.swap_remove(slot as usize);
        if let Some(&(moved, _)) = v.get(slot as usize) {
            if let Some(r) = self.entities.get_mut(&moved) {
                r.slot = slot;
            }
        }
        if v.is_empty() {
            self.cells.remove(&cell); // las celdas vacías no ocupan memoria
        }
    }

    /// Crea una entidad. Devuelve `false` si el id ya existe.
    pub fn spawn(&mut self, id: EntityId, pos: Vec3) -> bool {
        if self.entities.contains_key(&id) {
            return false;
        }
        let cell = self.cell_of(pos);
        let slot = self.push_to_cell(cell, id, pos);
        self.entities.insert(id, Rec { pos, cell, slot });
        true
    }

    pub fn despawn(&mut self, id: EntityId) -> bool {
        match self.entities.remove(&id) {
            Some(r) => {
                self.remove_from_cell(r.cell, r.slot);
                true
            }
            None => false,
        }
    }

    /// Mueve una entidad. Devuelve `Some` solo si cambió de celda.
    pub fn move_to(&mut self, id: EntityId, pos: Vec3) -> Option<CellMove> {
        let new_cell = self.cell_of(pos);
        let r = self.entities.get_mut(&id)?;
        r.pos = pos;
        if r.cell == new_cell {
            let (cell, slot) = (r.cell, r.slot);
            if let Some(v) = self.cells.get_mut(&cell) {
                v[slot as usize].1 = pos;
            }
            return None;
        }
        let (from, old_slot) = (r.cell, r.slot);
        self.remove_from_cell(from, old_slot);
        let slot = self.push_to_cell(new_cell, id, pos);
        let r = self.entities.get_mut(&id)?;
        r.cell = new_cell;
        r.slot = slot;
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
        let mut lo = self.cell_of(Vec3::new(center.x - r, center.y - r, center.z - r));
        let mut hi = self.cell_of(Vec3::new(center.x + r, center.y + r, center.z + r));
        (lo.y, hi.y) = self.clamp_y(lo.y, hi.y);
        let span = (hi.x - lo.x + 1) as i64 * (hi.y - lo.y + 1) as i64 * (hi.z - lo.z + 1) as i64;
        let mut out = Vec::new();
        let mut visit = |items: &[(EntityId, Vec3)]| {
            for &(id, p) in items {
                let d2 = p.dist2(center);
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
            let mut lo = self.cell_of(Vec3::new(c.x - r, c.y - r, c.z - r));
            let mut hi = self.cell_of(Vec3::new(c.x + r, c.y + r, c.z + r));
            (lo.y, hi.y) = self.clamp_y(lo.y, hi.y);
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
        let mut v: Vec<EntityId> = cells.iter().flat_map(|c| self.entities_in(*c)).collect();
        v.sort_unstable();
        v
    }
}

/// Las entidades que rodean una celda, reunidas **una sola vez** para todos los observadores que están en
/// ella (en una multitud, decenas de jugadores comparten casi la misma vecindad).
///
/// Cada observador solo tiene que filtrar por distancia una lista contigua que ya está ordenada por id,
/// así que se ahorran las consultas a la rejilla y la ordenación por observador. El resultado es
/// exactamente el de [`Interest::step`].
pub struct Neighborhood {
    cell: Cell,
    cell_size: f32,
    reach: f32,
    /// `(id, posición)` ordenado por id.
    items: Vec<(EntityId, Vec3)>,
}

impl World {
    /// Reúne las entidades que pueden estar a menos de `reach` de cualquier punto de `cell`
    /// (con un margen de una celda extra, para que el redondeo de coma flotante no importe).
    pub fn neighborhood(&self, cell: Cell, reach: f32) -> Neighborhood {
        let k = (reach / self.cell_size).ceil() as i32 + 1;
        let mut items: Vec<(EntityId, Vec3)> = Vec::new();
        for x in cell.x - k..=cell.x + k {
            let (y0, y1) = self.clamp_y(cell.y - k, cell.y + k);
            for y in y0..=y1 {
                for z in cell.z - k..=cell.z + k {
                    if let Some(v) = self.cells.get(&Cell { x, y, z }) {
                        items.extend_from_slice(v);
                    }
                }
            }
        }
        items.sort_unstable_by_key(|e| e.0);
        Neighborhood {
            cell,
            cell_size: self.cell_size,
            reach,
            items,
        }
    }
}

impl Neighborhood {
    pub fn cell(&self) -> Cell {
        self.cell
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Lo mismo que [`Interest::step`], a partir de la vecindad ya reunida. Devuelve `None` si esta
    /// vecindad no sirve para ese observador (no está en la celda, o su radio con histéresis supera
    /// el alcance reunido); entonces hay que usar `Interest::step`.
    pub fn visible(
        &self,
        hysteresis: f32,
        observer: EntityId,
        pos: Vec3,
        radius: f32,
        prev: &[EntityId],
    ) -> Option<Vec<EntityId>> {
        let rq = radius * hysteresis;
        let f = |v: f32| (v / self.cell_size).floor() as i32;
        let here = Cell {
            x: f(pos.x),
            y: f(pos.y),
            z: f(pos.z),
        };
        if rq > self.reach || here != self.cell {
            return None;
        }
        let (r2, rq2) = (radius * radius, rq * rq);
        let mut out = Vec::with_capacity(prev.len() + 16);
        for &(id, p) in &self.items {
            if id == observer {
                continue;
            }
            let d2 = p.dist2(pos);
            if d2 <= r2 || (d2 <= rq2 && prev.binary_search(&id).is_ok()) {
                out.push(id);
            }
        }
        Some(out)
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

    /// Núcleo puro de una actualización: lo que `observer` (en `pos`) ve ahora, dado lo que veía antes.
    ///
    /// No toca ningún estado compartido: solo lee `world`. Por eso puede ejecutarse a la vez para muchos
    /// observadores en hilos distintos y el resultado es el mismo que en un solo hilo. `prev` y la
    /// salida están ordenados por id.
    pub fn step(
        world: &World,
        hysteresis: f32,
        observer: EntityId,
        pos: Vec3,
        radius: f32,
        prev: &[EntityId],
    ) -> Vec<EntityId> {
        let r2 = radius * radius;
        // Los candidatos salen ordenados por id, así que el resultado también lo está.
        world
            .query_radius_d2(pos, radius * hysteresis)
            .into_iter()
            .filter(|&(id, d2)| id != observer && (d2 <= r2 || prev.binary_search(&id).is_ok()))
            .map(|(id, _)| id)
            .collect()
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
            let now = Interest::step(world, self.hysteresis, obs, pos, radius, &prev);
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
    /// Estado interno (nunca es 0): permite llevar un generador a otro servidor y que siga igual.
    pub fn state(&self) -> u64 {
        self.0
    }
    /// Reconstruye un generador a partir de [`Rng::state`].
    pub fn from_state(state: u64) -> Self {
        Rng(state.max(1))
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
        assert_eq!(w.entities_in(Cell { x: 0, y: 0, z: 0 }).count(), 0);
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

    impl World {
        /// Comprueba que cada ficha apunta a su hueco y que no sobra ni falta nada en las celdas.
        fn check_invariants(&self) {
            let mut in_cells = 0usize;
            for (cell, v) in &self.cells {
                assert!(!v.is_empty(), "celda vacía sin liberar");
                for (slot, &(id, pos)) in v.iter().enumerate() {
                    let r = self
                        .entities
                        .get(&id)
                        .expect("entidad en celda pero sin ficha");
                    assert_eq!(r.cell, *cell, "celda de {id}");
                    assert_eq!(r.slot as usize, slot, "hueco de {id}");
                    assert_eq!(r.pos, pos, "posición duplicada de {id}");
                    assert_eq!(self.cell_of(pos), *cell, "{id} fuera de su celda");
                }
                in_cells += v.len();
            }
            assert_eq!(in_cells, self.entities.len());
        }
    }

    #[test]
    fn la_rejilla_mantiene_sus_invariantes_bajo_miles_de_operaciones() {
        let mut rng = Rng::new(77);
        let mut w = World::new(8.0);
        let mut alive: Vec<u64> = Vec::new();
        let mut next = 0u64;
        for step in 0..20_000 {
            match rng.next_u64() % 10 {
                0..=2 => {
                    // Muchas entidades en pocas celdas, para que los huecos se reordenen a menudo.
                    let p = Vec3::new(rng.range(-20.0, 20.0), 0.0, rng.range(-20.0, 20.0));
                    assert!(w.spawn(next, p));
                    alive.push(next);
                    next += 1;
                }
                3 if !alive.is_empty() => {
                    let i = (rng.next_u64() as usize) % alive.len();
                    assert!(w.despawn(alive.swap_remove(i)));
                }
                _ if !alive.is_empty() => {
                    let id = alive[(rng.next_u64() as usize) % alive.len()];
                    let p = Vec3::new(rng.range(-22.0, 22.0), 0.0, rng.range(-22.0, 22.0));
                    w.move_to(id, p);
                    assert_eq!(w.position(id), Some(p));
                }
                _ => {}
            }
            if step % 500 == 0 {
                w.check_invariants();
                // Y las consultas siguen coincidiendo con la fuerza bruta.
                let c = Vec3::new(rng.range(-20.0, 20.0), 0.0, rng.range(-20.0, 20.0));
                let r = rng.range(1.0, 25.0);
                let mut brute: Vec<u64> = alive
                    .iter()
                    .copied()
                    .filter(|&id| w.position(id).unwrap().dist2(c) <= r * r)
                    .collect();
                brute.sort_unstable();
                assert_eq!(w.query_radius(c, r), brute, "paso {step}");
            }
        }
        w.check_invariants();
        assert_eq!(w.len(), alive.len());
        assert!(!w.despawn(u64::MAX));
    }

    #[test]
    fn la_vecindad_compartida_da_exactamente_lo_mismo_que_la_consulta_directa() {
        let mut rng = Rng::new(31);
        let mut w = World::new(16.0);
        for id in 0..1500u64 {
            // Algunas entidades pegadas a los bordes de celda y a distintas alturas.
            let (mut x, mut z) = (rng.range(-120.0, 120.0), rng.range(-120.0, 120.0));
            let y = rng.range(-20.0, 20.0);
            if rng.next_u64().is_multiple_of(5) {
                x = (x / 16.0).round() * 16.0;
            }
            if rng.next_u64().is_multiple_of(5) {
                z = (z / 16.0).round() * 16.0;
            }
            w.spawn(id, Vec3::new(x, y, z));
        }
        let hysteresis = 1.25f32;
        let mut checked = 0;
        let mut prev_sets: HashMap<u64, Vec<u64>> = HashMap::new();
        for round in 0..4 {
            for obs in 0..60u64 {
                let pos = w.position(obs).unwrap();
                let cell = w.cell_of(pos);
                let radius = 18.0 + (obs % 5) as f32 * 4.0;
                let hood = w.neighborhood(cell, radius * hysteresis);
                let prev = prev_sets.get(&obs).cloned().unwrap_or_default();
                let direct = Interest::step(&w, hysteresis, obs, pos, radius, &prev);
                let shared = hood
                    .visible(hysteresis, obs, pos, radius, &prev)
                    .expect("la vecindad debe servir");
                assert_eq!(shared, direct, "ronda {round} observador {obs}");
                prev_sets.insert(obs, direct);
                checked += 1;
            }
            // Las entidades se mueven y cambian de celda entre rondas.
            for id in 0..1500u64 {
                let p = w.position(id).unwrap();
                w.move_to(
                    id,
                    Vec3::new(p.x + rng.range(-9.0, 9.0), p.y, p.z + rng.range(-9.0, 9.0)),
                );
            }
        }
        assert_eq!(checked, 240);
        // No sirve (y lo dice) si el observador está en otra celda o si pide más alcance del reunido.
        let pos = w.position(3).unwrap();
        let hood = w.neighborhood(w.cell_of(pos), 20.0);
        assert!(hood.visible(1.25, 3, pos, 20.0, &[]).is_none(), "25 > 20");
        let far = Vec3::new(pos.x + 100.0, pos.y, pos.z);
        assert!(
            hood.visible(1.25, 3, far, 10.0, &[]).is_none(),
            "otra celda"
        );
        assert!(hood.visible(1.25, 3, pos, 10.0, &[]).is_some());
        assert!(!hood.is_empty() && hood.len() > 1);
    }

    #[test]
    fn el_generador_se_puede_trasladar_con_su_estado() {
        let mut a = Rng::new(77);
        for _ in 0..10 {
            a.next_u64();
        }
        let mut b = Rng::from_state(a.state());
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::from_state(0).state(), 0, "el estado 0 se evita");
    }

    #[test]
    fn el_recorte_vertical_nunca_excluye_entidades() {
        // Mundo vacío: ninguna consulta debe fallar ni desbordar.
        let mut w = World::new(16.0);
        assert!(w.query_radius(Vec3::new(0.0, 0.0, 0.0), 50.0).is_empty());
        assert!(w
            .active_cells(&[(Vec3::new(0.0, 0.0, 0.0), 50.0)])
            .is_empty());
        assert!(w.neighborhood(Cell { x: 0, y: 0, z: 0 }, 30.0).is_empty());
        // Entidades a alturas muy distintas, también negativas.
        w.spawn(1, Vec3::new(1.0, 0.0, 1.0));
        w.spawn(2, Vec3::new(2.0, 90.0, 2.0));
        w.spawn(3, Vec3::new(3.0, -90.0, 3.0));
        assert_eq!(w.query_radius(Vec3::new(0.0, 0.0, 0.0), 5.0), vec![1]);
        assert_eq!(w.query_radius(Vec3::new(0.0, 90.0, 0.0), 5.0), vec![2]);
        assert_eq!(w.query_radius(Vec3::new(0.0, -90.0, 0.0), 5.0), vec![3]);
        assert_eq!(
            w.query_radius(Vec3::new(0.0, 0.0, 0.0), 200.0),
            vec![1, 2, 3]
        );
        // Aunque se vacíen las capas, las consultas siguen siendo correctas.
        w.despawn(2);
        w.despawn(3);
        assert_eq!(
            w.query_radius(Vec3::new(0.0, 90.0, 0.0), 5.0),
            Vec::<u64>::new()
        );
        assert_eq!(w.query_radius(Vec3::new(0.0, 0.0, 0.0), 200.0), vec![1]);
        let hood = w.neighborhood(w.cell_of(Vec3::new(1.0, 0.0, 1.0)), 20.0);
        assert_eq!(hood.len(), 1);
    }

    #[test]
    fn step_es_puro_y_aplica_la_histeresis() {
        let mut w = World::new(16.0);
        for (id, x) in [(1u64, 0.0f32), (2, 9.0), (3, 11.0), (4, 14.0), (5, 40.0)] {
            w.spawn(id, Vec3::new(x, 0.0, 0.0));
        }
        let o = Vec3::new(0.0, 0.0, 0.0);
        // Sin pasado: solo lo que está dentro del radio (10), sin contarse a sí mismo.
        let now = Interest::step(&w, 1.5, 1, o, 10.0, &[]);
        assert_eq!(now, vec![2]);
        // Con pasado: lo que ya se veía sigue visible hasta radio × histéresis (15).
        let now = Interest::step(&w, 1.5, 1, o, 10.0, &[3, 4, 5]);
        assert_eq!(now, vec![2, 3, 4], "5 está fuera incluso con histéresis");
        // Mismas entradas, misma salida (no hay estado oculto).
        assert_eq!(now, Interest::step(&w, 1.5, 1, o, 10.0, &[3, 4, 5]));
        // Y `update` usa exactamente este núcleo.
        let mut im = Interest::new(1.5);
        im.update(&w, &[(1, 10.0)]);
        assert_eq!(im.visible_ids(1), &[2][..]);
        im.update(&w, &[(1, 10.0)]);
        assert_eq!(im.visible_ids(1), &[2][..]);
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
