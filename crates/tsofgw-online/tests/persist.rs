//! Persistencia y directorio de sesiones: caídas de shards con cola rota, reinicios, particiones largas,
//! directorio caído o perdido. Todo con el cluster completo en marcha y bots cruzando bordes.
//!
//! Lo que se exige, además de lo de siempre (ningún NPC ni jugador perdido o duplicado): que **ninguna
//! cuenta** esté nunca en dos shards a la vez (se vigila cada 100 ms simulados durante toda la marcha),
//! que una cuenta que ya cambió de shard no desaparezca ni retroceda, y que el directorio coincida con lo
//! que guardan los shards.

use tsofgw_online::bus::{PeerBus, PeerMsg, SimBusConfig};
use tsofgw_online::cluster_scenario::{
    bot_account, ClusterRun, ClusterScenario, Reconnect, Violations,
};
use tsofgw_online::sim::STEP_Q;
use tsofgw_online::{GameConfig, PersistConfig};
use tsofgw_store::CrashKind;
use tsofgw_transport::SimConfig;

fn scenario(directory: bool) -> ClusterScenario {
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
            // Valores pequeños para que, en pocos segundos simulados, haya guardados y compactaciones.
            npc_checkpoint_ticks: 40,
            player_save_ticks: 20,
            sync_ticks: 10,
            compact_bytes: 256 << 10,
            slow_retry_ticks: 20,
            login_timeout_ticks: 400,
        }),
        directory,
        reconnect: Reconnect {
            enabled: true,
            delay_ms: 500,
            any_shard: directory,
        },
        ..ClusterScenario::default()
    }
}

fn all_playing(run: &ClusterRun) -> usize {
    run.bots.iter().filter(|b| b.client.is_playing()).count()
}

/// Avanza hasta que todos los clientes juegan (o se acaba el plazo).
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

fn assert_clean(run: &ClusterRun, what: &str) {
    assert_eq!(
        run.violations,
        Violations::default(),
        "{what}: se violó un invariante en marcha. La primera vez:\n{}",
        run.first_violation.as_deref().unwrap_or("(sin detalle)")
    );
}

#[test]
fn un_cluster_con_almacen_se_comporta_como_sin_el_y_guarda_lo_que_cambia() {
    for directory in [false, true] {
        let s = scenario(directory);
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(
            wait_all_playing(&mut run, 6000),
            "no entraron todos (directorio: {directory})"
        );
        run.run_for(s.duration_ms);
        run.settle();
        run.run_for(s.settle_ms);
        let inv = check_exact(&mut run, &format!("directorio {directory}"));
        assert_eq!(inv.players_playing, s.clients);
        assert_clean(&run, "");
        // Hubo traspasos de jugador de verdad, y las cuentas constan en los almacenes.
        let done: u64 = run
            .shards
            .iter()
            .map(|sh| sh.cluster_stats().unwrap().handoffs_completed)
            .sum();
        assert!(done >= 20, "pocos traspasos: {done}");
        // El camino seguro (escribir antes de entregar, anotar en el directorio, esperar al destino que
        // también escribe) cuesta unos pocos ticks, no segundos: en una red en calma, la mayoría de los
        // traspasos se cierra en uno o dos ticks y ninguno pasa de medio segundo.
        let (acked, sum, max) = run.shards.iter().fold((0u64, 0u64, 0u32), |a, sh| {
            let c = sh.cluster_stats().unwrap();
            (
                a.0 + c.handover_acked,
                a.1 + c.handover_ticks_sum,
                a.2.max(c.handover_ticks_max),
            )
        });
        assert!(acked >= done - 2, "traspasos sin acuse: {acked} de {done}");
        assert!(
            max <= 6,
            "un traspaso tardó {max} ticks en una red en calma"
        );
        assert!(
            sum <= acked * 3,
            "los traspasos tardan de media más de 3 ticks: {sum} ticks en {acked}"
        );
        assert_eq!(inv.accounts.accounts, s.clients, "{:?}", inv.accounts);
        assert_eq!(inv.accounts.outbox, 0, "{:?}", inv.accounts);
        if directory {
            let d = run.dir.as_ref().unwrap();
            assert_eq!(d.account_count(), s.clients);
            assert!(d.stats().committed >= done, "{:?}", d.stats());
            assert_eq!(d.stats().rejected + d.stats().unknown, 0);
        }
        for sh in &run.shards {
            let st = sh.persist_stats().unwrap();
            assert!(st.commits > 0 && st.npc_saves > 0, "{st:?}");
            assert!(!sh.persistence_failed());
        }
    }
}

