//! Simulación determinista de Raft: pérdidas, duplicados, retrasos y reordenaciones, particiones, y réplicas
//! que se caen (también a mitad de un paso, perdiendo lo que no llegó a guardarse) y vuelven.
//!
//! Se comprueba **seguridad** en cada paso:
//! * a lo sumo un líder por término;
//! * todo lo que una réplica da por confirmado coincide con lo que dio cualquier otra (acuerdo).
//!
//! Y **vivacidad** al final: con la red sana y todas las réplicas arriba, hay líder, todas convergen y los
//! mandatos nuevos se confirman.

use std::collections::{BTreeMap, HashMap};
use tsofgw_online::raft::*;

#[derive(Clone, Default)]
struct Disk {
    term: u64,
    voted: Option<usize>,
    base_index: u64,
    base_term: u64,
    log: BTreeMap<u64, Entry>,
    /// Máquina de estados: los mandatos aplicados (sin las entradas vacías) y hasta dónde.
    sm: Vec<Vec<u8>>,
    applied: u64,
}

impl Disk {
    fn apply(&mut self, ops: Vec<Persist>) {
        for op in ops {
            match op {
                Persist::Meta { term, voted_for } => {
                    self.term = term;
                    self.voted = voted_for;
                }
                Persist::Put { index, entry } => {
                    self.log.insert(index, entry);
                }
                Persist::TruncateFrom { index } => {
                    self.log.split_off(&index);
                }
                Persist::Base { index, term } => {
                    self.base_index = index;
                    self.base_term = term;
                    self.log = self.log.split_off(&(index + 1));
                }
            }
        }
    }

    fn boot(&self) -> Boot {
        let mut log = Vec::new();
        for (next, (i, e)) in (self.base_index + 1..).zip(&self.log) {
            assert_eq!(*i, next, "diario no contiguo en disco");
            log.push(e.clone());
        }
        Boot {
            term: self.term,
            voted_for: self.voted,
            base_index: self.base_index,
            base_term: self.base_term,
            log,
            applied: self.applied,
        }
    }
}

fn enc_sm(sm: &[Vec<u8>]) -> Vec<u8> {
    let mut v = Vec::new();
    for c in sm {
        v.push(c.len() as u8);
        v.extend_from_slice(c);
    }
    v
}

fn dec_sm(b: &[u8]) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let n = b[i] as usize;
        v.push(b[i + 1..i + 1 + n].to_vec());
        i += 1 + n;
    }
    v
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, per_thousand: u64) -> bool {
        self.below(1000) < per_thousand
    }
}

/// Lo que se ejercitó en una pasada de caos (para comprobar que la prueba no es vacía).
#[derive(Default, Debug)]
struct Cobertura {
    mandatos_confirmados: u64,
    elecciones: u64,
    instantaneas: u64,
    conflictos: u64,
    compactaciones: u64,
    caidas: u64,
    abdicaciones: u64,
}

struct Chaos {
    drop: u64,
    dup: u64,
    max_delay: u64,
    crash: u64,
    crash_mid_step: u64,
    partition: u64,
}

struct Sim {
    n: usize,
    cfg: RaftConfig,
    disks: Vec<Disk>,
    nodes: Vec<Option<Raft>>,
    restart_at: Vec<u64>,
    net: Vec<(u64, usize, usize, Vec<u8>)>,
    blocked: Vec<Vec<bool>>,
    rng: Rng,
    now: u64,
    leaders: HashMap<u64, usize>,
    committed: BTreeMap<u64, Entry>,
    next_cmd: u64,
    proposed_ok: Vec<(u64, u64, Vec<u8>)>, // (índice, término, mandato) aceptados por un líder
    stats_crashes: u64,
    stats_msgs: u64,
    cov: Cobertura,
}

