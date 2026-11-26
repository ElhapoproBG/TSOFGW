//! Directorio de sesiones **replicado con Raft** (0.8.0): el cluster completo en marcha, con 3 y 5 réplicas del
//! directorio, y caídas del líder, de minorías, de mayorías y de todo a la vez.
//!
//! Lo que se exige, además de todo lo de siempre (nadie perdido ni duplicado, ninguna cuenta en dos shards):
//! que haya como mucho un líder por término, que todas las réplicas lleguen al mismo estado, que sin mayoría no se
//! complete ningún traspaso nuevo y que con ella el cluster siga sin que nadie lo note más que por un retraso.

use tsofgw_online::bus::SimBusConfig;
use tsofgw_online::cluster_scenario::{
    bot_account, ClusterRun, ClusterScenario, Reconnect, Violations,
};
use tsofgw_online::raft::RaftConfig;
use tsofgw_online::{GameConfig, PersistConfig};
use tsofgw_store::CrashKind;
use tsofgw_transport::SimConfig;

fn scenario(replicas: usize) -> ClusterScenario {
    ClusterScenario {
        grid: (2, 2),
        clients: 24,
        patrollers: 18,
        patrol_ms: 5000,
        duration_ms: 20_000,
        settle_ms: 6000,
        net: SimConfig {
            latency_ms: 20,
            jitter_ms: 5,
            loss: 0.0,
            duplicate: 0.0,
            seed: 3,
        },
        bus: SimBusConfig {
            latency_ms: 8,
            jitter_ms: 4,
            loss: 0.0,
            duplicate: 0.0,
            seed: 11,
        },
        game: GameConfig {
            npc_count: 800,
            npc_speed: 10.0,
            spawn_half_extent: 40.0,
            max_visible: 100_000,
            ..GameConfig::default()
        },
        persist: Some(PersistConfig {
            npc_checkpoint_ticks: 40,
            player_save_ticks: 20,
            sync_ticks: 10,
            compact_bytes: 256 << 10,
            slow_retry_ticks: 20,
            login_timeout_ticks: 400,
        }),
        directory: true,
        dir_replicas: replicas,
        reconnect: Reconnect {
            enabled: true,
            delay_ms: 500,
            any_shard: true,
        },
        ..ClusterScenario::default()
    }
}

fn all_playing(run: &ClusterRun) -> usize {
    run.bots.iter().filter(|b| b.client.is_playing()).count()
}

fn wait_all_playing(run: &mut ClusterRun, max_ms: u64) -> bool {
    let end = run.now + max_ms;
    while run.now < end {
        run.step();
        if all_playing(run) == run.bots.len() {
            return true;
        }
    }
    false
}

fn wait_stable(run: &mut ClusterRun, max_ms: u64, stable_ms: u64) -> bool {
    let end = run.now + max_ms;
    let mut since: Option<u64> = None;
    while run.now < end {
        run.step();
        if all_playing(run) == run.bots.len() {
            let t = *since.get_or_insert(run.now);
            if run.now - t >= stable_ms {
                return true;
            }
        } else {
            since = None;
        }
    }
    false
}

fn step_until(run: &mut ClusterRun, max_ms: u64, mut f: impl FnMut(&ClusterRun) -> bool) -> bool {
    let end = run.now + max_ms;
    while run.now < end {
        run.step();
        if f(run) {
            return true;
        }
    }
    false
}

fn assert_clean(run: &ClusterRun, what: &str) {
    assert_eq!(
        run.violations,
        Violations::default(),
        "{what}: se violó un invariante en marcha. La primera vez:\n{}",
        run.first_violation.as_deref().unwrap_or("(sin detalle)")
    );
}

fn check_exact(run: &mut ClusterRun, what: &str) -> tsofgw_online::cluster_scenario::Invariants {
    let mut inv = run.check(true);
    for _ in 0..60 {
        if inv.is_exact() {
            break;
        }
        run.run_for(50);
        inv = run.check(true);
    }
    assert!(inv.is_exact(), "{what}: {inv:?}");
    inv
}