#[test]
fn un_shard_que_cae_y_vuelve_recupera_sus_npc_y_nadie_se_pierde_ni_se_duplica() {
    for kind in [
        CrashKind::Clean,
        CrashKind::Torn(37),
        CrashKind::Torn(3000),
        CrashKind::Hole,
    ] {
        let s = scenario(false);
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(wait_all_playing(&mut run, 6000));
        run.run_for(9000);
        // Sin pérdidas de red ni caídas, el reparto es exacto justo antes.
        let before = run.check(false);
        assert_eq!(before.npc_lost, 0);
        run.crash_shard(1, kind);
        run.run_for(1500);
        let rep = run.restart_shard(1).unwrap();
        assert!(!rep.first_start, "{rep:?}");
        assert!(rep.npcs > 0, "no restauró NPC: {rep:?}");
        run.run_for(8000);
        run.settle();
        run.run_for(s.settle_ms);
        let inv = check_exact(&mut run, &format!("{kind:?}"));
        assert_eq!(
            inv.players_playing, s.clients,
            "{kind:?}: no volvieron todos"
        );
        assert_clean(&run, "");
    }
}

#[test]
fn una_tormenta_de_caidas_aleatorias_no_pierde_ni_duplica_nada() {
    for directory in [false, true] {
        let mut s = scenario(directory);
        s.net.loss = 0.02;
        s.bus.loss = 0.02;
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(wait_all_playing(&mut run, 8000));
        let storm = run.chaos(10, 0xC4A05 + directory as u64).unwrap();
        assert_eq!(storm.rounds, 10);
        assert!(storm.shard_crashes >= 6, "{storm:?}");
        assert_eq!(storm.directory_crashes > 0, directory, "{storm:?}");
        run.run_for(4000);
        run.settle();
        // Con todo en calma, los clientes vuelven a entrar y el cluster converge.
        assert!(
            wait_all_playing(&mut run, 40_000),
            "directorio {directory}: solo juegan {}/{}",
            all_playing(&run),
            s.clients
        );
        run.run_for(s.settle_ms);
        let inv = check_exact(&mut run, &format!("directorio {directory}"));
        assert_eq!(inv.players_playing, s.clients);
        assert_eq!(inv.accounts.outbox, 0, "{:?}", inv.accounts);
        assert_clean(&run, "");
        assert!(run.restarts >= 8, "pocos reinicios: {}", run.restarts);
        // La tormenta debe haber pasado por los caminos difíciles, no solo por los fáciles.
        let t = run.total_stats();
        let sum = |f: fn(&tsofgw_online::RestoreReport) -> usize| -> usize {
            run.restores.iter().map(|(_, r)| f(r)).sum()
        };
        println!(
            "directorio {directory}: reinicios {} (dir {}), reconexiones {}, traspasos {} (cancelados {}), \
             reenvíos {}, adopciones sin cliente {}, repetidos {} (por época {}), redirecciones al entrar {}, \
             esperas al entrar {} | restaurado: {} NPC, {} traspasos de NPC, {} traspasos de jugador, {} residentes",
            run.restarts,
            run.dir_restarts,
            run.reconnects,
            t.handoffs_completed,
            t.handoffs_aborted,
            t.handover_retries,
            t.offline_adoptions,
            t.npc_xfer_dups,
            t.handovers_stale,
            t.login_redirects,
            t.login_waits,
            sum(|r| r.npcs),
            sum(|r| r.npc_transfers),
            sum(|r| r.handovers),
            sum(|r| r.residents)
        );
    }
}

#[test]
fn bot_account_es_estable() {
    assert_eq!(bot_account(0), 1000);
}

// ---------------------------------------------------------------------------------------------------
// Caídas en el punto exacto de un traspaso
// ---------------------------------------------------------------------------------------------------

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

/// Avanza hasta que todos los clientes llevan `stable_ms` seguidos jugando. Esperar solo a «todos
/// juegan» no basta: tras la caída de un servidor, sus clientes siguen creyendo que juegan hasta que
/// vence el plazo del transporte (5 s) y entonces se desconectan.
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

/// El reparto, exacto. Con los clientes quietos los NPC siguen moviéndose y siempre hay alguno
/// cruzando un borde, de modo que en un instante concreto puede haber un traspaso en vuelo (y un NPC
/// contado en los dos lados). Se da un margen de 3 s para ver un instante sin ninguno: un NPC
/// duplicado o perdido de verdad no desaparece con el tiempo y se sigue viendo en todos.
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

/// Deja que el cluster converja con los clientes quietos y comprueba que todo cuadra.
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

