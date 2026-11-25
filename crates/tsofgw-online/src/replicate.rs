//! Replicación hacia un jugador: interés, snapshot candidato, niveles de detalle y presupuesto de bytes.
//!
//! Todo lo que hace [`replicate`] es **independiente entre jugadores**: lee el mundo y los estados
//! (compartidos, de solo lectura) y modifica únicamente al propio jugador. Por eso el servidor lo
//! ejecuta en varios hilos a la vez sin que cambie ningún resultado.
//!
//! ## Niveles de detalle (LOD)
//! Lo que está lejos del observador se refresca con menos frecuencia: en los ticks en que no le toca,
//! el snapshot repite el **último valor que se le envió** (nunca uno más viejo: eso haría retroceder a la
//! entidad en pantalla). El escalonado `(tick + id) % cada` reparte los refrescos en el tiempo.
//!
//! Límite inherente al delta contra una línea base *confirmada*: un valor ya enviado pero aún sin
//! confirmar se sigue enviando en cada snapshot hasta que llega el ack (≈ un RTT). El ahorro de una
//! banda de `cada` ticks es, por tanto, aproximadamente `(cada − RTT_en_ticks) / cada`: se nota de
//! verdad con periodos largos (8 o más) y poco con periodos cortos.
//!
//! ## Presupuesto de bytes
//! Si el snapshot codificado supera `snapshot_budget`, solo se envían las diferencias de las entidades
//! más cercanas al observador; el resto se retrasa (se «sujeta» el valor anterior). Lo que se registra
//! como enviado es exactamente lo que el cliente reconstruirá, así que nada se desincroniza.

use std::time::Instant;

use tsofgw_net::{varint_len, zigzag_len, EntityMap, EntityState, Snapshot};
use tsofgw_transport::SessionId;
use tsofgw_world::{Cell, EntityId, FastMap, Interest, Neighborhood, World};

use crate::protocol as proto;
use crate::server::{vec3_from_q, GameConfig, Player};
use crate::states::Entities;

/// Una banda de nivel de detalle: lo que está a menos de `within` × (radio de interés) del observador
/// se actualiza cada `every` ticks (1 = en cada tick).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LodBand {
    pub within: f32,
    pub every: u32,
}

/// Valores recomendados: lo cercano (hasta el 50 % del radio) a 20 Hz, hasta el 80 % a 10 Hz y el resto a 5 Hz.
pub fn default_lod() -> Vec<LodBand> {
    vec![
        LodBand {
            within: 0.5,
            every: 1,
        },
        LodBand {
            within: 0.8,
            every: 2,
        },
        LodBand {
            within: f32::INFINITY,
            every: 4,
        },
    ]
}

/// Lo que comparten todos los hilos durante la replicación (todo de solo lectura).
pub(crate) struct ReplCtx<'a> {
    pub world: &'a World,
    pub ents: &'a Entities,
    pub cfg: &'a GameConfig,
    pub tick: u32,
    /// Vecindades ya reunidas para las celdas con varios jugadores.
    hoods: &'a FastMap<Cell, Neighborhood>,
    /// `(distancia² máxima en 1/64 u, cada cuántos ticks)`, de cerca a lejos.
    lod: Vec<(i64, u32)>,
}

impl<'a> ReplCtx<'a> {
    pub fn new(
        world: &'a World,
        ents: &'a Entities,
        cfg: &'a GameConfig,
        tick: u32,
        hoods: &'a FastMap<Cell, Neighborhood>,
    ) -> Self {
        let lod = cfg
            .lod
            .iter()
            .map(|b| {
                let q = b.within as f64 * cfg.interest_radius as f64 * 64.0;
                let d2 = if q.is_finite() && q < 3.0e9 {
                    (q * q) as i64
                } else {
                    i64::MAX
                };
                (d2, b.every.max(1))
            })
            .collect();
        ReplCtx {
            world,
            ents,
            cfg,
            tick,
            hoods,
            lod,
        }
    }

    /// Cada cuántos ticks se refresca algo a esa distancia² (en 1/64 u).
    fn period(&self, d2: i64) -> u32 {
        for &(max, every) in &self.lod {
            if d2 <= max {
                return every;
            }
        }
        self.lod.last().map_or(1, |l| l.1)
    }
}

/// Lo que produce la replicación de un jugador: el mensaje listo para enviar y las medidas.
pub(crate) struct Outgoing {
    pub session: SessionId,
    pub msg: Vec<u8>,
    pub full: bool,
    /// Bytes del snapshot codificado (sin cabeceras).
    pub payload: usize,
    /// Entidades cuya actualización se sujetó por nivel de detalle.
    pub lod_held: u32,
    /// El presupuesto obligó a retrasar parte de las diferencias.
    pub trimmed: bool,
    pub interest_us: u64,
    pub build_us: u64,
    pub encode_us: u64,
}