fn finish_and_check(run: &mut ClusterRun, clients: usize, what: &str) {
    run.settle();
    assert!(
        wait_stable(run, 60_000, 6500),
        "{what}: solo juegan {}/{clients}",
        all_playing(run)
    );
    run.run_for(3000);
    let inv = check_exact(run, what);
    assert_eq!(inv.players_playing, clients, "{what}");
    assert_eq!(inv.accounts.outbox, 0, "{what}: {:?}", inv.accounts);
    assert_clean(run, what);
}

/// Espera a que haya un líder (y solo uno) entre las réplicas vivas.
fn wait_leader(run: &mut ClusterRun, max_ms: u64) -> Option<usize> {
    let mut found = None;
    step_until(run, max_ms, |r| {
        let leaders: Vec<usize> = r
            .reps
            .iter()
            .enumerate()
            .filter(|(_, d)| d.as_ref().is_some_and(|d| d.is_leader()))
            .map(|(i, _)| i)
            .collect();
        if leaders.len() == 1 {
            found = Some(leaders[0]);
            true
        } else {
            false
        }
    });
    found
}

/// Como mucho un líder por término, siempre.
fn assert_one_leader_per_term(run: &ClusterRun) {
    let mut seen = std::collections::BTreeMap::new();
    for (i, d) in run.reps.iter().enumerate() {
        if let Some(d) = d {
            if d.is_leader() {
                if let Some(other) = seen.insert(d.raft_term(), i) {
                    panic!("dos líderes en el término {}: {other} y {i}", d.raft_term());
                }
            }
        }
    }
}

/// Todas las réplicas vivas con lo mismo aplicado tienen el mismo estado de cuentas.
fn assert_replicas_agree(run: &ClusterRun, what: &str) {
    let alive: Vec<_> = run.reps.iter().flatten().collect();
    let top = alive.iter().map(|d| d.applied_index()).max().unwrap_or(0);
    let same: Vec<_> = alive.iter().filter(|d| d.applied_index() == top).collect();
    assert!(!same.is_empty());
    for d in &same[1..] {
        assert_eq!(
            d.account_count(),
            same[0].account_count(),
            "{what}: distinto número de cuentas"
        );
        assert_eq!(
            d.accounts_per_shard(),
            same[0].accounts_per_shard(),
            "{what}"
        );
        for i in 0..run.bots.len() {
            let a = bot_account(i);
            assert_eq!(d.lookup(a), same[0].lookup(a), "{what}: cuenta {a}");
        }
    }
}

#[test]
fn con_tres_y_con_cinco_replicas_el_cluster_se_comporta_y_las_replicas_coinciden() {
    for replicas in [3, 5] {
        let s = scenario(replicas);
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(wait_all_playing(&mut run, 15_000), "réplicas {replicas}");
        let leader = wait_leader(&mut run, 5000).expect("sin líder");
        run.run_for(s.duration_ms);
        assert_one_leader_per_term(&run);
        assert!(run.reps.iter().flatten().count() == replicas);
        finish_and_check(&mut run, s.clients, &format!("{replicas} réplicas"));
        let t = run.total_stats();
        assert!(
            t.handoffs_completed > 0,
            "réplicas {replicas}: ningún traspaso"
        );
        // Las réplicas se ponen al día entre sí.
        run.run_for(2000);
        assert_replicas_agree(&run, "sin fallos");
        let l2 = run.leader_replica().expect("líder");
        // Sin fallos el liderazgo es estable (no se re-eligió sin motivo).
        assert_eq!(
            l2, leader,
            "réplicas {replicas}: cambió el líder sin fallos"
        );
        let r = run.report();
        let d = r.directory.as_ref().unwrap();
        assert_eq!(d.accounts, s.clients, "réplicas {replicas}");
        println!("{}", r.render());
    }
}