#[test]
fn una_caida_con_un_traspaso_de_npc_a_medias_no_pierde_ni_duplica_el_npc() {
    // Quién cae: el que envía, el que recibe, o los dos.
    for victim in ["origen", "destino", "ambos"] {
        for kind in [CrashKind::Clean, CrashKind::Torn(11)] {
            let mut s = scenario(false);
            s.clients = 4;
            s.patrollers = 0;
            s.game.npc_count = 1500;
            s.game.npc_speed = 14.0;
            let mut run = ClusterRun::new(&s).unwrap();
            assert!(wait_all_playing(&mut run, 6000));
            let mut found = None;
            let ok = step_until(&mut run, 30_000, |r| {
                for (i, sh) in r.shards.iter().enumerate() {
                    if let Some(&(id, to)) = sh.npc_transfers_out().first() {
                        found = Some((i, id, to as usize));
                        return true;
                    }
                }
                false
            });
            assert!(ok, "no hubo ningún traspaso de NPC");
            let (src, _id, dst) = found.unwrap();
            let victims: Vec<usize> = match victim {
                "origen" => vec![src],
                "destino" => vec![dst],
                _ => vec![src, dst],
            };
            for &v in &victims {
                run.crash_shard(v, kind);
            }
            run.run_for(800);
            let mut reports = Vec::new();
            for &v in &victims {
                reports.push(run.restart_shard(v).unwrap());
            }
            if victim != "destino" {
                assert!(
                    reports.iter().any(|r| r.npc_transfers >= 1),
                    "{victim}/{kind:?}: el origen no recuperó su traspaso: {reports:?}"
                );
            }
            run.run_for(5000);
            finish_and_check(&mut run, s.clients, &format!("{victim}/{kind:?}"));
            let inv = run.check(false);
            assert_eq!(
                (inv.npc_lost, inv.npc_duplicated),
                (0, 0),
                "{victim}/{kind:?}"
            );
        }
    }
}

/// En qué punto del traspaso de un jugador se tira lo que se tire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Before {
    /// El origen ya congeló al jugador y lo anotó, pero el directorio aún no lo ha anotado.
    Directory,
    /// El directorio ya anotó el cambio; el destino aún no adoptó al jugador.
    Adoption,
    /// El destino ya adoptó al jugador; el origen aún no tiene el acuse.
    Ack,
}

/// Avanza hasta que algún traspaso de jugador esté en el punto pedido y lo devuelve como
/// `(origen, destino, cuenta, época)`.
fn wait_for_moment(
    run: &mut ClusterRun,
    moment: Before,
    max_ms: u64,
) -> Option<(usize, usize, u64, u64)> {
    let mut found = None;
    step_until(run, max_ms, |r| {
        for (i, sh) in r.shards.iter().enumerate() {
            for o in sh.handover_outbox() {
                let to = o.to as usize;
                let at_dest = r.shards[to].account_records(o.account).resident;
                let hit = match moment {
                    Before::Directory => !o.committed,
                    Before::Adoption => o.committed && at_dest.is_none_or(|(_, e)| e < o.epoch),
                    Before::Ack => o.committed && at_dest.is_some_and(|(_, e)| e == o.epoch),
                };
                if hit {
                    found = Some((i, to, o.account, o.epoch));
                    return true;
                }
            }
        }
        false
    });
    found
}

fn handoff_crash_case(directory: bool, moment: Before, victims: &str, kind: CrashKind) {
    let what = format!("dir={directory} {moment:?} cae={victims} {kind:?}");
    let mut s = scenario(directory);
    s.clients = 12;
    s.patrollers = 12;
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 8000), "{what}");
    let Some((src, dst, account, epoch)) = wait_for_moment(&mut run, moment, 40_000) else {
        // Sin directorio no existe el primer punto (el cambio nace ya anotado); cualquier otro que no
        // se dé es que la prueba no está probando lo que dice.
        assert!(
            !directory && moment == Before::Directory,
            "{what}: no se dio el momento"
        );
        return;
    };
    let victims: Vec<usize> = match victims {
        "origen" => vec![src],
        "destino" => vec![dst],
        "directorio" => vec![],
        _ => vec![src, dst],
    };
    for &v in &victims {
        run.crash_shard(v, kind);
    }
    if victims.is_empty() && directory {
        run.crash_directory(kind);
    }
    run.run_for(1200);
    for &v in &victims {
        run.restart_shard(v).unwrap();
    }
    if victims.is_empty() && directory {
        run.restart_directory().unwrap();
    }
    // La cuenta no puede desaparecer ni retroceder: o consta como residente con la época nueva, o sigue
    // en el buzón de salida de quien la entregaba.
    run.run_for(8000);
    finish_and_check(&mut run, s.clients, &what);
    let mut holders = 0;
    for sh in &run.shards {
        let r = sh.account_records(account);
        if r.resident.is_some_and(|(_, e)| e >= epoch) {
            holders += 1;
        }
    }
    assert_eq!(
        holders,
        1,
        "{what}: la cuenta {account} tiene {holders} dueños\n{}",
        run.describe_account(account)
    );
}