impl Sim {
    fn new(n: usize, seed: u64, cfg: RaftConfig) -> Sim {
        let mut s = Sim {
            n,
            cfg,
            disks: vec![Disk::default(); n],
            nodes: (0..n).map(|_| None).collect(),
            restart_at: vec![0; n],
            net: Vec::new(),
            blocked: vec![vec![false; n]; n],
            rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
            now: 0,
            leaders: HashMap::new(),
            committed: BTreeMap::new(),
            next_cmd: 1,
            proposed_ok: Vec::new(),
            stats_crashes: 0,
            stats_msgs: 0,
            cov: Cobertura::default(),
        };
        for i in 0..n {
            s.start(i);
        }
        s
    }

    fn start(&mut self, i: usize) {
        let d = &self.disks[i];
        // Lo aplicado a la máquina de estados es lo que hay en disco.
        self.nodes[i] = Some(Raft::new(i, self.n, self.cfg.clone(), d.boot(), self.now));
    }

    fn flush(&mut self, i: usize, chaos: &Chaos) {
        let node = self.nodes[i].as_mut().unwrap();
        // ¿Caída a mitad de paso? Se pierde todo lo que no llegó a guardarse ni enviarse.
        if self.rng.chance(chaos.crash_mid_step) {
            let down = 200 + self.rng.below(800);
            self.crash(i, down);
            return;
        }
        let mut disk = self.disks[i].clone();
        disk.apply(node.take_persist());
        for ev in node.take_events() {
            match ev {
                Event::Committed { index, entry } => {
                    match self.committed.get(&index) {
                        Some(prev) => assert_eq!(
                            prev, &entry,
                            "ACUERDO ROTO en el índice {index}: dos réplicas confirmaron cosas distintas"
                        ),
                        None => {
                            self.committed.insert(index, entry.clone());
                        }
                    }
                    assert_eq!(index, disk.applied + 1, "aplicación fuera de orden en {i}");
                    if !entry.data.is_empty() {
                        disk.sm.push(entry.data);
                    }
                    disk.applied = index;
                }
                Event::Restore { index, data, .. } => {
                    disk.sm = dec_sm(&data);
                    disk.applied = index;
                }
            }
        }
        if node.wants_snapshot() {
            node.provide_snapshot(disk.applied, enc_sm(&disk.sm));
        }
        node.maybe_compact(disk.applied);
        disk.apply(node.take_persist());
        self.disks[i] = disk;
        // Seguridad de elección.
        if node.is_leader() {
            let t = node.term();
            let prev = self.leaders.insert(t, i);
            assert!(
                prev.is_none() || prev == Some(i),
                "DOS LÍDERES en el término {t}: {prev:?} y {i}"
            );
        }
        // Solo ahora sale algo al mundo.
        for (to, m) in node.take_outbox() {
            self.send(i, to, m, chaos);
        }
    }

    fn send(&mut self, from: usize, to: usize, m: Msg, chaos: &Chaos) {
        self.stats_msgs += 1;
        let bytes = m.encode();
        assert!(
            bytes.len() <= 1100,
            "mensaje demasiado grande: {}",
            bytes.len()
        );
        assert_eq!(
            Msg::decode(&bytes).as_ref(),
            Some(&m),
            "codificación no simétrica"
        );
        if self.blocked[from][to] || self.rng.chance(chaos.drop) {
            return;
        }
        let copies = if self.rng.chance(chaos.dup) { 2 } else { 1 };
        for _ in 0..copies {
            let at = self.now + 1 + self.rng.below(chaos.max_delay);
            self.net.push((at, from, to, bytes.clone()));
        }
    }

    fn harvest(&mut self, i: usize) {
        if let Some(node) = self.nodes[i].as_ref() {
            let st = node.stats();
            self.cov.instantaneas += st.snapshots_installed;
            self.cov.conflictos += st.conflicts_truncated;
            self.cov.compactaciones += st.compactions;
            self.cov.abdicaciones += st.stepped_down + st.quorum_lost;
        }
    }