#[test]
fn si_cae_el_lider_se_elige_otro_y_los_traspasos_siguen() {
    for replicas in [3, 5] {
        let s = scenario(replicas);
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(wait_all_playing(&mut run, 15_000));
        run.run_for(6000);
        let old = wait_leader(&mut run, 5000).expect("sin líder");
        let term_old = run.reps[old].as_ref().unwrap().raft_term();
        let before = run.total_stats().handoffs_completed;
        run.crash_replica(old, CrashKind::Torn(9));
        let new = wait_leader(&mut run, 10_000).expect("no se eligió otro líder");
        assert_ne!(new, old);
        assert!(run.reps[new].as_ref().unwrap().raft_term() > term_old);
        run.run_for(15_000);
        assert_one_leader_per_term(&run);
        let during = run.total_stats().handoffs_completed - before;
        assert!(
            during >= 3,
            "réplicas {replicas}: con una réplica caída solo se completaron {during} traspasos"
        );
        assert_clean(&run, "líder caído");
        // La caída vuelve, se pone al día y no disputa el liderazgo.
        run.restart_replica(old).unwrap();
        run.run_for(8000);
        assert_replicas_agree(&run, "tras volver");
        finish_and_check(&mut run, s.clients, &format!("líder caído, {replicas}"));
    }
}

#[test]
fn sin_mayoria_nada_cruza_pero_nada_se_rompe_y_con_ella_todo_sigue() {
    let s = scenario(3);
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 15_000));
    run.run_for(6000);
    let leader = wait_leader(&mut run, 5000).expect("sin líder");
    let a = leader;
    let b = (leader + 1) % 3;
    run.crash_replica(a, CrashKind::Clean);
    run.crash_replica(b, CrashKind::Torn(5));
    run.run_for(1500);
    let before = run.total_stats().handoffs_completed;
    let mut max_outbox = 0;
    for _ in 0..8 {
        run.run_for(2000);
        max_outbox = max_outbox.max(run.check_accounts().outbox);
        // La réplica que queda sola no puede ser líder: no tiene quórum.
        assert!(
            !run.reps.iter().flatten().any(|d| d.is_leader()),
            "una réplica sola se cree líder"
        );
    }
    let during = run.total_stats().handoffs_completed - before;
    assert!(during <= 4, "se completaron {during} traspasos sin mayoría");
    assert!(max_outbox >= 1, "no hubo ningún traspaso en espera");
    assert_clean(&run, "sin mayoría");
    run.restart_replica(a).unwrap();
    run.run_for(6000);
    assert!(
        wait_leader(&mut run, 10_000).is_some(),
        "sin líder al volver la mayoría"
    );
    run.restart_replica(b).unwrap();
    run.run_for(8000);
    assert_replicas_agree(&run, "tras recuperar la mayoría");
    finish_and_check(&mut run, s.clients, "sin mayoría");
    assert!(run.total_stats().handoffs_completed > before + during);
}

#[test]
fn una_replica_muy_atrasada_se_pone_al_dia_con_una_instantanea() {
    let mut s = scenario(3);
    // Diario cortísimo: en pocos segundos se compacta y la réplica caída ya no puede seguirlo entrada a entrada.
    s.dir_raft = RaftConfig {
        compact_after: 12,
        keep_tail: 4,
        ..RaftConfig::default()
    };
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 15_000));
    let leader = wait_leader(&mut run, 5000).expect("sin líder");
    let victim = (leader + 1) % 3;
    run.crash_replica(victim, CrashKind::Clean);
    run.run_for(30_000);
    assert_clean(&run, "mientras cae");
    let log_before = run.reps[leader].as_ref().unwrap().raft_log_len();
    assert!(
        log_before < 100,
        "el diario del líder no se compacta: {log_before}"
    );
    run.restart_replica(victim).unwrap();
    run.run_for(10_000);
    assert_replicas_agree(&run, "tras la instantánea");
    let r = run.report();
    let d = r.directory.as_ref().unwrap();
    let installed: u64 = d.replicas.iter().map(|x| x.stats.snapshots_installed).sum();
    let compacted: u64 = d.replicas.iter().map(|x| x.stats.compactions).sum();
    assert!(compacted > 0, "no hubo compactaciones");
    assert!(installed > 0, "no se instaló ninguna instantánea: {d:?}");
    finish_and_check(&mut run, s.clients, "instantánea");
}