fn dist2_q(a: &[i32; 3], b: &[i32; 3]) -> i64 {
    (0..3).map(|k| (a[k] as i64 - b[k] as i64).pow(2)).sum()
}

/// Interés + snapshot + codificación de un jugador. Modifica solo a `p`.
pub(crate) fn replicate(ctx: &ReplCtx<'_>, session: SessionId, p: &mut Player) -> Outgoing {
    let t0 = Instant::now();
    let pos = vec3_from_q(p.state.pos);
    let (hy, radius) = (ctx.cfg.hysteresis, ctx.cfg.interest_radius);
    let shared = ctx
        .hoods
        .get(&ctx.world.cell_of(pos))
        .and_then(|h| h.visible(hy, p.entity, pos, radius, &p.visible));
    p.visible = match shared {
        Some(v) => v,
        None => Interest::step(ctx.world, hy, p.entity, pos, radius, &p.visible),
    };
    let t1 = Instant::now();

    // Línea base: la última que el cliente confirmó, si no es demasiado vieja.
    let tick = ctx.tick;
    let max_age = ctx.cfg.max_baseline_age;
    let base_idx = p
        .baseline_tick
        .and_then(|bt| p.history.iter().position(|(t, _)| *t == bt))
        .filter(|&i| tick.wrapping_sub(p.history[i].1.tick) <= max_age);
    let base = base_idx.map(|i| &p.history[i].1);

    let last = p.history.back().map(|(_, s)| s);
    let (list, lod_held) = candidate(ctx, p, base.and(last));
    let t2 = Instant::now();

    let snap = Snapshot {
        tick,
        entities: EntityMap::from_vec(list),
    };
    let budget = ctx.cfg.snapshot_budget;
    let (snap, full, encoded, trimmed) = match base {
        Some(b) => {
            let enc = snap.encode_delta(b);
            if budget == 0 || enc.len() <= budget {
                (snap, false, enc, false)
            } else {
                let (s, e) = trim_delta(&snap, b, p.entity, p.state.pos, budget);
                (s, false, e, true)
            }
        }
        None => {
            let enc = snap.encode_full();
            if budget == 0 || enc.len() <= budget {
                (snap, true, enc, false)
            } else {
                let (s, e) = trim_full(&snap, p.entity, p.state.pos, budget);
                (s, true, e, true)
            }
        }
    };

    let kind = if full {
        proto::SNAP_FULL
    } else {
        proto::SNAP_DELTA
    };
    let hdr = p.endpoint.next_header();
    let msg = proto::frame(hdr, kind, &proto::snapshot_body(p.last_processed, &encoded));
    p.sent.insert(hdr.seq, tick);
    p.history.push_back((tick, snap));
    while p.history.len() > ctx.cfg.history_max {
        p.history.pop_front();
    }
    let t3 = Instant::now();

    Outgoing {
        session,
        msg,
        full,
        payload: encoded.len(),
        lod_held,
        trimmed,
        interest_us: (t1 - t0).as_micros() as u64,
        build_us: (t2 - t1).as_micros() as u64,
        encode_us: (t3 - t2).as_micros() as u64,
    }
}

/// Lista ordenada por id de lo que debería ver el jugador ahora, con el tope de entidades y los niveles
/// de detalle aplicados. `last` es el último snapshot enviado (solo hay LOD si ya hay línea base).
/// Devuelve también cuántas entidades se sujetaron por LOD.
fn candidate(
    ctx: &ReplCtx<'_>,
    p: &Player,
    last: Option<&Snapshot>,
) -> (Vec<(EntityId, EntityState)>, u32) {
    let me = p.state.pos;
    let mut list: Vec<(EntityId, EntityState)> = Vec::with_capacity(p.visible.len() + 1);
    for &id in &p.visible {
        if let Some(st) = ctx.ents.get(id) {
            list.push((id, st.clone()));
        }
    }
    let cap = ctx.cfg.max_visible;
    if cap > 0 && list.len() > cap {
        // Demasiada gente a la vista: se quedan las `cap` entidades más cercanas. Se calcula el umbral de
        // distancia una sola vez y se filtra la lista (que sigue ordenada por id: no hay que reordenar).
        // Ante empates exactos en el umbral, se prefieren los ids más bajos.
        let d2s: Vec<i64> = list.iter().map(|(_, st)| dist2_q(&st.pos, &me)).collect();
        let mut sorted = d2s.clone();
        let (_, &mut limit, _) = sorted.select_nth_unstable(cap - 1);
        let mut ties = cap - d2s.iter().filter(|&&d| d < limit).count();
        let mut kept = Vec::with_capacity(cap + 1);
        for (item, d) in list.into_iter().zip(d2s) {
            if d < limit {
                kept.push(item);
            } else if d == limit && ties > 0 {
                ties -= 1;
                kept.push(item);
            }
        }
        list = kept;
    }
    // El propio jugador siempre va (el interés no incluye al observador); se inserta en su sitio.
    let at = list.partition_point(|(id, _)| *id < p.entity);
    list.insert(
        at,
        (
            p.entity,
            EntityState {
                pos: me,
                data: crate::server::KIND_PLAYER,
            },
        ),
    );

    let mut held = 0u32;
    if let Some(last) = last {
        if !ctx.lod.is_empty() {
            // Las dos listas están ordenadas por id: se recorren a la vez, sin buscar cada entidad.
            let before = last.entities.as_slice();
            let mut j = 0usize;
            for (id, st) in list.iter_mut() {
                while j < before.len() && before[j].0 < *id {
                    j += 1;
                }
                if *id == p.entity {
                    continue;
                }
                // Lo que no se envió nunca siempre se envía; lo que no cambió, no hay nada que sujetar.
                let Some(old) = before.get(j).filter(|e| e.0 == *id).map(|e| &e.1) else {
                    continue;
                };
                if old == st {
                    continue;
                }
                let every = ctx.period(dist2_q(&st.pos, &me));
                if every > 1 && !ctx.tick.wrapping_add(*id as u32).is_multiple_of(every) {
                    *st = old.clone();
                    held += 1;
                }
            }
        }
    }
    (list, held)
}