    fn crash(&mut self, i: usize, down_ms: u64) {
        self.stats_crashes += 1;
        self.harvest(i);
        self.nodes[i] = None;
        self.restart_at[i] = self.now + down_ms;
    }

    fn step(&mut self, chaos: &Chaos) {
        self.now += 1;
        // Reinicios.
        for i in 0..self.n {
            if self.nodes[i].is_none() && self.now >= self.restart_at[i] {
                self.start(i);
            }
        }
        // Entrega.
        let now = self.now;
        let mut due = Vec::new();
        self.net.retain(|m| {
            if m.0 <= now {
                due.push(m.clone());
                false
            } else {
                true
            }
        });
        due.sort_by_key(|m| (m.0, m.1, m.2));
        for (_, from, to, bytes) in due {
            if self.blocked[from][to] {
                continue;
            }
            let Some(node) = self.nodes[to].as_mut() else {
                continue;
            };
            let msg = Msg::decode(&bytes).unwrap();
            node.step(now, from, msg);
            self.flush(to, chaos);
        }
        for i in 0..self.n {
            if let Some(node) = self.nodes[i].as_mut() {
                node.tick(now);
                self.flush(i, chaos);
            }
        }
        // Caídas y particiones al azar.
        if chaos.crash > 0 && self.rng.chance(chaos.crash) {
            let i = self.rng.below(self.n as u64) as usize;
            if self.nodes.iter().filter(|n| n.is_some()).count() > 1 && self.nodes[i].is_some() {
                let down = 100 + self.rng.below(1500);
                self.crash(i, down);
            }
        }
        if chaos.partition > 0 && self.rng.chance(chaos.partition) {
            if self.rng.chance(400) {
                self.heal();
            } else {
                let i = self.rng.below(self.n as u64) as usize;
                let j = self.rng.below(self.n as u64) as usize;
                if i != j {
                    let both = self.rng.chance(700);
                    self.blocked[i][j] = true;
                    if both {
                        self.blocked[j][i] = true;
                    }
                }
            }
        }
    }

    fn heal(&mut self) {
        for row in self.blocked.iter_mut() {
            for b in row.iter_mut() {
                *b = false;
            }
        }
    }

    /// Un cliente intenta proponer un mandato nuevo a quien se crea líder.
    fn try_propose(&mut self, chaos: &Chaos) -> bool {
        for i in 0..self.n {
            let Some(node) = self.nodes[i].as_mut() else {
                continue;
            };
            if node.is_leader() {
                let cmd = self.next_cmd.to_le_bytes().to_vec();
                if let Some(idx) = node.propose(cmd.clone()) {
                    let term = node.term();
                    self.next_cmd += 1;
                    self.proposed_ok.push((idx, term, cmd));
                    self.flush(i, chaos);
                    return true;
                }
            }
        }
        false
    }

    fn leader(&self) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| n.as_ref().filter(|r| r.is_leader()).map(|_| i))
            .max_by_key(|&i| self.nodes[i].as_ref().unwrap().term())
    }
}

fn cfg_small() -> RaftConfig {
    RaftConfig {
        election_min_ms: 150,
        election_max_ms: 300,
        heartbeat_ms: 30,
        max_entries_per_append: 8,
        compact_after: 24,
        keep_tail: 6,
    }
}

fn calm() -> Chaos {
    Chaos {
        drop: 0,
        dup: 0,
        max_delay: 5,
        crash: 0,
        crash_mid_step: 0,
        partition: 0,
    }
}