#[test]
fn una_caida_en_cada_punto_del_traspaso_de_un_jugador_no_pierde_ni_duplica_la_cuenta() {
    for directory in [false, true] {
        for moment in [Before::Directory, Before::Adoption, Before::Ack] {
            for victims in ["origen", "destino", "ambos", "directorio"] {
                if victims == "directorio" && !directory {
                    continue;
                }
                for kind in [CrashKind::Clean, CrashKind::Torn(5)] {
                    handoff_crash_case(directory, moment, victims, kind);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Particiones largas, directorio caído o perdido, cuentas dobles
// ---------------------------------------------------------------------------------------------------

#[test]
fn una_particion_larga_en_pleno_traspaso_no_pierde_al_jugador() {
    // Es el fallo de la 0.4: un traspaso sin acuse durante mucho tiempo hacía perder al jugador. Ahora el
    // origen conserva su estado en disco y lo reenvía hasta que el destino lo acusa.
    for directory in [false, true] {
        for (moment, cut) in [
            (Before::Adoption, "destino"),
            (Before::Adoption, "origen"),
            (Before::Directory, "origen-directorio"),
        ] {
            if moment == Before::Directory && !directory {
                continue;
            }
            let what = format!("dir={directory} {moment:?} corte={cut}");
            let mut s = scenario(directory);
            s.clients = 12;
            s.patrollers = 12;
            let mut run = ClusterRun::new(&s).unwrap();
            assert!(wait_all_playing(&mut run, 8000), "{what}");
            let (src, dst, account, epoch) = wait_for_moment(&mut run, moment, 40_000)
                .unwrap_or_else(|| panic!("{what}: no se dio el momento"));
            let dir_id = run.partition.shard_count() as u16;
            match cut {
                "destino" => run.bus.isolate(dst as u16, true),
                "origen" => run.bus.isolate(src as u16, true),
                _ => run.bus.set_link_down(src as u16, dir_id, true),
            }
            // Mucho más que el plazo de un traspaso (`handoff_timeout_ticks`) y que cualquier reintento.
            for _ in 0..8 {
                run.run_for(5000);
                // Mientras dura el corte, el traspaso sigue pendiente en el origen: ni se rindió ni se
                // adoptó dos veces.
                assert!(
                    run.shards[src]
                        .handover_outbox()
                        .iter()
                        .any(|o| o.account == account),
                    "{what}: el origen abandonó el traspaso de la cuenta {account} a los {} ms",
                    run.now
                );
            }
            assert_clean(&run, &what);
            run.bus.isolate(dst as u16, false);
            run.bus.isolate(src as u16, false);
            run.bus.set_link_down(src as u16, dir_id, false);
            run.run_for(8000);
            finish_and_check(&mut run, s.clients, &what);
            let owners: Vec<usize> = (0..run.shards.len())
                .filter(|&i| {
                    run.shards[i]
                        .account_records(account)
                        .resident
                        .is_some_and(|(_, e)| e >= epoch)
                })
                .collect();
            assert_eq!(owners.len(), 1, "{what}: {}", run.describe_account(account));
        }
    }
}

#[test]
fn con_el_directorio_caido_nada_cruza_pero_nada_se_rompe_y_al_volver_todo_sigue() {
    let s = scenario(true);
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 8000));
    run.run_for(6000);
    run.crash_directory(CrashKind::Torn(7));
    let before = run.total_stats().handoffs_completed;
    let mut max_outbox = 0;
    for _ in 0..8 {
        run.run_for(2000);
        max_outbox = max_outbox.max(run.check_accounts().outbox);
    }
    // Sin directorio no se completa ningún traspaso nuevo (como mucho, alguno que ya iba a la mitad)...
    let during = run.total_stats().handoffs_completed - before;
    assert!(
        during <= 4,
        "se completaron {during} traspasos sin directorio"
    );
    // ...y los que se intentan quedan esperando en el origen, sin perder a nadie.
    assert!(max_outbox >= 1, "no hubo ningún traspaso en espera");
    assert_clean(&run, "");
    run.restart_directory().unwrap();
    run.run_for(8000);
    finish_and_check(&mut run, s.clients, "directorio caído");
    assert!(run.total_stats().handoffs_completed > before + during);
}

#[test]
fn una_cuenta_que_entra_dos_veces_a_la_vez_solo_existe_una_vez() {
    let mut s = scenario(true);
    s.clients = 6;
    s.patrollers = 0;
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 8000));
    // Alguien abre el juego otra vez con la cuenta 1000, y por un shard distinto del suyo.
    let n = run.shards.len();
    for k in 1..n {
        run.spawn_duplicate_bot(0, k);
    }
    run.run_for(12_000);
    assert_clean(&run, "");
    let playing_same_account: usize = std::iter::once(&run.bots[0])
        .chain(run.extras.iter())
        .filter(|b| b.client.is_playing())
        .count();
    assert_eq!(
        playing_same_account, 1,
        "la misma cuenta juega {playing_same_account} veces"
    );
    let owners = run
        .shards
        .iter()
        .filter(|sh| sh.simulated_player_ids().contains(&bot_account(0)))
        .count();
    assert_eq!(owners, 1);
    assert_eq!(run.check_accounts().resident_duplicates, 0);
}

#[test]
fn perdido_el_directorio_se_reconstruye_con_lo_que_guardan_los_shards() {
    let s = scenario(true);
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 8000));
    run.run_for(12_000);
    // Se apaga todo y el disco del directorio no está: se arranca otro vacío y se reconstruye.
    for sh in 0..run.shards.len() {
        run.crash_shard(sh, CrashKind::Clean);
    }
    run.replace_directory_with_empty().unwrap();
    assert_eq!(run.dir.as_ref().unwrap().account_count(), 0);
    let report = run.rebuild_directory().unwrap();
    assert_eq!(report.accounts, s.clients, "{report:?}");
    assert!(report.conflicts.is_empty(), "{report:?}");
    for sh in 0..run.shards.len() {
        run.restart_shard(sh).unwrap();
    }
    run.run_for(3000);
    finish_and_check(&mut run, s.clients, "directorio reconstruido");
    assert_eq!(run.dir.as_ref().unwrap().account_count(), s.clients);
}