#[test]
fn una_tormenta_de_caidas_con_tres_y_con_cinco_replicas_no_pierde_ni_duplica_nada() {
    for (replicas, seed) in [(3, 0x7A11), (5, 0x5E55)] {
        let mut s = scenario(replicas);
        s.net.loss = 0.02;
        s.bus.loss = 0.02;
        s.dir_raft = RaftConfig {
            compact_after: 64,
            keep_tail: 16,
            ..RaftConfig::default()
        };
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(wait_all_playing(&mut run, 15_000));
        let storm = run.chaos(12, seed).unwrap();
        assert!(storm.directory_crashes >= 3, "{storm:?}");
        assert!(storm.shard_crashes >= 5, "{storm:?}");
        run.run_for(4000);
        run.settle();
        for i in 0..replicas {
            if run.reps[i].is_none() {
                run.restart_replica(i).unwrap();
            }
        }
        run.run_for(6000);
        assert_one_leader_per_term(&run);
        assert!(wait_leader(&mut run, 10_000).is_some());
        assert!(
            wait_all_playing(&mut run, 60_000),
            "réplicas {replicas}: solo juegan {}/{}",
            all_playing(&run),
            s.clients
        );
        run.run_for(s.settle_ms);
        let inv = check_exact(&mut run, &format!("tormenta, {replicas}"));
        assert_eq!(inv.players_playing, s.clients);
        assert_eq!(inv.accounts.outbox, 0, "{:?}", inv.accounts);
        assert_clean(&run, "tormenta");
        assert_replicas_agree(&run, "tras la tormenta");
        assert!(run.rep_restarts >= 3, "pocos reinicios de réplica");
        println!(
            "réplicas {replicas}: reinicios shards {} réplicas {}",
            run.restarts, run.rep_restarts
        );
    }
}

#[test]
fn perdido_el_directorio_replicado_se_reconstruye_con_lo_que_guardan_los_shards() {
    let s = scenario(3);
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 15_000));
    run.run_for(10_000);
    for sh in 0..run.shards.len() {
        run.crash_shard(sh, CrashKind::Clean);
    }
    // Se pierde el grupo entero: tres réplicas nuevas y vacías.
    for i in 0..3 {
        run.crash_replica(i, CrashKind::Clean);
        run.rep_media[i] = tsofgw_store::MemMedium::new();
    }
    for i in 0..3 {
        run.restart_replica(i).unwrap();
    }
    assert!(wait_leader(&mut run, 10_000).is_some());
    let report = run.rebuild_directory().unwrap();
    assert_eq!(report.accounts, s.clients, "{report:?}");
    assert!(report.conflicts.is_empty(), "{report:?}");
    run.run_for(4000);
    for sh in 0..run.shards.len() {
        run.restart_shard(sh).unwrap();
    }
    run.run_for(3000);
    finish_and_check(&mut run, s.clients, "directorio replicado reconstruido");
    assert_replicas_agree(&run, "reconstruido");
    let l = run.leader_replica().unwrap();
    assert_eq!(run.reps[l].as_ref().unwrap().account_count(), s.clients);
}