/// Al final: red sana, todas arriba; debe haber líder, converger y confirmar mandatos nuevos.
fn settle_and_check(sim: &mut Sim, ch: &Chaos) {
    sim.heal();
    for i in 0..sim.n {
        if sim.nodes[i].is_none() {
            sim.restart_at[i] = sim.now;
        }
    }
    let ok = Chaos {
        drop: 0,
        dup: 0,
        max_delay: ch.max_delay,
        crash: 0,
        crash_mid_step: 0,
        partition: 0,
    };
    // Tiempo para elegir líder.
    for _ in 0..3000 {
        sim.step(&ok);
    }
    assert!(sim.leader().is_some(), "sin líder tras sanar la red");
    // Mandatos nuevos que deben confirmarse en todas las réplicas.
    let mut fresh = Vec::new();
    for _ in 0..30 {
        for _ in 0..50 {
            if sim.try_propose(&ok) {
                break;
            }
            sim.step(&ok);
        }
        fresh.push(sim.next_cmd - 1);
        for _ in 0..5 {
            sim.step(&ok);
        }
    }
    for _ in 0..3000 {
        sim.step(&ok);
    }
    let applied: Vec<u64> = sim.disks.iter().map(|d| d.applied).collect();
    let max = *applied.iter().max().unwrap();
    assert!(
        applied.iter().all(|&a| a == max),
        "las réplicas no convergen: {applied:?}"
    );
    let reference = &sim.disks[0].sm;
    for (i, d) in sim.disks.iter().enumerate() {
        assert_eq!(&d.sm, reference, "estado distinto en la réplica {i}");
    }
    for c in fresh {
        assert!(
            reference.contains(&c.to_le_bytes().to_vec()),
            "el mandato {c} se aceptó tras sanar y no está en el estado"
        );
    }
    // Sin duplicados en el estado.
    let mut seen = std::collections::HashSet::new();
    for c in reference {
        assert!(seen.insert(c.clone()), "mandato repetido en el estado");
    }
}

#[test]
fn una_replica_sola_se_elige_y_confirma() {
    let mut sim = Sim::new(1, 1, cfg_small());
    let ch = calm();
    for _ in 0..1000 {
        sim.step(&ch);
    }
    assert_eq!(sim.leader(), Some(0));
    for _ in 0..10 {
        assert!(sim.try_propose(&ch));
        sim.step(&ch);
    }
    assert_eq!(sim.disks[0].sm.len(), 10);
}

#[test]
fn tres_replicas_sanas_eligen_un_lider_y_replican() {
    let mut sim = Sim::new(3, 2, cfg_small());
    let ch = calm();
    for _ in 0..2000 {
        sim.step(&ch);
    }
    assert!(sim.leader().is_some());
    for _ in 0..40 {
        while !sim.try_propose(&ch) {
            sim.step(&ch);
        }
        for _ in 0..3 {
            sim.step(&ch);
        }
    }
    for _ in 0..500 {
        sim.step(&ch);
    }
    for d in &sim.disks {
        assert_eq!(d.sm.len(), 40);
    }
    // Un solo líder estable: ninguna elección extra en régimen.
    let terms: Vec<u64> = sim
        .nodes
        .iter()
        .map(|n| n.as_ref().unwrap().term())
        .collect();
    assert!(
        terms.iter().all(|&t| t == terms[0]),
        "términos distintos: {terms:?}"
    );
}

#[test]
fn si_cae_el_lider_se_elige_otro_y_no_se_pierde_lo_confirmado() {
    let mut sim = Sim::new(3, 3, cfg_small());
    let ch = calm();
    for _ in 0..1500 {
        sim.step(&ch);
    }
    for _ in 0..20 {
        while !sim.try_propose(&ch) {
            sim.step(&ch);
        }
        for _ in 0..4 {
            sim.step(&ch);
        }
    }
    let l = sim.leader().unwrap();
    let before = sim.disks[l].sm.clone();
    sim.crash(l, 100_000); // no vuelve
    for _ in 0..2000 {
        sim.step(&ch);
    }
    let l2 = sim.leader().expect("sin líder nuevo");
    assert_ne!(l, l2);
    assert!(sim.disks[l2].sm.starts_with(&before) || before.starts_with(&sim.disks[l2].sm));
    while !sim.try_propose(&ch) {
        sim.step(&ch);
    }
    for _ in 0..300 {
        sim.step(&ch);
    }
    assert_eq!(sim.disks[l2].sm.len(), 21);
}