enum Change {
    Set(EntityState),
    Remove,
}

/// Una diferencia entre el snapshot deseado y la línea base, con su prioridad (más cerca = antes).
struct Diff {
    id: EntityId,
    change: Change,
    /// Cómo estaba en la línea base (si estaba).
    old: Option<EntityState>,
    /// Posición en el orden de prioridad (0 = la más cercana al observador).
    rank: usize,
}

/// Recorta un delta que no cabe: aplica sobre la línea base solo las diferencias más cercanas al
/// observador (la mayor cantidad que entre en `budget`) y retrasa el resto.
///
/// El tamaño de cada candidato se calcula exactamente sin construirlo ni codificarlo (una pasada por las
/// diferencias, sin reservar memoria), así que la búsqueda de cuántas caben cuesta una fracción de lo
/// que cuesta un solo snapshot; el resultado se construye y codifica una vez.
fn trim_delta(
    snap: &Snapshot,
    base: &Snapshot,
    me_id: EntityId,
    me: [i32; 3],
    budget: usize,
) -> (Snapshot, Vec<u8>) {
    let cur = snap.entities.as_slice();
    let old = base.entities.as_slice();
    // Diferencias en orden de id (recorrido simultáneo), cada una con su distancia al observador.
    let mut diffs: Vec<Diff> = Vec::new();
    let mut dists: Vec<i64> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < cur.len() || j < old.len() {
        let take_cur = j >= old.len() || (i < cur.len() && cur[i].0 <= old[j].0);
        let take_old = i >= cur.len() || (j < old.len() && old[j].0 <= cur[i].0);
        match (take_cur, take_old) {
            (true, true) => {
                if cur[i].1 != old[j].1 {
                    dists.push(dist2_q(&cur[i].1.pos, &me));
                    diffs.push(Diff {
                        id: cur[i].0,
                        change: Change::Set(cur[i].1.clone()),
                        old: Some(old[j].1.clone()),
                        rank: 0,
                    });
                }
                i += 1;
                j += 1;
            }
            (true, false) => {
                dists.push(dist2_q(&cur[i].1.pos, &me));
                diffs.push(Diff {
                    id: cur[i].0,
                    change: Change::Set(cur[i].1.clone()),
                    old: None,
                    rank: 0,
                });
                i += 1;
            }
            (false, true) => {
                dists.push(dist2_q(&old[j].1.pos, &me));
                diffs.push(Diff {
                    id: old[j].0,
                    change: Change::Remove,
                    old: None,
                    rank: 0,
                });
                j += 1;
            }
            (false, false) => break,
        }
    }
    // Prioridad: más cerca primero; a igual distancia, primero yo mismo y luego el id menor.
    let mut order: Vec<(i64, bool, EntityId, usize)> = diffs
        .iter()
        .enumerate()
        .map(|(n, d)| (dists[n], d.id != me_id, d.id, n))
        .collect();
    order.sort_unstable();
    for (rank, &(_, _, _, n)) in order.iter().enumerate() {
        diffs[n].rank = rank;
    }

    // Bytes que ocuparía el delta si se aplican las `k` diferencias de mayor prioridad: exactamente lo
    // que escribe `Snapshot::encode_delta`.
    let size = |k: usize| -> usize {
        let (mut n_removed, mut n_changed) = (0u64, 0u64);
        let (mut prev_removed, mut prev_changed) = (0u64, 0u64);
        let mut body = 0usize;
        for d in &diffs {
            if d.rank >= k {
                continue;
            }
            match &d.change {
                Change::Remove => {
                    body += varint_len(d.id - prev_removed);
                    prev_removed = d.id;
                    n_removed += 1;
                }
                Change::Set(e) => {
                    body += varint_len(d.id - prev_changed) + 1; // id y byte de campos
                    prev_changed = d.id;
                    n_changed += 1;
                    match &d.old {
                        Some(o) => {
                            for a in 0..3 {
                                if e.pos[a] != o.pos[a] {
                                    body += zigzag_len(e.pos[a] as i64 - o.pos[a] as i64);
                                }
                            }
                            if e.data != o.data {
                                body += varint_len(e.data as u64);
                            }
                        }
                        None => {
                            body += e.pos.iter().map(|&c| zigzag_len(c as i64)).sum::<usize>()
                                + varint_len(e.data as u64);
                        }
                    }
                }
            }
        }
        varint_len(snap.tick as u64)
            + varint_len(base.tick as u64)
            + varint_len(n_removed)
            + varint_len(n_changed)
            + body
    };
    // El snapshot resultante si se aplican las `k` diferencias de mayor prioridad: una fusión lineal.
    let build = |k: usize| -> Snapshot {
        let mut out: Vec<(EntityId, EntityState)> = Vec::with_capacity(old.len() + diffs.len());
        let mut b = 0usize;
        for d in &diffs {
            while b < old.len() && old[b].0 < d.id {
                out.push(old[b].clone());
                b += 1;
            }
            let exists = b < old.len() && old[b].0 == d.id;
            if d.rank < k {
                if let Change::Set(e) = &d.change {
                    out.push((d.id, e.clone()));
                }
                if exists {
                    b += 1;
                }
            } else if exists {
                out.push(old[b].clone());
                b += 1;
            }
        }
        out.extend_from_slice(&old[b..]);
        Snapshot {
            tick: snap.tick,
            entities: EntityMap::from_vec(out),
        }
    };
    // Mayor k tal que las k diferencias más cercanas caben. Con k = 0 el delta es solo la cabecera.
    let (mut lo, mut hi) = (0usize, diffs.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if size(mid) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let s = build(lo);
    let e = s.encode_delta(base);
    debug_assert_eq!(
        e.len(),
        size(lo),
        "el tamaño calculado difiere del codificado"
    );
    (s, e)
}

/// Recorta un snapshot completo que no cabe: se envían las entidades más cercanas (el propio jugador
/// siempre); las demás llegarán en deltas posteriores. Igual que `trim_delta`, el tamaño de cada
/// candidato se calcula sin codificarlo.
fn trim_full(snap: &Snapshot, me_id: EntityId, me: [i32; 3], budget: usize) -> (Snapshot, Vec<u8>) {
    let ents = snap.entities.as_slice();
    let mut order: Vec<(i64, bool, EntityId, usize)> = ents
        .iter()
        .enumerate()
        .map(|(n, (id, e))| (dist2_q(&e.pos, &me), *id != me_id, *id, n))
        .collect();
    order.sort_unstable();
    // Posición de cada entidad (en orden de id) en el orden de prioridad.
    let mut rank = vec![0usize; ents.len()];
    for (r, &(_, _, _, n)) in order.iter().enumerate() {
        rank[n] = r;
    }
    let size = |k: usize| -> usize {
        let mut prev = 0u64;
        let mut body = 0usize;
        for (n, (id, e)) in ents.iter().enumerate() {
            if rank[n] < k {
                body += varint_len(id - prev)
                    + e.pos.iter().map(|&c| zigzag_len(c as i64)).sum::<usize>()
                    + varint_len(e.data as u64);
                prev = *id;
            }
        }
        varint_len(snap.tick as u64) + varint_len(k as u64) + body
    };
    let (mut lo, mut hi) = (1usize.min(ents.len()), ents.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if size(mid) <= budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let s = Snapshot {
        tick: snap.tick,
        entities: EntityMap::from_vec(
            ents.iter()
                .enumerate()
                .filter(|&(n, _)| rank[n] < lo)
                .map(|(_, e)| e.clone())
                .collect(),
        ),
    };
    let e = s.encode_full();
    debug_assert_eq!(
        e.len(),
        size(lo),
        "el tamaño calculado difiere del codificado"
    );
    (s, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{KIND_NPC, KIND_PLAYER, NPC_BASE};
    use crate::sim::PlayerState;
    use crate::states::Own;
    use tsofgw_net::Snapshot;
    use tsofgw_world::Vec3;

    use std::sync::LazyLock;
    static NO_HOODS: LazyLock<FastMap<Cell, Neighborhood>> = LazyLock::new(FastMap::default);

    fn st(x: i32, z: i32) -> EntityState {
        EntityState {
            pos: [x, 0, z],
            data: KIND_NPC,
        }
    }

    fn snap(tick: u32, ents: &[(u64, EntityState)]) -> Snapshot {
        Snapshot {
            tick,
            entities: ents.iter().cloned().collect::<EntityMap>(),
        }
    }

    fn player(entity: u64, pos: [i32; 3], visible: Vec<u64>) -> Player {
        let mut p = Player::new(entity, entity);
        p.state = PlayerState { pos };
        p.joined = true;
        p.visible = visible;
        p
    }

    fn cfg_with_lod() -> GameConfig {
        GameConfig {
            interest_radius: 100.0,
            lod: default_lod(), // hasta 50 u cada tick, hasta 80 u cada 2, el resto cada 4
            ..GameConfig::default()
        }
    }

    #[test]
    fn el_lod_sujeta_el_ultimo_valor_enviado_y_no_el_de_la_base() {
        let world = World::new(32.0);
        let mut ents = Entities::new(3);
        // Cerca (10 u), media (60 u) y lejos (90 u) del observador, que está en el origen.
        let q = |u: f32| (u * 64.0) as i32;
        ents.push_npc(st(q(10.0) + 7, 0), Own::Owned); // valor actual, cerca
        ents.push_npc(st(q(60.0) + 7, 0), Own::Owned); // valor actual, media
        ents.push_npc(st(q(90.0) + 7, 0), Own::Owned); // valor actual, lejos
        let cfg = cfg_with_lod();
        let p = player(1, [0, 0, 0], vec![NPC_BASE, NPC_BASE + 1, NPC_BASE + 2]);
        // Lo último enviado (sin confirmar) tiene un valor más reciente que el de la línea base.
        let last = snap(
            9,
            &[
                (1, st(0, 0)),
                (NPC_BASE, st(q(10.0) + 3, 0)),
                (NPC_BASE + 1, st(q(60.0) + 3, 0)),
                (NPC_BASE + 2, st(q(90.0) + 3, 0)),
            ],
        );
        let mut held_ticks = 0;
        for tick in 10..22u32 {
            let ctx = ReplCtx::new(&world, &ents, &cfg, tick, &NO_HOODS);
            let (list, held) = candidate(&ctx, &p, Some(&last));
            let get = |id: u64| list.iter().find(|(i, _)| *i == id).unwrap().1.clone();
            // Lo cercano se refresca siempre.
            assert_eq!(get(NPC_BASE).pos[0], q(10.0) + 7);
            // Lo lejano, o se refresca (valor actual) o repite exactamente lo último enviado (nunca otra cosa).
            let far = get(NPC_BASE + 2).pos[0];
            let refresh = (tick + (NPC_BASE + 2) as u32).is_multiple_of(4);
            assert_eq!(
                far,
                if refresh { q(90.0) + 7 } else { q(90.0) + 3 },
                "tick {tick}"
            );
            let mid = get(NPC_BASE + 1).pos[0];
            let refresh = (tick + (NPC_BASE + 1) as u32).is_multiple_of(2);
            assert_eq!(
                mid,
                if refresh { q(60.0) + 7 } else { q(60.0) + 3 },
                "tick {tick}"
            );
            held_ticks += held;
        }
        assert!(held_ticks > 0);
    }

    #[test]
    fn el_lod_no_sujeta_lo_que_el_cliente_aun_no_tiene() {
        let world = World::new(32.0);
        let mut ents = Entities::new(1);
        ents.push_npc(st(90 * 64 + 5, 0), Own::Owned); // lejos
        let cfg = cfg_with_lod();
        let p = player(1, [0, 0, 0], vec![NPC_BASE]);
        let last = snap(5, &[(1, st(0, 0))]); // el cliente no sabe nada de esa entidad
        for tick in 6..20u32 {
            let ctx = ReplCtx::new(&world, &ents, &cfg, tick, &NO_HOODS);
            let (list, held) = candidate(&ctx, &p, Some(&last));
            assert_eq!(held, 0);
            assert!(list
                .iter()
                .any(|(id, s)| *id == NPC_BASE && s.pos[0] == 90 * 64 + 5));
        }
    }

    #[test]
    fn el_propio_jugador_siempre_va_al_dia() {
        let world = World::new(32.0);
        let ents = Entities::new(0);
        let cfg = cfg_with_lod();
        let p = player(1, [640, 0, -64], vec![]);
        let last = snap(
            3,
            &[(
                1,
                EntityState {
                    pos: [0, 0, 0],
                    data: KIND_PLAYER,
                },
            )],
        );
        for tick in 4..12u32 {
            let ctx = ReplCtx::new(&world, &ents, &cfg, tick, &NO_HOODS);
            let (list, _) = candidate(&ctx, &p, Some(&last));
            assert_eq!(list[0].1.pos, [640, 0, -64]);
        }
    }

    fn line(n: u64, x0: i32, step: i32, shift: i32) -> Snapshot {
        let ents: Vec<(u64, EntityState)> = (0..n)
            .map(|i| (100 + i, st(x0 + step * i as i32 + shift, 0)))
            .collect();
        snap(10, &ents)
    }

    #[test]
    fn recortar_un_delta_envia_primero_lo_mas_cercano_y_cabe() {
        // 40 entidades en fila; el observador está al principio. Todas se movieron respecto de la base.
        let base = line(40, 0, 640, 0);
        let mut cur = line(40, 0, 640, 12);
        cur.tick = 11;
        let me = [0, 0, 0];
        let full = cur.encode_delta(&base);
        let budget = full.len() / 3;
        let (s, enc) = trim_delta(&cur, &base, 100, me, budget);
        assert!(enc.len() <= budget, "{} > {}", enc.len(), budget);
        // El resultado es decodificable contra la base y coincide con lo que se registra como enviado.
        assert_eq!(Snapshot::decode_delta(&enc, &base).unwrap(), s);
        // Lo recortado son las más lejanas: las primeras (cercanas) tienen el valor nuevo, las últimas el viejo.
        let moved: Vec<u64> = s
            .entities
            .iter()
            .filter(|(id, e)| e.pos[0] != base.entities[id].pos[0])
            .map(|(id, _)| *id)
            .collect();
        assert!(!moved.is_empty() && moved.len() < 40);
        let k = moved.len() as u64;
        assert_eq!(
            moved,
            (100..100 + k).collect::<Vec<_>>(),
            "las k más cercanas"
        );
        // Con presupuesto de sobra no se recorta nada.
        let (s2, _) = trim_delta(&cur, &base, 100, me, usize::MAX);
        assert_eq!(s2.entities, cur.entities);
        // Con presupuesto cero se queda en la línea base (solo la cabecera).
        let (s0, e0) = trim_delta(&cur, &base, 100, me, 0);
        assert_eq!(s0.entities, base.entities);
        assert!(e0.len() <= 8);
    }

    #[test]
    fn recortar_un_delta_trata_entradas_y_salidas() {
        let base = snap(5, &[(1, st(0, 0)), (2, st(64, 0)), (3, st(6400, 0))]);
        // Sale la 2 (lejos de la base: cuesta poco), entra la 9 muy cerca, la 3 se mueve (lejos).
        let cur = snap(6, &[(1, st(1, 0)), (3, st(6500, 0)), (9, st(10, 10))]);
        // Un byte menos de lo que cuesta todo: debe quedar fuera lo más lejano (el movimiento de 3).
        let budget = cur.encode_delta(&base).len() - 1;
        let (s, enc) = trim_delta(&cur, &base, 1, [0, 0, 0], budget);
        assert_eq!(Snapshot::decode_delta(&enc, &base).unwrap(), s);
        assert!(enc.len() <= budget);
        // Lo más cercano (yo mismo, la entrada 9 y la salida de 2) entra antes que el movimiento lejano de 3.
        assert_eq!(s.entities.get(&1).unwrap().pos, [1, 0, 0]);
        assert!(s.entities.contains_key(&9));
        assert!(!s.entities.contains_key(&2), "la salida de 2 sí se aplica");
        assert_eq!(s.entities.get(&3).unwrap().pos, [6400, 0, 0], "la 3 espera");
        // Y con tan poco presupuesto que solo cabe mi propio movimiento, lo demás espera.
        let (s1, e1) = trim_delta(&cur, &base, 1, [0, 0, 0], 7);
        assert!(e1.len() <= 7);
        assert_eq!(s1.entities.get(&1).unwrap().pos, [1, 0, 0]);
        assert!(!s1.entities.contains_key(&9) && s1.entities.contains_key(&2));
    }

    #[test]
    fn recortar_un_snapshot_completo_conserva_al_jugador_y_lo_mas_cercano() {
        let mut ents: Vec<(u64, EntityState)> = (0..60u64)
            .map(|i| (100 + i, st(64 * (i as i32 + 1), 0)))
            .collect();
        ents.push((
            7,
            EntityState {
                pos: [0, 0, 0],
                data: KIND_PLAYER,
            },
        ));
        let snap = snap(3, &ents);
        let full = snap.encode_full();
        let budget = full.len() / 4;
        let (s, enc) = trim_full(&snap, 7, [0, 0, 0], budget);
        assert!(enc.len() <= budget);
        assert_eq!(Snapshot::decode_full(&enc).unwrap(), s);
        assert!(s.entities.contains_key(&7), "el propio jugador siempre va");
        let n = s.entities.len() as u64;
        assert!(n > 5 && n < 61);
        // Son las más cercanas: ids consecutivos desde 100.
        let others: Vec<u64> = s.entities.keys().copied().filter(|&i| i != 7).collect();
        assert_eq!(others, (100..100 + others.len() as u64).collect::<Vec<_>>());
        // Con un presupuesto absurdamente pequeño, al menos se envía al propio jugador.
        let (tiny, _) = trim_full(&snap, 7, [0, 0, 0], 1);
        assert_eq!(tiny.entities.len(), 1);
        assert!(tiny.entities.contains_key(&7));
        let _ = Vec3::new(0.0, 0.0, 0.0);
    }

    // ---- Referencia lenta: la búsqueda por codificación completa de cada candidato. Debe dar
    // exactamente lo mismo que el cálculo de tamaños.

    fn reference_trim_delta(
        snap: &Snapshot,
        base: &Snapshot,
        me_id: EntityId,
        me: [i32; 3],
        budget: usize,
    ) -> (Snapshot, Vec<u8>) {
        let cur = snap.entities.as_slice();
        let old = base.entities.as_slice();
        let mut diffs: Vec<(EntityId, Option<EntityState>, i64)> = Vec::new(); // (id, Some = fijar | None = quitar, distancia)
        let (mut i, mut j) = (0, 0);
        while i < cur.len() || j < old.len() {
            let take_cur = j >= old.len() || (i < cur.len() && cur[i].0 <= old[j].0);
            let take_old = i >= cur.len() || (j < old.len() && old[j].0 <= cur[i].0);
            match (take_cur, take_old) {
                (true, true) => {
                    if cur[i].1 != old[j].1 {
                        diffs.push((
                            cur[i].0,
                            Some(cur[i].1.clone()),
                            dist2_q(&cur[i].1.pos, &me),
                        ));
                    }
                    i += 1;
                    j += 1;
                }
                (true, false) => {
                    diffs.push((
                        cur[i].0,
                        Some(cur[i].1.clone()),
                        dist2_q(&cur[i].1.pos, &me),
                    ));
                    i += 1;
                }
                (false, true) => {
                    diffs.push((old[j].0, None, dist2_q(&old[j].1.pos, &me)));
                    j += 1;
                }
                (false, false) => break,
            }
        }
        let mut order: Vec<(i64, bool, EntityId, usize)> = diffs
            .iter()
            .enumerate()
            .map(|(n, d)| (d.2, d.0 != me_id, d.0, n))
            .collect();
        order.sort_unstable();
        let mut rank = vec![0usize; diffs.len()];
        for (r, &(_, _, _, n)) in order.iter().enumerate() {
            rank[n] = r;
        }
        let build = |k: usize| -> Snapshot {
            let mut out: Vec<(EntityId, EntityState)> = Vec::new();
            let mut b = 0usize;
            for (n, d) in diffs.iter().enumerate() {
                while b < old.len() && old[b].0 < d.0 {
                    out.push(old[b].clone());
                    b += 1;
                }
                let exists = b < old.len() && old[b].0 == d.0;
                if rank[n] < k {
                    if let Some(e) = &d.1 {
                        out.push((d.0, e.clone()));
                    }
                    if exists {
                        b += 1;
                    }
                } else if exists {
                    out.push(old[b].clone());
                    b += 1;
                }
            }
            out.extend_from_slice(&old[b..]);
            Snapshot {
                tick: snap.tick,
                entities: EntityMap::from_vec(out),
            }
        };
        let (mut lo, mut hi) = (0usize, diffs.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if build(mid).encode_delta(base).len() <= budget {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let s = build(lo);
        let e = s.encode_delta(base);
        (s, e)
    }

    fn reference_trim_full(
        snap: &Snapshot,
        me_id: EntityId,
        me: [i32; 3],
        budget: usize,
    ) -> (Snapshot, Vec<u8>) {
        let mut order: Vec<(i64, bool, u64)> = snap
            .entities
            .iter()
            .map(|(&id, e)| (dist2_q(&e.pos, &me), id != me_id, id))
            .collect();
        order.sort_unstable();
        let build = |k: usize| Snapshot {
            tick: snap.tick,
            entities: EntityMap::from_vec(
                order[..k]
                    .iter()
                    .map(|&(_, _, id)| (id, snap.entities[&id].clone()))
                    .collect(),
            ),
        };
        let (mut lo, mut hi) = (1usize.min(order.len()), order.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if build(mid).encode_full().len() <= budget {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let s = build(lo);
        let e = s.encode_full();
        (s, e)
    }

    /// Entidades al azar con coordenadas pequeñas, grandes y extremas, y datos variados.
    fn random_snapshot(rng: &mut tsofgw_world::Rng, tick: u32, n: usize, id_gap: u64) -> Snapshot {
        let mut id = 0u64;
        let mut v = Vec::new();
        for _ in 0..n {
            id += 1 + rng.next_u64() % id_gap;
            let c = |rng: &mut tsofgw_world::Rng| match rng.next_u64() % 8 {
                // Grandes (varint de 5 bytes) pero sin desbordar la distancia al cuadrado, que en el
                // servidor real está acotada por el tamaño del mundo.
                0 => 1_000_000_000 - (rng.next_u64() % 4) as i32,
                1 => -1_000_000_000 + (rng.next_u64() % 4) as i32,
                2 => 0,
                3 => (rng.next_u64() % 400_000) as i32 - 200_000,
                _ => (rng.next_u64() % 4000) as i32 - 2000,
            };
            v.push((
                id + if rng.next_u64().is_multiple_of(5) {
                    NPC_BASE
                } else {
                    0
                },
                EntityState {
                    pos: [c(rng), c(rng), c(rng)],
                    data: [KIND_NPC, KIND_PLAYER, 300, u16::MAX][(rng.next_u64() % 4) as usize],
                },
            ));
        }
        v.sort_by_key(|e| e.0);
        v.dedup_by_key(|e| e.0);
        Snapshot {
            tick,
            entities: EntityMap::from_vec(v),
        }
    }

    /// Un snapshot parecido al anterior: algunos se mueven, otros se van y otros aparecen.
    fn evolve(rng: &mut tsofgw_world::Rng, base: &Snapshot) -> Snapshot {
        let mut v: Vec<(u64, EntityState)> = Vec::new();
        for (id, e) in base.entities.as_slice() {
            match rng.next_u64() % 10 {
                0 => {} // desaparece
                1..=4 => {
                    let mut e = e.clone();
                    for p in e.pos.iter_mut() {
                        if rng.next_u64().is_multiple_of(3) {
                            *p = p.wrapping_add((rng.next_u64() % 600) as i32 - 300);
                        }
                    }
                    if rng.next_u64().is_multiple_of(9) {
                        e.data = e.data.wrapping_add(1);
                    }
                    v.push((*id, e));
                }
                _ => v.push((*id, e.clone())),
            }
        }
        for k in 0..(rng.next_u64() % 12) {
            v.push((
                900_000_000 + k * 7 + rng.next_u64() % 5,
                EntityState {
                    pos: [
                        (rng.next_u64() % 900) as i32,
                        0,
                        (rng.next_u64() % 900) as i32,
                    ],
                    data: KIND_NPC,
                },
            ));
        }
        v.sort_by_key(|e| e.0);
        v.dedup_by_key(|e| e.0);
        Snapshot {
            tick: base.tick + 1 + (rng.next_u64() % 3) as u32,
            entities: EntityMap::from_vec(v),
        }
    }

    #[test]
    fn el_recorte_por_calculo_de_tamanos_da_exactamente_lo_mismo_que_codificar_cada_candidato() {
        let mut rng = tsofgw_world::Rng::new(2024);
        let (mut trimmed_delta, mut trimmed_full) = (0, 0);
        for round in 0..400 {
            let n = [0usize, 1, 2, 5, 30, 150, 400][round % 7];
            let gap = [1u64, 3, 200, 70_000][(round / 7) % 4];
            let base = random_snapshot(&mut rng, 100 + round as u32, n, gap);
            let cur = evolve(&mut rng, &base);
            let me_id = base.entities.as_slice().first().map_or(1, |e| e.0);
            let me = [
                (rng.next_u64() % 800) as i32 - 400,
                0,
                (rng.next_u64() % 800) as i32 - 400,
            ];
            let full = cur.encode_full().len();
            let delta = cur.encode_delta(&base).len();
            for budget in [
                0,
                1,
                3,
                8,
                delta / 4,
                delta / 2,
                delta.saturating_sub(1),
                delta,
                full / 3,
                full / 2,
                full,
            ] {
                let (s1, e1) = trim_delta(&cur, &base, me_id, me, budget);
                let (s2, e2) = reference_trim_delta(&cur, &base, me_id, me, budget);
                assert_eq!(e1, e2, "delta: ronda {round}, presupuesto {budget}");
                assert_eq!(s1.entities.as_slice(), s2.entities.as_slice());
                assert_eq!(s1.tick, s2.tick);
                if e1.len() < delta {
                    trimmed_delta += 1;
                }
                if !cur.entities.as_slice().is_empty() {
                    let (s1, e1) = trim_full(&cur, me_id, me, budget);
                    let (s2, e2) = reference_trim_full(&cur, me_id, me, budget);
                    assert_eq!(e1, e2, "completo: ronda {round}, presupuesto {budget}");
                    assert_eq!(s1.entities.as_slice(), s2.entities.as_slice());
                    if e1.len() < full {
                        trimmed_full += 1;
                    }
                }
            }
        }
        // La prueba no es vacía: hubo muchos recortes reales.
        assert!(
            trimmed_delta > 300 && trimmed_full > 300,
            "{trimmed_delta} {trimmed_full}"
        );
    }
}