#[test]
fn si_el_directorio_no_conoce_la_cuenta_el_traspaso_se_cancela_y_el_jugador_sigue_donde_estaba() {
    let s = scenario(true);
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 8000));
    run.run_for(3000);
    // Un directorio vacío (sin reconstruir): para él no existe ninguna cuenta.
    run.replace_directory_with_empty().unwrap();
    run.run_for(20_000);
    let t = run.total_stats();
    assert!(t.dir_unknown > 0, "no hubo ningún «desconocida»: {t:?}");
    // Los traspasos se cancelan (nadie cambia de dueño sin anotación) y nadie pierde su cuenta ni la tiene
    // doble; los jugadores siguen jugando donde estaban.
    assert_clean(&run, "");
    run.settle();
    run.run_for(5000);
    let inv = run.check(false);
    assert_eq!(inv.players_playing, s.clients);
    assert_eq!(inv.accounts.resident_duplicates, 0);
    assert_eq!(inv.accounts.lost, 0);
    assert_eq!(inv.accounts.outbox, 0, "{:?}", inv.accounts);
    assert_eq!((inv.npc_lost, inv.npc_duplicated), (0, 0));
}

// ---------------------------------------------------------------------------------------------------
// Fallos del almacén justo al hacer una promesa
// ---------------------------------------------------------------------------------------------------

/// Quién ve fallar su disco, y en qué momento exacto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailAt {
    /// El origen, cuando el cliente ya fue redirigido y está a punto de pedirle la autoridad.
    SourceBeforeActivate,
    /// El directorio, con la petición de cambio de dueño en camino.
    DirectoryBeforeCommit,
    /// El destino, con el estado de la cuenta en camino.
    DestinationBeforeAdopt,
    /// El destino de un NPC, con el NPC en camino.
    NpcDestination,
}