#[test]
fn sin_mayoria_no_se_confirma_nada_y_al_volver_se_recupera() {
    let mut sim = Sim::new(3, 4, cfg_small());
    let ch = calm();
    for _ in 0..1500 {
        sim.step(&ch);
    }
    let l = sim.leader().unwrap();
    // El líder queda aislado de los otros dos.
    for j in 0..3 {
        if j != l {
            sim.blocked[l][j] = true;
            sim.blocked[j][l] = true;
        }
    }
    let applied_before = sim.disks[l].applied;
    for _ in 0..5 {
        let _ = sim.try_propose(&ch);
        sim.step(&ch);
    }
    for _ in 0..2000 {
        sim.step(&ch);
    }
    assert_eq!(
        sim.disks[l].applied, applied_before,
        "el líder aislado confirmó sin mayoría"
    );
    assert!(
        !sim.nodes[l].as_ref().unwrap().is_leader(),
        "debía abdicar sin quórum"
    );
    settle_and_check(&mut sim, &ch);
}

#[test]
fn una_replica_muy_atrasada_se_pone_al_dia_con_una_instantanea() {
    let mut sim = Sim::new(3, 5, cfg_small());
    let ch = calm();
    for _ in 0..1500 {
        sim.step(&ch);
    }
    let l = sim.leader().unwrap();
    let lag = (l + 1) % 3;
    sim.crash(lag, 1_000_000_000);
    // Muchos más mandatos que el diario que se conserva.
    for _ in 0..200 {
        while !sim.try_propose(&ch) {
            sim.step(&ch);
        }
        for _ in 0..2 {
            sim.step(&ch);
        }
    }
    for _ in 0..300 {
        sim.step(&ch);
    }
    let l = sim.leader().unwrap();
    assert!(
        sim.nodes[l].as_ref().unwrap().base_index() > 50,
        "el diario no se compactó"
    );
    sim.restart_at[lag] = sim.now;
    for _ in 0..3000 {
        sim.step(&ch);
    }
    let l = sim.leader().unwrap();
    let installed: u64 = sim
        .nodes
        .iter()
        .map(|n| n.as_ref().unwrap().stats().snapshots_installed)
        .sum();
    assert!(installed >= 1, "debía haber instalado una instantánea");
    assert_eq!(sim.disks[lag].sm, sim.disks[l].sm);
    assert_eq!(sim.disks[l].sm.len(), 200);
}

fn caos(seed: u64, n: usize, ch: Chaos, steps: u64) -> Cobertura {
    let mut sim = Sim::new(n, seed, cfg_small());
    for s in 0..steps {
        sim.step(&ch);
        if s % 7 == 0 {
            sim.try_propose(&ch);
        }
    }
    settle_and_check(&mut sim, &ch);
    for i in 0..sim.n {
        sim.harvest(i);
    }
    let mut c = std::mem::take(&mut sim.cov);
    c.mandatos_confirmados = sim.disks[0].sm.len() as u64;
    c.caidas = sim.stats_crashes;
    c.elecciones = sim.leaders.len() as u64;
    c
}

fn suma(a: &mut Cobertura, b: Cobertura) {
    a.mandatos_confirmados += b.mandatos_confirmados;
    a.elecciones += b.elecciones;
    a.instantaneas += b.instantaneas;
    a.conflictos += b.conflictos;
    a.compactaciones += b.compactaciones;
    a.caidas += b.caidas;
    a.abdicaciones += b.abdicaciones;
}

#[test]
fn caos_con_perdidas_y_duplicados() {
    let mut total = Cobertura::default();
    for seed in 1..=12 {
        suma(
            &mut total,
            caos(
                seed,
                3,
                Chaos {
                    drop: 150,
                    dup: 100,
                    max_delay: 40,
                    crash: 0,
                    crash_mid_step: 0,
                    partition: 0,
                },
                8000,
            ),
        );
    }
    eprintln!("{total:?}");
    assert!(total.mandatos_confirmados > 500 && total.elecciones >= 12);
}

