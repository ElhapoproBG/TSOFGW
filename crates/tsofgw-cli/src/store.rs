//! El directorio de sesiones y las herramientas del almacén durable: `directory`, `store info`,
//! `store rebuild-directory`.

use std::time::{Duration, Instant};

use tsofgw_online::partition::{Partition, ShardId};
use tsofgw_online::persist::store_key_for;
use tsofgw_online::{
    rebuild_from_shards, summarize_directory_store, summarize_shard_store, ClusterConfig,
    Directory, DirectoryConfig, UdpBus,
};
use tsofgw_store::{DynStore, OpenMode, Store, StoreConfig};

use crate::online::{
    exposure_warning, game_config, key_from_file, master_key, open_medium, parse_addr_list,
    parse_grid, reject_unknown,
};
use crate::{flag, has, num};

fn open_mode(args: &[String]) -> OpenMode {
    if has(args, "--repair") {
        OpenMode::Repair
    } else {
        OpenMode::Recover
    }
}

fn cluster_key(args: &[String]) -> Result<Vec<u8>, String> {
    let path = flag(args, "--cluster-key-file").ok_or(
        "falta --cluster-key-file (la misma clave de cluster que usan los shards y el directorio)",
    )?;
    key_from_file(&path, "la clave de cluster")
}