#[test]
fn una_carpeta_no_se_abre_con_otra_identidad_ni_en_el_otro_modo() {
    use tsofgw_online::bus::SimBus;
    use tsofgw_online::{Directory, DirectoryConfig};
    use tsofgw_store::{MemMedium, OpenMode};

    let bus = SimBus::new(2 + 3, SimBusConfig::default());
    let key = Directory::store_key(b"k");
    let open = |m: &MemMedium, replicas: usize, replica: usize| {
        Directory::open(
            Box::new(m.clone()),
            &key,
            Box::new(bus.end((2 + replica) as u16)),
            2,
            DirectoryConfig {
                replicas,
                replica,
                ..DirectoryConfig::default()
            },
            OpenMode::Recover,
        )
        .map(|_| ())
    };
    // Réplica 1 de 3: queda marcada en su carpeta.
    let m = MemMedium::new();
    open(&m, 3, 1).unwrap();
    open(&m, 3, 1).unwrap();
    let e = open(&m, 3, 2).unwrap_err();
    assert!(e.contains("réplica 1 de 3"), "{e}");
    let e = open(&m, 5, 1).unwrap_err();
    assert!(e.contains("réplica 1 de 3"), "{e}");
    let e = open(&m, 1, 0).unwrap_err();
    assert!(e.contains("réplica"), "{e}");
    // Una carpeta de un directorio sin réplicas (con cuentas) no se abre como réplica.
    let m = MemMedium::new();
    {
        let (mut d, _) = Directory::open(
            Box::new(m.clone()),
            &key,
            Box::new(bus.end(2)),
            2,
            DirectoryConfig::default(),
            OpenMode::Recover,
        )
        .unwrap();
        d.import(&[(7, 0, 1)]).unwrap();
        d.shutdown().unwrap();
    }
    let e = open(&m, 3, 0).unwrap_err();
    assert!(e.contains("sin réplicas"), "{e}");
    // Número de réplicas imposible.
    for (n, i) in [(2usize, 0usize), (4, 0), (9, 0), (3, 3)] {
        assert!(open(&MemMedium::new(), n, i).is_err(), "{n} {i}");
    }
}

#[test]
fn los_mensajes_hostiles_al_directorio_replicado_se_descartan_sin_danar_nada() {
    use tsofgw_online::bus::{PeerBus, PeerMsg};
    let s = scenario(3);
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 15_000));
    let leader = wait_leader(&mut run, 5000).expect("sin líder");
    run.run_for(4000);
    let n = run.shards.len() as u16;
    let applied_before = run.reps[leader].as_ref().unwrap().applied_index();
    let mal_before: u64 = run.reps.iter().flatten().map(|d| d.stats().malformed).sum();
    // Basura, un mensaje de Raft de un shard (que no es réplica), peticiones de cuentas imposibles, un traspaso a
    // sí mismo, y una petición de réplica a réplica.
    let junk: Vec<Vec<u8>> = vec![
        vec![],
        vec![0xFF; 3],
        vec![15, 1, 2, 3],
        PeerMsg::Raft(vec![9; 40]).encode(),
        PeerMsg::DirLogin {
            account: u64::MAX,
            home: 0,
        }
        .encode(),
        PeerMsg::DirLogin {
            account: 5000,
            home: 77,
        }
        .encode(),
        PeerMsg::DirTransfer {
            xfer: 1,
            account: 1000,
            epoch: 1,
            to: 0,
        }
        .encode(),
    ];
    for r in 0..3u16 {
        for j in &junk {
            run.bus.end(0).send(run.now, n + r, j.clone());
        }
    }
    // De réplica a réplica solo vale Raft.
    let login = PeerMsg::DirLogin {
        account: 1000,
        home: 0,
    }
    .encode();
    run.bus.end(n).send(run.now, n + 1, login.clone());
    run.bus.end(n + 1).send(run.now, n + 2, login);
    // Un mensaje de Raft de verdad mal formado, mandado desde una réplica.
    run.bus
        .end(n + 2)
        .send(run.now, n, PeerMsg::Raft(vec![0xAB; 17]).encode());
    run.run_for(3000);
    let mal_after: u64 = run.reps.iter().flatten().map(|d| d.stats().malformed).sum();
    assert!(mal_after > mal_before, "no se contó nada mal formado");
    assert_one_leader_per_term(&run);
    // Nada de eso cambió el estado: ninguna cuenta se movió, nadie se coló en el diario y las cuentas siguen donde estaban.
    assert!(run
        .reps
        .iter()
        .flatten()
        .all(|d| d.account_count() == s.clients));
    assert!(
        run.reps[run.leader_replica().unwrap()]
            .as_ref()
            .unwrap()
            .applied_index()
            >= applied_before
    );
    run.run_for(s.duration_ms);
    finish_and_check(&mut run, s.clients, "mensajes hostiles");
    assert_replicas_agree(&run, "mensajes hostiles");
}