#[test]
fn caos_con_particiones() {
    let mut total = Cobertura::default();
    for seed in 1..=12 {
        suma(
            &mut total,
            caos(
                seed,
                3,
                Chaos {
                    drop: 50,
                    dup: 50,
                    max_delay: 30,
                    crash: 0,
                    crash_mid_step: 0,
                    partition: 8,
                },
                10_000,
            ),
        );
    }
    eprintln!("{total:?}");
    assert!(total.mandatos_confirmados > 300 && total.elecciones >= 12);
    assert!(total.abdicaciones > 0, "ningún líder abdicó");
}

#[test]
fn caos_con_caidas_y_caidas_a_mitad_de_paso() {
    let mut total = Cobertura::default();
    for seed in 1..=12 {
        suma(
            &mut total,
            caos(
                seed,
                3,
                Chaos {
                    drop: 50,
                    dup: 50,
                    max_delay: 30,
                    crash: 4,
                    crash_mid_step: 2,
                    partition: 0,
                },
                10_000,
            ),
        );
    }
    eprintln!("{total:?}");
    assert!(total.caidas > 50 && total.mandatos_confirmados > 300);
    assert!(total.compactaciones > 0);
}

#[test]
fn caos_total_con_tres_y_con_cinco_replicas() {
    let mut total = Cobertura::default();
    for seed in 1..=10 {
        for n in [3usize, 5] {
            suma(
                &mut total,
                caos(
                    seed * 100 + n as u64,
                    n,
                    Chaos {
                        drop: 100,
                        dup: 80,
                        max_delay: 50,
                        crash: 4,
                        crash_mid_step: 2,
                        partition: 8,
                    },
                    15_000,
                ),
            );
        }
    }
    eprintln!("{total:?}");
    assert!(total.caidas > 100 && total.elecciones > 40 && total.mandatos_confirmados > 300);
}

#[test]
fn los_mensajes_mal_formados_se_rechazan() {
    let m = Msg::Append {
        term: 3,
        prev_index: 2,
        prev_term: 1,
        commit: 1,
        entries: vec![Entry {
            term: 3,
            data: vec![1, 2, 3],
        }],
    }
    .encode();
    // Cualquier truncado se rechaza; cualquier resto también.
    for cut in 0..m.len() {
        assert!(Msg::decode(&m[..cut]).is_none(), "truncado en {cut}");
    }
    let mut largo = m.clone();
    largo.push(0);
    assert!(Msg::decode(&largo).is_none());
    assert!(Msg::decode(&[]).is_none());
    assert!(Msg::decode(&[99, 0, 0]).is_none());
    // Un booleano que no es 0 ni 1.
    let mut r = Msg::VoteReply {
        term: 1,
        granted: true,
    }
    .encode();
    *r.last_mut().unwrap() = 2;
    assert!(Msg::decode(&r).is_none());
    // Un término 0 en una entrada.
    let mut z = m.clone();
    z[1 + 8 * 4 + 1..1 + 8 * 4 + 1 + 8].copy_from_slice(&0u64.to_le_bytes());
    assert!(Msg::decode(&z).is_none());
}

// ---------------------------------------------------------------------------------------------------------------
// Casos de libro, con la máquina de estados movida a mano (sin simulador): los errores sutiles de Raft solo
// aparecen en una secuencia muy concreta y una simulación aleatoria puede tardar mucho en encontrarla.
// ---------------------------------------------------------------------------------------------------------------

fn solo(id: usize, n: usize, boot: Boot) -> Raft {
    Raft::new(id, n, RaftConfig::default(), boot, 0)
}