/// `tsofgw directory`: el directorio de sesiones de un cluster. Dice en qué shard vive cada cuenta y
/// arbitra los cambios de shard; guarda su mapa en `--data-dir` y solo responde lo que ya está en disco.
///
/// Debe arrancarse con la misma rejilla, la misma lista de shards, la misma clave de cluster y los mismos
/// parámetros de mundo (`--npcs`, `--radius`, `--sim-radius`, `--tick-ms`, `--seed`) que los shards: el bus
/// descarta lo que viene de un proceso con otra configuración.
pub fn cmd_directory(args: &[String]) -> Result<(), String> {
    reject_unknown(
        args,
        &[
            "--grid",
            "--peers",
            "--bind",
            "--replicas",
            "--replica",
            "--cluster-key-file",
            "--data-dir",
            "--duration",
            "--compact-mb",
            "--npcs",
            "--radius",
            "--sim-radius",
            "--tick-ms",
            "--seed",
        ],
        &["--repair"],
    )?;
    let (cols, rows) = parse_grid(
        &flag(args, "--grid").ok_or("falta --grid COLUMNASxFILAS (la misma que los shards)")?,
        "--grid",
    )?;
    let peers = parse_addr_list(
        &flag(args, "--peers")
            .ok_or("falta --peers DIR0,DIR1,... (el bus de cada shard, en orden)")?,
        "--peers",
    )?;
    let key = cluster_key(args)?;
    let dir = flag(args, "--data-dir").ok_or(
        "falta --data-dir (el directorio guarda dónde vive cada cuenta: sin disco no sirve)",
    )?;
    // Grupo replicado: `--replicas A0,A1,A2 --replica I` (las direcciones de bus de todas, en orden, y cuál es esta).
    let replicas = flag(args, "--replicas")
        .map(|l| parse_addr_list(&l, "--replicas"))
        .transpose()?;
    let replica: usize = num(args, "--replica", 0)?;
    let nrep = replicas.as_ref().map_or(1, Vec::len);
    match &replicas {
        Some(l) => {
            if !matches!(l.len(), 3 | 5 | 7) {
                return Err(format!(
                    "--replicas: un grupo de 3, 5 o 7 réplicas (hay {}); para una sola usa --bind",
                    l.len()
                ));
            }
            if flag(args, "--replica").is_none() {
                return Err("con --replicas indica cuál es esta con --replica I (0-based)".into());
            }
            if replica >= l.len() {
                return Err(format!(
                    "--replica {replica}: el grupo tiene {} réplicas (0..{})",
                    l.len(),
                    l.len() - 1
                ));
            }
        }
        None => {
            if flag(args, "--replica").is_some() {
                return Err("--replica solo tiene sentido con --replicas".into());
            }
        }
    }
    let bind = match (flag(args, "--bind"), &replicas) {
        (Some(b), _) => b,
        (None, Some(l)) => l[replica].to_string(),
        (None, None) => {
            return Err(
                "falta --bind DIR (la dirección del bus del directorio: la que reciben los shards con --directory)"
                    .into(),
            )
        }
    };
    let duration: u64 = num(args, "--duration", 0)?;
    let compact_mb: u64 = num(
        args,
        "--compact-mb",
        DirectoryConfig::default().compact_bytes >> 20,
    )?;
    if !(1..=4096).contains(&compact_mb) {
        return Err("--compact-mb debe estar entre 1 y 4096".into());
    }
    let game = game_config(args, 1)?;
    let partition = Partition::world_grid(cols, rows)?;
    let n = partition.shard_count();
    if peers.len() != n {
        return Err(format!(
            "--peers debe tener {n} direcciones (una por shard) y tiene {}",
            peers.len()
        ));
    }
    // El directorio comparte con los shards el resumen de la configuración del mundo (con la marca de
    // que hay directorio): así el bus descarta a quien venga con otra.
    let mut cluster = ClusterConfig::new(0, partition, &game)?;
    cluster.directory = true;
    cluster.dir_replicas = nrep;
    let mut bus = UdpBus::bind((n + replica) as ShardId, &bind, n + nrep, &key)?
        .with_config_digest(cluster.digest(&game));
    for (j, a) in peers.iter().enumerate() {
        bus.set_peer(j as ShardId, *a)?;
    }
    if let Some(l) = &replicas {
        for (j, a) in l.iter().enumerate() {
            if j != replica {
                bus.set_peer((n + j) as ShardId, *a)?;
            }
        }
    }
    let local = bus
        .local_addr()
        .map_err(|e| format!("no se pudo leer la dirección del bus: {e}"))?;
    let medium = open_medium(&dir)?;
    let (mut d, rec) = Directory::open(
        Box::new(medium),
        &Directory::store_key(&key),
        Box::new(bus),
        n,
        DirectoryConfig {
            compact_bytes: compact_mb << 20,
            replicas: nrep,
            replica,
            ..DirectoryConfig::default()
        },
        open_mode(args),
    )?;
    println!(
        "TSOFGW directorio de sesiones escuchando en {local} (cluster de {n} shards, {} cuentas conocidas). Ctrl-C para detener.",
        d.account_count()
    );
    if rec.created {
        println!("Persistencia en {dir}: primer arranque.");
    } else {
        println!(
            "Persistencia en {dir}: restaurado — {} cuentas (generación {}, {} registros del diario).{}",
            d.account_count(),
            rec.generation,
            rec.records,
            if rec.torn_bytes > 0 {
                format!(" Se descartó una cola rota de {} bytes.", rec.torn_bytes)
            } else {
                String::new()
            }
        );
    }
    if rec.repaired_bytes > 0 {
        eprintln!(
            "ADVERTENCIA: --repair descartó {} bytes dañados en medio del diario; lo que venía después se perdió.",
            rec.repaired_bytes
        );
    }
    if let Some(aviso) = exposure_warning(&[("bus del directorio", local)]) {
        eprintln!("{aviso}");
    }
    tsofgw_sys::stop::install();
    let start = Instant::now();
    let mut last_report = 0u64;
    let mut warned = false;
    loop {
        let now = start.elapsed().as_millis() as u64;
        d.update(now);
        if now - last_report >= 5000 {
            last_report = now;
            let st = d.stats();
            if nrep > 1 {
                println!(
                    "[{:>5} s] réplica {replica}/{nrep}: {} | término {} | aplicado hasta {} | diario {} entradas | propuestas {} redirigidas {} sin líder {}",
                    now / 1000,
                    if d.is_leader() { "LÍDER" } else { "seguidora" },
                    d.raft_term(),
                    d.applied_index(),
                    d.raft_log_len(),
                    st.proposed,
                    st.redirects,
                    st.no_leader
                );
            }
            println!(
                "[{:>5} s] cuentas {} (por shard {:?}) | entradas {} | cambios de dueño: confirmados {}, repetidos {}, rechazados {}, desconocidos {} | mal formados {} | commits {}",
                now / 1000,
                d.account_count(),
                d.accounts_per_shard(),
                st.logins,
                st.committed,
                st.idempotent,
                st.rejected,
                st.unknown,
                st.malformed,
                st.commits
            );
        }
        if d.failed() && !warned {
            warned = true;
            eprintln!(
                "ERROR: el almacén del directorio falló al escribir. Ya no responde nada que no esté en disco, así que \
                 nadie puede entrar ni cambiar de shard con una cuenta hasta que se reinicie. Revisa el disco."
            );
        }
        if tsofgw_sys::stop::requested() {
            println!("Parada pedida: cerrando el directorio...");
            break;
        }
        if duration > 0 && now >= duration * 1000 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    d.shutdown()?;
    println!("Cierre limpio: almacén del directorio sincronizado y compactado.");
    Ok(())
}

/// `tsofgw store info|rebuild-directory ...`
pub fn cmd_store(args: &[String]) -> Result<(), String> {
    match args.split_first() {
        Some((sub, rest)) if sub == "info" => cmd_store_info(rest),
        Some((sub, rest)) if sub == "rebuild-directory" => cmd_rebuild_directory(rest),
        Some((sub, _)) => Err(format!(
            "subcomando desconocido: store {sub} (usa `store info` o `store rebuild-directory`)"
        )),
        None => Err("falta el subcomando: `store info` o `store rebuild-directory`".into()),
    }
}

/// ¿Hay ya un almacén en la carpeta? Se mira sin abrir nada: abrir una carpeta vacía crearía uno, y una
/// herramienta de consulta no debe dejar un almacén vacío donde el operador se equivocó de ruta.
fn has_store(dir: &str) -> bool {
    std::fs::read_dir(dir)
        .map(|it| {
            it.flatten().any(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.starts_with("snap-") && n.ends_with(".bin")
            })
        })
        .unwrap_or(false)
}