#[test]
fn un_traspaso_de_epoca_vieja_o_pedido_por_quien_no_es_el_dueno_se_rechaza() {
    use tsofgw_online::bus::{PeerBus, PeerMsg};
    let mut s = scenario(3);
    s.patrollers = 0;
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 15_000));
    let leader = wait_leader(&mut run, 5000).expect("sin líder");
    run.run_for(3000);
    let n = run.shards.len() as u16;
    let account = bot_account(0);
    let (owner, epoch) = run.reps[leader].as_ref().unwrap().lookup(account).unwrap();
    let other = (owner + 1) % n;
    let rejected0 = run.reps[leader].as_ref().unwrap().stats().rejected;
    // Del dueño, pero con una época que no es la de la cuenta.
    run.bus.end(owner).send(
        run.now,
        n + leader as u16,
        PeerMsg::DirTransfer {
            xfer: 90_001,
            account,
            epoch: epoch + 5,
            to: other,
        }
        .encode(),
    );
    // Con la época buena, pero de un shard que no es el dueño.
    let intruder = (owner + 2) % n;
    run.bus.end(intruder).send(
        run.now,
        n + leader as u16,
        PeerMsg::DirTransfer {
            xfer: 90_002,
            account,
            epoch,
            to: other,
        }
        .encode(),
    );
    run.run_for(600);
    let d = run.reps[leader].as_ref().unwrap();
    assert!(
        d.stats().rejected >= rejected0 + 2,
        "no se rechazaron las dos peticiones: {} → {}",
        rejected0,
        d.stats().rejected
    );
    let (sh, ep) = d.lookup(account).unwrap();
    assert!(ep < epoch + 5, "la época saltó a {ep}");
    assert!(
        (sh == owner && ep == epoch) || ep > epoch,
        "estado imposible: ({sh}, {ep}) desde ({owner}, {epoch})"
    );
    run.run_for(s.duration_ms);
    finish_and_check(&mut run, s.clients, "peticiones viejas");
    assert_replicas_agree(&run, "peticiones viejas");
}

#[test]
fn una_replica_que_cae_en_seco_recupera_todo_lo_que_habia_aplicado_y_contestado() {
    // Si una réplica contestara o enviara sin sincronizar su disco, una caída con la cola rota le haría olvidar lo
    // que ya había confirmado a otras: aquí, en cada caída, lo recuperado debe ser lo aplicado, ni una entrada menos.
    for kind in [CrashKind::Torn(7), CrashKind::Hole, CrashKind::Clean] {
        let s = scenario(3);
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(wait_all_playing(&mut run, 15_000));
        let leader = wait_leader(&mut run, 5000).expect("sin líder");
        run.run_for(12_000);
        for victim in [(leader + 1) % 3, leader] {
            let d = run.reps[victim].as_ref().unwrap();
            let (applied, term) = (d.applied_index(), d.raft_term());
            assert!(applied >= 20, "poca actividad: {applied}");
            assert!(d.stats().commits > 0, "nunca sincronizó");
            run.crash_replica(victim, kind);
            run.restart_replica(victim).unwrap();
            let d = run.reps[victim].as_ref().unwrap();
            assert_eq!(
                d.applied_index(),
                applied,
                "{kind:?}: la réplica {victim} olvidó lo que había aplicado"
            );
            assert!(d.raft_term() >= term, "{kind:?}: el término retrocedió");
            run.run_for(4000);
        }
        finish_and_check(&mut run, s.clients, &format!("caída en seco {kind:?}"));
        assert_replicas_agree(&run, "caída en seco");
    }
}