fn boot_con(term: u64, voted_for: Option<usize>, log_terms: &[u64]) -> Boot {
    Boot {
        term,
        voted_for,
        base_index: 0,
        base_term: 0,
        log: log_terms
            .iter()
            .map(|&t| Entry {
                term: t,
                data: vec![1],
            })
            .collect(),
        applied: 0,
    }
}

fn respuesta_de_voto(r: &mut Raft, now: u64, from: usize, term: u64, li: u64, lt: u64) -> bool {
    r.step(
        now,
        from,
        Msg::Vote {
            term,
            last_index: li,
            last_term: lt,
        },
    );
    r.take_outbox()
        .into_iter()
        .find_map(|(to, m)| match m {
            Msg::VoteReply { granted, .. } if to == from => Some(granted),
            _ => None,
        })
        .expect("sin respuesta al voto")
}

#[test]
fn el_voto_solo_se_da_a_quien_tiene_un_diario_al_menos_tan_nuevo() {
    // Mi diario: dos entradas del término 3.
    let now = 10_000;
    let casos = [
        // (último índice, último término del candidato, ¿se le vota?)
        (5, 2, false), // más largo pero de un término más viejo: NO
        (1, 3, false), // del mismo término pero más corto: NO
        (2, 3, true),  // igual: sí
        (3, 3, true),  // más largo del mismo término: sí
        (1, 4, true),  // más corto pero de un término más nuevo: sí
    ];
    for (li, lt, esperado) in casos {
        let mut r = solo(0, 3, boot_con(3, None, &[3, 3]));
        let got = respuesta_de_voto(&mut r, now, 1, 4, li, lt);
        assert_eq!(got, esperado, "candidato con último ({li}, término {lt})");
    }
}

#[test]
fn un_voto_se_guarda_antes_de_contestar_y_no_se_da_dos_veces_en_el_mismo_termino() {
    let now = 10_000;
    let mut r = solo(0, 3, boot_con(3, None, &[3]));
    assert!(respuesta_de_voto(&mut r, now, 1, 4, 1, 3));
    // Lo que Raft pide guardar incluye el voto, y el aviso va en la misma vuelta que la respuesta.
    let guardar = r.take_persist();
    assert!(
        guardar.contains(&Persist::Meta {
            term: 4,
            voted_for: Some(1)
        }),
        "el voto no se pidió guardar: {guardar:?}"
    );
    // Otro candidato del mismo término: no.
    assert!(!respuesta_de_voto(&mut r, now + 5, 2, 4, 1, 3));
    // Tras una caída (se rearranca con lo guardado) sigue sin votar al segundo.
    let mut r2 = solo(
        0,
        3,
        Boot {
            term: 4,
            voted_for: Some(1),
            ..boot_con(4, Some(1), &[3])
        },
    );
    assert!(!respuesta_de_voto(&mut r2, now, 2, 4, 1, 3));
    // Y al mismo que ya votó sí se le repite el voto (por si se perdió la respuesta).
    assert!(respuesta_de_voto(&mut r2, now + 5, 1, 4, 1, 3));
}