/// Abre un almacén ya existente con su clave. Nunca crea uno nuevo.
fn open_existing(
    dir: &str,
    key: &[u8; 32],
    mode: OpenMode,
) -> Result<(DynStore, tsofgw_store::Recovery), String> {
    if !has_store(dir) {
        return Err(format!(
            "{dir} no contiene ningún almacén (¿carpeta equivocada?); no se ha creado nada"
        ));
    }
    let medium: Box<dyn tsofgw_store::Medium> = Box::new(open_medium(dir)?);
    let (store, rec) = Store::open(
        medium,
        key,
        StoreConfig {
            mode,
            ..StoreConfig::default()
        },
    )
    .map_err(|e| format!("no se pudo abrir el almacén de {dir}: {e}"))?;
    Ok((store, rec))
}

/// `tsofgw store info --data-dir D --cluster-key-file K (--shard I | --directory)`
/// `tsofgw store info --data-dir D --master-file K --standalone`
///
/// Abre la carpeta como lo haría el servidor al arrancar (descarta una cola rota por una caída) y cuenta
/// lo que hay. No la uses con el servidor en marcha: su bloqueo no te dejará.
fn cmd_store_info(args: &[String]) -> Result<(), String> {
    reject_unknown(
        args,
        &[
            "--data-dir",
            "--cluster-key-file",
            "--master-file",
            "--shard",
        ],
        &["--directory", "--standalone", "--repair"],
    )?;
    let dir = flag(args, "--data-dir").ok_or("falta --data-dir")?;
    let roles = [
        flag(args, "--shard").is_some(),
        has(args, "--directory"),
        has(args, "--standalone"),
    ];
    if roles.iter().filter(|&&r| r).count() != 1 {
        return Err("indica de quién es la carpeta: --shard I, --directory o --standalone".into());
    }
    let mode = open_mode(args);
    if let Some(s) = flag(args, "--shard") {
        let shard: ShardId = s.parse().map_err(|_| format!("--shard inválido: {s}"))?;
        let key = store_key_for(&cluster_key(args)?, Some(shard));
        let (store, rec) = open_existing(&dir, &key, mode)?;
        print_store_header(&format!("shard {shard}"), &dir, &store, &rec);
        let r = summarize_shard_store(&store);
        println!(
            "  mundo inicializado: {} | último tick guardado: {}",
            if r.initialized {
                "sí"
            } else {
                "NO (¿no es el almacén de un shard?)"
            },
            r.tick.map_or("ninguno".into(), |t| t.to_string())
        );
        println!(
            "  NPC: {} | traspasos de NPC sin acuse: {} | traspasos de NPC adoptados que se recuerdan: {}",
            r.npcs, r.npc_transfers_out, r.adopted_transfers
        );
        println!(
            "  cuentas residentes: {} | traspasos de jugador sin acuse: {} (esperando al directorio: {}) | lápidas: {}",
            r.residents, r.handovers, r.handovers_awaiting_directory, r.tombstones
        );
        if r.unknown > 0 {
            println!(
                "  ADVERTENCIA: {} claves que no se reconocen (¿otra versión?)",
                r.unknown
            );
        }
    } else if has(args, "--directory") {
        let key = Directory::store_key(&cluster_key(args)?);
        let (store, rec) = open_existing(&dir, &key, mode)?;
        print_store_header("directorio de sesiones", &dir, &store, &rec);
        let r = summarize_directory_store(&store);
        match r.shards {
            Some(n) => println!(
                "  cluster de {n} shards | cuentas: {} (por shard {:?})",
                r.accounts, r.per_shard
            ),
            None => println!("  ADVERTENCIA: sin registro de identidad (¿no es un directorio?)"),
        }
        if r.unknown > 0 {
            println!("  ADVERTENCIA: {} registros que no se entienden", r.unknown);
        }
    } else {
        let key = store_key_for(&master_key(args)?, None);
        let (store, rec) = open_existing(&dir, &key, mode)?;
        print_store_header("servidor suelto", &dir, &store, &rec);
        let r = summarize_shard_store(&store);
        println!(
            "  mundo inicializado: {} | último tick guardado: {}",
            if r.initialized {
                "sí"
            } else {
                "NO (¿no es el almacén de un servidor?)"
            },
            r.tick.map_or("ninguno".into(), |t| t.to_string())
        );
        println!("  NPC: {} | cuentas residentes: {}", r.npcs, r.residents);
    }
    Ok(())
}