fn failing_disk_case(directory: bool, at: FailAt) {
    let what = format!("dir={directory} {at:?}");
    let mut s = scenario(directory);
    s.clients = 12;
    s.patrollers = 12;
    s.game.npc_count = 1200;
    s.game.npc_speed = 14.0;
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 8000), "{what}");
    // Quién va a fallar y cuándo.
    let victim: Option<usize>;
    match at {
        FailAt::SourceBeforeActivate => {
            let mut v = None;
            let ok = step_until(&mut run, 40_000, |r| {
                for (i, sh) in r.shards.iter().enumerate() {
                    for a in sh.simulated_player_ids() {
                        if sh.player_handoff_state(a) == Some("redirecting") {
                            v = Some(i);
                            return true;
                        }
                    }
                }
                false
            });
            assert!(ok, "{what}: no se dio el momento");
            victim = v;
            run.media[victim.unwrap()].fail_syncs(1);
        }
        FailAt::DirectoryBeforeCommit => {
            assert!(
                wait_for_moment(&mut run, Before::Directory, 40_000).is_some(),
                "{what}"
            );
            victim = None;
            run.dir_medium.as_ref().unwrap().fail_syncs(1);
        }
        FailAt::DestinationBeforeAdopt => {
            let (_src, dst, _a, _e) = wait_for_moment(&mut run, Before::Adoption, 40_000)
                .unwrap_or_else(|| panic!("{what}: no se dio el momento"));
            victim = Some(dst);
            run.media[dst].fail_syncs(1);
        }
        FailAt::NpcDestination => {
            let mut v = None;
            let ok = step_until(&mut run, 40_000, |r| {
                for sh in &r.shards {
                    if let Some(&(_, to)) = sh.npc_transfers_out().first() {
                        v = Some(to as usize);
                        return true;
                    }
                }
                false
            });
            assert!(ok, "{what}: no hubo traspaso de NPC");
            victim = v;
            run.media[victim.unwrap()].fail_syncs(1);
        }
    }
    run.run_for(3000);
    // El componente cuyo disco falló lo sabe, y la promesa que estaba a punto de hacer se quedó
    // retenida: no salió nada que dependiera de lo que no se pudo anotar.
    match victim {
        Some(v) => {
            assert!(
                run.shards[v].persistence_failed(),
                "{what}: el shard {v} no vio el fallo"
            );
            let st = run.shards[v].persist_stats().unwrap();
            assert!(
                st.deferred_dropped > 0,
                "{what}: no había ninguna promesa en juego: {st:?}"
            );
        }
        None => {
            let d = run.dir.as_ref().unwrap();
            assert!(d.failed(), "{what}: el directorio no vio el fallo");
            assert!(
                d.stats().replies_dropped > 0,
                "{what}: ninguna respuesta retenida: {:?}",
                d.stats()
            );
        }
    }
    // Aun fallado, ni se duplica ni se pierde nada (lo que no se pudo anotar no se prometió).
    assert_clean(&run, &what);
    // Un operador lo reinicia.
    match victim {
        Some(v) => {
            run.crash_shard(v, CrashKind::Clean);
            run.run_for(500);
            run.restart_shard(v).unwrap();
        }
        None => {
            run.crash_directory(CrashKind::Clean);
            run.run_for(500);
            run.restart_directory().unwrap();
        }
    }
    run.run_for(8000);
    finish_and_check(&mut run, s.clients, &what);
}