#[test]
fn un_lider_nuevo_no_confirma_entradas_de_terminos_viejos_contando_replicas() {
    // El error de la figura 8 del artículo: un líder cuenta que una entrada de un término anterior está en una
    // mayoría y la da por confirmada, y un líder posterior puede sobrescribirla. La regla: solo se confirma por
    // cuenta lo del término actual (y eso arrastra lo anterior).
    let mut r = solo(0, 3, boot_con(2, None, &[1]));
    // Elección: pre-votación, votación, líder en el término 3.
    r.tick(10_000);
    let t = 10_001;
    r.step(
        t,
        1,
        // (la respuesta a una pre-votación lleva el término de quien contesta, no el propuesto)
        Msg::PreVoteReply {
            term: 2,
            granted: true,
        },
    );
    r.step(
        t,
        1,
        Msg::VoteReply {
            term: 3,
            granted: true,
        },
    );
    assert!(r.is_leader(), "debía ser líder");
    assert_eq!(r.last_index(), 2, "debía añadir su entrada vacía");
    let _ = (r.take_outbox(), r.take_persist(), r.take_events());
    // La réplica 1 solo tiene la entrada vieja (índice 1, término 1): está en la mayoría, pero NO se confirma.
    r.step(
        t + 5,
        1,
        Msg::AppendReply {
            term: 3,
            success: true,
            index: 1,
        },
    );
    assert_eq!(r.commit_index(), 0, "se confirmó una entrada de término viejo");
    assert!(r.take_events().is_empty());
    // Cuando la réplica tiene también la del término actual, se confirma todo.
    r.step(
        t + 10,
        1,
        Msg::AppendReply {
            term: 3,
            success: true,
            index: 2,
        },
    );
    assert_eq!(r.commit_index(), 2);
    let confirmadas: Vec<u64> = r
        .take_events()
        .into_iter()
        .filter_map(|e| match e {
            Event::Committed { index, .. } => Some(index),
            _ => None,
        })
        .collect();
    assert_eq!(confirmadas, vec![1, 2]);
}

/// Un «disco» mínimo para comprobar que lo que Raft pide guardar reproduce su diario en memoria.
fn aplicar_a_disco(disco: &mut BTreeMap<u64, Entry>, ops: Vec<Persist>) {
    for op in ops {
        match op {
            Persist::Put { index, entry } => {
                disco.insert(index, entry);
            }
            Persist::TruncateFrom { index } => disco.retain(|&i, _| i < index),
            Persist::Meta { .. } | Persist::Base { .. } => {}
        }
    }
}

#[test]
fn un_conflicto_con_el_lider_descarta_lo_viejo_en_memoria_y_en_disco() {
    // Mi diario: tres entradas del término 1 (ninguna confirmada). El líder del término 2 tiene la primera y una
    // distinta en el segundo lugar: lo mío desde ahí sobra.
    let mut r = solo(0, 3, boot_con(1, None, &[1, 1, 1]));
    let mut disco: BTreeMap<u64, Entry> = (1..=3)
        .map(|i| {
            (
                i,
                Entry {
                    term: 1,
                    data: vec![1],
                },
            )
        })
        .collect();
    r.step(
        100,
        1,
        Msg::Append {
            term: 2,
            prev_index: 1,
            prev_term: 1,
            commit: 0,
            entries: vec![Entry {
                term: 2,
                data: vec![9],
            }],
        },
    );
    aplicar_a_disco(&mut disco, r.take_persist());
    assert_eq!(r.last_index(), 2, "el diario en memoria conserva lo viejo");
    assert_eq!(r.term_at(2), Some(2));
    let en_disco: Vec<(u64, u64)> = disco.iter().map(|(&i, e)| (i, e.term)).collect();
    assert_eq!(en_disco, vec![(1, 1), (2, 2)], "el disco conserva lo viejo");
    assert!(r.stats().conflicts_truncated >= 1);
}

#[test]
fn un_seguidor_rechaza_entradas_si_su_diario_no_coincide_en_el_punto_anterior() {
    // Tengo [1, 1]; el líder del término 3 dice que antes de lo nuevo hay una entrada 2 del término 2: no coincide.
    let mut r = solo(0, 3, boot_con(2, None, &[1, 1]));
    r.step(
        100,
        1,
        Msg::Append {
            term: 3,
            prev_index: 2,
            prev_term: 2,
            commit: 0,
            entries: vec![Entry {
                term: 3,
                data: vec![7],
            }],
        },
    );
    assert_eq!(r.last_index(), 2, "aceptó entradas sin coincidencia previa");
    assert_eq!(r.term_at(2), Some(1));
    let resp = r.take_outbox();
    assert!(
        resp.iter().any(|(to, m)| *to == 1
            && matches!(m, Msg::AppendReply { success: false, .. })),
        "debía rechazar: {resp:?}"
    );
}