fn print_store_header(what: &str, dir: &str, store: &DynStore, rec: &tsofgw_store::Recovery) {
    let st = store.stats();
    println!(
        "Almacén de {what} en {dir} (generación {})",
        store.generation()
    );
    println!(
        "  recuperación: {} registros del diario repetidos ({} operaciones), cola rota descartada: {} bytes, reparado: {} bytes",
        rec.records, rec.ops, rec.torn_bytes, rec.repaired_bytes
    );
    println!(
        "  diario desde el último checkpoint: {:.1} KB | {} entradas en el checkpoint | {} claves en total",
        store.log_len() as f64 / 1000.0,
        rec.snapshot_entries,
        store.len()
    );
    let _ = st;
}

/// `tsofgw store rebuild-directory --shard-dirs D0,D1,.. --data-dir DIR --cluster-key-file K`
///
/// Reconstruye el directorio de sesiones a partir de lo que guarda cada shard (cada uno sabe qué cuentas
/// tiene y cuáles está entregando). Para cuando se pierde la carpeta del directorio. **Todos los shards
/// deben estar parados**, y la carpeta del directorio, vacía.
fn cmd_rebuild_directory(args: &[String]) -> Result<(), String> {
    reject_unknown(
        args,
        &["--shard-dirs", "--data-dir", "--cluster-key-file"],
        &["--repair", "--accept-conflicts"],
    )?;
    let list = flag(args, "--shard-dirs")
        .ok_or("falta --shard-dirs D0,D1,... (la carpeta de datos de cada shard, en orden)")?;
    let dirs: Vec<String> = list.split(',').map(|d| d.trim().to_string()).collect();
    if dirs.iter().any(|d| d.is_empty()) || dirs.len() > 256 {
        return Err(
            "--shard-dirs: una carpeta por shard (entre 1 y 256), separadas por comas".into(),
        );
    }
    let out = flag(args, "--data-dir").ok_or("falta --data-dir (la carpeta del directorio)")?;
    if dirs.contains(&out) {
        return Err("--data-dir no puede ser la carpeta de ningún shard".into());
    }
    let key = cluster_key(args)?;
    let mode = open_mode(args);
    let mut stores: Vec<(ShardId, DynStore)> = Vec::new();
    for (i, d) in dirs.iter().enumerate() {
        let shard = i as ShardId;
        let (store, _) = open_existing(d, &store_key_for(&key, Some(shard)), mode)
            .map_err(|e| format!("shard {i}: {e}"))?;
        if !summarize_shard_store(&store).initialized {
            return Err(format!("shard {i}: {d} no es el almacén de un shard"));
        }
        println!(
            "shard {i}: {d} leído ({} cuentas residentes)",
            summarize_shard_store(&store).residents
        );
        stores.push((shard, store));
    }
    let view: Vec<(ShardId, &DynStore)> = stores.iter().map(|(s, st)| (*s, st)).collect();
    let (entries, report) = rebuild_from_shards(&view)?;
    println!(
        "Reconstrucción: {} cuentas ({} residentes, {} con un traspaso en curso, {} en conflicto)",
        report.accounts,
        report.residents,
        report.in_transit,
        report.conflicts.len()
    );
    if !report.conflicts.is_empty() {
        let shown: Vec<String> = report
            .conflicts
            .iter()
            .take(20)
            .map(u64::to_string)
            .collect();
        println!(
            "  cuentas reclamadas por dos shards con la misma época (no se sabe cuál es la buena): {}{}",
            shown.join(", "),
            if report.conflicts.len() > 20 { ", ..." } else { "" }
        );
        if !has(args, "--accept-conflicts") {
            return Err(
                "hay cuentas en conflicto: revísalas en los shards, o repite con --accept-conflicts para anotarlas \
                 en el shard donde están residentes (el de menor número si son varios)"
                    .into(),
            );
        }
    }
    let medium = open_medium(&out)?;
    let (mut d, _) = Directory::open_offline(
        Box::new(medium),
        &Directory::store_key(&key),
        dirs.len(),
        mode,
    )?;
    let n = d.import(&entries)?;
    d.shutdown()?;
    println!(
        "Directorio escrito en {out}: {n} cuentas. Ya puedes arrancar el directorio y los shards."
    );
    Ok(())
}