#[test]
fn un_almacen_que_falla_justo_al_prometer_no_rompe_ninguna_promesa() {
    for directory in [false, true] {
        for at in [
            FailAt::SourceBeforeActivate,
            FailAt::DirectoryBeforeCommit,
            FailAt::DestinationBeforeAdopt,
            FailAt::NpcDestination,
        ] {
            if !directory && at == FailAt::DirectoryBeforeCommit {
                continue;
            }
            failing_disk_case(directory, at);
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// Cierre limpio y caída: qué se conserva
// ---------------------------------------------------------------------------------------------------

/// Distancia entre dos posiciones cuantizadas, en unidades cuantizadas.
fn dist_q(a: [i32; 3], b: [i32; 3]) -> f64 {
    let d: Vec<f64> = (0..3).map(|i| f64::from(a[i]) - f64::from(b[i])).collect();
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

fn dist_f(a: [f32; 3], b: [f32; 3]) -> f64 {
    let d: Vec<f64> = (0..3).map(|i| f64::from(a[i]) - f64::from(b[i])).collect();
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

#[test]
fn un_cierre_limpio_devuelve_el_mundo_exactamente_como_estaba() {
    for directory in [false, true] {
        let mut s = scenario(directory);
        s.game.npc_count = 1000;
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(wait_all_playing(&mut run, 8000));
        run.run_for(7000);
        for sh in 0..run.shards.len() {
            run.run_for(517);
            let npcs = run.shards[sh].npc_dump();
            assert!(npcs.len() > 50, "shard {sh}: casi sin NPC ({})", npcs.len());
            let players: Vec<(u64, [i32; 3])> = run.shards[sh]
                .simulated_player_ids()
                .into_iter()
                .map(|a| (a, run.shards[sh].player_position(a).unwrap()))
                .collect();
            let residents_before = run.shards[sh].resident_accounts();
            let rep = run.restart_shard_cleanly(sh).unwrap();
            assert!(!rep.first_start, "{rep:?}");
            // Los NPC, con todo lo que decide su camino (rumbo, cuenta atrás del giro y generador).
            let after = run.shards[sh].npc_dump();
            assert_eq!(
                after, npcs,
                "directorio {directory}, shard {sh}: los NPC no volvieron iguales"
            );
            // Los jugadores conectados quedaron guardados donde estaban, y nadie más cambió.
            for (a, pos) in &players {
                assert_eq!(
                    run.shards[sh].resident_position(*a),
                    Some(*pos),
                    "directorio {directory}, shard {sh}: la cuenta {a} no quedó donde estaba"
                );
            }
            let residents_after = run.shards[sh].resident_accounts();
            for r in &residents_before {
                assert!(residents_after.contains(r), "se perdió el residente {r:?}");
            }
            assert_eq!(rep.npcs, npcs.len(), "{rep:?}");
            // No se pierde un solo paso: tras el cierre limpio no hay nada que repetir.
            assert_clean(&run, "");
        }
        finish_and_check(&mut run, s.clients, &format!("directorio {directory}"));
    }
}

#[test]
fn una_caida_pierde_como_mucho_el_ultimo_intervalo_de_movimiento() {
    let s = scenario(false);
    let pc = s.persist.clone().unwrap();
    let tick_s = s.game.tick_ms as f64 / 1000.0;
    let mut run = ClusterRun::new(&s).unwrap();
    assert!(wait_all_playing(&mut run, 8000));
    run.run_for(8000);
    // Lo máximo que puede haberse movido algo desde su última anotación: un intervalo de guardado más
    // lo que aún no estaba sincronizado, con dos ticks de margen.
    let npc_bound = f64::from(s.game.npc_speed)
        * tick_s
        * f64::from(pc.npc_checkpoint_ticks + pc.sync_ticks + 2);
    let player_bound =
        f64::from(STEP_Q) * f64::from(pc.player_save_ticks + pc.sync_ticks + s.game.credit_cap + 2);
    let mut moved_npcs = 0usize;
    let mut moved_players = 0usize;
    for round in 0..8 {
        let sh = round % run.shards.len();
        run.run_for(311 + 97 * round as u64);
        let npcs = run.shards[sh].npc_dump();
        let players: Vec<(u64, [i32; 3])> = run.shards[sh]
            .simulated_player_ids()
            .into_iter()
            .map(|a| (a, run.shards[sh].player_position(a).unwrap()))
            .collect();
        run.crash_shard(sh, CrashKind::Clean);
        run.run_for(200);
        run.restart_shard(sh).unwrap();
        let after = run.shards[sh].npc_dump();
        // Ningún NPC aparece ni desaparece por la caída.
        let ids = |v: &[tsofgw_online::NpcDump]| v.iter().map(|n| n.id).collect::<Vec<_>>();
        assert_eq!(
            ids(&after),
            ids(&npcs),
            "ronda {round}, shard {sh}: cambió el conjunto de NPC"
        );
        for (b, a) in npcs.iter().zip(&after) {
            let d = dist_f(b.pos, a.pos);
            assert!(
                d <= npc_bound,
                "NPC {}: retrocedió {d:.1} (cota {npc_bound:.1})",
                b.id
            );
            if b.pos != a.pos {
                moved_npcs += 1;
            }
        }
        for (acc, pos) in &players {
            let rec = run.shards[sh]
                .resident_position(*acc)
                .unwrap_or_else(|| panic!("ronda {round}: la cuenta {acc} no tenía registro"));
            let d = dist_q(*pos, rec);
            assert!(
                d <= player_bound,
                "cuenta {acc}: retrocedió {d:.1} (cota {player_bound:.1})"
            );
            if *pos != rec {
                moved_players += 1;
            }
        }
        run.run_for(1500);
    }
    // La prueba no es vacua: la caída sí perdió movimiento, y aun así dentro de la cota.
    assert!(
        moved_npcs > 0,
        "ninguna caída perdió movimiento de NPC: la cota no se ejercita"
    );
    assert!(
        moved_players > 0,
        "ninguna caída perdió movimiento de jugadores"
    );
    finish_and_check(&mut run, s.clients, "tras las caídas");
}

// ---------------------------------------------------------------------------------------------------
// Mensajes viejos o repetidos
// ---------------------------------------------------------------------------------------------------

#[test]
fn un_traspaso_viejo_o_repetido_no_resucita_ni_hace_retroceder_a_nadie() {
    for directory in [false, true] {
        let s = scenario(directory);
        let mut run = ClusterRun::new(&s).unwrap();
        assert!(wait_all_playing(&mut run, 8000));
        // Esperar a que una cuenta cambie de shard de verdad: el origen guarda su lápida y el
        // destino la tiene viva, conectada y con la época nueva.
        let mut found: Option<(usize, usize, u64, u64)> = None;
        let ok = step_until(&mut run, 90_000, |r| {
            for (i, sh) in r.shards.iter().enumerate() {
                for k in 0..s.clients {
                    let acc = bot_account(k);
                    let Some((e, to)) = sh.account_records(acc).tombstone else {
                        continue;
                    };
                    let d = &r.shards[to as usize];
                    if d.simulated_player_ids().contains(&acc)
                        && d.account_records(acc).resident.map(|x| x.1) == Some(e)
                    {
                        found = Some((i, to as usize, acc, e));
                        return true;
                    }
                }
            }
            false
        });
        assert!(ok, "directorio {directory}: ninguna cuenta cambió de shard");
        let (src, dst, acc, e) = found.unwrap();
        let other = (0..run.shards.len())
            .find(|&x| x != src && x != dst)
            .unwrap();
        run.run_for(1500); // que se mueva: la posición viva ya no es la del traspaso
        let live = run.shards[dst].player_position(acc).unwrap();
        let far = [live[0] + 40_000, live[1], live[2] + 40_000];
        let stale = |st: &tsofgw_online::ClusterStats| (st.handovers_stale, st.handover_conflicts);
        let (d0, s0) = (
            stale(run.shards[dst].cluster_stats().unwrap()),
            stale(run.shards[src].cluster_stats().unwrap()),
        );
        let rec_dst = run.shards[dst].account_records(acc);
        let rec_src = run.shards[src].account_records(acc);

        // El mismo traspaso otra vez, uno de una época anterior, y otro de un tercer shard: todos
        // llegan al destino, que ya está en la época `e`.
        let send = |run: &ClusterRun, from: usize, to: usize, xfer: u32, epoch: u64| {
            let msg = PeerMsg::Handover {
                xfer,
                player: acc,
                pos: far,
                last_processed: 0,
                epoch,
            };
            run.bus
                .end(from as u16)
                .send(run.now, to as u16, msg.encode());
        };
        send(&run, src, dst, 9001, e);
        send(&run, src, dst, 9002, e - 1);
        send(&run, other, dst, 9003, e);
        // Y al origen, que ya no la tiene: uno de vuelta y otro de un tercero, con la época de su lápida.
        send(&run, dst, src, 9004, e);
        send(&run, other, src, 9005, e);
        run.run_for(400);

        let (d1, s1) = (
            stale(run.shards[dst].cluster_stats().unwrap()),
            stale(run.shards[src].cluster_stats().unwrap()),
        );
        assert_eq!(
            d1.0 - d0.0,
            3,
            "directorio {directory}: el destino debía dar por repetidos los 3"
        );
        assert_eq!(
            s1.0 - s0.0,
            2,
            "directorio {directory}: el origen debía dar por repetidos los 2"
        );
        assert_eq!(
            (d1.1, s1.1),
            (d0.1, s0.1),
            "nada de esto es un conflicto: ya se conocía"
        );
        // La cuenta ni se movió de golpe ni se duplicó ni se resucitó.
        let now = run.shards[dst].player_position(acc).unwrap();
        // Se movió lo que andando a toda velocidad pueden dar 400 ms (más la ráfaga tolerada); el
        // traspaso falso la habría llevado a 56 000 unidades de allí.
        let walked =
            f64::from(STEP_Q) * (400 / s.game.tick_ms + u64::from(s.game.credit_cap) + 4) as f64;
        assert!(
            dist_q(now, live) <= walked,
            "directorio {directory}: la cuenta saltó de {live:?} a {now:?}"
        );
        assert_eq!(
            run.shards[dst].account_records(acc).resident.map(|x| x.1),
            Some(e)
        );
        assert_eq!(run.shards[dst].account_records(acc).outbox, rec_dst.outbox);
        assert_eq!(
            run.shards[src].account_records(acc),
            rec_src,
            "el origen cambió su registro"
        );
        assert!(!run.shards[src].owns_player(acc));
        assert_clean(&run, "");

        // Un traspaso con una época mayor que la que ya tiene una cuenta conectada aquí sería una
        // segunda copia: se rechaza y se cuenta como conflicto, sin tocar nada.
        let c0 = run.shards[dst].cluster_stats().unwrap().handover_conflicts;
        send(&run, src, dst, 9006, e + 5);
        run.run_for(400);
        let c1 = run.shards[dst].cluster_stats().unwrap().handover_conflicts;
        assert_eq!(
            c1 - c0,
            1,
            "directorio {directory}: no se contó el conflicto"
        );
        assert_eq!(
            run.shards[dst].account_records(acc).resident.map(|x| x.1),
            Some(e)
        );
        assert_clean(&run, "");
        finish_and_check(&mut run, s.clients, &format!("directorio {directory}"));
    }
}
